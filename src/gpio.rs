//! GPIO access for the rotary dial, the hook switch and the two LEDs.
//!
//! The hardware layer is split in two: the timing and decoding rules below are
//! pure logic and therefore testable anywhere, while opening the actual pins is
//! delegated to a platform backend ([`linux`] on Raspberry Pi OS). That backend
//! reports level changes rather than levels, so [`Watcher`] has to rebuild the
//! 1 ms sample grid the decoder was written against from the kernel's own
//! timestamps.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::sync::lock;

#[cfg(target_os = "linux")]
#[path = "gpio/linux.rs"]
mod backend;

#[cfg(not(target_os = "linux"))]
#[path = "gpio/stub.rs"]
mod backend;

pub use backend::Gpio;

/// Debounce time for the hook switch, matching `bouncetime=100` in the Python version.
pub const HOOK_DEBOUNCE: Duration = Duration::from_millis(100);

/// Interval at which the dial is sampled, matching `SAMPLE_RATE` in the original.
const DIAL_SAMPLE: Duration = Duration::from_millis(1);

/// How long the watcher waits when nothing is settling. Only a level change or
/// an owed decoder sample can wake it, so this is just a safety net, not a
/// sampling rate.
const IDLE_WAIT: Duration = Duration::from_secs(3600);

/// Upper bound on the samples a single wake-up feeds. A callback that blocked
/// for longer than this has already lost the dial state, and the decoder must
/// not turn that into minutes of busy work.
const MAX_SAMPLES: u32 = 10_000;

/// How many 1 ms samples `elapsed` is worth, and what is left over for the next
/// call.
///
/// Only the sub-millisecond remainder is carried forward, which is what keeps the
/// samples exactly one millisecond apart over a whole dial rotation instead of
/// drifting. The time that does not fit through [`MAX_SAMPLES`] is *dropped*.
///
/// Dropping it is the whole point. Carrying it is the more obvious bookkeeping
/// and it is wrong: the leftover is added in again on the next call, and on the
/// one after that, so a single gap longer than the cap leaves `carry` holding
/// however long the phone sat unused and makes every later call step the decoder
/// through ten seconds of contact levels at a time. The debounce is then measured
/// against those ten seconds instead of against real time, and every bounce of the
/// dial contacts - which is what `NSA_DEBOUNCE` exists to reject - reads as a full
/// rotation and yields a digit of its own. A single rotation of "0" then arrives
/// as `0` followed by a handful of `0`s, no short code matches any more, and the
/// phone reports "number too long" until it is restarted.
fn grid(elapsed: Duration, carry: Duration) -> (u32, Duration) {
    let total = carry + elapsed;
    let samples = total.as_nanos() / DIAL_SAMPLE.as_nanos();

    if samples > u128::from(MAX_SAMPLES) {
        return (MAX_SAMPLES, Duration::ZERO);
    }

    let samples = samples as u32;
    (samples, total - DIAL_SAMPLE * samples)
}

/// Position of the impulse contact inside the single line request.
const NSI: usize = 0;

/// Position of the normally-closed contact inside the single line request.
const NSA: usize = 1;

/// Position of the hook switch inside the single line request.
const HOOK: usize = 2;

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

/// Software PWM frequency in Hz for the LEDs.
///
/// The frequency *is* the CPU cost: every period is two GPIO writes and a timer
/// wait, so 8 kHz - as in the Python original - woke a thread 8000 times a
/// second to dim a light that cannot show it. That measured 16% of a core on the
/// Pi Zero.
///
/// It must stay high enough that the LED does not visibly flicker, and that
/// threshold is higher than the textbook flicker fusion frequency suggests: it
/// depends on the observer, and on the brightness and viewing angle. A bright
/// amber light in a dark room is close to the worst case - peripheral vision
/// resolves flicker far above what foveal vision can. Measured on the real LED,
/// 200 Hz flickered visibly and 1000 Hz did not; 500 Hz is the next step down
/// from there, and only the real LED can say whether it is enough. If it ever
/// flickers again, raise it - the CPU is cheap, the flicker is not.
const PWM_FREQUENCY: f64 = 500.0;

