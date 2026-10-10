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

use crate::sip::AudioBridge;

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
/// call's audio stream closes.
///
/// The sound card is opened here rather than at startup on purpose. It is the
/// same card the earpiece tones play on, and ALSA will not share a USB device
/// between two open streams, so holding it for the life of the process would
/// silence the dial tone and the earpiece test. Opening per call means the card
/// is free whenever no conversation is up.
pub fn start(call: rvoip::sip::EndpointCall, device: &str, verbose: bool) {
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

        let (sender, receiver) = audio.split();
        crate::sip::pump_audio(sender, receiver, bridge, verbose).await;
    });
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
}

impl RingBuffer {
    pub fn new() -> Self {
        RingBuffer { inner: Mutex::new(VecDeque::with_capacity(QUEUE_DEPTH)) }
    }

    /// Append a frame, dropping the oldest one once the queue is full.
    pub fn push(&self, samples: Vec<i16>) {
        let mut queue = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if queue.len() >= QUEUE_DEPTH {
            queue.pop_front();
        }
        queue.push_back(samples);
    }

    /// Take the oldest frame, if any.
    pub fn pop(&self) -> Option<Vec<i16>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).pop_front()
    }

    /// Is the buffer empty?
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }
}

/// Playback and capture on the USB sound card, driven through ALSA.
#[cfg(target_os = "linux")]
pub mod alsa {
    use super::{RingBuffer, RATE, FRAME};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use crate::sip::AudioBridge;

    /// Playback and capture on the USB sound card.
    pub struct UsbBridge {
        /// Samples from the caller waiting for the earpiece.
        playback: Arc<RingBuffer>,
        /// Samples captured from the microphone waiting to be sent.
        capture: Arc<RingBuffer>,
        active: Arc<AtomicBool>,
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

            let bridge = UsbBridge {
                playback: Arc::new(RingBuffer::new()),
                capture: Arc::new(RingBuffer::new()),
                active: Arc::new(AtomicBool::new(true)),
            };

            bridge.spawn_playback(playback_pcm)?;
            bridge.spawn_capture(capture_pcm)?;

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

        /// Write queued samples to the earpiece until the call ends.
        fn spawn_playback(&self, pcm: alsa::pcm::PCM) -> Result<(), String> {
            let queue = self.playback.clone();
            let active = self.active.clone();

            thread::Builder::new()
                .name("alsa-playback".to_string())
                .spawn(move || {
                    let io = match pcm.io_i16() {
                        Ok(io) => io,
                        Err(e) => {
                            eprintln!("Wiedergabe-Format falsch: {}", e);
                            return;
                        }
                    };
                    let silence = vec![0i16; FRAME];

                    while active.load(Ordering::SeqCst) {
                        // Feed silence when idle so the card keeps running and
                        // the earpiece does not click between frames.
                        let samples = queue.pop().unwrap_or_else(|| silence.clone());
                        if let Err(e) = io.writei(&samples) {
                            eprintln!("Wiedergabe fehlgeschlagen: {}", e);
                            active.store(false, Ordering::SeqCst);
                            return;
                        }
                    }
                })
                .map_err(|e| format!("Wiedergabe-Thread nicht startbar: {}", e))?;

            Ok(())
        }

        /// Read microphone frames into the capture buffer for the SIP pump.
        fn spawn_capture(&self, pcm: alsa::pcm::PCM) -> Result<(), String> {
            let queue = self.capture.clone();
            let active = self.active.clone();

            thread::Builder::new()
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

            Ok(())
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
    fn bridge_falls_back_to_silent_when_card_is_missing() {
        // A device name that cannot exist must not panic, just degrade.
        let bridge = open_bridge("this-device-does-not-exist");
        bridge.push_microphone(vec![0; FRAME]);
        assert!(bridge.take_speaker().is_none());
    }
}
