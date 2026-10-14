//! Bridge between the SIP audio path and the USB sound card.
//!
//! `rvoip` hands out raw PCM16 frames; the phone's earpiece and microphone live
//! on the USB sound card. On Raspberry Pi OS the card is driven directly through
//! ALSA (`usb`, the alias from the phone's `asound.conf`), which keeps the sound
//! path independent of whatever default device ALSA happens to pick.
//!
//! Both directions are ring buffers shared between a dedicated ALSA thread and
//! the async SIP pump. ALSA reads and writes block, so they must not run on the
//! async runtime's worker threads.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::sip::AudioBridge;
use crate::sync::lock;

/// G.711 in SIP runs at 8 kHz mono with 20 ms frames.
pub const RATE: u32 = crate::sip::SAMPLE_RATE;
pub const FRAME: usize = crate::sip::FRAME_SAMPLES;

/// ALSA device for call audio: the USB sound card with the earpiece and mic.
///
/// The `plug:` prefix matters. That card is a USB codec that only runs at
/// 44.1 or 48 kHz, and it offers a single channel; without the plugin ALSA
/// rejects the 8 kHz that G.711 needs. With it, alsa-lib converts on the way
/// in and out, so the SIP side stays at its native 8 kHz.
///
/// Note that this is *not* the device the earpiece tones use: those go through
/// the plain `usb` alias, which already matches the stereo files in `sounds/`.
/// The two cannot hold the card at the same time, which is why the bridge is
/// opened per call rather than once at startup.
pub const CALL_DEVICE: &str = "plug:usb";

/// Frames buffered in each direction. Deep enough to ride out scheduling
/// jitter, short enough not to add noticeable delay to the caller's voice.
const QUEUE_DEPTH: usize = 25;

/// Spin up the audio pumps for one call.
///
/// Both directions run for as long as the call lasts; the tasks end when the
/// call's audio stream closes, and the card is released with them.
///
/// The sound card is opened here rather than at startup on purpose. It is the
/// same card the earpiece tones play on, and ALSA will not share a USB device
/// between two open streams, so holding it for the life of the process would
/// silence the dial tone and the earpiece test. Opening per call means the card
/// is free whenever no conversation is up - which only holds if the bridge
/// really lets go of it again, see [`alsa::UsbBridge::stop`].
///
/// `card` is how the bridge announces that it holds the card, so whoever ends
/// the call can take it away again. See [`CallAudio`].
pub fn start(call: rvoip::sip::EndpointCall, device: &str, verbose: bool, card: Arc<CallAudio>) {
    let device = device.to_string();

    // Everything happens off the SIP event loop. `audio()` has to claim the
    // call's media authority quickly, and opening ALSA takes long enough that
    // doing it inline would stall the event loop and let a BYE tear the session
    // down in the meantime.
    tokio::spawn(async move {
        let audio = match call.audio().await {
            Ok(audio) => audio,
            Err(e) => {
                println!("SIP: Audio nicht verfügbar: {}", e);
                return;
            }
        };

        // ALSA negotiation is blocking, so it goes on its own thread.
        let bridge = match tokio::task::spawn_blocking(move || open_bridge(&device)).await {
            Ok(bridge) => bridge,
            Err(e) => {
                println!("SIP: Soundkarte nicht startbar: {}", e);
                return;
            }
        };

        // The card is taken before the first frame moves, so the SIP loop can
        // hand it back the moment the call is over.
        card.hold(bridge.clone());

        let (sender, receiver) = audio.split();
        crate::sip::pump_audio(sender, receiver, bridge, card, verbose).await;
    });
}

/// Who is holding the call sound card.
///
/// The card can only ever belong to one thing: ALSA refuses to share a USB
/// device, so a conversation and an earpiece tone cannot both have it. That
/// makes the end of a call the interesting moment - the phone reacts to it by
/// playing the busy tone, on the card the conversation was using a moment ago.
///
/// So the bridge registers itself here, and the SIP loop releases the card
/// *before* it reports the end of the call. Letting the phone hear about the
/// end first would lose the race: the busy tone it wants to play would look for
/// a card that the conversation is still holding while it shuts down, find it
/// busy, and stay silent.
#[derive(Default)]
pub struct CallAudio {
    bridge: Mutex<Option<Arc<dyn AudioBridge>>>,
}

