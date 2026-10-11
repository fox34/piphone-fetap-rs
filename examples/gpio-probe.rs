//! Diagnostic probe for the GPIO backend.
//!
//! Not part of the phone: this tries the libgpiod calls in isolation so a
//! failing read can be pinned to a specific request or ioctl argument.
//!
//!     cargo run --example gpio-probe

use gpiod::{AsValues, AsValuesMut, Bias, Chip, EdgeDetect, Masked, Options};

fn main() {
    let path = "/dev/gpiochip0";

    let chip = match Chip::new(path) {
        Ok(chip) => {
            println!("Chip {} geöffnet", chip);
            chip
        }
        Err(e) => {
            eprintln!("Chip {} nicht lesbar: {}", path, e);
            std::process::exit(1);
        }
    };

    println!("Chips auf dem System:");
    match Chip::list_devices() {
        Ok(devices) => {
            for device in devices {
                println!("  {}", device.display());
            }
        }
        Err(e) => println!("  (nicht auflistbar: {})", e),
    }

    let pin = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(23);

    println!("\n=== Pin {} ===", pin);

    // 1. Without a bias.
    match chip.request_lines(Options::input(&[pin]).consumer("probe-nobias")) {
        Ok(lines) => {
            println!("Anforderung ohne Bias: ok");
            try_read(&lines, "ohne Bias");
        }
        Err(e) => println!("Anforderung ohne Bias: FEHLER {}", e),
    }

    // 2. With a pull-up, as the phone uses it.
    match chip.request_lines(
        Options::input(&[pin])
            .bias(Bias::PullUp)
            .consumer("probe-bias"),
    ) {
        Ok(lines) => {
            println!("Anforderung mit Pull-Up: ok");
            try_read(&lines, "mit Pull-Up");
        }
        Err(e) => println!("Anforderung mit Pull-Up: FEHLER {}", e),
    }

    // 3. Write access, for comparison with the LED path.
    match chip.request_lines(
        Options::output(&[pin])
            .values([false])
            .consumer("probe-out"),
    ) {
        Ok(lines) => {
            println!("Ausgabe: ok");
            let values = Masked::<u8>::default().with(0, Some(false));
            match lines.set_values(values) {
                Ok(()) => println!("  schreiben low: ok"),
                Err(e) => println!("  schreiben low: FEHLER {}", e),
            }
        }
        Err(e) => println!("Ausgabe: FEHLER {}", e),
    }

    // 4. Edge detection, which is how the phone reads the contacts.
    println!("\n=== Flanken auf Pin {} (2 s) ===", pin);
    let lines = match chip.request_lines(
        Options::input(&[pin])
            .bias(Bias::PullUp)
            .edge(EdgeDetect::Both)
            .consumer("probe-edge"),
    ) {
        Ok(lines) => {
            println!("Anforderung mit Flankenerkennung: ok");
            lines
        }
        Err(e) => {
            println!("Anforderung mit Flankenerkennung: FEHLER {}", e);
            println!("Ohne Flanken liest der Wählscheiben-Thread nichts.");
            return;
        }
    };

    println!("Kontakt kurz schließen und wieder öffnen, sonst passiert hier nichts.");
    let (sender, changes) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = lines;
        while let Ok(event) = lines.read_event() {
            if sender.send(event).is_err() {
                return;
            }
        }
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut seen = 0;
    while std::time::Instant::now() < deadline {
        match changes.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(event) => {
                seen += 1;
                println!("  Flanke #{}: {}", seen, event);
            }
            Err(_) => break,
        }
    }

    if seen == 0 {
        println!("  keine Flanke gesehen - Verkabelung oder Flankenerkennung prüfen.");
    }
}

/// Try several ways of reading, to see which one the kernel accepts.
fn try_read(lines: &gpiod::Lines<gpiod::Input>, label: &str) {
    // a) Plain integer: every bit implicitly selected.
    match lines.get_values(0u8) {
        Ok(values) => println!("  [{}] get_values(0u8): ok, bit0={}", label, values.get(0).unwrap_or(false)),
        Err(e) => println!("  [{}] get_values(0u8): FEHLER {}", label, e),
    }

    // b) Masked, mask set by `with`.
    let requested = Masked::<u8>::default().with(0, Some(false));
    match lines.get_values(requested) {
        Ok(values) => println!("  [{}] get_values(Masked::with(0,Some(false))): ok, bit0={}", label, values.get(0).unwrap_or(false)),
        Err(e) => println!("  [{}] get_values(Masked::with(0,Some(false))): FEHLER {}", label, e),
    }

    // c) Masked with an empty mask, which is what the code used to pass.
    let requested = Masked::<u8>::default().with(0, Some(false));
    println!(
        "  [{}] mask={:#04x} bits={:#04x}",
        label,
        requested.mask,
        requested.bits
    );
}