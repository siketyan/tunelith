//! USB transfers for the drivers that run the device in user space, behind a
//! trait so that a chip driver can be tested without one. nusb stays inside
//! this module.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::FutureExt;
use futures::future::{Either, select};
use futures_timer::Delay;
#[cfg(not(target_arch = "wasm32"))]
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

/// Holds a value of nusb, `Send` and `Sync` in WebUSB too, where it holds JS
/// objects: wasm32 without atomics has the one thread to run them on.
#[derive(Clone, Debug)]
struct Local<T>(T);

#[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
unsafe impl<T> Send for Local<T> {}
#[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
unsafe impl<T> Sync for Local<T> {}

impl<F: Future> Future for Local<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // SAFETY: the future is never moved out of `Local`.
        unsafe { self.map_unchecked_mut(|local| &mut local.0) }.poll(cx)
    }
}

/// An interface claimed through nusb.
pub struct NusbTransport(Local<nusb::Interface>);

impl NusbTransport {
    pub fn new(interface: nusb::Interface) -> Self {
        Self(Local(interface))
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
        let ep = self.0.0.endpoint::<Bulk, Out>(ep);
        Local(async move {
            let mut ep = ep?;
            ep.submit(data.into());
            let completion = with_timeout(ep.next_complete(), timeout).await?;
            Ok(completion.status?)
        })
    }

    fn bulk_in_queue(&self, ep: u8) -> io::Result<NusbBulkIn> {
        Ok(NusbBulkIn(Local(self.0.0.endpoint::<Bulk, In>(ep)?)))
    }
}

pub struct NusbBulkIn(Local<nusb::Endpoint<Bulk, In>>);

impl BulkIn for NusbBulkIn {
    fn submit(&mut self, len: usize) {
        let ep = &mut self.0.0;
        let packet = ep.max_packet_size();
        let buf = ep.allocate(len.div_ceil(packet) * packet);
        ep.submit(buf);
    }

    fn next_complete(&mut self) -> impl Future<Output = io::Result<Vec<u8>>> + Send {
        Local(self.0.0.next_complete().map(|completion| {
            completion.status?;
            let mut data = completion.buffer.into_vec();
            data.truncate(completion.actual_len);
            Ok(data)
        }))
    }
}

/// A USB device found by [`list_devices`].
#[derive(Clone, Debug)]
pub struct UsbDeviceInfo {
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial: Option<String>,
    /// The bus and the chain of hub ports from it, `9-1.2` for instance. WebUSB
    /// tells neither, so there it is the place in the list of devices.
    pub port_path: String,
    /// `bcdUSB`, 0x0200 for USB 2.0.
    pub usb_version: u16,
    inner: Local<nusb::DeviceInfo>,
}

// Elsewhere than in WebUSB, nusb blocks on the calls below unless it has a
// runtime of its own to await them on, so they run on the thread pool of
// `blocking`.

pub async fn list_devices() -> io::Result<Vec<UsbDeviceInfo>> {
    #[cfg(target_arch = "wasm32")]
    let devices = Local(async { nusb::list_devices().await }).await?;
    #[cfg(not(target_arch = "wasm32"))]
    let devices = blocking::unblock(|| nusb::list_devices().wait()).await?;
    Ok(devices
        .zip(0usize..)
        .map(|(d, _i)| UsbDeviceInfo {
            vendor_id: d.vendor_id(),
            product_id: d.product_id(),
            serial: d.serial_number().map(str::to_owned),
            #[cfg(target_arch = "wasm32")]
            port_path: _i.to_string(),
            #[cfg(not(target_arch = "wasm32"))]
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
            inner: Local(d),
        })
        .collect())
}

impl UsbDeviceInfo {
    /// Opens the device and claims the interface, taking it from a kernel
    /// driver bound to it.
    pub async fn open(&self, interface: u8) -> io::Result<NusbTransport> {
        let info = self.inner.0.clone();
        #[cfg(target_arch = "wasm32")]
        return Local(async move {
            let device = info.open().await?;
            Ok(NusbTransport::new(
                device.detach_and_claim_interface(interface).await?,
            ))
        })
        .await;
        #[cfg(not(target_arch = "wasm32"))]
        blocking::unblock(move || {
            let device = info.open().wait()?;
            Ok(NusbTransport::new(
                device.detach_and_claim_interface(interface).wait()?,
            ))
        })
        .await
    }
}
