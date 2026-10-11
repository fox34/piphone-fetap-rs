//! Check playback on the real hardware, one path per device.
//!
//! Plays a WAV and an MP3 on the loudspeaker and in the earpiece, reports how
//! long each took, and exercises the repeat loop and an early stop. The
//! generated call-progress tones are checked in the earpiece afterwards, both
//! on a free card and after a call-audio bridge has held it.
//!
//!     cargo run --example playback-probe -- <dir>
//!     cargo run --example playback-probe -- <dir> tones
//!
//! The second form leaves out the files and only plays the tones, which is the
//! quiet way to repeat this on a phone that is in service.
//!
//! Note that this is audible.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use piphone::audio::{Audio, Tone};
use piphone::callaudio;

/// How much slower than real time playback may get before the run is worth a
/// look. Decoding MP3 on a Pi Zero 2W is not free, but the card should still
/// keep up comfortably rather than falling behind.
const SLOWDOWN_LIMIT: f64 = 2.0;

/// How long a WAV should take, from its own header, so the check needs no
/// hardcoded table. Returns 0 for MP3, where only a decoded length would do.
fn expected_seconds(path: &std::path::Path) -> f64 {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return 0.0,
    };
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" {
        return 0.0;
    }
    // Walk the chunks to find `fmt ` and `data`.
    let mut pos = 12;
    let (mut rate, mut channels, mut bits) = (0u32, 0u16, 0u16);
    let mut data_len = 0u32;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        if id == b"fmt " && pos + 8 + 16 <= bytes.len() {
            channels = u16::from_le_bytes(bytes[pos + 10..pos + 12].try_into().unwrap());
            rate = u32::from_le_bytes(bytes[pos + 12..pos + 16].try_into().unwrap());
            bits = u16::from_le_bytes(bytes[pos + 22..pos + 24].try_into().unwrap());
        } else if id == b"data" {
            data_len = len as u32;
        }
        pos += 8 + len + (len & 1);
    }
    // The `sounds/` WAVs are 32-bit, so the frame size has to come from the
    // header rather than assuming 16-bit samples.
    let bytes_per_frame = channels as u32 * (bits as u32 / 8).max(1);
    if rate == 0 || bytes_per_frame == 0 || data_len == 0 {
        return 0.0;
    }
    data_len as f64 / bytes_per_frame as f64 / rate as f64
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "sounds".to_string()));
    // `tones` skips the files and only plays the generated call-progress tones.
    // That is the quiet half of the probe, the one worth repeating on a phone
    // that is in service.
    let tones_only = std::env::args().nth(2).is_some_and(|section| section == "tones");

    let audio = Audio::default();
    let mut failures = 0;

    if !tones_only {
        // A WAV and an MP3 per channel: the WAVs are the card's native format, the
        // MP3s need converting, and `ring-07` is mono, so the `plug:` wrapper has
        // to resample and remix it.
        let cases = [
            ("Lautsprecher WAV", dir.join("test-loud.wav")),
            ("Lautsprecher MP3", dir.join("ring-05.mp3")),
            ("Hörer WAV", dir.join("test-earpiece.wav")),
            ("Hörer MP3", dir.join("ring-07.mp3")),
        ];

        for (what, path) in cases {
            let expected = expected_seconds(&path);

            let started = Instant::now();
            let channel = if what.starts_with("Hörer") {
                audio.play_earpiece(path.to_str().unwrap())
            } else {
                audio.play_speaker(path.to_str().unwrap())
            };

            // `wait` returns once the thread is done, whether it played the file or
            // gave up on it. The elapsed time is the real signal: a file that failed
            // to open returns instantly, a played one takes about its length.
            channel.wait();
            let elapsed = started.elapsed().as_secs_f64();

            if expected > 0.0 && elapsed + 0.3 < expected {
                println!(
                    "{:<24} {}  FEHLER: {:.2} s statt ~{:.1} s",
                    what,
                    path.display(),
                    elapsed,
                    expected
                );
                failures += 1;
            } else if elapsed > expected * SLOWDOWN_LIMIT && expected > 0.3 {
                // Playback may run long when ALSA has to pad a short file, but a
                // file that takes clearly longer than it is is worth a look.
                println!(
                    "{:<24} {}  {:.2} s statt ~{:.1} s (langsam)",
                    what,
                    path.display(),
                    elapsed,
                    expected
                );
            } else {
                println!("{:<24} {}  {:.2} s", what, path.display(), elapsed);
            }
        }

        // The ringtone loop: play, then cut it off well before the track ends.
        let ring = dir.join("ring-07.wav");
        println!("\nKlingelton in Schleife (3 s, dann Abbruch): {}", ring.display());
        audio.play_speaker_repeat(ring.to_str().unwrap());
        if !audio.speaker.is_playing() {
            println!("  FEHLER: Klingelton läuft nicht");
            failures += 1;
        }
        std::thread::sleep(Duration::from_secs(3));
        let started = Instant::now();
        audio.stop_speaker();
        let cut = started.elapsed();
    
        // `stop` has to block until the thread let go of the sound card, otherwise
        // the next sound would race the old stream for the device.
        if cut > Duration::from_secs(1) {
            println!("  FEHLER: Abbruch dauerte {:?}, das Blockieren auf den Schnitt fehlt", cut);
            failures += 1;
        } else {
            println!("  Klingelton nach {:?} gestoppt, Kanal frei.", cut);
        }
    
        // The loop has to keep going: a repeat that stops after one pass is silent
    // after the first ring, which is the failure that matters for the ringtones.
        let ring = dir.join("ring-01.wav");
        let one_pass = expected_seconds(&ring);
        println!(
            "\nKlingelton in Schleife ({:.1} s pro Durchgang, 6 s laufen lassen) …",
            one_pass
        );
        audio.play_speaker_repeat(ring.to_str().unwrap());
        std::thread::sleep(Duration::from_secs(6));
        if !audio.speaker.is_playing() {
            println!("  FEHLER: Die Schleife ist vorzeitig zu Ende");
            failures += 1;
        } else {
            let passes = (6.0 / one_pass.max(0.01)) as u32;
            println!("  Läuft noch nach 6 s, also mindestens {} Durchgänge.", passes);
        }
        audio.stop_speaker();
    
        // And the next sound has to start cleanly on the freed channel.
        println!("\nNach dem Abbruch wieder abspielen …");
        let after = dir.join("confirm.wav");
        let expected = expected_seconds(&after);
        let started = Instant::now();
        let channel = audio.play_speaker(after.to_str().unwrap());
        channel.wait();
        let elapsed = started.elapsed().as_secs_f64();
        if expected > 0.0 && elapsed + 0.3 < expected {
            println!("  FEHLER: Kein Ton nach dem Abbruch ({:.2} s statt ~{:.1} s)", elapsed, expected);
            failures += 1;
        } else {
            println!("  Ton abgespielt ({:.2} s).", elapsed);
        }
    }

    // The call-progress tones are generated, so the only thing that can go
    // wrong is the card refusing them: each has to run for as long as it is
    // asked to and must not stop on its own.
    println!("\nWähltöne im Hörer (je 3 s, dann Abbruch) …");
    for tone in [
        Tone::Frei,
        Tone::Besetzt,
        Tone::Freiton,
        Tone::Ungueltig,
        Tone::NichtVerbunden,
    ] {
        audio.play_earpiece_tone(tone);
        if !audio.earpiece.is_playing() {
            println!("  FEHLER: {tone:?} läuft nicht");
            failures += 1;
            continue;
        }
        std::thread::sleep(Duration::from_secs(3));
        if !audio.earpiece.is_playing() {
            println!("  FEHLER: {tone:?} ist vorzeitig zu Ende");
            failures += 1;
        } else {
            println!("  {tone:?} läuft nach 3 s weiter.");
        }

        let started = Instant::now();
        audio.stop_earpiece();
        let cut = started.elapsed();
        if cut > Duration::from_secs(1) {
            println!("  FEHLER: {tone:?} brauchte {:?} zum Abbruch", cut);
            failures += 1;
        }
    }

    // The tones and the call audio share one card. A call holds it for as long
    // as it lasts, so the bridge has to hand it back at the end of the call -
    // otherwise the busy tone that follows a call finds the device busy and
    // stays silent. This walks the path the phone takes when the other side
    // hangs up: hold the card, release it, then play.
    println!("\nWählton nach einem Gespräch …");
    {
        let card = callaudio::CallAudio::default();
        card.hold(callaudio::open_bridge(callaudio::CALL_DEVICE));
        std::thread::sleep(Duration::from_millis(500));
        card.release();
    }

    audio.play_earpiece_tone(Tone::Frei);
    std::thread::sleep(Duration::from_secs(2));
    if audio.earpiece.is_playing() {
        println!("  Frei läuft nach der Brücke.");
    } else {
        println!("  FEHLER: Nach einem Gespräch bleibt der Hörer stumm - die Brücke gibt die Karte nicht frei.");
        failures += 1;
    }
    audio.stop_earpiece();

    println!();
    if failures == 0 {
        println!("Alle Klänge abgespielt.");
    } else {
        println!("{} Fehler.", failures);
        std::process::exit(1);
    }
}
