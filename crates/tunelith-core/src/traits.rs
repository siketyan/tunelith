use futures::future::BoxFuture;
use futures::stream::BoxStream;

use crate::{DeviceInfo, Result, Signal, StreamFormat, TuneParams, TunerInfo};

/// The bytes a tuner receives, in chunks of any size.
pub type ByteStream = BoxStream<'static, Result<Vec<u8>>>;

/// A backend, which finds devices and opens them.
pub trait Driver: Send + Sync {
    fn probe(&self) -> BoxFuture<'_, Result<Vec<DeviceInfo>>>;

    fn open<'a>(&'a self, device: &'a DeviceInfo) -> BoxFuture<'a, Result<Box<dyn Device>>>;
}

/// A physical device, with one or more tuners.
pub trait Device: Send + Sync {
    fn info(&self) -> &DeviceInfo;

    fn tuners(&self) -> &[TunerInfo];

    /// Takes the tuner for the caller alone until the handle is dropped.
    fn open_tuner(&self, index: usize) -> BoxFuture<'_, Result<Box<dyn Tuner>>>;
}

/// A tuner taken by [`Device::open_tuner`].
pub trait Tuner: Send {
    /// Tunes and waits for the signal to lock.
    fn tune(&mut self, params: TuneParams) -> BoxFuture<'_, Result<()>>;

    fn signal(&mut self) -> BoxFuture<'_, Result<Signal>>;

    /// Powers the LNB of a satellite antenna on or off.
    fn set_lnb(&mut self, on: bool) -> BoxFuture<'_, Result<()>>;

    /// Starts giving out what the tuner receives, in the format of the system
    /// it was last tuned to.
    fn stream(&mut self) -> BoxFuture<'_, Result<(StreamFormat, ByteStream)>>;
}

/// An I2C bus, over which a bridge reaches the tuner and demodulator chips.
pub trait I2c: Send {
    fn write(&mut self, addr: u8, data: &[u8]) -> impl Future<Output = Result<()>> + Send;

    /// Writes `data`, then reads `buf.len()` bytes back.
    fn write_read(
        &mut self,
        addr: u8,
        data: &[u8],
        buf: &mut [u8],
    ) -> impl Future<Output = Result<()>> + Send;
}
