//! Sound output: ringtones, call-progress tones and the two hardware test tones.
//!
//! Playback is done in-process: files are decoded with symphonia and written
//! straight to ALSA. The `i2s` and `usb` names are the ALSA aliases defined in
//! the phone's `asound.conf`, opened through the `plug:` wrapper so that
//! alsa-lib converts rate, sample format and channel count. That is the same
//! trick [`crate::callaudio`] uses for call audio.
//!
//! The call-progress tones are not files: they are plain tone sequences, so
//! they are rendered into memory as PCM by [`Tone`] instead of being decoded.
//! A sequence repeats until playback is stopped, which is what the phone wants
//! from a dial tone anyway - how long it runs is the phone's decision, not the
//! length of a sound file.
//!
//! ALSA reads and writes block, so every sound plays on its own thread, never
//! on one of the async runtime's workers.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use crate::sync::lock;

/// ALSA device names, matching the `asound.conf` setup of the phone.
const SPEAKER_DEVICE: &str = "i2s";
const EARPIECE_DEVICE: &str = "usb";

/// Sample rate the generated tones are computed at.
///
/// Nothing reads this off a sound card or a file, so it is simply the rate the
/// sine is generated with. The `plug:` wrapper resamples it to whatever the
/// card runs at.
const TONE_RATE: u32 = 48_000;

/// Channel count of the generated tones.
///
/// The USB codec in the earpiece runs single channel; the wrapper mixes to
/// whatever it wants.
#[cfg(target_os = "linux")]
const TONE_CHANNELS: u32 = 1;

/// Peak amplitude of a generated tone, about a quarter of full scale.
///
/// The tones share the earpiece with the call audio and the test tone, so they
/// are kept well below clipping rather than being as loud as they could be.
const TONE_AMPLITUDE: f64 = 12_000.0;

/// Fade at both ends of a tone step, in milliseconds.
///
/// A tone that begins or ends at full amplitude clicks, which is painfully
/// audible in an earpiece held against the ear.
const TONE_FADE_MS: u32 = 5;

/// Samples handed to ALSA at a time, in short slices so that a stop request is
/// honoured within a few milliseconds.
#[cfg(target_os = "linux")]
const TONE_CHUNK: usize = 960;

/// How long the playback stub sleeps between checks for a stop request.
///
/// Only used off the phone, where there is no sound card to block on.
#[cfg(not(target_os = "linux"))]
const POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// One playback channel, tracked independently of the other.
#[derive(Clone, Default)]
pub struct Channel {
    /// The sound currently playing, if any.
    playback: Arc<Mutex<Option<Playback>>>,
    /// Serialises start/stop so two near-simultaneous calls cannot leave two
    /// threads fighting over the sound card.
    gate: Arc<Mutex<()>>,
}

/// A sound being played, from the point of view of the threads waiting for it.
struct Playback {
    /// Set by [`Channel::stop`] to make the playback thread finish early.
    stop: Arc<AtomicBool>,
    /// Signalled once the thread is done, whether it finished or was stopped.
    done: Arc<Done>,
}

/// Signals that the playback thread has released the sound card.
///
/// A condvar rather than a bare flag, so [`Channel::stop`] does not have to
/// poll while it waits for the thread's current ALSA write to return.
#[derive(Default)]
struct Done {
    finished: Mutex<bool>,
    signal: Condvar,
}

impl Done {
    /// Mark the thread as finished and wake everyone waiting on it.
    fn finish(&self) {
        *lock(&self.finished) = true;
        self.signal.notify_all();
    }

    /// Has the thread finished?
    fn is_finished(&self) -> bool {
        *lock(&self.finished)
    }

