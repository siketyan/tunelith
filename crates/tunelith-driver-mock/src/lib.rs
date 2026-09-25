//! A device that is not there, for testing what sits above the drivers where
//! there is no tuner: one device of two tuners, receiving every system, that
//! locks on whatever it is tuned to and gives out null packets.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use futures::stream::{self, StreamExt};
use futures_timer::Delay;
use tunelith_core::{
    ByteStream, Device, DeviceInfo, Driver, Error, Result, Signal, StreamFormat, System,
    TuneParams, Tuner, TunerInfo,
};

const ID: &str = "mock:0";
const TUNERS: usize = 2;
/// The carrier-to-noise ratio every tuner reports once tuned.
pub const CNR_DB: f64 = 30.0;
/// The packets in each chunk of a stream, one every [`INTERVAL`]: 9.6 Mbit/s.
const PACKETS: usize = 64;
const INTERVAL: Duration = Duration::from_millis(10);

pub struct MockDriver;

pub fn driver() -> MockDriver {
    MockDriver
}

fn info() -> DeviceInfo {
    DeviceInfo {
        id: ID.to_owned(),
        name: "Tunelith mock".to_owned(),
    }
}

impl Driver for MockDriver {
    fn probe(&self) -> BoxFuture<'_, Result<Vec<DeviceInfo>>> {
        async { Ok(vec![info()]) }.boxed()
    }

    fn open<'a>(&'a self, device: &'a DeviceInfo) -> BoxFuture<'a, Result<Box<dyn Device>>> {
        async move {
            if device.id != ID {
                return Err(Error::NotFound(device.id.clone()));
            }
            Ok(Box::new(MockDevice {
                info: info(),
                tuners: (0..TUNERS)
                    .map(|i| TunerInfo {
                        id: format!("{ID}#{i}"),
                        systems: vec![System::IsdbT, System::IsdbS, System::IsdbS3],
                    })
                    .collect(),
                opened: Arc::default(),
            }) as Box<dyn Device>)
        }
        .boxed()
    }
}

struct MockDevice {
    info: DeviceInfo,
    tuners: Vec<TunerInfo>,
    opened: Arc<[AtomicBool; TUNERS]>,
}

impl Device for MockDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn tuners(&self) -> &[TunerInfo] {
        &self.tuners
    }

    fn open_tuner(&self, index: usize) -> BoxFuture<'_, Result<Box<dyn Tuner>>> {
        async move {
            let Some(opened) = self.opened.get(index) else {
                return Err(Error::NotFound(format!("{ID}#{index}")));
            };
            if opened.swap(true, Ordering::AcqRel) {
                return Err(io::Error::from(io::ErrorKind::ResourceBusy).into());
            }
            Ok(Box::new(MockTuner {
                opened: self.opened.clone(),
                index,
                params: None,
            }) as Box<dyn Tuner>)
        }
        .boxed()
    }
}

struct MockTuner {
    opened: Arc<[AtomicBool; TUNERS]>,
    index: usize,
    params: Option<TuneParams>,
}

impl Tuner for MockTuner {
    fn tune(&mut self, params: TuneParams) -> BoxFuture<'_, Result<()>> {
        async move {
            params.validate()?;
            self.params = Some(params);
            Ok(())
        }
        .boxed()
    }

    fn signal(&mut self) -> BoxFuture<'_, Result<Signal>> {
        let locked = self.params.is_some();
        async move {
            Ok(Signal {
                locked,
                cnr_db: locked.then_some(CNR_DB),
            })
        }
        .boxed()
    }

    fn set_lnb(&mut self, _on: bool) -> BoxFuture<'_, Result<()>> {
        async { Ok(()) }.boxed()
    }

    fn stream(&mut self) -> BoxFuture<'_, Result<(StreamFormat, ByteStream)>> {
        let params = self.params;
        async move {
            let format = params.ok_or(Error::NoLock)?.stream_format();
            let stream = stream::unfold(0u8, move |counter| async move {
                Delay::new(INTERVAL).await;
                let chunk = (0..PACKETS)
                    .flat_map(|i| null_packet(format, counter.wrapping_add(i as u8)))
                    .collect();
                Some((Ok(chunk), counter.wrapping_add(PACKETS as u8)))
            });
            Ok((format, stream.boxed()))
        }
        .boxed()
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        async {}.boxed()
    }
}

impl Drop for MockTuner {
    fn drop(&mut self) {
        self.opened[self.index].store(false, Ordering::Release);
    }
}

/// A null packet of 188 bytes: a TS one, its continuity counter taken from
/// `counter`, or a TLV one.
fn null_packet(format: StreamFormat, counter: u8) -> [u8; 188] {
    let mut packet = [0xff; 188];
    match format {
        StreamFormat::Ts => packet[..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10 | counter & 0x0f]),
        StreamFormat::Tlv => packet[..4].copy_from_slice(&[0x7f, 0xff, 0x00, 184]),
    }
    packet
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use tunelith_core::{Driver, System, TuneParams};

    use super::*;

    #[test]
    fn tunes_and_streams() {
        block_on(async {
            let driver = driver();
            let info = driver.probe().await.unwrap().remove(0);
            let device = driver.open(&info).await.unwrap();
            let mut tuner = device.open_tuner(0).await.unwrap();
            assert!(device.open_tuner(0).await.is_err());

            tuner
                .tune(TuneParams {
                    system: System::IsdbT,
                    frequency_khz: 515_143,
                    stream_id: None,
                    polarization: None,
                })
                .await
                .unwrap();
            assert_eq!(tuner.signal().await.unwrap().cnr_db, Some(CNR_DB));

            let (format, mut stream) = tuner.stream().await.unwrap();
            assert_eq!(format, StreamFormat::Ts);
            let chunk = stream.next().await.unwrap().unwrap();
            assert_eq!(chunk.len(), PACKETS * 188);
            assert!(chunk.chunks(188).all(|p| p[0] == 0x47));

            drop(stream);
            tuner.close().await;
            assert!(device.open_tuner(0).await.is_ok());
        });
    }
}
