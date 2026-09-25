// SPDX-License-Identifier: GPL-2.0-only
//! What the device models share: the [`Device`] and [`Tuner`] over a
//! [`Board`], which is what sets a model apart.
//!
//! The tuning sequence follows `ptx_chrdev.c` of px4_drv,
//! Copyright (c) 2018-2021 nns779.

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use futures::executor::block_on;
use futures::future::BoxFuture;
use futures::lock::Mutex;
use futures::{FutureExt, StreamExt};
use futures_timer::Delay;
use tunelith_core::usb::{NusbTransport, UsbTransport};
use tunelith_core::{
    ByteStream, Device, DeviceInfo, Error, Result, Signal, StreamFormat, System, TuneParams, Tuner,
    TunerInfo,
};

use crate::it930x::{EP_STREAM, It930x};
use crate::stream::Hub;

pub type Bridge = It930x<NusbTransport>;

/// How long to wait for the TS to lock, in polls 10 ms apart.
const LOCK_POLLS: u32 = 300;
/// The demodulators report a lock before the demodulation settles, which
/// breaks the first packets of an ISDB-T stream; px4_drv waits this long from
/// the tuning before giving the stream out.
const ISDB_T_SETTLE: Duration = Duration::from_millis(350);

pub enum Lock {
    Locked,
    Waiting,
    /// The demodulator gave up on the signal.
    Failed,
}

/// A device model: its bridges and the chips of each tuner.
pub trait Board: Send + 'static {
    fn bridge(&mut self, bridge: usize) -> &mut Bridge;

    /// The bridge and the input the TS of a tuner comes in on.
    fn source(&self, index: usize) -> (usize, usize);

    /// Readies a tuner for use.
    fn open(&mut self, index: usize) -> impl Future<Output = Result<()>> + Send;

    /// Puts a tuner back to rest; it cannot fail, as there is no one to tell.
    fn close(&mut self, index: usize) -> impl Future<Output = ()> + Send;

    /// Sets the tuner and the demodulator on the signal.
    fn tune(
        &mut self,
        index: usize,
        params: &TuneParams,
    ) -> impl Future<Output = Result<()>> + Send;

    fn lock(&mut self, index: usize, system: System) -> impl Future<Output = Result<Lock>> + Send;

    /// Picks the stream out of the transponder once it locks, if the model
    /// does it after the lock.
    fn select(
        &mut self,
        _index: usize,
        _params: &TuneParams,
    ) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }

    fn signal(
        &mut self,
        index: usize,
        system: System,
    ) -> impl Future<Output = Result<Signal>> + Send;

    fn set_lnb(&mut self, _index: usize, _on: bool) -> impl Future<Output = Result<()>> + Send {
        async { Err(io::Error::from(io::ErrorKind::Unsupported).into()) }
    }

    /// Lets the TS of a tuner out to the bridge.
    fn start_capture(&mut self, _index: usize) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }
}

struct State<B> {
    board: B,
    opened: Vec<bool>,
}

struct Shared<B> {
    state: Mutex<State<B>>,
    hubs: Vec<Hub>,
}

/// A device over `board`, whose tuners receive `systems`, and whose bridges
/// tag the packets of their inputs or not, as `tagged` says of each.
pub fn device<B: Board>(
    info: DeviceInfo,
    board: B,
    systems: Vec<Vec<System>>,
    tagged: &[bool],
) -> Box<dyn Device> {
    let tuners = systems
        .into_iter()
        .enumerate()
        .map(|(i, systems)| TunerInfo {
            id: format!("{}#{i}", info.id),
            systems,
        })
        .collect::<Vec<_>>();
    Box::new(BoardDevice {
        shared: Arc::new(Shared {
            state: Mutex::new(State {
                board,
                opened: vec![false; tuners.len()],
            }),
            hubs: tagged.iter().map(|&t| Hub::new(t)).collect(),
        }),
        info,
        tuners,
    })
}

struct BoardDevice<B> {
    info: DeviceInfo,
    tuners: Vec<TunerInfo>,
    shared: Arc<Shared<B>>,
}

impl<B: Board> Device for BoardDevice<B> {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn tuners(&self) -> &[TunerInfo] {
        &self.tuners
    }

    fn open_tuner(&self, index: usize) -> BoxFuture<'_, Result<Box<dyn Tuner>>> {
        async move {
            let Some(info) = self.tuners.get(index) else {
                return Err(Error::NotFound(format!("{}#{index}", self.info.id)));
            };
            let mut state = self.shared.state.lock().await;
            if state.opened[index] {
                return Err(io::Error::from(io::ErrorKind::ResourceBusy).into());
            }
            state.board.open(index).await?;
            state.opened[index] = true;
            Ok(Box::new(BoardTuner {
                shared: self.shared.clone(),
                index,
                systems: info.systems.clone(),
                system: None,
            }) as Box<dyn Tuner>)
        }
        .boxed()
    }
}

struct BoardTuner<B: Board> {
    shared: Arc<Shared<B>>,
    index: usize,
    systems: Vec<System>,
    /// The system last tuned to.
    system: Option<System>,
}

