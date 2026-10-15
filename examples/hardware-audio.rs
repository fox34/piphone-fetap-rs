//! Check both sound paths on the real hardware.
//!
//! Plays the uploaded test tone on the loudspeaker and in the earpiece, and
//! opens the call-audio bridge so the microphone and earpiece can be verified
//! with a live call.
//!
//!     cargo run --example hardware-audio [VERZEICHNIS]
//!
//! Note that this is audible: it plays the tones of the sounds directory, which
//! is where /config puts them.

use std::path::PathBuf;
use std::time::Duration;

use piphone::audio::Audio;
use piphone::callaudio;
use piphone::sounds::{self, Sound};

fn main() {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(sounds::DIR));

    // The tones are files rather than configuration options, so there is
    // nothing to read here - the phone looks for the very same names.
    let audio = Audio::default();

    let test_loud = Sound::TestLoud.path(&dir);
    println!("Lautsprecher (MAX98357, ALSA \"i2s\"): {}", test_loud);
    audio.play_speaker(&test_loud).wait();

    let test_earpiece = Sound::TestEarpiece.path(&dir);
    println!("Hörer (USB-Soundkarte, ALSA \"usb\"): {}", test_earpiece);
    audio.play_earpiece(&test_earpiece).wait();

    let ringtone = Sound::Ringtone.path(&dir);
    println!("Klingelton (Endlosschleife, 3s): {}", ringtone);
    audio.play_speaker_repeat(&ringtone);
    std::thread::sleep(Duration::from_secs(3));
    audio.stop_speaker();
    println!("  Klingelton gestoppt.");

    // Welche Datei für eine Rufnummer klingelt, sagt ihr Name - nachzuprüfen,
    // ohne dafür einen Anruf machen zu müssen:
    //   cargo run --example hardware-audio -- /usr/local/share/piphone/sounds 08912345
    if let Some(caller) = std::env::args().nth(2) {
        match sounds::ringtone_for(&dir, &caller) {
            Some(path) => println!("Klingelton für {}: {}", caller, path.display()),
            None => println!("Klingelton für {}: keine Datei", caller),
        }
    }

    println!("Gesprächsaudio über die USB-Soundkarte öffnen …");
    let _bridge = callaudio::open_bridge(callaudio::CALL_DEVICE);
    println!("  Brücke bereit - Hörer und Mikrofon stehen bereit für einen Anruf.");

    println!("Fertig.");
}
