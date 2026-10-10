//! Live test of the rotary dial decoding.
//!
//! Mirrors `tests/test-waehlscheibe.py`: turn the dial and check that the
//! expected digit is reported. This is the test to run after rewiring the dial
//! or changing the timing constants.
//!
//!     cargo run --example hardware-dial

use piphone::gpio::{Gpio, Watcher};

/// BCM pins, matching the wiring table in the README.
const PIN_NSI: u32 = 23;
const PIN_NSA: u32 = 24;
const PIN_GABEL: u32 = 15;

fn main() {
    let gpio = match Gpio::new() {
        Ok(gpio) => gpio,
        Err(e) => {
            eprintln!("GPIO nicht verfügbar: {}", e);
            std::process::exit(1);
        }
    };

    let mut watcher = match Watcher::new(&gpio, PIN_NSI, PIN_NSA, PIN_GABEL) {
        Ok(watcher) => watcher,
        Err(e) => {
            eprintln!("Kontakte nicht verfügbar: {}", e);
            std::process::exit(1);
        }
    };

    println!("Nummernschalter: NSI = {}, NSA = {}", PIN_NSI, PIN_NSA);
    println!("Wähle eine Ziffer von 0 bis 9 und prüfe, ob sie erkannt wird.");
    println!("Beenden mit Strg+C.\n");

    let mut number = String::new();

    let mut on_digit = |digit: u32| {
        number.push(char::from_digit(digit, 10).unwrap_or('?'));
        println!("Ziffer erkannt: {} (bisher: {})", digit, number);
    };

    let mut on_hook = |hungup: bool| {
        println!("Hörer {}", if hungup { "aufgelegt" } else { "abgehoben" });
    };

    // Runs until the process is interrupted; dropping the watcher then releases
    // the lines.
    watcher.run(&mut on_digit, &mut on_hook);
}