    /// Block until the thread has finished.
    fn wait(&self) {
        let mut finished = lock(&self.finished);
        while !*finished {
            finished = self.signal.wait(finished).unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Channel {
    /// Start `path` on `device`, replacing whatever was playing.
    ///
    /// With `repeat`, the sound is followed by a second of silence (the pause
    /// between rings) and played again, indefinitely.
    fn start(&self, path: &str, device: &str, repeat: bool) {
        if path.is_empty() {
            // Nothing configured: whatever was playing stops here.
            self.stop();
            return;
        }

        let channel = channel_name(device);
        let what = path.to_string();
        let (path, device) = (path.to_string(), device.to_string());
        self.start_with(channel, what, move |stop| play(&path, &device, repeat, stop));
    }

    /// Start a generated `tone` on `device`, replacing whatever was playing.
    ///
    /// The sequence repeats until the channel is stopped, so the tone lasts as
    /// long as the phone needs it.
    fn start_tone(&self, tone: Tone, device: &'static str) {
        let what = tone.label().to_string();
        self.start_with(channel_name(device), what, move |stop| play_tone(tone, device, stop));
    }

    /// Run one sound on a thread of its own, replacing whatever was playing.
    ///
    /// `what` names the sound in a log line, `channel` the thread. The body
    /// plays until it is done or `stop` is set; the channel learns which it
    /// was from the completion signal, not from the return value.
    fn start_with<F>(&self, channel: &str, what: String, body: F)
    where
        F: FnOnce(&AtomicBool) -> Result<(), String> + Send + 'static,
    {
        let _gate = lock(&self.gate);
        self.stop();

        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(Done::default());

        // The thread owns the ALSA stream and exits once `stop` is set. Its
        // handle is dropped, which detaches it: every path that has to wait for
        // the sound card does so on `done` instead.
        let flag = stop.clone();
        let finished = done.clone();
        let label = what.clone();
        let result = std::thread::Builder::new()
            .name(format!("audio-{}", channel))
            .spawn(move || {
                let outcome = body(&flag);
                if let Err(e) = outcome {
                    // Nothing to do but report: the caller has already moved
                    // on, and a missing sound must not take the phone down.
                    println!("Klang {} konnte nicht abgespielt werden: {}", label, e);
                }
                // Release anyone waiting in `wait` or `stop`.
                finished.finish();
            });

        match result {
            Ok(_) => {
                *lock(&self.playback) =
                    Some(Playback { stop, done });
            }
            Err(e) => println!("Klang {} konnte nicht gestartet werden: {}", what, e),
        }
    }

    /// Stop whatever is playing on this channel.
    pub fn stop(&self) {
        // Take the playback out before signalling it: waiting for the thread
        // below must not hold the lock, or `wait` could never see the thread
        // finish.
        let playback = lock(&self.playback).take();
        if let Some(playback) = playback {
            playback.stop.store(true, Ordering::SeqCst);
            // Wait for the thread to release the sound card before returning. A
            // caller that starts the next sound straight away must not race the
            // old stream for the device. The thread signals once it is done,
            // which is as soon as its current ALSA write returns.
            playback.done.wait();
        }
    }

    /// Wait for the current sound to finish.
    ///
    /// Used for confirmation tones and before the system goes down. Another
    /// thread can still call [`Channel::stop`] meanwhile, which cuts the sound
    /// short - that is how lifting the handset stops the sleep music.
    pub fn wait(&self) {
        let done = {
            let slot = lock(&self.playback);
            // `None` means nothing is playing, or another thread already stopped
            // and reaped it.
            match slot.as_ref() {
                None => return,
                Some(playback) => playback.done.clone(),
            }
        };

        done.wait();
    }

    /// Is a sound currently playing?
    ///
    /// The finished flag is checked as well as the slot: a sound that ran to its
    /// end is over even though nobody has taken it out of the channel yet.
    pub fn is_playing(&self) -> bool {
        let slot = lock(&self.playback);
        match slot.as_ref() {
            None => false,
            Some(playback) => !playback.done.is_finished(),
        }
    }
}

/// The thread name of one of the two playback devices.
fn channel_name(device: &str) -> &'static str {
    if device == SPEAKER_DEVICE {
        "speaker"
    } else {
        "earpiece"
    }
}

/// Decode and play one file, optionally looping it.
///
/// Runs on its own thread. Returns once the sound is over, or as soon as `stop`
/// is set.
#[cfg(target_os = "linux")]
fn play(path: &str, device: &str, repeat: bool, stop: &AtomicBool) -> Result<(), String> {
    use std::fs::File;

    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::errors::Error as SymphoniaError;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    // Open through the `plug:` wrapper so alsa-lib, not this code, adapts the
    // file's rate, sample format and channel count to the card.
    let pcm = alsa::pcm::PCM::new(&format!("plug:{}", device), alsa::Direction::Playback, false)
        .map_err(|e| format!("Gerät \"{}\" nicht verfügbar: {}", device, e))?;

    let file = File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    // The extension is only a hint; the probe decides by content, which also
    // covers files whose name and format disagree.
    let mut hint = Hint::new();
    if let Some(ext) = path.rsplit('.').next().filter(|e| !e.is_empty() && *e != path) {
        hint.with_extension(ext);
    }

    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| format!("nicht lesbar: {}", e))?;

    // Everything the stream needs is read here, in its own scope: `track` borrows
    // `format`, which has to be free again for `next_packet` in the loop below.
    let (mut decoder, rate, channels) = {
        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| "keine Tonspur".to_string())?;

        let params = match track.codec_params.as_ref() {
            Some(CodecParameters::Audio(params)) => params,
            _ => return Err("keine Tonspur".to_string()),
        };

        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(|e| format!("kein passender Decoder: {}", e))?;

        (
            decoder,
            params.sample_rate.unwrap_or(48_000),
            params.channels.as_ref().map(|c| c.count()).unwrap_or(2).max(1),
        )
    };

    // Ask ALSA for the file's own format. The plug wrapper converts it to
    // whatever the hardware actually wants.
    configure(&pcm, rate, channels as u32)?;
    pcm.start().map_err(|e| format!("Wiedergabe nicht startbar: {}", e))?;

