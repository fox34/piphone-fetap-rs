//! Check both sound paths on the real hardware.
//!
//! Plays the configured test tone on the loudspeaker and in the earpiece, and
//! opens the call-audio bridge so the microphone and earpiece can be verified
//! with a live call.
//!
//!     cargo run --example hardware-audio
//!
//! Note that this is audible: it plays the tones listed in `[sounds]`.

use std::path::PathBuf;
use std::time::Duration;

use piphone::audio::Audio;
use piphone::callaudio;
use piphone::config;

fn main() {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/boot/piphone/config.toml"));

    // Falling back to the sample keeps this runnable without a real config.
    let data = match config::read(&path) {
        Ok(data) => data,
        Err(e) => {
            eprintln!("{} nicht lesbar ({}), verwende Beispielkonfiguration.", path.display(), e);
            config::read(
                &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("support/config-example.toml"),
            )
            .expect("Beispielkonfiguration lesen")
        }
    };
    let (data, _) = config::validate(&data, None);

    let sound = |key: &str| config::option_str(&data, "sounds", key);
    let audio = Audio::default();

    println!("Lautsprecher (MAX98357, ALSA \"i2s\"): {}", sound("test_loud"));
    audio.play_speaker(&sound("test_loud")).wait();

    println!("Hörer (USB-Soundkarte, ALSA \"usb\"): {}", sound("test_earpiece"));
    audio.play_earpiece(&sound("test_earpiece")).wait();

    println!("Klingelton (Endlosschleife, 3s): {}", sound("ring"));
    audio.play_speaker_repeat(&sound("ring"));
    std::thread::sleep(Duration::from_secs(3));
    audio.stop_speaker();
    println!("  Klingelton gestoppt.");

    println!("Gesprächsaudio über die USB-Soundkarte öffnen …");
    let _bridge = callaudio::open_bridge(callaudio::CALL_DEVICE);
    println!("  Brücke bereit - Hörer und Mikrofon stehen bereit für einen Anruf.");

    println!("Fertig.");
}