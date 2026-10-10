//! GPIO backend for Raspberry Pi OS, using the character device interface.
//!
//! Raspberry Pi OS exposes the SoC's pins as `/dev/gpiochip0`. Requesting a line
//! through `gpiod` is the same as `GPIO.setup(..., pull_up_down=PUD_UP)` in the
//! Python version: the pull-up is what holds the dial and hook contacts at a
//! defined level while they are open.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gpiod::{AsValues, AsValuesMut, Bias, Chip, Lines, Masked, Options};

/// Consumer name shown in `/sys/kernel/debug/gpio` while the phone runs.
const CONSUMER: &str = "piphone";

/// GPIO controller shared by all pins of the phone.
pub struct Gpio {
    chip: Chip,
}

impl Gpio {
    /// Open the GPIO character device (BCM pin numbering).
    pub fn new() -> Result<Self, String> {
        let chip = Chip::new("/dev/gpiochip0")
            .map_err(|e| format!("GPIO nicht verfügbar (/dev/gpiochip0): {}", e))?;
        Ok(Gpio { chip })
    }

    /// Claim a pin as output, starting at `initial` level.
    pub fn output(&self, pin: u32, initial: bool) -> Result<Output, String> {
        let lines = self
            .chip
            .request_lines(
                Options::output(&[pin])
                    .values([initial])
                    .consumer(CONSUMER),
            )
            .map_err(|e| format!("GPIO-Pin {} nicht verfügbar: {}", pin, e))?;

        Ok(Output { lines: Arc::new(lines) })
    }

    /// Claim a pin as input with the internal pull-up enabled.
    ///
    /// Both the dial contacts and the hook switch are wired to GND, so the
    /// pull-up holds the line high while the contact is open.
    pub fn input(&self, pin: u32) -> Result<Input, String> {
        let lines = self
            .chip
            .request_lines(
                Options::input(&[pin])
                    .bias(Bias::PullUp)
                    .consumer(CONSUMER),
            )
            .map_err(|e| format!("GPIO-Pin {} nicht verfügbar: {}", pin, e))?;

        Ok(Input { lines, pin, reported: AtomicBool::new(false) })
    }
}

/// An output line. Cheap to clone so the PWM thread can drive it.
#[derive(Clone)]
pub struct Output {
    lines: Arc<Lines<gpiod::Output>>,
}

impl Output {
    /// A cheap copy for another thread.
    pub fn clone_shared(&self) -> Self {
        self.clone()
    }

    /// Drive the line; returns `false` if the write failed.
    pub fn set(&self, on: bool) -> bool {
        let values = Masked::<u8>::default().with(0, Some(on));
        self.lines.set_values(values).is_ok()
    }
}

/// An input line that reports its level.
pub struct Input {
    lines: Lines<gpiod::Input>,
    pin: u32,
    /// Set once the line has failed to read, so the problem is reported but not
    /// repeated on every one of the ~1000 reads per second.
    reported: AtomicBool,
}

impl Input {
    /// Is the line pulled low, i.e. is the contact closed?
    ///
    /// A failed read counts as open, so a broken contact cannot make the phone
    /// believe a call was answered.
    pub fn is_low(&self) -> bool {
        // The mask selects which lines to read, and has to name the line: an
        // empty mask is rejected by the kernel.
        let requested = Masked::<u8>::default().with(0, Some(false));

        match self.lines.get_values(requested) {
            Ok(values) => !values.get(0).unwrap_or(true),
            Err(e) => {
                if !self.reported.swap(true, Ordering::SeqCst) {
                    eprintln!("GPIO-Pin {} nicht lesbar: {}", self.pin, e);
                }
                false
            }
        }
    }
}