//! GPIO access for the rotary dial, the hook switch and the two LEDs.
//!
//! The hardware layer is split in two: the timing and decoding rules below are
//! pure logic and therefore testable anywhere, while opening the actual pins is
//! delegated to a platform backend ([`linux`] on Raspberry Pi OS).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
#[path = "gpio/linux.rs"]
mod backend;

#[cfg(not(target_os = "linux"))]
#[path = "gpio/stub.rs"]
mod backend;

pub use backend::Gpio;

/// Debounce time for the hook switch, matching `bouncetime=100` in the Python version.
pub const HOOK_DEBOUNCE: Duration = Duration::from_millis(100);

/// Interval at which the hook contact is sampled.
const HOOK_SAMPLE: Duration = Duration::from_millis(5);

/// Interval at which the dial is sampled, matching `SAMPLE_RATE` in the original.
const DIAL_SAMPLE: Duration = Duration::from_millis(1);

/// Number of dial samples per NSA debounce tick. The original watches NSA every
/// 5 ms while counting impulses every millisecond.
const NSA_TICK_SAMPLES: u32 = 5;

/// Impulse limits, in milliseconds. A valid pulse is low for at least
/// `LOW_PULSE_DURATION` and is followed by a high gap of at least
/// `HIGH_PULSE_DURATION`.
const LOW_PULSE_DURATION: u32 = 20 / 5;
const HIGH_PULSE_DURATION: u32 = 40 / 5;

/// NSA has to read low (or high) for this many ticks before the dial is
/// considered turned, which filters out contact bounce.
const NSA_DEBOUNCE: u32 = 10;

/// Software PWM frequency in Hz for the LEDs (as in the Python original).
const PWM_FREQUENCY: f64 = 8000.0;

/// Blink frequency for the status LED during boot.
const BLINK_FREQUENCY: f64 = 1.0;

// -----------------------------------------------------------------------------
// LEDs
// -----------------------------------------------------------------------------

/// A single output pin, optionally driven by a background software PWM thread.
pub struct Output {
    line: backend::Output,
    stop: Arc<AtomicBool>,
    pwm: Option<thread::JoinHandle<()>>,
}

impl Output {
    fn new(line: backend::Output) -> Self {
        Output { line, stop: Arc::new(AtomicBool::new(false)), pwm: None }
    }

    /// Switch the pin on or off.
    fn set(&self, on: bool) {
        if !self.line.set(on) {
            eprintln!("GPIO-Ausgang nicht schreibbar.");
        }
    }

    /// Drive the pin with a software PWM at `duty` percent.
    ///
    /// `frequency` only matters for the blink during boot; the night and wake
    /// lights always run at [`PWM_FREQUENCY`], where the LED cannot show flicker.
    fn start_pwm(&mut self, duty: f64, frequency: f64) {
        self.stop_pwm();

        // Full brightness needs no PWM at all, which avoids the extra thread.
        if duty >= 100.0 && frequency >= PWM_FREQUENCY {
            self.set(true);
            return;
        }

        let stop = self.stop.clone();
        stop.store(false, Ordering::SeqCst);

        let period = Duration::from_secs_f64(1.0 / frequency.clamp(0.1, PWM_FREQUENCY));
        let duty = duty.clamp(0.0, 100.0);
        let on_time = Duration::from_secs_f64(period.as_secs_f64() * duty / 100.0);
        let off_time = period.saturating_sub(on_time);

        // The output handle is shared with this thread only; `set` takes `&self`.
        let line = self.line.clone_shared();
        self.pwm = Some(thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                line.set(true);
                thread::sleep(on_time);
                line.set(false);
                thread::sleep(off_time);
            }
        }));
    }

    fn stop_pwm(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.pwm.take() {
            let _ = handle.join();
        }
        self.stop.store(false, Ordering::SeqCst);
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.stop_pwm();
        self.set(false);
    }
}

/// The two indicator lights: the amber night light in the back, the green status
/// LED on the front.
/// The lights are shared across threads through an `Arc`, and starting the PWM
/// thread needs exclusive access, so each pin sits behind its own lock.
pub struct Led {
    night: Option<Mutex<Output>>,
    night_duty: f64,
    night_on: AtomicBool,
    wake: Option<Mutex<Output>>,
    wake_duty: f64,
    wake_on: AtomicBool,
}

