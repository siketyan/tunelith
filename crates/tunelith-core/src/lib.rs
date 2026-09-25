//! Core types and traits of Tunelith, a low-level tuner abstraction for the
//! Japanese digital broadcasting systems (ISDB-T, ISDB-S and ISDB-S3).
//!
//! A backend is a [`Driver`], which finds and opens [`Device`]s, each with one
//! or more [`Tuner`]s. A [`Registry`] puts the drivers together and gives each
//! device to the first of them that reports it.

mod error;
mod registry;
mod traits;
mod types;

#[cfg(all(feature = "dvb", target_os = "linux"))]
pub mod dvb;
#[cfg(feature = "usb")]
pub mod usb;

pub use error::{Error, Result};
pub use registry::{Found, Registry};
pub use traits::{ByteStream, Device, Driver, I2c, Tuner};
pub use types::{
    DeviceInfo, Polarization, Signal, StreamFormat, StreamId, System, TuneParams, TunerInfo,
};