impl CallAudio {
    /// Give the card back and wait until it really is free.
    ///
    /// Cheap when nobody holds it, so it can be called on every end of a call.
    pub fn release(&self) {
        let bridge = lock(&self.bridge).take();
        // Outside the lock: stopping a bridge waits for its ALSA threads.
        if let Some(bridge) = bridge {
            bridge.stop();
        }
    }

    /// Register the bridge of the call that is up, which is to say: the card is
    /// taken from here on.
    pub fn hold(&self, bridge: Arc<dyn AudioBridge>) {
        let previous = lock(&self.bridge).replace(bridge);
        drop(previous);
    }
}

/// A bridge that drops all audio.
///
/// Used when no sound card can be opened, so the phone still registers and can
/// still place and receive calls instead of failing outright.
pub struct NullBridge;

impl AudioBridge for NullBridge {
    fn push_microphone(&self, _samples: Vec<i16>) {}
    fn take_speaker(&self) -> Option<Vec<i16>> {
        None
    }
}

/// Ring buffer shared between an ALSA thread and the async SIP pump.
pub struct RingBuffer {
    inner: Mutex<VecDeque<Vec<i16>>>,
    /// Fires when a frame is pushed.
    ///
    /// The consumer on the other side has to stay in step with a 20 ms frame
    /// clock. Looking for a frame in a loop would either burn a core, or - at a
    /// sane interval - add most of a frame of delay to every frame that arrives
    /// just after a look, and that is the caller's own voice. Being told that
    /// something arrived costs one atomic flag and no waiting at all.
    arrived: Arc<Notify>,
}

impl RingBuffer {
    pub fn new() -> Self {
        RingBuffer {
            inner: Mutex::new(VecDeque::with_capacity(QUEUE_DEPTH)),
            arrived: Arc::new(Notify::new()),
        }
    }

    /// Append a frame, dropping the oldest one once the queue is full.
    pub fn push(&self, samples: Vec<i16>) {
        let mut queue = lock(&self.inner);
        if queue.len() >= QUEUE_DEPTH {
            queue.pop_front();
        }
        queue.push_back(samples);
        drop(queue);

        // Woken after the lock is released: a waiter takes the lock itself, and
        // `notify_one` keeps a permit for the case where nobody is waiting yet.
        self.arrived.notify_one();
    }

    /// Take the oldest frame, if any.
    pub fn pop(&self) -> Option<Vec<i16>> {
        lock(&self.inner).pop_front()
    }

    /// A handle the consumer can wait on to learn that a frame has arrived.
    ///
    /// `Notify` keeps a permit when it is notified with nobody waiting, so a
    /// frame that arrives between the consumer's last [`Self::pop`] and its
    /// first wait is not lost - that gap is where the frame would otherwise go
    /// missing and the consumer would sit out a whole frame time.
    pub fn arrivals(&self) -> Arc<Notify> {
        self.arrived.clone()
    }

    /// Is the buffer empty?
    pub fn is_empty(&self) -> bool {
        lock(&self.inner).is_empty()
    }
}

/// Playback and capture on the USB sound card, driven through ALSA.
#[cfg(target_os = "linux")]
pub mod alsa {
    use super::{RingBuffer, RATE, FRAME};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use tokio::sync::Notify;

    use crate::sip::AudioBridge;
    use crate::sync::lock;

    /// Playback and capture on the USB sound card.
    pub struct UsbBridge {
        /// Samples from the caller waiting for the earpiece.
        playback: Arc<RingBuffer>,
        /// Samples captured from the microphone waiting to be sent.
        capture: Arc<RingBuffer>,
        active: Arc<AtomicBool>,
        /// The two ALSA threads, kept so they can be waited for.
        ///
        /// The card is only really free once they have returned and dropped
        /// their handles, which is why [`UsbBridge::stop`] joins them.
        threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    }