impl Led {
    /// Set up both LEDs. A pin of `0` switches that LED off.
    pub fn new(
        gpio: &Gpio,
        night_light_pin: u32,
        night_light_duty: f64,
        wake_light_pin: u32,
        wake_light_duty: f64,
        verbose: bool,
    ) -> Result<Self, String> {
        let night = match night_light_pin {
            0 => None,
            pin => {
                if verbose {
                    println!(
                        "Initialisiere Nachtlicht mit Duty {}% an Pin {}.",
                        night_light_duty, pin
                    );
                }
                Some(Output::new(gpio.output(pin, false)?))
            }
        };

        let wake = match wake_light_pin {
            0 => None,
            pin => {
                if verbose {
                    println!(
                        "Initialisiere Aufwachlicht mit Duty {}% an Pin {}.",
                        wake_light_duty, pin
                    );
                }
                Some(Output::new(gpio.output(pin, false)?))
            }
        };

        Ok(Led {
            night: night.map(Mutex::new),
            night_duty: night_light_duty,
            night_on: AtomicBool::new(false),
            wake: wake.map(Mutex::new),
            wake_duty: wake_light_duty,
            wake_on: AtomicBool::new(false),
        })
    }

    /// Is the night light on? Reported to the web interface.
    pub fn night_light_on_state(&self) -> bool {
        self.night_on.load(Ordering::SeqCst)
    }

    /// Is the wake-up light on? Reported to the web interface.
    pub fn wake_light_on_state(&self) -> bool {
        self.wake_on.load(Ordering::SeqCst)
    }

    /// Switch a light on or off, optionally at a different brightness.
    fn set_light(
        light: &Option<Mutex<Output>>,
        state: &AtomicBool,
        duty: f64,
        on: bool,
        override_duty: Option<f64>,
    ) {
        if let Some(output) = light {
            let mut output = output.lock().unwrap_or_else(|e| e.into_inner());
            if on {
                output.start_pwm(override_duty.unwrap_or(duty), PWM_FREQUENCY);
            } else {
                output.stop_pwm();
                output.set(false);
            }
        }
        state.store(on, Ordering::SeqCst);
    }

    /// Switch the night light on, optionally at a different brightness.
    pub fn night_light_on(&self, duty_cycle: Option<f64>) {
        Self::set_light(&self.night, &self.night_on, self.night_duty, true, duty_cycle);
    }

    /// Switch the night light off.
    pub fn night_light_off(&self) {
        Self::set_light(&self.night, &self.night_on, self.night_duty, false, None);
    }

    /// Switch the wake-up light on, optionally at a different brightness.
    pub fn wake_light_on(&self, duty_cycle: Option<f64>) {
        Self::set_light(&self.wake, &self.wake_on, self.wake_duty, true, duty_cycle);
    }

    /// Switch the wake-up light off.
    pub fn wake_light_off(&self) {
        Self::set_light(&self.wake, &self.wake_on, self.wake_duty, false, None);
    }

    /// Blink the status LED while booting, so the phone signals that it starts.
    pub fn wake_light_blink(&self) {
        if let Some(output) = &self.wake {
            let mut output = output.lock().unwrap_or_else(|e| e.into_inner());
            output.start_pwm(50.0, BLINK_FREQUENCY);
        }
    }
}

// -----------------------------------------------------------------------------
// Rotary dial
// -----------------------------------------------------------------------------

/// Turns a train of dial impulses into digits.
///
/// The dial's impulse contact (NSI) opens and closes once per digit, so ten
/// pulses mean the digit 0. The normally-closed NSA contact signals that the
/// dial is turned away from rest at all. Kept free of I/O so the timing rules
/// can be tested without a dial attached.
pub struct PulseDecoder {
    /// The impulse counter is running, i.e. the dial was turned far enough.
    counting: bool,
    impulses: u32,
    low_pulse: u32,
    high_pulse: u32,
    /// Consecutive NSA samples seen so far, in 5 ms ticks.
    nsa_low_count: u32,
    nsa_high_count: u32,
    /// Samples since the last NSA tick.
    tick: u32,
}