impl<B: Board> BoardTuner<B> {
    async fn tune(&mut self, params: TuneParams) -> Result<()> {
        params.validate()?;
        if !self.systems.contains(&params.system) {
            return Err(Error::Unsupported(params.system));
        }

        self.system = None;
        let started = Instant::now();
        self.shared
            .state
            .lock()
            .await
            .board
            .tune(self.index, &params)
            .await?;

        let mut locked = false;
        for _ in 0..LOCK_POLLS {
            let lock = {
                let mut state = self.shared.state.lock().await;
                state.board.lock(self.index, params.system).await?
            };
            match lock {
                Lock::Locked => {
                    locked = true;
                    break;
                }
                Lock::Failed => break,
                Lock::Waiting => Delay::new(Duration::from_millis(10)).await,
            }
        }
        if !locked {
            return Err(Error::NoLock);
        }

        self.shared
            .state
            .lock()
            .await
            .board
            .select(self.index, &params)
            .await?;

        if params.system == System::IsdbT
            && let Some(rest) = ISDB_T_SETTLE.checked_sub(started.elapsed())
        {
            Delay::new(rest).await;
        }
        self.system = Some(params.system);
        Ok(())
    }

    async fn signal(&mut self) -> Result<Signal> {
        let Some(system) = self.system else {
            return Ok(Signal::default());
        };
        let mut state = self.shared.state.lock().await;
        state.board.signal(self.index, system).await
    }

    async fn set_lnb(&mut self, on: bool) -> Result<()> {
        let mut state = self.shared.state.lock().await;
        state.board.set_lnb(self.index, on).await
    }

    async fn stream(&mut self) -> Result<(StreamFormat, ByteStream)> {
        if self.system.is_none() {
            return Err(Error::InvalidParams("the tuner has not been tuned"));
        }

        let mut state = self.shared.state.lock().await;
        let (bridge, slot) = state.board.source(self.index);
        let hub = &self.shared.hubs[bridge];
        let (rx, start) = hub.attach(slot);
        let result = async {
            if start {
                state.board.bridge(bridge).purge_psb().await?;
            }
            state.board.start_capture(self.index).await?;
            if start {
                hub.start(state.board.bridge(bridge).usb().bulk_in_queue(EP_STREAM)?);
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            if start {
                hub.abort(slot);
            } else {
                hub.detach(slot);
            }
            return Err(e);
        }

        Ok((StreamFormat::Ts, rx.map(Ok).boxed()))
    }
}

impl<B: Board> Tuner for BoardTuner<B> {
    fn tune(&mut self, params: TuneParams) -> BoxFuture<'_, Result<()>> {
        BoardTuner::tune(self, params).boxed()
    }

    fn signal(&mut self) -> BoxFuture<'_, Result<Signal>> {
        BoardTuner::signal(self).boxed()
    }

    fn set_lnb(&mut self, on: bool) -> BoxFuture<'_, Result<()>> {
        BoardTuner::set_lnb(self, on).boxed()
    }

    fn stream(&mut self) -> BoxFuture<'_, Result<(StreamFormat, ByteStream)>> {
        BoardTuner::stream(self).boxed()
    }
}

impl<B: Board> Drop for BoardTuner<B> {
    fn drop(&mut self) {
        let shared = self.shared.clone();
        let index = self.index;
        // ponytail: released on a thread of its own, as Drop cannot wait;
        // a tuner opened right after may find it still busy.
        thread::spawn(move || {
            block_on(async {
                let mut state = shared.state.lock().await;
                let (bridge, slot) = state.board.source(index);
                shared.hubs[bridge].detach(slot);
                let _ = state.board.set_lnb(index, false).await;
                state.board.close(index).await;
                state.opened[index] = false;
            })
        });
    }
}

/// Opens a bridge and sets it up for `inputs`.
pub async fn open_bridge(
    usb: &tunelith_core::usb::UsbDeviceInfo,
    inputs: &[crate::it930x::Input],
    firmware: &[u8],
) -> Result<Bridge> {
    let mut bridge = It930x::new(usb.open(0).await?, usb.usb_version);
    bridge.raise().await?;
    if bridge.read_reg(0x4979).await? == 0 {
        return Err(crate::error("EEPROM error"));
    }
    bridge.load_firmware(firmware).await?;
    bridge.init_warm(inputs).await?;
    Ok(bridge)
}

/// The LNB supply on GPIO 11, on while any tuner wants it.
pub struct Lnb(Vec<bool>);

impl Lnb {
    pub const GPIO: u8 = 11;

    pub fn new(tuners: usize) -> Self {
        Self(vec![false; tuners])
    }

    pub async fn set(&mut self, bridge: &mut Bridge, index: usize, on: bool) -> Result<()> {
        if self.0[index] == on {
            return Ok(());
        }
        let others = self.0.iter().enumerate().any(|(i, &o)| i != index && o);
        if !others {
            bridge.write_gpio(Self::GPIO, on).await?;
        }
        self.0[index] = on;
        Ok(())
    }
}
