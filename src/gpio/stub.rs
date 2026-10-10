//! GPIO backend for platforms without a GPIO character device.
//!
//! The phone only runs on Raspberry Pi OS; this backend exists so the crate
//! still builds and the hardware-independent parts (configuration, web
//! interface, dial timing) can be tested on a development machine.

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
    pub fn input(&self, _pin: u32) -> Result<Input, String> {
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

/// An input line that always reads as open.
pub struct Input;

impl Input {
    /// Always open, so no dial digit and no hook movement is produced.
    pub fn is_low(&self) -> bool {
        false
    }
}