/// Blink frequency for the status LED during boot.
const BLINK_FREQUENCY: f64 = 1.0;

// -----------------------------------------------------------------------------
// LEDs
// -----------------------------------------------------------------------------

/// The two half-periods of a software PWM, or `None` when `duty` sits at an end
/// of its range and the pin can be driven to a constant level instead.
///
/// `frequency` only matters for the blink during boot; the night and wake lights
/// always run at [`PWM_FREQUENCY`], which is far enough above the flicker fusion
/// threshold of a bare LED that it cannot show one.
fn timings(duty: f64, frequency: f64) -> Option<(Duration, Duration)> {
    let duty = duty.clamp(0.0, 100.0);

    // Both ends of the range are a plain level. Clamping *before* the test is
    // what puts an out-of-range duty here instead of in a thread that toggles as
    // fast as it can with one of its two half-periods at zero length.
    if duty <= 0.0 || duty >= 100.0 {
        return None;
    }

    let period = Duration::from_secs_f64(1.0 / frequency.clamp(0.1, PWM_FREQUENCY));
    let on_time = Duration::from_secs_f64(period.as_secs_f64() * duty / 100.0);

    Some((on_time, period.saturating_sub(on_time)))
}

/// Sleep until `from + duration`.
///
/// `thread::sleep_until` would say this exactly, but it is still unstable
/// (rust-lang/rust#113752), so the remaining time is computed here instead.
///
/// The caller chooses what `from` is, and that choice is what decides what stays
/// accurate under a late wake-up. A sleep can only end late, never early, so the
/// overrun has to land somewhere; anchoring the two halves of a cycle to
/// different points is deliberate - see [`Output::start_pwm`].
///
/// A duration already elapsed returns immediately, so the thread never busy-waits
/// if a write was slow.
fn sleep_until(from: Instant, duration: Duration) {
    let remaining = (from + duration).saturating_duration_since(Instant::now());
    if !remaining.is_zero() {
        thread::sleep(remaining);
    }
}

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
    /// A duty of 0 or 100 - the two ends of the range - needs no thread at all,
    /// whatever frequency was asked for, because there is nothing to modulate.
    fn start_pwm(&mut self, duty: f64, frequency: f64) {
        self.stop_pwm();

        match timings(duty, frequency) {
            // `timings` clamps, so the only way to get here is a duty at one of
            // the two ends of its range.
            None => self.set(duty >= 100.0),
            Some((on_time, off_time)) => {
                let stop = self.stop.clone();
                stop.store(false, Ordering::SeqCst);

                // The output handle is shared with this thread only; `set` takes `&self`.
                let line = self.line.clone_shared();
                let spawned = thread::Builder::new()
                    .name("gpio-pwm".to_string())
                    .spawn(move || {
                        while !stop.load(Ordering::SeqCst) {
                            let cycle = Instant::now();

                            // The on-pulse is measured from the start of the cycle,
                            // so the duty - the brightness - is what was asked for
                            // even if the cycle starts late.
                            line.set(true);
                            sleep_until(cycle, on_time);

                            // The off-time is measured from the *end* of the
                            // on-pulse, and this is the part that must not be
                            // traded away. A sleep can only ever end late, so
                            // anchoring the gap to the cycle would let an overrun
                            // eat into it: once the overrun is as long as the gap,
                            // the gap closes and the LED jumps to full brightness
                            // until the next cycle. That showed up as a visibly
                            // varying light level, because how much of the gap
                            // survived depended on whatever else the CPU was doing.
                            line.set(false);
                            sleep_until(Instant::now(), off_time);
                        }
                    });

                match spawned {
                    Ok(handle) => self.pwm = Some(handle),
                    // Every other thread in the crate reports a failed spawn
                    // rather than panicking on it. `thread::spawn` panics, and the
                    // release profile is `panic = "abort"`, so a Pi Zero that
                    // cannot start one more thread would lose the phone over a
                    // dimmed LED.
                    Err(e) => eprintln!("PWM-Thread nicht startbar: {}", e),
                }
            }
        }
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
            let mut output = lock(&output);
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
            let mut output = lock(&output);
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

    /// Samples still needed before a digit can be reported, assuming the
    /// contacts keep the levels they have now.
    ///
    /// A digit is only finished once NSA has read high for longer than the
    /// debounce, and that happens without any further level change - so this is
    /// what stops the watcher from sleeping through the end of a rotation.
    /// `None` when nothing is pending.
    ///
    /// `nsa_low` is the contact's level *now*, not the one the last sample saw.
    /// The two differ right after a change, because no sample has been stepped
    /// for the new level yet, and asking the decoder here instead would let a
    /// finished dial sit unread until the next unrelated contact moved.
    pub fn pending_samples(&self, nsa_low: bool) -> Option<u32> {
        if !self.counting || nsa_low {
            return None;
        }

        let ticks = (NSA_DEBOUNCE + 1).saturating_sub(self.nsa_high_count);
        Some(ticks.max(1) * NSA_TICK_SAMPLES)
    }
}