    impl UsbBridge {
        /// Open the card's capture and playback streams.
        pub fn open(device: &str) -> Result<Self, String> {
            let playback_pcm = alsa::pcm::PCM::new(device, alsa::Direction::Playback, false)
                .map_err(|e| format!("Wiedergabe \"{}\" nicht verfügbar: {}", device, e))?;
            configure(&playback_pcm)?;
            playback_pcm.start().map_err(|e| format!("Wiedergabe nicht startbar: {}", e))?;

            let capture_pcm = alsa::pcm::PCM::new(device, alsa::Direction::Capture, false)
                .map_err(|e| format!("Aufnahme \"{}\" nicht verfügbar: {}", device, e))?;
            configure(&capture_pcm)?;
            capture_pcm.start().map_err(|e| format!("Aufnahme nicht startbar: {}", e))?;

            let mut bridge = UsbBridge {
                playback: Arc::new(RingBuffer::new()),
                capture: Arc::new(RingBuffer::new()),
                active: Arc::new(AtomicBool::new(true)),
                threads: Mutex::new(Vec::new()),
            };

            // If the second stream cannot be opened, the local bridge is dropped
            // on the way out, which stops and joins the thread that already owns
            // the first one and hands the card back.
            if let Err(e) = bridge.spawn_playback(playback_pcm) {
                return Err(e);
            }
            if let Err(e) = bridge.spawn_capture(capture_pcm) {
                return Err(e);
            }

            Ok(bridge)
        }

        /// Open the USB sound card, or fall back to a silent bridge.
        pub fn open_or_silent(device: &str) -> Arc<dyn AudioBridge> {
            match Self::open(device) {
                Ok(bridge) => {
                    println!("Gesprächsaudio über ALSA-Gerät \"{}\" aktiv.", device);
                    Arc::new(bridge)
                }
                Err(e) => {
                    println!("{} - Gespräche laufen ohne Audio.", e);
                    Arc::new(super::NullBridge)
                }
            }
        }

        /// Let go of the card and wait until it really is free.
        ///
        /// This is what makes the phone usable again after a call: the dial tone
        /// and the other earpiece sounds open the very same card, and ALSA will
        /// not share it, so a bridge that merely stopped writing silence would
        /// still hold the device and every later tone would stay silent.
        ///
        /// Idempotent, and safe to call from [`Drop`].
        pub fn stop(&self) {
            self.active.store(false, Ordering::SeqCst);
            for thread in lock(&self.threads).drain(..) {
                // The threads own the streams and close them on their way out.
                let _ = thread.join();
            }
        }

        /// Write queued samples to the earpiece until the call ends.
        fn spawn_playback(&mut self, pcm: alsa::pcm::PCM) -> Result<(), String> {
            let queue = self.playback.clone();
            let active = self.active.clone();

            let handle = thread::Builder::new()
                .name("alsa-playback".to_string())
                .spawn(move || {
                    let io = match pcm.io_i16() {
                        Ok(io) => io,
                        Err(e) => {
                            eprintln!("Wiedergabe-Format falsch: {}", e);
                            return;
                        }
                    };
                    let mut samples = vec![0i16; FRAME];

                    while active.load(Ordering::SeqCst) {
                        // One buffer for the whole call, refilled on every pass:
                        // this runs fifty times a second for as long as the call
                        // lasts, and it used to be a `Vec` allocation per pass.
                        // `resize` rather than `copy_from_slice`, because a frame
                        // off the wire is not guaranteed to be one frame long and
                        // must not panic this thread over it.
                        match queue.pop() {
                            Some(received) => {
                                samples.clear();
                                samples.extend_from_slice(&received);
                            }
                            // Silence when idle, so the card keeps running and
                            // the earpiece does not click between frames.
                            None => {
                                samples.clear();
                                samples.resize(FRAME, 0);
                            }
                        }
                        if let Err(e) = io.writei(&samples) {
                            eprintln!("Wiedergabe fehlgeschlagen: {}", e);
                            active.store(false, Ordering::SeqCst);
                            return;
                        }
                    }
                })
                .map_err(|e| format!("Wiedergabe-Thread nicht startbar: {}", e))?;

            self.remember_thread(handle);
            Ok(())
        }

