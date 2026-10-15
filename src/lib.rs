//! PiPhone - a headless SIP telephone in a historical rotary dial phone.
//!
//! The crate is split so the interesting parts can be tested without hardware:
//!
//! - [`config`] holds the schema of the TOML configuration
//! - [`command`] names the things the phone can be told to do
//! - [`sounds`] holds the sound files the phone plays
//! - [`sip`] wraps the `rvoip` SIP stack that replaced `linphonec`
//! - [`webserver`] serves the status and configuration pages
//! - [`phone`] is the state machine tying them together
//! - [`gpio`] and [`callaudio`] talk to the hardware
//! - [`command`] names the features, [`sync`] holds the locking helper,
//!   [`lockfile`] keeps a second instance off the hardware

pub mod audio;
pub mod callaudio;
pub mod command;
pub mod config;
pub mod gpio;
pub mod lockfile;
pub mod pages;
pub mod phone;
pub mod sip;
pub mod sounds;
pub mod sync;
pub mod webserver;

pub use phone::{Options, Phone};
