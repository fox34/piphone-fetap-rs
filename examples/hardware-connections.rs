//! Live view of the rotary dial and the hook switch.
//!
//! Mirrors `tests/test-connections.py`: shows the raw contacts while you turn
//! the dial and lift the handset, which is how a new wiring is checked.
//!
//!     cargo run --example hardware-connections

use std::thread;
use std::time::{Duration, Instant};

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

    let watcher = match Watcher::new(&gpio, PIN_NSI, PIN_NSA, PIN_GABEL) {
        Ok(watcher) => watcher,
        Err(e) => {
            eprintln!("Kontakte nicht verfügbar: {}", e);
            std::process::exit(1);
        }
    };

    println!("Nummernschalter: NSI = {}, NSA = {}", PIN_NSI, PIN_NSA);
    println!("Gabelkontakt:    {}", PIN_GABEL);
    println!("Hörer liegt auf: {}", if watcher.is_hungup() { "der Gabel" } else { "abgehoben" });
    println!("Beenden mit Strg+C.\n");

    // Poll the same inputs the phone uses, but print instead of decoding.
    let start = Instant::now();
    let mut last_line = String::new();

    loop {
        // NSA is the working contact: it pulls to ground while the dial is
        // turned away from rest, so a low level means a dial is in progress.
        let dialing = watcher.nsa_is_low();
        let nsi = watcher.nsi_is_low();
        let hungup = watcher.is_hungup();

        let line = format!(
            "{:>7.1}s  Wählvorgang: {:<5}  Impulskontakt: {:<5}  Hörer: {}",
            start.elapsed().as_secs_f64(),
            if dialing { "ja" } else { "nein" },
            if nsi { "geschlossen" } else { "offen" },
            if hungup { "aufgelegt" } else { "abgehoben" },
        );

        if line != last_line {
            println!("{}", line);
            last_line = line;
        }

        // Poll quickly while dialling, slowly at rest.
        thread::sleep(if dialing { Duration::from_millis(10) } else { Duration::from_millis(100) });
    }
}