        /// Read microphone frames into the capture buffer for the SIP pump.
        fn spawn_capture(&mut self, pcm: alsa::pcm::PCM) -> Result<(), String> {
            let queue = self.capture.clone();
            let active = self.active.clone();

            let handle = thread::Builder::new()
                .name("alsa-capture".to_string())
                .spawn(move || {
                    let io = match pcm.io_i16() {
                        Ok(io) => io,
                        Err(e) => {
                            eprintln!("Aufnahme-Format falsch: {}", e);
                            return;
                        }
                    };
                    let mut buffer = vec![0i16; FRAME];

                    while active.load(Ordering::SeqCst) {
                        match io.readi(&mut buffer) {
                            Ok(0) => thread::sleep(Duration::from_millis(1)),
                            Ok(count) => queue.push(buffer[..count].to_vec()),
                            Err(e) => {
                                eprintln!("Aufnahme fehlgeschlagen: {}", e);
                                active.store(false, Ordering::SeqCst);
                                return;
                            }
                        }
                    }
                })
                .map_err(|e| format!("Aufnahme-Thread nicht startbar: {}", e))?;

            self.remember_thread(handle);
            Ok(())
        }

        /// Keep a thread so [`UsbBridge::stop`] can wait for it.
        fn remember_thread(&self, thread: std::thread::JoinHandle<()>) {
            lock(&self.threads).push(thread);
        }
    }

    impl Drop for UsbBridge {
        fn drop(&mut self) {
            // Whoever holds the bridge last releases the card, so a bridge that
            // is dropped without a `stop` still does not strand the device.
            self.stop();
        }
    }

    impl AudioBridge for UsbBridge {
        /// Samples from the caller, queued for the earpiece.
        fn push_microphone(&self, samples: Vec<i16>) {
            self.playback.push(samples);
        }

        /// Samples from the microphone, for the next RTP frame.
        fn take_speaker(&self) -> Option<Vec<i16>> {
            self.capture.pop()
        }

        /// The SIP pump can be woken when the microphone produced something.
        fn arrivals(&self) -> Option<Arc<Notify>> {
            Some(self.capture.arrivals())
        }

        /// Hand the card back, rather than waiting for the last drop: the phone
        /// plays the dial tone on it as soon as the call is over.
        fn stop(&self) {
            UsbBridge::stop(self);
        }
    }

    /// Ask ALSA for 8 kHz mono S16_LE, which is what G.711 expects.
    fn configure(pcm: &alsa::pcm::PCM) -> Result<(), String> {
        use alsa::pcm::{Access, Format, HwParams};
        use alsa::ValueOr;

        // `any` starts from an unconstrained set; every `set_*` below only
        // narrows that local set down, without touching the device. The order
        // matters to ALSA's constraint solver: rate and channels first, sample
        // format last, which is the order alsa-utils uses.
        let hwp = HwParams::any(pcm).map_err(|e| e.to_string())?;
        hwp.set_rate(RATE, ValueOr::Nearest).map_err(|e| e.to_string())?;
        hwp.set_channels(1).map_err(|e| e.to_string())?;
        hwp.set_access(Access::RWInterleaved).map_err(|e| e.to_string())?;
        hwp.set_format(Format::s16()).map_err(|e| e.to_string())?;

        // Small period size so 20 ms frames arrive promptly without much latency.
        hwp.set_period_size_near(FRAME as alsa::pcm::Frames, ValueOr::Nearest)
            .map_err(|e| e.to_string())?;
        hwp.set_buffer_size_near(FRAME as alsa::pcm::Frames * 8)
            .map_err(|e| e.to_string())?;

        // Apply the narrowed set to the device. Without this the stream stays
        // unconfigured and the following `start` fails with an I/O error.
        pcm.hw_params(&hwp)
            .map_err(|e| format!("Audio-Hardware konnte nicht eingestellt werden: {}", e))?;
        Ok(())
    }
}

