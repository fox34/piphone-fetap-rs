//! PiPhone - a headless SIP telephone in a historical rotary dial phone.
//!
//! The crate is split so the interesting parts can be tested without hardware:
//!
//! - [`config`] holds the schema of the TOML configuration
//! - [`sip`] wraps the `rvoip` SIP stack that replaced `linphonec`
//! - [`webserver`] serves the status and configuration pages
//! - [`phone`] is the state machine tying them together
//! - [`gpio`] and [`callaudio`] talk to the hardware

pub mod audio;
pub mod callaudio;
pub mod config;
pub mod gpio;
pub mod pages;
pub mod phone;
pub mod sip;
pub mod webserver;

pub use phone::{Options, Phone};