    let io = pcm.io_i16().map_err(|e| format!("Wiedergabe-Format falsch: {}", e))?;

    // The silence between two rings, appended after every pass.
    let gap = silence(rate, channels as u32);

    // One buffer for the whole sound: the decoder hands over a packet at a time,
    // and a fresh `Vec` per packet is an allocation per few milliseconds for as
    // long as the sound plays.
    let mut converted: Vec<i16> = Vec::new();

    loop {
        // Decode and write one pass over the file.
        let mut failure = None;

        loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }

            let packet = match format.next_packet() {
                Ok(Some(packet)) => packet,
                // End of this pass. Not an error.
                Ok(None) => break,
                Err(e) => {
                    failure = Some(e.to_string());
                    break;
                }
            };

            match decoder.decode(&packet) {
                Ok(decoded) => {
                    if write(&io, &decoded, channels, &mut converted).is_err() {
                        // The stream is gone. Give up on the sound entirely
                        // rather than fall through to the next repeat, which
                        // would spin on an already empty file.
                        return Ok(());
                    }
                }
                // A corrupt frame is skipped rather than ending the sound.
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(e) => {
                    failure = Some(e.to_string());
                    break;
                }
            }
        }

        if let Some(e) = failure {
            return Err(e);
        }

        if !repeat || stop.load(Ordering::SeqCst) {
            // Everything is queued, but ALSA still holds part of it in the
            // hardware buffer. Without draining, `wait` would return while the
            // last fraction of a second is still being played - audible as a
            // confirmation tone that gets cut off. Draining blocks until the
            // card has played everything, so the sound really is over.
            return drain(&pcm, stop);
        }

        // The pause between two rings. If it gets cut short, playback is being
        // stopped and the next round will notice.
        if write_samples(&io, &gap, channels).is_err() {
            return Ok(());
        }
    }
}

/// Wait for the hardware buffer to play out, giving up as soon as `stop` is set.
///
/// Blocking drain is what makes a sound last as long as the file says it does,
/// but it would also make `stop` wait out the rest of a long track, which is
/// exactly what lifting the handset must not do. So it waits in short slices and
/// bails on the stop request.
///
/// Only ever called from [`play`], which is Linux-only - but it takes an
/// `alsa::pcm::PCM`, so it has to be gated with it. Ungated, it was the one thing
/// in this module that stopped the crate from compiling off-target, which is
/// where the tests and the web page tests are supposed to run.
#[cfg(target_os = "linux")]
fn drain(pcm: &alsa::pcm::PCM, stop: &AtomicBool) -> Result<(), String> {
    // How long to wait per slice. Short enough that a stop request feels
    // immediate, long enough not to spin.
    const SLICE_MS: u32 = 20;

    // `avail` reports the room left in the hardware buffer, so the buffer is
    // empty once it reaches this size.
    let buffer = match pcm.hw_params_current().and_then(|h| h.get_buffer_size()) {
        Ok(size) => size as i64,
        // Without a buffer size there is nothing to wait for.
        Err(_) => return Ok(()),
    };
    if buffer <= 0 {
        return Ok(());
    }

    loop {
        if stop.load(Ordering::SeqCst) {
            // Throw away whatever is still queued instead of playing it out.
            let _ = pcm.drop();
            return Ok(());
        }

        match pcm.avail() {
            Ok(avail) if avail >= buffer => return Ok(()),
            // The stream has already freed itself, which is the outcome we
            // wanted anyway.
            Ok(_) => {}
            Err(_) => return Ok(()),
        }

        // Block briefly rather than spin. `wait` returns as soon as the card
        // wants more data, so a drained buffer ends the wait at once.
        let _ = pcm.wait(Some(SLICE_MS));
    }
}

/// Hand one decoded buffer to ALSA as interleaved i16.
#[cfg(target_os = "linux")]
/// Hand one decoded buffer to ALSA as interleaved i16, through `converted`.
///
/// `converted` is refilled rather than reallocated: it is the same buffer for
/// every packet of the sound, so a pass over a file costs one allocation
/// instead of one per packet.
#[cfg(target_os = "linux")]
fn write(
    io: &alsa::pcm::IO<i16>,
    decoded: &symphonia::core::audio::GenericAudioBufferRef<'_>,
    channels: usize,
    converted: &mut Vec<i16>,
) -> Result<(), ()> {
    // The destination has to be exactly the right length, so it is sized from the
    // decoded buffer rather than merely given capacity.
    converted.resize(decoded.samples_interleaved(), 0);
    // `copy_to_slice_interleaved` converts the decoded sample format to i16 and
    // interleaves the planes in one step, so MP3 and 32-bit WAV both land in the
    // right shape.
    decoded.slice(..).copy_to_slice_interleaved::<i16, _>(&mut converted[..]);

    write_samples(io, &converted[..], channels)
}