/// Open the call-audio bridge for this platform.
pub fn open_bridge(device: &str) -> Arc<dyn AudioBridge> {
    #[cfg(target_os = "linux")]
    {
        alsa::UsbBridge::open_or_silent(device)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = device;
        Arc::new(NullBridge)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn arriving_frames_wake_a_waiter() {
        let runtime = tokio::runtime::Runtime::new().expect("Laufzeitumgebung");
        runtime.block_on(async {
            let buffer = RingBuffer::new();
            let arrived = buffer.arrivals();

            // A frame that arrives before anybody waits must not be lost. The
            // pump looks in the buffer first and only then waits, and that gap
            // is exactly where a frame would disappear - the pump would then sit
            // out a whole frame time before noticing it.
            buffer.push(vec![1, 2, 3]);
            let woken = tokio::time::timeout(Duration::from_millis(100), arrived.notified()).await;
            assert!(woken.is_ok(), "eine bereits angekommene Frame muss wecken");

            assert_eq!(buffer.pop(), Some(vec![1, 2, 3]));
        });
    }

    #[test]
    fn ring_buffer_drops_oldest_when_full() {
        let buffer = RingBuffer::new();
        for i in 0..(QUEUE_DEPTH + 5) {
            buffer.push(vec![i as i16]);
        }
        // The first five pushes were discarded, so 5 comes out first.
        assert_eq!(buffer.pop().unwrap()[0], 5i16);
        assert!(!buffer.is_empty());
    }

    #[test]
    fn ring_buffer_is_fifo() {
        let buffer = RingBuffer::new();
        assert!(buffer.is_empty());
        buffer.push(vec![1, 2, 3]);
        buffer.push(vec![4]);
        assert_eq!(buffer.pop(), Some(vec![1, 2, 3]));
        assert_eq!(buffer.pop(), Some(vec![4]));
        assert_eq!(buffer.pop(), None);
        assert!(buffer.is_empty());
    }

    #[test]
    fn null_bridge_keeps_phone_running() {
        let bridge = NullBridge;
        bridge.push_microphone(vec![0; FRAME]);
        assert!(bridge.take_speaker().is_none());
    }

    #[test]
    fn stopping_a_bridge_holds_nothing() {
        // The earpiece tones open the same card the bridge holds, so a bridge
        // that has been stopped must be gone as far as the device is concerned.
        // Without a sound card there is nothing to release, but every path into
        // that code has to at least run: `stop` twice, like the call end and the
        // final drop would.
        let bridge = open_bridge("this-device-does-not-exist");
        bridge.stop();
        bridge.stop();
        drop(bridge);
    }

    /// A bridge that only counts how often it was stopped.
    struct CountingBridge(Arc<AtomicUsize>);

    impl AudioBridge for CountingBridge {
        fn push_microphone(&self, _samples: Vec<i16>) {}
        fn take_speaker(&self) -> Option<Vec<i16>> {
            None
        }
        fn stop(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn releasing_the_card_stops_the_bridge_of_the_call() {
        // The phone answers the end of a call by playing the busy tone, on the
        // card the conversation had. If the card were still held at that point,
        // the tone would be refused for being busy - and stay unheard.
        let card = CallAudio::default();
        let stops = Arc::new(AtomicUsize::new(0));
        card.hold(Arc::new(CountingBridge(stops.clone())));

        card.release();
        assert_eq!(stops.load(Ordering::SeqCst), 1, "die Brücke muss freigegeben werden");

        // Releasing again is what every later end of a call does; it must not
        // wait on a bridge that is gone.
        card.release();
        assert_eq!(stops.load(Ordering::SeqCst), 1, "zweimal freigeben darf nicht doppelt anhalten");
    }

    #[test]
    fn releasing_an_untouched_card_does_nothing() {
        // Every call that was never answered ends this way.
        let card = CallAudio::default();
        card.release();
    }

    #[test]
    fn bridge_falls_back_to_silent_when_card_is_missing() {
        // A device name that cannot exist must not panic, just degrade.
        let bridge = open_bridge("this-device-does-not-exist");
        bridge.push_microphone(vec![0; FRAME]);
        assert!(bridge.take_speaker().is_none());
    }
}
