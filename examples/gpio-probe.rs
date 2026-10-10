//! Diagnostic probe for the GPIO backend.
//!
//! Not part of the phone: this tries the libgpiod calls in isolation so a
//! failing read can be pinned to a specific request or ioctl argument.
//!
//!     cargo run --example gpio-probe

use gpiod::{AsValues, AsValuesMut, Bias, Chip, Masked, Options};

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