/// Hand interleaved i16 to ALSA, blocking until it is accepted.
///
/// `writei` reports frames, not samples, and is allowed to take fewer than
/// offered, so the remainder is written in a loop rather than dropped.
#[cfg(target_os = "linux")]
fn write_samples(io: &alsa::pcm::IO<i16>, samples: &[i16], channels: usize) -> Result<(), ()> {
    // ALSA only takes whole frames, so a trailing partial frame is dropped
    // rather than handed over as-is. In practice this never fires: the decoder
    // emits exactly `channels` samples per frame and the gap buffer is sized to
    // match.
    let samples = &samples[..samples.len() - samples.len() % channels];
    if samples.is_empty() {
        return Ok(());
    }

    let mut written = 0;
    while written < samples.len() {
        match io.writei(&samples[written..]) {
            // No progress means the stream is not going to recover.
            Ok(0) => return Err(()),
            Ok(frames) => written += frames * channels,
            Err(_) => return Err(()),
        }
    }

    Ok(())
}

/// A block of silence, used for the pause between two rings.
#[cfg(any(target_os = "linux", test))]
fn silence(rate: u32, channels: u32) -> Vec<i16> {
    vec![0i16; rate as usize * channels as usize]
}

/// Ask ALSA for `rate`/`channels` S16_LE, the format the player writes.
#[cfg(target_os = "linux")]
fn configure(pcm: &alsa::pcm::PCM, rate: u32, channels: u32) -> Result<(), String> {
    use alsa::pcm::{Access, Format, HwParams};
    use alsa::ValueOr;

    // `any` starts unconstrained; each `set_*` only narrows the local set down.
    // The order matters to ALSA's constraint solver: rate and channels first,
    // sample format last, which is the order alsa-utils uses.
    let hwp = HwParams::any(pcm).map_err(|e| e.to_string())?;

    // `ValueOr::Less`, not `Nearest`, and this is the whole reason the earpiece
    // works. The USB codec refuses an *approximate* rate: with `Nearest` the
    // parameter set stays inexact and `snd_pcm_start` fails with a broken pipe,
    // while `aplay` on the very same card succeeds. `Less` makes ALSA commit to
    // the rate the hardware actually runs (44100 comes back as 44099), and then
    // the plug wrapper takes care of any residual difference.
    hwp.set_rate(rate, ValueOr::Less).map_err(|e| e.to_string())?;
    hwp.set_channels(channels).map_err(|e| e.to_string())?;
    hwp.set_access(Access::RWInterleaved).map_err(|e| e.to_string())?;
    hwp.set_format(Format::s16()).map_err(|e| e.to_string())?;

    // Without applying the narrowed set the stream stays unconfigured and the
    // following `start` fails with an I/O error.
    pcm.hw_params(&hwp).map_err(|e| format!("Wiedergabe konnte nicht eingestellt werden: {}", e))?;
    Ok(())
}

/// Playback stub for development machines, which have no ALSA sound card.
///
/// Mirrors [`crate::callaudio`]: the crate builds and its tests run off-target.
#[cfg(not(target_os = "linux"))]
fn play(_path: &str, _device: &str, _repeat: bool, stop: &AtomicBool) -> Result<(), String> {
    // Sleep in short slices so `stop` is honoured the way it is on the phone.
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(POLL);
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Generated tones
// -----------------------------------------------------------------------------

/// The call-progress tones, rendered into memory instead of decoded from files.
///
/// These are plain tone sequences and nothing about them gains from being
/// stored as audio: a file would only have to be decoded to arrive at exactly
/// what is written down in [`Tone::steps`]. Every sequence repeats until
/// playback is stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Freizeichen: a steady 425 Hz for as long as the phone dials.
    Frei,
    /// Besetzt: 425 Hz for half a second, then half a second of silence.
    Besetzt,
    /// Freiton: what the caller hears while the other end is still ringing.
    Freiton,
    /// Ungültige Kurzwahl: three short rising tones, then a long pause.
    Ungueltig,
    /// Kein SIP-Stack bereit: a short 425 Hz burst, then a short pause.
    NichtVerbunden,
}

/// One step of a tone sequence: a frequency and how long it lasts.
///
/// A frequency of `0` is silence, which needs no special case in the generator
/// because the sine of zero is zero. A duration of `0` lasts until playback is
/// stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Step {
    /// Frequency in Hz, or `0` for silence.
    hz: u32,
    /// Duration in milliseconds, or `0` for "until stopped".
    ms: u32,
}

impl Step {
    /// A tone of `ms` milliseconds, or an endless one when `ms` is `0`.
    const fn tone(hz: u32, ms: u32) -> Self {
        Step { hz, ms }
    }

    /// `ms` milliseconds of silence.
    const fn silence(ms: u32) -> Self {
        Step { hz: 0, ms }
    }

