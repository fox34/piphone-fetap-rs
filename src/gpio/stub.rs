//! GPIO backend for platforms without a GPIO character device.
//!
//! The phone only runs on Raspberry Pi OS; this backend exists so the crate
//! still builds and the hardware-independent parts (configuration, web
//! interface, dial timing) can be tested on a development machine.

use std::thread;
use std::time::Duration;

use super::Change;

/// GPIO controller shared by all pins of the phone.
pub struct Gpio;

impl Gpio {
    /// Always fails: there is no GPIO on this platform.
    pub fn new() -> Result<Self, String> {
        Err("GPIO steht nur unter Linux zur Verfügung.".to_string())
    }

    /// Always fails: there is no GPIO on this platform.
    pub fn output(&self, _pin: u32, _initial: bool) -> Result<Output, String> {
        Err("GPIO steht nur unter Linux zur Verfügung.".to_string())
    }

    /// Always fails: there is no GPIO on this platform.
    pub fn inputs(&self, _pins: &[u32]) -> Result<Inputs, String> {
        Err("GPIO steht nur unter Linux zur Verfügung.".to_string())
    }
}

/// An output line that swallows writes.
#[derive(Clone)]
pub struct Output;

impl Output {
    /// A cheap copy for another thread.
    pub fn clone_shared(&self) -> Self {
        self.clone()
    }

    /// Pretend the write succeeded.
    pub fn set(&self, _on: bool) -> bool {
        true
    }
}

/// The phone's inputs, without a chip behind them.
pub struct Inputs;

impl Inputs {
    /// Always open, so no contact change is ever seen.
    pub fn is_low(&self, _index: usize) -> bool {
        false
    }

    /// Never reports a change; the wait has to pass, or the caller would spin.
    pub fn wait(&self, timeout: Duration) -> Vec<Change> {
        thread::sleep(timeout);
        Vec::new()
    }
}