/// A level change on one of the phone's inputs, as reported by the backend.
pub struct Change {
    /// Position of the line inside the request passed to [`Gpio::inputs`].
    pub index: usize,
    /// Is the line low after the change, i.e. is the contact closed?
    pub low: bool,
    /// When the change happened, on the kernel's monotonic clock. Differences
    /// between two changes are exact, no matter what the consumer was doing -
    /// that is the whole point, and the reason this is not an [`Instant`].
    pub time: Duration,
    /// When the change was picked up here. A few microseconds behind
    /// [`Change::time`], and the right clock for waiting out a debounce.
    pub seen: Instant,
}

/// Watch the dial and the hook switch until the process ends.
///
/// All three contacts are claimed in one request and read as level changes, so
/// the thread sleeps until the wiring actually moves instead of asking the chip
/// a thousand times a second. Each change carries the kernel's own timestamp,
/// which lets [`PulseDecoder`] keep running on its millisecond grid even when a
/// callback kept the thread busy for a while.
pub struct Watcher {
    contacts: backend::Inputs,
    decoder: PulseDecoder,
    /// Impulse contact level, as of the last change that was applied.
    nsi_low: bool,
    /// Normally-closed contact level, as of the last change that was applied.
    nsa_low: bool,
    /// Kernel time the decoder grid was last advanced to.
    applied: Option<Duration>,
    /// When that kernel time was, on this machine's clock.
    last_seen: Instant,
    /// Elapsed time too short to be a whole sample yet.
    carry: Duration,
    /// Last reported hook state (`true` = handset on the cradle).
    hooked: bool,
    /// State the hook is currently settling towards.
    pending: bool,
    /// When `pending` was first seen.
    since: Instant,
}