    /// This step as interleaved i16 samples.
    fn samples(&self) -> Vec<i16> {
        // An endless step becomes a single second, which the playback loop
        // repeats. Every frequency here completes a whole number of cycles per
        // second, so that loop has no seam and the tone really is continuous.
        let frames = if self.ms == 0 {
            TONE_RATE as usize
        } else {
            TONE_RATE as usize * self.ms as usize / 1000
        };

        let radians = 2.0 * std::f64::consts::PI * self.hz as f64 / TONE_RATE as f64;
        (0..frames)
            .map(|frame| ((radians * frame as f64).sin() * TONE_AMPLITUDE * fade(frame, frames)) as i16)
            .collect()
    }
}

impl Tone {
    /// A steady 425 Hz, endless: the buffer is one second and the playback loop
    /// repeats it, which is seamless because 425 cycles fit exactly into one.
    const FREI: [Step; 1] = [Step::tone(425, 0)];

    /// Half a second of dial tone, half a second of silence.
    const BESETZT: [Step; 2] = [Step::tone(425, 480), Step::silence(480)];

    /// The ringback the caller hears: a second of tone, then four of silence.
    const FREITON: [Step; 2] = [Step::tone(425, 1000), Step::silence(4000)];

    /// Three rising tones of a third of a second, then a long pause.
    const UNGUELTIG: [Step; 4] = [
        Step::tone(950, 330),
        Step::tone(1400, 330),
        Step::tone(1800, 330),
        Step::silence(1600),
    ];

    /// A short burst, then just as much silence.
    const NICHT_VERBUNDEN: [Step; 2] = [Step::tone(425, 125), Step::silence(125)];

    /// The sequence this tone plays, step by step.
    fn steps(self) -> &'static [Step] {
        match self {
            Tone::Frei => &Self::FREI,
            Tone::Besetzt => &Self::BESETZT,
            Tone::Freiton => &Self::FREITON,
            Tone::Ungueltig => &Self::UNGUELTIG,
            Tone::NichtVerbunden => &Self::NICHT_VERBUNDEN,
        }
    }

    /// How this tone is called in a log line.
    fn label(self) -> &'static str {
        match self {
            Tone::Frei => "Freizeichen",
            Tone::Besetzt => "Besetztton",
            Tone::Freiton => "Freiton",
            Tone::Ungueltig => "Ton für ungültige Kurzwahl",
            Tone::NichtVerbunden => "Ton ohne SIP-Verbindung",
        }
    }

    /// One pass over the sequence as interleaved i16 samples.
    ///
    /// The caller loops this: every tone here is meant to run on until the
    /// phone stops it.
    ///
    /// Rendered once per tone and then reused. A pass is up to 240 000 samples
    /// of sine - the ringback runs five seconds - and it used to be built from
    /// scratch on every start, which is once per dial tone. The tones do not
    /// change while the phone runs, so there is nothing to be gained by building
    /// them twice.
    fn samples(self) -> &'static [i16] {
        // One cell per tone, so each gets its own.
        static FREI: OnceLock<Vec<i16>> = OnceLock::new();
        static BESETZT: OnceLock<Vec<i16>> = OnceLock::new();
        static FREITON: OnceLock<Vec<i16>> = OnceLock::new();
        static UNGUELTIG: OnceLock<Vec<i16>> = OnceLock::new();
        static NICHT_VERBUNDEN: OnceLock<Vec<i16>> = OnceLock::new();

        let cell = match self {
            Tone::Frei => &FREI,
            Tone::Besetzt => &BESETZT,
            Tone::Freiton => &FREITON,
            Tone::Ungueltig => &UNGUELTIG,
            Tone::NichtVerbunden => &NICHT_VERBUNDEN,
        };

        cell.get_or_init(|| self.steps().iter().flat_map(|step| step.samples()).collect())
    }
}

/// The gain of sample `frame` of a step `frames` samples long.
///
/// It ramps up and down over [`TONE_FADE_MS`], so neither edge of a tone can
/// click. It is `1.0` everywhere in between.
fn fade(frame: usize, frames: usize) -> f64 {
    let length = (TONE_RATE as usize * TONE_FADE_MS as usize / 1000).min(frames / 2);
    if length == 0 {
        return 1.0;
    }

    let rise = (frame + 1).min(length) as f64 / length as f64;
    let fall = (frames - frame).min(length) as f64 / length as f64;
    rise.min(fall)
}