impl Default for PulseDecoder {
    fn default() -> Self {
        PulseDecoder::new()
    }
}

impl PulseDecoder {
    pub fn new() -> Self {
        PulseDecoder {
            counting: false,
            impulses: 0,
            low_pulse: 0,
            high_pulse: 0,
            nsa_low_count: 0,
            nsa_high_count: 0,
            tick: 0,
        }
    }

    /// Feed one sample of both contacts; returns a digit once the dial returns
    /// to rest.
    pub fn step(&mut self, nsi_low: bool, nsa_low: bool) -> Option<u32> {
        let mut digit = None;

        // The NSA contact is only looked at every 5 ms.
        self.tick += 1;
        if self.tick >= NSA_TICK_SAMPLES {
            self.tick = 0;

            if nsa_low {
                // Dial turned away from rest.
                self.nsa_high_count = 0;
                if self.nsa_low_count > NSA_DEBOUNCE {
                    // Low long enough: start counting impulses.
                    if !self.counting {
                        self.counting = true;
                        self.low_pulse = 0;
                        self.high_pulse = 0;
                    }
                } else {
                    self.nsa_low_count += 1;
                }
            } else {
                // Dial back at rest: the digit is complete.
                self.nsa_low_count = 0;
                if self.nsa_high_count > NSA_DEBOUNCE {
                    if self.counting {
                        self.counting = false;
                        digit = Some(self.impulses % 10);
                        self.impulses = 0;
                    }
                } else {
                    self.nsa_high_count += 1;
                }
            }
        }

        // Impulses are counted every millisecond, but only while dialling.
        if self.counting {
            if nsi_low {
                self.low_pulse += 1;
                if self.low_pulse > LOW_PULSE_DURATION {
                    self.high_pulse = 0; // reset the last high pulse
                }
            } else {
                self.high_pulse += 1;
                if self.high_pulse > HIGH_PULSE_DURATION {
                    if self.low_pulse > LOW_PULSE_DURATION {
                        self.impulses += 1;
                    }
                    self.low_pulse = 0; // wait for the next falling slope
                }
            }
        }

        digit
    }
}

/// Watch the dial and the hook switch until the process ends.
///
/// Both are sampled from one thread: libgpiod's edge events are not available on
/// every kernel, and a millisecond poll is cheap enough for this purpose.
pub struct Watcher {
    nsi: backend::Input,
    nsa: backend::Input,
    hook: backend::Input,
    decoder: PulseDecoder,
    /// Last reported hook state (`true` = handset on the cradle).
    hooked: bool,
    /// State the hook is currently settling towards.
    pending: bool,
    /// When `pending` was first seen.
    since: Instant,
}

impl Watcher {
    pub fn new(gpio: &Gpio, nsi: u32, nsa: u32, hook: u32) -> Result<Self, String> {
        let nsi = gpio.input(nsi)?;
        let nsa = gpio.input(nsa)?;
        let hook_input = gpio.input(hook)?;

        let hooked = !hook_input.is_low();
        Ok(Watcher {
            nsi,
            nsa,
            hook: hook_input,
            decoder: PulseDecoder::new(),
            hooked,
            pending: hooked,
            since: Instant::now(),
        })
    }

    /// Is the handset resting on the cradle?
    pub fn is_hungup(&self) -> bool {
        self.hooked
    }

    /// Is the impulse contact (NSI) closed?
    ///
    /// Exposed for the wiring test; the phone itself only uses the decoded
    /// digits from [`Watcher::run`].
    pub fn nsi_is_low(&self) -> bool {
        self.nsi.is_low()
    }

    /// Is the normally-closed contact (NSA) closed, i.e. is the dial turned?
    pub fn nsa_is_low(&self) -> bool {
        self.nsa.is_low()
    }

    /// Sample the contacts, reporting every digit and every hook change.
    ///
    /// Must not be called from more than one thread.
    pub fn run(&mut self, on_digit: &mut dyn FnMut(u32), on_hook: &mut dyn FnMut(bool)) -> ! {
        loop {
            // The dial is the time-critical input, so it sets the pace.
            if let Some(digit) = self.decoder.step(self.nsi.is_low(), self.nsa.is_low()) {
                on_digit(digit);
            }

            self.poll_hook(on_hook);

            thread::sleep(DIAL_SAMPLE);
        }
    }