impl Watcher {
    pub fn new(gpio: &Gpio, nsi: u32, nsa: u32, hook: u32) -> Result<Self, String> {
        let contacts = gpio.inputs(&[nsi, nsa, hook])?;

        let nsi_low = contacts.is_low(NSI);
        let nsa_low = contacts.is_low(NSA);
        let hooked = !contacts.is_low(HOOK);

        Ok(Watcher {
            contacts,
            decoder: PulseDecoder::new(),
            nsi_low,
            nsa_low,
            // The levels before the first change are not observable, and the
            // dial is idle at startup, so the grid starts with the first edge.
            applied: None,
            last_seen: Instant::now(),
            carry: Duration::ZERO,
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
        self.contacts.is_low(NSI)
    }

    /// Is the normally-closed contact (NSA) closed, i.e. is the dial turned?
    pub fn nsa_is_low(&self) -> bool {
        self.contacts.is_low(NSA)
    }

    /// Watch the contacts, reporting every digit and every hook change.
    ///
    /// Must not be called from more than one thread.
    pub fn run(&mut self, on_digit: &mut dyn FnMut(u32), on_hook: &mut dyn FnMut(bool)) -> ! {
        let mut digits = Vec::new();

        loop {
            for change in self.contacts.wait(self.wake_after()) {
                self.apply(change, &mut digits);
            }

            // No change this time, but the decoder may still owe a digit: the
            // dial reached the end of its travel without a further level change.
            self.advance(self.now(), &mut digits);

            // Reporting after the batch, so a callback that takes its time
            // cannot push further changes out of the decoder's own timeline.
            for digit in digits.drain(..) {
                on_digit(digit);
            }

            if let Some(hooked) = self.settle() {
                on_hook(hooked);
            }
        }
    }

    /// Bring the decoder up to `change`, then take on the new level.
    fn apply(&mut self, change: Change, digits: &mut Vec<u32>) {
        // The line held its previous level until this moment, and the decoder
        // has to see that time before it sees the new level.
        self.advance(change.time, digits);

        self.last_seen = change.seen;

        match change.index {
            NSI => self.nsi_low = change.low,
            NSA => self.nsa_low = change.low,
            // high = handset on the cradle
            _ => {
                let level = !change.low;
                if level != self.pending {
                    self.pending = level;
                    self.since = change.seen;
                }
            }
        }
    }

    /// Feed the decoder the samples the current levels added up to `until`.
    ///
    /// The sub-millisecond leftovers stay in `carry`, so the samples stay exactly
    /// one millisecond apart over a whole dial rotation instead of drifting. A gap
    /// that outgrows [`MAX_SAMPLES`] loses its tail instead - see [`grid`].
    fn advance(&mut self, until: Duration, digits: &mut Vec<u32>) {
        let Some(from) = self.applied else {
            self.applied = Some(until);
            return;
        };

        let (samples, carry) = grid(until.saturating_sub(from), self.carry);

        for _ in 0..samples {
            if let Some(digit) = self.decoder.step(self.nsi_low, self.nsa_low) {
                digits.push(digit);
            }
        }

        self.carry = carry;
        self.applied = Some(until);
    }

    /// The kernel clock, as far as this watcher is concerned: the time of the
    /// last change, plus however long ago that was. Only the difference from the
    /// last change is ever used, so the two clocks are never compared across.
    fn now(&self) -> Duration {
        self.applied.unwrap_or(Duration::ZERO)
            + Instant::now().saturating_duration_since(self.last_seen)
    }

    /// Report the hook change once the contact has been stable long enough.
    fn settle(&mut self) -> Option<bool> {
        if self.pending == self.hooked || self.since.elapsed() < HOOK_DEBOUNCE {
            return None;
        }

        self.hooked = self.pending;
        Some(self.hooked)
    }

    /// How long this watcher may sleep: until the hook stops bouncing, or until
    /// the decoder has been fed the samples it is waiting for. With neither
    /// pending, only a level change can wake it.
    fn wake_after(&self) -> Duration {
        let dial = match self.decoder.pending_samples(self.nsa_low) {
            Some(samples) => DIAL_SAMPLE * samples,
            None => IDLE_WAIT,
        };

        dial.min(self.settle_after())
    }

    /// How long the next change may take before the hook has to be looked at
    /// again: until the debounce runs out, or indefinitely while the handset
    /// stays where it was reported.
    fn settle_after(&self) -> Duration {
        if self.pending == self.hooked {
            return IDLE_WAIT;
        }

        (self.since + HOOK_DEBOUNCE).saturating_duration_since(Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The nanoseconds of a duration, as an integer, for comparisons.
    fn ns(d: Duration) -> i128 {
        d.as_nanos() as i128
    }

    #[test]
    fn the_range_ends_need_no_thread() {
        assert_eq!(timings(0.0, PWM_FREQUENCY), None);
        assert_eq!(timings(100.0, PWM_FREQUENCY), None);

        // A duty outside the range is clamped to an end, so it needs no thread
        // either - it used to produce one with a zero-length half period, which
        // toggled the pin as fast as it could.
        assert_eq!(timings(-1.0, PWM_FREQUENCY), None);
        assert_eq!(timings(150.0, PWM_FREQUENCY), None);

        // The blink is not an exception: at full brightness it is just a steady
        // light, whatever its frequency.
        assert_eq!(timings(100.0, BLINK_FREQUENCY), None);
    }

    #[test]
    fn the_half_periods_add_up_to_the_period() {
        for frequency in [BLINK_FREQUENCY, 100.0, PWM_FREQUENCY] {
            let period = ns(Duration::from_secs_f64(1.0 / frequency));
            for duty in [1.0, 25.0, 50.0, 75.0, 99.0] {
                let (on, off) = timings(duty, frequency).expect("duty in range needs a thread");

                assert!(ns(on) > 0, "duty {} at {} Hz has no on time", duty, frequency);
                assert!(ns(off) > 0, "duty {} at {} Hz has no off time", duty, frequency);
                assert_eq!(ns(on) + ns(off), period, "duty {} at {} Hz", duty, frequency);
            }
        }
    }

    #[test]
    fn the_duty_is_the_share_of_the_period_that_is_on() {
        for duty in [10.0, 33.0, 50.0, 90.0] {
            let (on, off) = timings(duty, PWM_FREQUENCY).expect("duty in range needs a thread");
            let total = ns(on) + ns(off);

            // Integer division, so this holds to within one nanosecond.
            let expected = total / 100 * duty as i128;
            assert!(
                (ns(on) - expected).abs() <= 1,
                "duty {} gave {} ns on out of {} ns",
                duty,
                ns(on),
                total
            );
        }
    }

    #[test]
    fn an_even_duty_splits_the_period_evenly() {
        let (on, off) = timings(50.0, PWM_FREQUENCY).expect("half duty needs a thread");
        assert_eq!(ns(on), ns(off));
    }

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
    fn an_idle_decoder_owes_nothing() {
        // This is what lets the watcher sleep until a contact moves.
        let mut decoder = PulseDecoder::new();
        for _ in 0..2000 {
            decoder.step(true, false);
        }

        assert_eq!(decoder.pending_samples(false), None);
    }

    #[test]
    fn a_finished_dial_is_owed_before_the_next_sample() {
        let mut decoder = PulseDecoder::new();
        let settle = NSA_TICK_SAMPLES * (NSA_DEBOUNCE + 5);

        // Turn far enough that the impulse counter runs.
        for _ in 0..settle {
            decoder.step(false, true);
        }
        assert_eq!(decoder.pending_samples(true), None, "dial still turning");

        // The dial is back at rest, but no sample has been stepped for that new
        // level yet. Asking the decoder whether NSA is high here is exactly the
        // mistake that left a finished digit unread until the hook moved.
        assert!(decoder.pending_samples(false).is_some(), "digit is owed right away");
    }

    #[test]
    fn a_digit_is_owed_until_the_dial_settled() {
        let mut decoder = PulseDecoder::new();
        let settle = NSA_TICK_SAMPLES * (NSA_DEBOUNCE + 5);

        // While the dial is turning, nothing is owed: the digit is not complete.
        for _ in 0..settle {
            assert_eq!(decoder.step(false, true), None);
        }
        assert_eq!(decoder.pending_samples(true), None);

        // Back at rest, the digit is owed until the NSA debounce has run out.
        let mut digit = decoder.step(false, false);
        let mut owed = decoder.pending_samples(false).expect("dial at rest owes a digit");
        assert!(owed > 0);

        for _ in 0..settle {
            digit = decoder.step(false, false).or(digit);

            let Some(left) = decoder.pending_samples(false) else { break };
            assert!(left <= owed, "the debt never grows");
            owed = left;
        }

        assert_eq!(digit, Some(0), "a rotation without impulses is a 0");
        assert_eq!(decoder.pending_samples(false), None, "nothing owed once reported");
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

    #[test]
    fn a_long_gap_is_dropped_instead_of_carried_forward() {
        // The phone sits unused for hours between calls, so the first change after
        // an idle period is capped - that part is fine.
        let (samples, carry) = grid(Duration::from_secs(300), Duration::ZERO);
        assert_eq!(samples, MAX_SAMPLES, "the cap bounds the work");

        // What must not survive is the tail. Carried, it is fed in again on the
        // next call and the one after, and every call would keep stepping the
        // decoder through MAX_SAMPLES worth of levels no matter how little time
        // had actually passed.
        assert_eq!(carry, Duration::ZERO, "the unfed time is dropped");

        // The call right after an idle period is the one that matters: it has to
        // see only the milliseconds that really went by.
        let (samples, carry) = grid(DIAL_SAMPLE * 5, carry);
        assert_eq!(samples, 5, "five milliseconds must stay five milliseconds");
        assert_eq!(carry, Duration::ZERO);
    }

    #[test]
    fn the_sub_millisecond_remainder_is_kept() {
        // This is what `carry` is for: the samples stay exactly one millisecond
        // apart across a whole rotation instead of drifting by the rounding.
        let (_, carry) = grid(Duration::from_nanos(1_100_000), Duration::ZERO);
        assert_eq!(carry, Duration::from_micros(100));

        // The next millisecond starts where the last one stopped, not where the
        // caller's rounding left off.
        let (samples, carry) = grid(Duration::from_nanos(950_000), carry);
        assert_eq!(samples, 1);
        assert_eq!(carry, Duration::from_micros(50), "nothing is lost across two calls");
    }

    /// The NSA contact bounces for a few milliseconds when the dial snaps back.
    /// `NSA_DEBOUNCE` is what tells that apart from a turn - but only if the
    /// decoder is fed the time that really passed. A grid flooded with
    /// `MAX_SAMPLES` stretches every bounce to ten seconds, which reads as a full
    /// rotation and produces a digit of its own.
    #[test]
    fn a_flooded_grid_turns_a_bounce_into_a_digit() {
        let mut honest = PulseDecoder::new();
        let mut digits = Vec::new();

        // A five millisecond bounce: one tick of NSA, and back.
        for _ in 0..5 {
            digits.extend(honest.step(true, true));
        }
        for _ in 0..5 {
            digits.extend(honest.step(true, false));
        }
        assert_eq!(digits, Vec::<u32>::new(), "a bounce is not a rotation");

        // The same bounce on a flooded grid. This is what the leftover in `carry`
        // used to produce, once the phone had been idle for longer than the cap.
        let mut flooded = PulseDecoder::new();
        let mut digits = Vec::new();

        for _ in 0..MAX_SAMPLES {
            digits.extend(flooded.step(true, true));
        }
        for _ in 0..MAX_SAMPLES {
            digits.extend(flooded.step(true, false));
        }
        assert_eq!(digits, vec![0], "ten seconds of contact is a rotation");
    }

    #[test]
    fn the_cap_still_settles_a_rotation_that_was_interrupted() {
        // Truncating must not leave the decoder stuck: the levels that follow a
        // dropped gap are still constant, so feeding the cap settles the debounce
        // and the digit is reported once real time has passed again.
        let mut decoder = PulseDecoder::new();
        let settle = NSA_TICK_SAMPLES * (NSA_DEBOUNCE + 5);

        // Turn away from rest, then lose the thread for longer than the cap.
        for _ in 0..settle {
            decoder.step(true, true);
        }
        assert!(decoder.counting, "the turn was seen");

        // Back at rest, fed the cap rather than the hours that really passed.
        let mut digit = None;
        for _ in 0..MAX_SAMPLES {
            digit = decoder.step(true, false).or(digit);
        }
        assert_eq!(digit, Some(0), "the digit survives the gap");
        assert!(!decoder.counting, "and the decoder is idle again");
    }
}