/// Play a generated tone on `device` until `stop` is set.
///
/// Runs on its own thread, like [`play`]. Runs on ALSA in exactly the same
/// shape, so the `plug:` wrapper does the same rate and channel conversion.
#[cfg(target_os = "linux")]
fn play_tone(tone: Tone, device: &str, stop: &AtomicBool) -> Result<(), String> {
    // Asked for before the card is opened. Building a tone is a lot of sine, and
    // a stream that is already running with nothing queued to play is a silence
    // at the start of every dial tone.
    let samples = tone.samples();

    let pcm = alsa::pcm::PCM::new(&format!("plug:{}", device), alsa::Direction::Playback, false)
        .map_err(|e| format!("Gerät \"{}\" nicht verfügbar: {}", device, e))?;

    // Unlike a file, the format is ours to choose; the wrapper converts it to
    // whatever the hardware wants.
    configure(&pcm, TONE_RATE, TONE_CHANNELS)?;
    pcm.start().map_err(|e| format!("Wiedergabe nicht startbar: {}", e))?;

    let io = pcm.io_i16().map_err(|e| format!("Wiedergabe-Format falsch: {}", e))?;

    // The sequence repeats for as long as the phone wants the tone, and is
    // written in short slices so a stop request does not have to wait out a
    // whole pass.
    loop {
        for chunk in samples.chunks(TONE_CHUNK) {
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            // The stream is gone: end the tone rather than spin on a device
            // that has nothing left to say.
            if write_samples(&io, chunk, TONE_CHANNELS as usize).is_err() {
                return Ok(());
            }
        }
    }
}

/// Playback stub for development machines, which have no ALSA sound card.
///
/// The tones are generated, but there is nothing to hand them to, so this
/// mirrors [`play`] and only waits for the stop request.
#[cfg(not(target_os = "linux"))]
fn play_tone(_tone: Tone, _device: &str, stop: &AtomicBool) -> Result<(), String> {
    // Sleep in short slices so `stop` is honoured the way it is on the phone.
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(POLL);
    }
    Ok(())
}

/// The two playback channels of the phone.
#[derive(Clone, Default)]
pub struct Audio {
    /// Loudspeaker behind the MAX98357 amplifier.
    pub speaker: Channel,
    /// Earpiece and microphone on the USB sound card.
    pub earpiece: Channel,
}

impl Audio {
    /// Play a file on the loudspeaker.
    pub fn play_speaker(&self, path: &str) -> Channel {
        self.speaker.start(path, SPEAKER_DEVICE, false);
        self.speaker.clone()
    }

    /// Play a file on the loudspeaker, repeating it indefinitely.
    pub fn play_speaker_repeat(&self, path: &str) {
        self.speaker.start(path, SPEAKER_DEVICE, true);
    }

    /// Play a file in the earpiece.
    ///
    /// Also silences the loudspeaker: picking up the handset to dial should not
    /// leave the sleep music running underneath the dial tone.
    pub fn play_earpiece(&self, path: &str) -> Channel {
        self.stop_speaker();
        self.earpiece.start(path, EARPIECE_DEVICE, false);
        self.earpiece.clone()
    }

    /// Play a call-progress tone in the earpiece.
    ///
    /// The tone is generated, so there is no file that could be missing, and it
    /// repeats until [`Audio::stop_earpiece`] is called - the phone decides how
    /// long a dial tone runs. Like [`Audio::play_earpiece`] this silences the
    /// loudspeaker, so the sleep music does not play underneath it.
    pub fn play_earpiece_tone(&self, tone: Tone) -> Channel {
        self.stop_speaker();
        self.earpiece.start_tone(tone, EARPIECE_DEVICE);
        self.earpiece.clone()
    }

    /// Stop playback on the loudspeaker.
    pub fn stop_speaker(&self) {
        self.speaker.stop();
    }

    /// Stop playback in the earpiece.
    pub fn stop_earpiece(&self) {
        self.earpiece.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn nothing_plays_on_a_fresh_channel() {
        let channel = Channel::default();
        assert!(!channel.is_playing());
    }

    #[test]
    fn empty_path_does_not_start_anything() {
        let channel = Channel::default();
        channel.start("", SPEAKER_DEVICE, false);
        assert!(!channel.is_playing());
    }

    #[test]
    fn starting_replaces_the_previous_sound() {
        let channel = Channel::default();
        channel.start("", SPEAKER_DEVICE, false);
        assert!(!channel.is_playing());
        channel.start("", SPEAKER_DEVICE, true);
        assert!(!channel.is_playing());
    }

    #[test]
    fn waiting_returns_at_once_when_nothing_plays() {
        let channel = Arc::new(Channel::default());
        let (tx, rx) = std::sync::mpsc::channel();

        let waiter = {
            let channel = channel.clone();
            std::thread::spawn(move || {
                channel.wait();
                tx.send("waited").unwrap();
            })
        };

        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "waited");
        waiter.join().unwrap();
    }

    /// Install a stand-in for a running sound, so the channel's threading can be
    /// exercised without a sound card.
    ///
    /// Returns the stop flag and the completion signal, which is what the real
    /// playback thread owns.
    fn fake_sound(channel: &Channel) -> (Arc<AtomicBool>, Arc<Done>) {
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(Done::default());
        *lock(&channel.playback) =
            Some(Playback { stop: stop.clone(), done: done.clone() });
        (stop, done)
    }

