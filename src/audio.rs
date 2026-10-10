//! Sound output: ringtones, dial tones and the two hardware test tones.
//!
//! Playback shells out to the tools the phone already has configured: `aplay`
//! for plain WAV (faster, but needs the exact device format) and SoX's `play`
//! for everything else and for looping. The `i2s` and `usb` names are the ALSA
//! aliases defined in the phone's `asound.conf`.

use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// ALSA device names, matching the `asound.conf` setup of the phone.
const SPEAKER_DEVICE: &str = "i2s";
const EARPIECE_DEVICE: &str = "usb";

/// One playback channel, tracked independently of the other.
#[derive(Clone, Default)]
pub struct Channel {
    process: Arc<Mutex<Option<Child>>>,
    /// Serialises start/stop so two near-simultaneous calls cannot leave two
    /// processes fighting over the sound card.
    gate: Arc<Mutex<()>>,
}

impl Channel {
    /// Start `path` on `device`, replacing whatever was playing.
    ///
    /// With `repeat`, the file is padded by a second (the pause between rings)
    /// and looped, which only SoX can do.
    fn start(&self, path: &str, device: &str, repeat: bool) {
        let _gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        self.stop();

        if path.is_empty() {
            return;
        }

        let child = if !repeat && path.ends_with(".wav") {
            Command::new("aplay")
                .args(["-q", "-D", device, path])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
        } else {
            let mut command = Command::new("/usr/bin/play");
            command.arg("-q").arg(path).arg("-t").arg("alsa");
            if repeat {
                command.arg("pad").arg("0").arg("1").arg("repeat").arg("99");
            }
            command
                .env("AUDIODEV", device)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
        };

        match child {
            Ok(process) => {
                *self.process.lock().unwrap_or_else(|e| e.into_inner()) = Some(process);
            }
            Err(e) => println!("Klang {} konnte nicht gestartet werden: {}", path, e),
        }
    }

    /// Stop whatever is playing on this channel.
    pub fn stop(&self) {
        // Take the process out before killing it: `wait()` blocks, and holding
        // the lock across that would stop another thread from ever stopping it.
        let process = self.process.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut process) = process {
            let _ = process.kill();
            let _ = process.wait();
        }
    }

    /// Wait for the current sound to finish.
    ///
    /// Used for confirmation tones and before the system goes down. Another
    /// thread can still call [`Channel::stop`] meanwhile, which cuts the sound
    /// short - that is how lifting the handset stops the sleep music.
    ///
    /// The child stays in the slot while it plays: taking it out first would
    /// make this thread its only owner, and a later [`stop`](Self::stop) would
    /// find nothing to kill, so the sleep music would run to the end of its
    /// track however often it was asked to stop.
    pub fn wait(&self) {
        loop {
            let finished = {
                let mut slot = self.process.lock().unwrap_or_else(|e| e.into_inner());
                match slot.as_mut() {
                    // `None` means another thread already stopped and reaped it.
                    None => true,
                    Some(child) => child.try_wait().map(|status| status.is_some()).unwrap_or(true),
                }
            };

            if finished {
                // Reaps the child, and keeps `is_playing` honest.
                drop(self.process.lock().unwrap_or_else(|e| e.into_inner()).take());
                return;
            }

            // The lock is released between checks, so `stop` can reach the child.
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Is a sound currently playing?
    pub fn is_playing(&self) -> bool {
        self.process.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }
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

    /// Play a file in the earpiece, repeating it indefinitely.
    pub fn play_earpiece_repeat(&self, path: &str) {
        self.earpiece.start(path, EARPIECE_DEVICE, true);
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
    fn waiting_on_one_thread_does_not_block_stopping_from_another() {
        // `wait` blocks on the child process; `stop` must still be able to kill
        // it, which is how lifting the handset cuts the sleep music short.
        let channel = Arc::new(Channel::default());
        let (tx, rx) = std::sync::mpsc::channel();

        let waiter = {
            let channel = channel.clone();
            std::thread::spawn(move || {
                channel.wait();
                tx.send("waited").unwrap();
            })
        };

        std::thread::sleep(Duration::from_millis(50));
        channel.stop();

        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "wait() did not return after stop()"
        );
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
}