    /// Report a hook change once the contact has been stable long enough.
    fn poll_hook(&mut self, on_hook: &mut dyn FnMut(bool)) {
        let level = !self.hook.is_low(); // high = handset on the cradle

        if level != self.pending {
            // Bouncing: start settling towards the new level again.
            self.pending = level;
            self.since = Instant::now();
            return;
        }

        if level == self.hooked {
            return; // still the state we already reported
        }

        if self.since.elapsed() >= HOOK_DEBOUNCE {
            self.hooked = level;
            on_hook(level);
        }
    }
}

/// Poll the hook switch on its own, without the dial.
///
/// Used by the tests and by setups that do not use a rotary dial.
pub fn watch_hook(watcher: &mut Watcher, on_hook: &mut dyn FnMut(bool)) -> ! {
    loop {
        watcher.poll_hook(on_hook);
        thread::sleep(HOOK_SAMPLE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a full dial rotation of `pulses` impulses and return the digit.
    fn rotate(pulses: u32) -> Option<u32> {
        let mut decoder = PulseDecoder::new();
        let mut digit = None;

        // Turn away from rest; the debounce needs more than NSA_DEBOUNCE ticks.
        for _ in 0..(NSA_TICK_SAMPLES * (NSA_DEBOUNCE + 5)) {
            digit = decoder.step(false, true).or(digit);
        }
        // Each impulse: a low pulse followed by a longer high gap.
        for _ in 0..pulses {
            for _ in 0..(LOW_PULSE_DURATION + 4) {
                decoder.step(true, true);
            }
            for _ in 0..(HIGH_PULSE_DURATION + 6) {
                decoder.step(false, true);
            }
        }
        // Back to rest, past the debounce threshold.
        for _ in 0..(NSA_TICK_SAMPLES * (NSA_DEBOUNCE + 5)) {
            digit = decoder.step(false, false).or(digit);
        }

        digit
    }

    #[test]
    fn decodes_every_digit() {
        for pulses in 1..=9 {
            assert_eq!(rotate(pulses), Some(pulses), "{} Impulse", pulses);
        }
    }

    #[test]
    fn ten_pulses_mean_zero() {
        assert_eq!(rotate(10), Some(0));
    }

    #[test]
    fn bounce_while_turning_is_ignored() {
        let mut decoder = PulseDecoder::new();
        // A glitch shorter than the debounce threshold must not start counting.
        let samples = NSA_TICK_SAMPLES * NSA_DEBOUNCE;
        for _ in 0..samples {
            assert_eq!(decoder.step(false, true), None);
        }
        for _ in 0..samples {
            assert_eq!(decoder.step(false, false), None);
        }
    }

    #[test]
    fn idle_dial_emits_nothing() {
        let mut decoder = PulseDecoder::new();
        for _ in 0..2000 {
            assert_eq!(decoder.step(true, false), None);
        }
    }

    #[test]
    fn impulses_outside_a_rotation_are_ignored() {
        let mut decoder = PulseDecoder::new();
        // NSI moves while NSA stays at rest: no digit.
        for _ in 0..2000 {
            assert_eq!(decoder.step(true, false), None);
        }
    }

    #[test]
    fn two_rotations_give_two_digits() {
        let mut decoder = PulseDecoder::new();
        let settle = NSA_TICK_SAMPLES * (NSA_DEBOUNCE + 5);
        let mut digits = Vec::new();

        for pulses in [3u32, 7] {
            for _ in 0..settle {
                decoder.step(false, true);
            }
            for _ in 0..pulses {
                for _ in 0..(LOW_PULSE_DURATION + 4) {
                    decoder.step(true, true);
                }
                for _ in 0..(HIGH_PULSE_DURATION + 6) {
                    decoder.step(false, true);
                }
            }
            for _ in 0..settle {
                if let Some(digit) = decoder.step(false, false) {
                    digits.push(digit);
                }
            }
        }

        assert_eq!(digits, vec![3, 7]);
    }
}