//! USB transfers for the drivers that run the device in user space, behind a
//! trait so that a chip driver can be tested without one. nusb stays inside
//! this module.

use std::future::Future;
use std::io;
use std::time::Duration;

use futures::FutureExt;
use futures::future::{Either, select};
use futures_timer::Delay;
use nusb::MaybeFuture;
use nusb::transfer::{Bulk, In, Out};

pub trait UsbTransport: Send + Sync + 'static {
    type BulkIn: BulkIn;

    fn bulk_out(
        &self,
        ep: u8,
        data: Vec<u8>,
        timeout: Duration,
    ) -> impl Future<Output = io::Result<()>> + Send;

    /// Opens a bulk IN endpoint to keep transfers in flight on.
    fn bulk_in_queue(&self, ep: u8) -> io::Result<Self::BulkIn>;

    /// Receives one transfer of up to `len` bytes.
    fn bulk_in(
        &self,
        ep: u8,
        len: usize,
        timeout: Duration,
    ) -> impl Future<Output = io::Result<Vec<u8>>> + Send {
        let queue = self.bulk_in_queue(ep);
        async move {
            let mut queue = queue?;
            queue.submit(len);
            with_timeout(queue.next_complete(), timeout).await?
        }
    }
}

/// A bulk IN endpoint, whose transfers complete in the order submitted.
/// Dropping it cancels the ones in flight.
pub trait BulkIn: Send + 'static {
    /// Submits a transfer of up to `len` bytes, rounded up to whole packets.
    fn submit(&mut self, len: usize);

    fn next_complete(&mut self) -> impl Future<Output = io::Result<Vec<u8>>> + Send;
}

/// Fails with [`io::ErrorKind::TimedOut`] if `future` takes longer than `timeout`.
pub async fn with_timeout<T>(future: impl Future<Output = T>, timeout: Duration) -> io::Result<T> {
    match select(std::pin::pin!(future), Delay::new(timeout)).await {
        Either::Left((value, _)) => Ok(value),
        Either::Right(_) => Err(io::ErrorKind::TimedOut.into()),
    }
}

/// An interface claimed through nusb.
pub struct NusbTransport(nusb::Interface);

impl NusbTransport {
    pub fn new(interface: nusb::Interface) -> Self {
        Self(interface)
    }
}

impl UsbTransport for NusbTransport {
    type BulkIn = NusbBulkIn;

    fn bulk_out(
        &self,
        ep: u8,
        data: Vec<u8>,
        timeout: Duration,
    ) -> impl Future<Output = io::Result<()>> + Send {
        let ep = self.0.endpoint::<Bulk, Out>(ep);
        async move {
            let mut ep = ep?;
            ep.submit(data.into());
            let completion = with_timeout(ep.next_complete(), timeout).await?;
            Ok(completion.status?)
        }
    }

    fn bulk_in_queue(&self, ep: u8) -> io::Result<NusbBulkIn> {
        Ok(NusbBulkIn(self.0.endpoint::<Bulk, In>(ep)?))
    }
}

pub struct NusbBulkIn(nusb::Endpoint<Bulk, In>);

impl BulkIn for NusbBulkIn {
    fn submit(&mut self, len: usize) {
        let packet = self.0.max_packet_size();
        let buf = self.0.allocate(len.div_ceil(packet) * packet);
        self.0.submit(buf);
    }

    fn next_complete(&mut self) -> impl Future<Output = io::Result<Vec<u8>>> + Send {
        self.0.next_complete().map(|completion| {
            completion.status?;
            let mut data = completion.buffer.into_vec();
            data.truncate(completion.actual_len);
            Ok(data)
        })
    }
}

/// A USB device found by [`list_devices`].
#[derive(Clone, Debug)]
pub struct UsbDeviceInfo {
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial: Option<String>,
    /// The bus and the chain of hub ports from it, `9-1.2` for instance.
    pub port_path: String,
    /// `bcdUSB`, 0x0200 for USB 2.0.
    pub usb_version: u16,
    inner: nusb::DeviceInfo,
}

// nusb blocks on the calls below unless it has a runtime of its own to await
// them on, so they run on the thread pool of `blocking`.

pub async fn list_devices() -> io::Result<Vec<UsbDeviceInfo>> {
    let devices = blocking::unblock(|| nusb::list_devices().wait()).await?;
    Ok(devices
        .map(|d| UsbDeviceInfo {
            vendor_id: d.vendor_id(),
            product_id: d.product_id(),
            serial: d.serial_number().map(str::to_owned),
            port_path: format!(
                "{}-{}",
                d.bus_id(),
                d.port_chain()
                    .iter()
                    .map(u8::to_string)
                    .collect::<Vec<_>>()
                    .join(".")
            ),
            usb_version: d.usb_version(),
            inner: d,
        })
        .collect())
}

impl UsbDeviceInfo {
    /// Opens the device and claims the interface, taking it from a kernel
    /// driver bound to it.
    pub async fn open(&self, interface: u8) -> io::Result<NusbTransport> {
        let info = self.inner.clone();
        blocking::unblock(move || {
            let device = info.open().wait()?;
            Ok(NusbTransport(
                device.detach_and_claim_interface(interface).wait()?,
            ))
        })
        .await
    }
}
