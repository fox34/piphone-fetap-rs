//! Diagnostic probe for the call-audio bridge.
//!
//! Not part of the phone: this tries several ALSA parameter combinations so a
//! failing sound card can be pinned to one specific setting.
//!
//!     cargo run --example audio-probe -- <device> <rate>

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};

fn rate() -> u32 {
    std::env::args()
        .nth(2)
        .and_then(|a| a.parse().ok())
        .unwrap_or(8_000)
}

/// The channel count to ask for.
fn channels() -> u32 {
    std::env::args()
        .nth(3)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1)
}

/// Apply one combination and report how far it got.
///
/// Returns `true` if the stream was configured and started.
fn attempt(pcm: &PCM, label: &str, order: &[&str], period: Option<alsa::pcm::Frames>, buffer: Option<alsa::pcm::Frames>) -> bool {
    let hwp = match HwParams::any(pcm) {
        Ok(hwp) => hwp,
        Err(e) => {
            println!("  {:<34} HwParams FEHLER {}", label, e);
            return false;
        }
    };

    // ALSA's constraint solver is order sensitive: setting channels before the
    // rate can leave no valid combination, and it reports EINVAL.
    for step in order {
        let result = match *step {
            "access" => hwp.set_access(Access::RWInterleaved).map(|_| ()),
            "format" => hwp.set_format(Format::s16()).map(|_| ()),
            "channels" => hwp.set_channels(channels()).map(|_| ()),
            "rate" => hwp.set_rate(rate(), ValueOr::Nearest).map(|_| ()),
            other => {
                println!("  {:<34} unbekannter Schritt {}", label, other);
                return false;
            }
        };

        if let Err(e) = result {
            println!("  {:<34} set_{} FEHLER {}", label, step, e);
            return false;
        }
    }

    if let Some(size) = period {
        if let Err(e) = hwp.set_period_size_near(size, ValueOr::Nearest) {
            println!("  {:<34} set_period({}) FEHLER {}", label, size, e);
            return false;
        }
    }
    if let Some(size) = buffer {
        if let Err(e) = hwp.set_buffer_size_near(size) {
            println!("  {:<34} set_buffer({}) FEHLER {}", label, size, e);
            return false;
        }
    }

    let mut report = match alsa::Output::buffer_open() {
        Ok(output) => output,
        Err(e) => {
            println!("  {:<34} dump FEHLER {}", label, e);
            return false;
        }
    };
    let _ = hwp.dump(&mut report);

    match pcm.start() {
        Ok(()) => {
            let actual = hwp.get_period_size().unwrap_or(-1);
            println!("  {:<34} ok (Periode {})", label, actual);
            true
        }
        Err(e) => {
            println!("  {:<34} start FEHLER {}", label, e);
            false
        }
    }
}

fn try_all(device: &str, direction: Direction, what: &str) {
    println!("=== {} auf {} @ {} Hz / {} Kanal ===", what, device, rate(), channels());

    let combinations: [(&str, &[&str], Option<alsa::pcm::Frames>, Option<alsa::pcm::Frames>); 5] = [
        ("rate,channels,format", &["rate", "channels", "format"], None, None),
        ("channels,rate,format", &["channels", "rate", "format"], None, None),
        ("format,access,rate,channels", &["format", "access", "rate", "channels"], None, None),
        ("rate,channels,format + Periode 160", &["rate", "channels", "format"], Some(160), None),
        ("rate,channels,format + Periode 160/1280", &["rate", "channels", "format"], Some(160), Some(1280)),
    ];

    for (label, order, period, buffer) in combinations {
        match PCM::new(device, direction, false) {
            Ok(pcm) => {
                attempt(&pcm, label, order, period, buffer);
            }
            Err(e) => println!("  {:<34} öffnen FEHLER {}", label, e),
        }
    }
    println!();
}

fn main() {
    let device = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "plug:usb".to_string());

    println!("Gerät {} @ {} Hz, {} Kanal\n", device, rate(), channels());

    try_all(&device, Direction::Playback, "Wiedergabe");
    try_all(&device, Direction::Capture, "Aufnahme");
}