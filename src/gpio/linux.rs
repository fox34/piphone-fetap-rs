//! GPIO backend for Raspberry Pi OS, using the character device interface.
//!
//! Raspberry Pi OS exposes the SoC's pins as `/dev/gpiochip0`. Requesting a line
//! through `gpiod` is the same as `GPIO.setup(..., pull_up_down=PUD_UP)` in the
//! Python version: the pull-up is what holds the dial and hook contacts at a
//! defined level while they are open.
//!
//! Every input of the phone is claimed in a single request with edge detection
//! enabled, and a reader thread blocks in `read_event()` to turn that into a
//! stream of level changes. This needs libgpiod 2.x: the crate only implements
//! edge events on the v2 ABI, and there the kernel stamps every edge with a
//! monotonic timestamp, which is what keeps the dial decoder on its millisecond
//! grid even while a callback is busy.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use gpiod::{AsValuesMut, Bias, Chip, Edge, EdgeDetect, Lines, Masked, Options};

use super::Change;

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

    /// Claim `pins` as inputs with the internal pull-up and edge detection.
    ///
    /// The dial contacts and the hook switch all pull to ground, so they share
    /// the pull-up and go into one request: a single blocking read then reports
    /// every level change the phone has to react to.
    pub fn inputs(&self, pins: &[u32]) -> Result<Inputs, String> {
        let lines = self
            .chip
            .request_lines(
                Options::input(pins)
                    .bias(Bias::PullUp)
                    .edge(EdgeDetect::Both)
                    .consumer(CONSUMER),
            )
            .map_err(|e| format!("GPIO-Pins {:?} nicht verfügbar: {}", pins, e))?;

        let levels = Arc::new(Mutex::new(Levels::read(&lines, pins)?));
        let (sender, changes) = mpsc::channel();

        // The line request owns the file descriptor, so the blocking read has to
        // live on its own thread; the watcher consumes what that thread reports.
        // Both sides need the level cache, so it is shared rather than moved.
        let seen = Arc::clone(&levels);
        thread::Builder::new()
            .name("gpio-edge".to_string())
            .spawn(move || read_edges(lines, seen, sender))
            .map_err(|e| format!("GPIO-Flanken nicht abfragbar: {}", e))?;

        Ok(Inputs { levels, changes, reported: AtomicBool::new(false) })
    }
}

/// Read the current levels once, so a watcher starts from the real wiring.
struct Levels {
    low: Vec<bool>,
}

impl Levels {
    fn read(lines: &Lines<gpiod::Input>, pins: &[u32]) -> Result<Self, String> {
        // The mask selects which lines to read, and has to name every one of
        // them: an empty mask is rejected by the kernel.
        let mut requested = Masked::<u8>::default();
        for index in 0..pins.len() {
            requested = requested.with(index as u8, Some(false));
        }

        let values = lines
            .get_values(requested)
            .map_err(|e| format!("GPIO-Pins {:?} nicht lesbar: {}", pins, e))?;

        // A line that cannot be read counts as open, so a broken contact cannot
        // make the phone believe a call was answered. `Masked` keeps the
        // levels in `bits`, and every requested line is in the mask.
        let low = (0..pins.len())
            .map(|index| values.bits & (1u8 << index) == 0)
            .collect();

        Ok(Levels { low })
    }
}

/// Block on the line request and report every level change.
fn read_edges(
    mut lines: Lines<gpiod::Input>,
    levels: Arc<Mutex<Levels>>,
    sender: mpsc::Sender<Change>,
) {
    loop {
        let event = match lines.read_event() {
            Ok(event) => event,
            Err(e) => {
                eprintln!("GPIO-Flanken nicht lesbar: {}", e);
                return;
            }
        };

        let change = Change {
            index: event.line as usize,
            // The lines are active-high, so a falling edge is a closed contact.
            low: event.edge == Edge::Falling,
            time: event.time,
            seen: Instant::now(),
        };

        // The wiring tests only ask for levels, so the cache has to follow the
        // stream even when nobody is waiting for changes.
        if let Some(level) = levels.lock().unwrap_or_else(|e| e.into_inner()).low.get_mut(change.index)
        {
            *level = change.low;
        }

        // Every consumer is gone, and a blocking read cannot be woken up from
        // here, so this thread stays parked until the process exits.
        if sender.send(change).is_err() {
            return;
        }
    }
}

/// The phone's inputs: the dial contacts and the hook switch.
pub struct Inputs {
    levels: Arc<Mutex<Levels>>,
    changes: Receiver<Change>,
    /// Set once the reader thread has stopped, to explain the silence.
    reported: AtomicBool,
}

impl Inputs {
    /// Level of one line, as of the most recent change.
    ///
    /// An unknown line counts as open, so a broken contact cannot make the
    /// phone believe a call was answered.
    pub fn is_low(&self, index: usize) -> bool {
        self.levels.lock().unwrap_or_else(|e| e.into_inner()).low.get(index).copied().unwrap_or(false)
    }

    /// Wait up to `timeout` for level changes, and return them in the order the
    /// kernel saw them.
    ///
    /// Returns an empty list when nothing happened. Waits forever once the
    /// reader thread is gone, because no level can change any more.
    pub fn wait(&self, timeout: Duration) -> Vec<Change> {
        let first = match self.changes.recv_timeout(timeout) {
            Ok(first) => first,
            Err(RecvTimeoutError::Timeout) => return Vec::new(),
            Err(RecvTimeoutError::Disconnected) => {
                if !self.reported.swap(true, Ordering::SeqCst) {
                    eprintln!("GPIO-Flanken abgebrochen, der Wählscheiben-Thread wartet.");
                }
                loop {
                    thread::park();
                }
            }
        };

        // Changes tend to arrive in bursts after the consumer was busy, and they
        // all belong to the same moment.
        let mut changes = vec![first];
        while let Ok(next) = self.changes.try_recv() {
            changes.push(next);
        }
        changes
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