    /// Start a thread that waits for the channel's sound to end.
    fn waiting_on(channel: &Arc<Channel>) -> (std::thread::JoinHandle<()>, mpsc::Receiver<&'static str>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = {
            let channel = channel.clone();
            std::thread::spawn(move || {
                channel.wait();
                tx.send("waited").unwrap();
            })
        };
        (waiter, rx)
    }

    #[test]
    fn a_sound_that_finished_is_not_playing_any_more() {
        let channel = Channel::default();
        let (_, done) = fake_sound(&channel);
        assert!(channel.is_playing());

        // The thread reached the end of the file. Nobody has taken the sound out
        // of the channel, but it is over.
        done.finish();
        assert!(!channel.is_playing());
    }

    #[test]
    fn a_sound_that_finishes_releases_a_waiting_thread() {
        let channel = Arc::new(Channel::default());
        let (_, done) = fake_sound(&channel);
        let (waiter, rx) = waiting_on(&channel);

        done.finish();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "waited");
        waiter.join().unwrap();
    }

    #[test]
    fn stop_releases_a_waiting_thread() {
        // `stop` must be able to cut a sound short while another thread waits on
        // it: that is how lifting the handset stops the sleep music.
        let channel = Arc::new(Channel::default());
        let (stop, done) = fake_sound(&channel);
        let (waiter, rx) = waiting_on(&channel);

        // This is the playback thread noticing the request and letting go.
        stop.store(true, Ordering::SeqCst);
        done.finish();

        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "waited");
        waiter.join().unwrap();
    }

    #[test]
    fn stop_returns_once_the_sound_has_let_go_of_the_device() {
        let channel = Arc::new(Channel::default());
        let (stop, done) = fake_sound(&channel);

        // The sound only lets go a moment after the stop request, which is what
        // finishing the current ALSA write looks like.
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            stop.store(true, Ordering::SeqCst);
            done.finish();
        });

        // `stop` must not return before the device is free, or the next sound
        // would race the old stream for it.
        channel.stop();
        assert!(!channel.is_playing());
        release.join().unwrap();
    }

    #[test]
    fn stop_does_not_block_a_waiting_thread() {
        let channel = Arc::new(Channel::default());
        let (stop, done) = fake_sound(&channel);
        let (waiter, rx) = waiting_on(&channel);

        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            stop.store(true, Ordering::SeqCst);
            done.finish();
        });

        channel.stop();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "waited");
        release.join().unwrap();
        waiter.join().unwrap();
    }

    #[test]
    fn stopping_an_idle_channel_is_harmless() {
        let channel = Channel::default();
        channel.stop();
        channel.wait();
        assert!(!channel.is_playing());
    }

    #[test]
    fn channels_are_independent() {
        // The earpiece silences the speaker, but a stopped earpiece must not
        // leave the speaker running.
        let audio = Audio::default();
        audio.stop_earpiece();
        assert!(!audio.speaker.is_playing());
        assert!(!audio.earpiece.is_playing());
    }

    #[test]
    fn silence_has_the_requested_length() {
        assert_eq!(silence(48_000, 2).len(), 96_000);
        assert_eq!(silence(8_000, 1).len(), 8_000);
    }

    // -----------------------------------------------------------------------------
    // Generated tones
    // -----------------------------------------------------------------------------

    /// The duration of `ms` milliseconds at the tone rate, in frames.
    fn frames(ms: u32) -> usize {
        TONE_RATE as usize * ms as usize / 1000
    }

    #[test]
    fn the_tones_are_the_sequences_they_are_supposed_to_be() {
        // Written out rather than derived, so a change to the cadence has to be
        // a deliberate edit of the test.
        assert_eq!(
            Tone::Frei.steps(),
            &[Step::tone(425, 0)],
            "Freizeichen ist ein Dauerton mit 425 Hz"
        );
        assert_eq!(
            Tone::Besetzt.steps(),
            &[Step::tone(425, 480), Step::silence(480)],
            "Besetzt: 425 Hz für 480 ms, dann 480 ms Stille"
        );
        assert_eq!(
            Tone::Freiton.steps(),
            &[Step::tone(425, 1000), Step::silence(4000)],
            "Freiton: 425 Hz für eine Sekunde, dann vier Sekunden Stille"
        );
        assert_eq!(
            Tone::Ungueltig.steps(),
            &[
                Step::tone(950, 330),
                Step::tone(1400, 330),
                Step::tone(1800, 330),
                Step::silence(1600),
            ],
            "Ungültige Kurzwahl: drei Töne, dann eine lange Pause"
        );
        assert_eq!(
            Tone::NichtVerbunden.steps(),
            &[Step::tone(425, 125), Step::silence(125)],
            "Nicht verbunden: kurzer Ton, kurze Pause"
        );
    }

    #[test]
    fn a_pass_over_a_tone_is_as_long_as_its_steps() {
        // Mono, so one sample per frame.
        assert_eq!(Tone::Besetzt.samples().len(), frames(960));
        assert_eq!(Tone::Freiton.samples().len(), frames(1000 + 4000));
        assert_eq!(Tone::Ungueltig.samples().len(), frames(330 * 3 + 1600));
        assert_eq!(Tone::NichtVerbunden.samples().len(), frames(250));
        // The dial tone has no end of its own: one second that the playback
        // loop repeats.
        assert_eq!(Tone::Frei.samples().len(), TONE_RATE as usize);
    }

    #[test]
    fn the_freiton_is_one_second_of_tone_every_five() {
        let samples = Tone::Freiton.samples();
        let (tone, silence) = samples.split_at(frames(1000));

        assert!(tone.iter().any(|sample| *sample != 0), "die erste Sekunde muss klingen");
        assert!(silence.iter().all(|sample| *sample == 0), "danach vier Sekunden Stille");
    }

    #[test]
    fn the_endless_tone_loops_without_a_seam() {
        // The playback loop repeats this buffer, so the last sample has to
        // continue into the first. 425 Hz completes 425 whole cycles per
        // second, which is what makes that true.
        let samples = Tone::Frei.samples();
        let crossings = samples.windows(2).filter(|pair| pair[0] <= 0 && pair[1] > 0).count();
        assert_eq!(crossings, 425, "425 Hz müssen 425 Nulldurchgänge pro Sekunde haben");

        // The sine starts at silence, so the loop has nothing to click on.
        assert_eq!(samples[0], 0);
    }

    #[test]
    fn silence_steps_are_silent_and_tones_are_not() {
        let samples = Tone::Besetzt.samples();
        let (tone, silence) = samples.split_at(frames(480));

        assert!(tone.iter().any(|sample| *sample != 0), "der erste Schritt muss klingen");
        assert!(silence.iter().all(|sample| *sample == 0), "der zweite Schritt muss still sein");
    }

    #[test]
    fn the_ungueltig_tone_sounds_three_times_and_then_pauses() {
        let samples = Tone::Ungueltig.samples();
        let peaks = (0..3)
            .map(|index| {
                let step = &samples[frames(330 * index)..frames(330 * (index + 1))];
                step.iter().map(|sample| sample.abs() as i32).max().unwrap_or(0)
            })
            .collect::<Vec<_>>();

        assert!(peaks.iter().all(|peak| *peak > 0), "alle drei Töne müssen klingen, {:?}", peaks);
        assert!(
            samples[frames(990)..].iter().all(|sample| *sample == 0),
            "nach dem dritten Ton folgt die lange Pause"
        );
    }

    #[test]
    fn a_tone_fades_in_and_out_instead_of_clicking() {
        let samples = Step::tone(425, 200).samples();
        let peak = samples.iter().map(|sample| sample.abs()).max().unwrap();

        assert!(peak as f64 > TONE_AMPLITUDE * 0.99, "die Mitte ist auf voller Lautstärke, war {peak}");
        assert_eq!(samples[0], 0, "der Ton darf nicht mit voller Lautstärke einsetzen");
        assert!(
            samples[samples.len() - 1].abs() < peak / 10,
            "… und muss am Ende auch ausblenden"
        );
    }

    #[test]
    fn no_sample_ever_clips() {
        for tone in [
            Tone::Frei,
            Tone::Besetzt,
            Tone::Freiton,
            Tone::Ungueltig,
            Tone::NichtVerbunden,
        ] {
            for sample in tone.samples() {
                assert!(
                    sample.unsigned_abs() as f64 <= TONE_AMPLITUDE,
                    "{tone:?} überschreitet die Amplitude mit {sample}"
                );
            }
        }
    }

    #[test]
    fn starting_a_tone_replaces_whatever_was_playing() {
        // The two channels share one sound card, so a tone has to take the
        // device over rather than race the sound already on it.
        let audio = Audio::default();
        let (stop, done) = fake_sound(&audio.earpiece);

        // The running sound only lets go a moment after the stop request.
        let finished = done.clone();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            stop.store(true, Ordering::SeqCst);
            finished.finish();
        });

        audio.play_earpiece_tone(Tone::Besetzt);
        assert!(done.is_finished(), "der Wählton muss den Kanal übernehmen");
        release.join().unwrap();
        audio.stop_earpiece();
    }

    #[test]
    fn a_tone_in_the_earpiece_silences_the_speaker() {
        // Same rule as `play_earpiece`: picking up the handset must not leave
        // the sleep music running underneath the dial tone.
        let audio = Audio::default();
        let (stop, done) = fake_sound(&audio.speaker);
        let finished = done.clone();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            stop.store(true, Ordering::SeqCst);
            finished.finish();
        });

        audio.play_earpiece_tone(Tone::Frei);
        assert!(done.is_finished(), "der Lautsprecher muss dem Wählton weichen");
        assert!(!audio.speaker.is_playing());
        release.join().unwrap();

        audio.stop_earpiece();
    }
}
