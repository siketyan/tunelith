// SPDX-License-Identifier: GPL-2.0-only
//! Tunelith in the browser: the USB tuners of `tunelith-driver-px4` over
//! WebUSB, for JavaScript through wasm-bindgen.
//!
//! Build for `wasm32-unknown-unknown` with `--cfg=web_sys_unstable_apis`,
//! WebUSB being among the unstable APIs of web-sys, then run `wasm-bindgen`
//! on the module. The page is to let the user pick the device with
//! `navigator.usb.requestDevice` before `openDevice`, or have a policy
//! (`WebUsbAllowDevicesForUrls`) allow it.
//!
//! `example/` has a page that records from a tuner, and how to build it.
#![cfg(target_arch = "wasm32")]

use std::cell::RefCell;
use std::rc::Rc;

use futures::StreamExt;
use futures::future::{AbortHandle, Abortable};
use futures::lock::Mutex;
use tunelith_core::{ByteStream, Driver, StreamId, TuneParams};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
#[derive(Clone, Copy)]
pub enum System {
    IsdbT = "ISDB-T",
    IsdbS = "ISDB-S",
    IsdbS3 = "ISDB-S3",
}

#[wasm_bindgen]
#[derive(Clone, Copy)]
pub enum Polarization {
    Right = "right",
    Left = "left",
}

/// Opens the first device the page may reach, loading `firmware`, the
/// `it930x-firmware.bin` of the user, into it.
// ponytail: the first device only; take an id once pages drive several.
#[wasm_bindgen(js_name = openDevice)]
pub async fn open_device(firmware: Vec<u8>) -> Result<Device, JsError> {
    let driver = tunelith_driver_px4::driver_with_firmware(firmware);
    let info = driver
        .probe()
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| JsError::new("no device"))?;
    Ok(Device(Rc::new(driver.open(&info).await?)))
}

#[wasm_bindgen]
pub struct Device(Rc<Box<dyn tunelith_core::Device>>);

#[wasm_bindgen]
impl Device {
    #[wasm_bindgen(getter)]
    pub fn name(&self) -> String {
        self.0.info().name.clone()
    }

    #[wasm_bindgen(getter, js_name = tunerCount)]
    pub fn tuner_count(&self) -> usize {
        self.0.tuners().len()
    }

    /// Takes the tuner at `index` until it is closed.
    #[wasm_bindgen(js_name = openTuner)]
    pub async fn open_tuner(&self, index: usize) -> Result<Tuner, JsError> {
        let device = self.0.clone();
        let tuner = device.open_tuner(index).await?;
        Ok(Tuner {
            tuner: Mutex::new(Some(tuner)),
            stream: RefCell::default(),
            reading: RefCell::default(),
        })
    }
}

#[wasm_bindgen]
pub struct Tuner {
    /// `None` once closed.
    tuner: Mutex<Option<Box<dyn tunelith_core::Tuner>>>,
    /// Out of `tuner`'s lock, so that a read waiting on a stalled stream does
    /// not hold up a `close` or a `tune`.
    stream: RefCell<Option<ByteStream>>,
    /// Aborts the read in progress, which `close` and `tune` end.
    reading: RefCell<Option<AbortHandle>>,
}

#[wasm_bindgen]
pub struct Signal {
    pub locked: bool,
    #[wasm_bindgen(js_name = cnrDb)]
    pub cnr_db: Option<f64>,
}

#[wasm_bindgen]
impl Tuner {
    /// Tunes and waits for the signal to lock. `frequency_khz` is the one on
    /// air, before the LNB for a satellite, which takes a `stream_id` too.
    pub async fn tune(
        &self,
        system: System,
        #[wasm_bindgen(js_name = frequencyKhz)] frequency_khz: u32,
        #[wasm_bindgen(js_name = streamId)] stream_id: Option<u16>,
        polarization: Option<Polarization>,
    ) -> Result<(), JsError> {
        let params = TuneParams {
            system: match system {
                System::IsdbT => tunelith_core::System::IsdbT,
                System::IsdbS => tunelith_core::System::IsdbS,
                System::IsdbS3 => tunelith_core::System::IsdbS3,
                System::__Invalid => return Err(JsError::new("unknown system")),
            },
            frequency_khz,
            stream_id: stream_id.map(StreamId),
            polarization: match polarization {
                None | Some(Polarization::Right) => None,
                Some(Polarization::Left) => Some(tunelith_core::Polarization::Left),
                Some(Polarization::__Invalid) => {
                    return Err(JsError::new("unknown polarization"));
                }
            },
        };
        params.validate()?;

        self.end_stream();
        tuner(&mut *self.tuner.lock().await)?.tune(params).await?;
        Ok(())
    }

    pub async fn signal(&self) -> Result<Signal, JsError> {
        let signal = tuner(&mut *self.tuner.lock().await)?.signal().await?;
        Ok(Signal {
            locked: signal.locked,
            cnr_db: signal.cnr_db,
        })
    }

    /// Powers the LNB of a satellite antenna on or off.
    #[wasm_bindgen(js_name = setLnb)]
    pub async fn set_lnb(&self, on: bool) -> Result<(), JsError> {
        tuner(&mut *self.tuner.lock().await)?.set_lnb(on).await?;
        Ok(())
    }

    /// The next bytes received, in the format of the system tuned to: TS
    /// packets, or TLV ones for ISDB-S3. `undefined` once the stream ends,
    /// closing or tuning the tuner ending it too.
    pub async fn read(&self) -> Result<Option<Vec<u8>>, JsError> {
        let (handle, registration) = AbortHandle::new_pair();
        if self.reading.borrow().is_some() {
            return Err(JsError::new("a read is in progress"));
        }
        *self.reading.borrow_mut() = Some(handle);
        match Abortable::new(self.next_chunk(), registration).await {
            // The handle is still this read's, no other read starting while
            // it is there.
            Ok(chunk) => {
                self.reading.take();
                chunk
            }
            // Whoever aborted it took the handle, and one of a read started
            // since may be there now.
            Err(_) => Ok(None),
        }
    }

    /// Lets go of the tuner, returning once another may open it.
    pub async fn close(&self) {
        self.end_stream();
        if let Some(tuner) = self.tuner.lock().await.take() {
            tuner.close().await;
        }
    }
}

impl Tuner {
    async fn next_chunk(&self) -> Result<Option<Vec<u8>>, JsError> {
        let taken = self.stream.take();
        let mut stream = match taken {
            Some(stream) => stream,
            None => tuner(&mut *self.tuner.lock().await)?.stream().await?.1,
        };
        let chunk = stream.next().await;
        *self.stream.borrow_mut() = Some(stream);
        Ok(chunk.transpose()?)
    }

    fn end_stream(&self) {
        if let Some(reading) = self.reading.take() {
            reading.abort();
        }
        self.stream.take();
    }
}

fn tuner(
    tuner: &mut Option<Box<dyn tunelith_core::Tuner>>,
) -> Result<&mut Box<dyn tunelith_core::Tuner>, JsError> {
    tuner
        .as_mut()
        .ok_or_else(|| JsError::new("the tuner is closed"))
}
