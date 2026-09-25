// SPDX-License-Identifier: GPL-2.0-only
//! The PX-MLT series and its kin: an IT930x with up to five CXD2856ER and
//! CXD2858ER pairs, every tuner receiving both ISDB-T and ISDB-S.
//!
//! Ported from `pxmlt_device.c` and `ptx_chrdev.c` of px4_drv,
//! Copyright (c) 2018-2021 nns779.

use std::io;
use std::sync::{Arc, Mutex as SyncMutex};
use std::thread;
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use futures::executor::block_on;
use futures::future::BoxFuture;
use futures::lock::Mutex;
use futures::{FutureExt, StreamExt};
use futures_timer::Delay;
use tunelith_core::usb::{BulkIn, NusbTransport, UsbDeviceInfo, UsbTransport};
use tunelith_core::{
    ByteStream, Device, DeviceInfo, Error, Result, Signal, StreamFormat, System, TuneParams, Tuner,
    TunerInfo,
};

use crate::cxd2856er::{Cxd2856er, Mode};
use crate::cxd2858er::Cxd2858er;
use crate::it930x::{self, EP_STREAM, Input, It930x};
use crate::{cnr, error};

/// The address, I2C bus and TS port of the demodulator of each tuner.
pub type Layout = &'static [(u8, u8, u8)];

const GPIO_BACKEND_RESET: u8 = 7;
const GPIO_BACKEND_POWER: u8 = 2;
const GPIO_LNB_POWER: u8 = 11;

/// What `pxmlt_chrdev_open` sets on the demodulator: register, value, mask.
const DEMOD_SETUP: &[(u8, u8, u8)] = &[
    (0x00, 0x00, 0xff),
    (0xc4, 0x80, 0x88),
    (0xc5, 0x01, 0x01),
    (0xc6, 0x03, 0x1f),
    (0x00, 0x60, 0xff),
    (0x52, 0x03, 0x1f),
    (0x00, 0x00, 0xff),
    (0xc8, 0x03, 0x1f),
    (0xc9, 0x03, 0x1f),
    (0x00, 0xa0, 0xff),
    (0xb9, 0x01, 0x01),
];

/// How long to wait for the TS to lock, in polls 10 ms apart.
const LOCK_POLLS: u32 = 300;
/// The CXD2856ER reports a lock before the demodulation settles, which
/// breaks the first packets of an ISDB-T stream; px4_drv waits this long from
/// the tuning before giving the stream out.
const ISDB_T_SETTLE: Duration = Duration::from_millis(350);

const TRANSFERS_IN_FLIGHT: usize = 8;
/// The buffered transfers each stream may fall behind by before its data is
/// dropped.
const STREAM_BACKLOG: usize = 64;

struct Chip {
    demod: Cxd2856er,
    tuner: Cxd2858er,
    i2c_bus: u8,
    open: bool,
    lnb: bool,
}

struct State {
    it930x: It930x<NusbTransport>,
    chips: Vec<Chip>,
    open_count: usize,
    lnb_count: usize,
}

#[derive(Default)]
struct Streams {
    senders: [Option<mpsc::Sender<Vec<u8>>>; 5],
    running: bool,
}

struct Shared {
    state: Mutex<State>,
    streams: SyncMutex<Streams>,
}

pub async fn open(
    usb: &UsbDeviceInfo,
    info: DeviceInfo,
    layout: Layout,
    firmware: &[u8],
) -> Result<Box<dyn Device>> {
    let mut it930x = It930x::new(usb.open(0).await?, usb.usb_version);
    it930x.raise().await?;
    if it930x.read_reg(0x4979).await? == 0 {
        return Err(error("EEPROM error"));
    }
    it930x.load_firmware(firmware).await?;

    let inputs: Vec<_> = layout
        .iter()
        .enumerate()
        .map(|(i, &(i2c_addr, i2c_bus, port))| Input {
            port,
            slave: i as u8,
            i2c_bus,
            i2c_addr,
            sync_byte: ((i as u8 + 1) << 4) | 0x07,
        })
        .collect();
    it930x.init_warm(&inputs).await?;

    it930x.gpio_output(GPIO_BACKEND_RESET).await?;
    it930x.write_gpio(GPIO_BACKEND_RESET, true).await?;
    it930x.gpio_output(GPIO_BACKEND_POWER).await?;
    it930x.write_gpio(GPIO_BACKEND_POWER, false).await?;
    it930x.gpio_output(GPIO_LNB_POWER).await?;
    it930x.write_gpio(GPIO_LNB_POWER, false).await?;

    let tuners = (0..layout.len())
        .map(|i| TunerInfo {
            id: format!("{}#{i}", info.id),
            systems: vec![System::IsdbT, System::IsdbS],
        })
        .collect();
    let chips = layout
        .iter()
        .map(|&(i2c_addr, i2c_bus, _)| Chip {
            demod: Cxd2856er::new(i2c_addr),
            tuner: Cxd2858er::default(),
            i2c_bus,
            open: false,
            lnb: false,
        })
        .collect();

    Ok(Box::new(PxMlt {
        info,
        tuners,
        shared: Arc::new(Shared {
            state: Mutex::new(State {
                it930x,
                chips,
                open_count: 0,
                lnb_count: 0,
            }),
            streams: SyncMutex::default(),
        }),
    }))
}

struct PxMlt {
    info: DeviceInfo,
    tuners: Vec<TunerInfo>,
    shared: Arc<Shared>,
}

impl Device for PxMlt {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn tuners(&self) -> &[TunerInfo] {
        &self.tuners
    }

    fn open_tuner(&self, index: usize) -> BoxFuture<'_, Result<Box<dyn Tuner>>> {
        async move {
            let mut guard = self.shared.state.lock().await;
            let state = &mut *guard;
            let chip = state
                .chips
                .get(index)
                .ok_or_else(|| Error::NotFound(format!("{}#{index}", self.info.id)))?;
            if chip.open {
                return Err(io::Error::from(io::ErrorKind::ResourceBusy).into());
            }

            if state.open_count == 0 {
                power_backend(&mut state.it930x, true).await?;
            }
            let result = init_chip(&mut state.it930x, &mut state.chips[index]).await;
            if result.is_err() && state.open_count == 0 {
                let _ = power_backend(&mut state.it930x, false).await;
            }
            result?;

            state.chips[index].open = true;
            state.open_count += 1;
            Ok(Box::new(PxMltTuner {
                shared: self.shared.clone(),
                index,
                mode: None,
            }) as Box<dyn Tuner>)
        }
        .boxed()
    }
}

async fn power_backend(it930x: &mut It930x<NusbTransport>, on: bool) -> Result<()> {
    if on {
        it930x.write_gpio(GPIO_BACKEND_RESET, false).await?;
        Delay::new(Duration::from_millis(80)).await;
        it930x.write_gpio(GPIO_BACKEND_POWER, true).await?;
        Delay::new(Duration::from_millis(20)).await;
    } else {
        let power = it930x.write_gpio(GPIO_BACKEND_POWER, false).await;
        let reset = it930x.write_gpio(GPIO_BACKEND_RESET, true).await;
        power.and(reset)?;
    }
    Ok(())
}

async fn init_chip(it930x: &mut It930x<NusbTransport>, chip: &mut Chip) -> Result<()> {
    let mut i2c = it930x.i2c(chip.i2c_bus);
    chip.demod.init(&mut i2c).await?;
    chip.tuner.init(&mut i2c, &chip.demod).await?;
    chip.demod.setup(&mut i2c, DEMOD_SETUP).await
}

struct PxMltTuner {
    shared: Arc<Shared>,
    index: usize,
    /// The system last tuned to.
    mode: Option<Mode>,
}

impl PxMltTuner {
    /// Runs `f` on the bridge and the chips of this tuner.
    async fn with_chip<T>(
        &self,
        f: impl AsyncFnOnce(&mut It930x<NusbTransport>, &mut Chip) -> Result<T>,
    ) -> Result<T> {
        let mut guard = self.shared.state.lock().await;
        let state = &mut *guard;
        f(&mut state.it930x, &mut state.chips[self.index]).await
    }

    async fn tune(&mut self, params: TuneParams) -> Result<()> {
        params.validate()?;
        let mode = match params.system {
            System::IsdbT => Mode::T,
            System::IsdbS => Mode::S,
            System::IsdbS3 => return Err(Error::Unsupported(params.system)),
        };
        let khz = match mode {
            Mode::T => params.frequency_khz,
            Mode::S => params.if_frequency_khz()?,
        };

        self.mode = None;
        let started = Instant::now();
        self.with_chip(async |it930x, chip| {
            let mut i2c = it930x.i2c(chip.i2c_bus);
            if let Some(stream_id) = params.stream_id {
                chip.demod.set_tsid(&mut i2c, stream_id.0).await?;
            }
            chip.demod.wakeup(&mut i2c, mode).await?;
            match mode {
                Mode::T => chip.tuner.tune_t(&mut i2c, &chip.demod, khz).await?,
                Mode::S => chip.tuner.tune_s(&mut i2c, &chip.demod, khz).await?,
            }
            chip.demod.post_tune(&mut i2c).await
        })
        .await?;

        let mut locked = false;
        for _ in 0..LOCK_POLLS {
            let (lock, gave_up) = self
                .with_chip(async |it930x, chip| {
                    chip.demod.lock_status(&mut it930x.i2c(chip.i2c_bus)).await
                })
                .await?;
            if lock {
                locked = true;
                break;
            }
            if gave_up {
                break;
            }
            Delay::new(Duration::from_millis(10)).await;
        }
        if !locked {
            return Err(Error::NoLock);
        }

        if mode == Mode::T
            && let Some(rest) = ISDB_T_SETTLE.checked_sub(started.elapsed())
        {
            Delay::new(rest).await;
        }
        self.mode = Some(mode);
        Ok(())
    }

    async fn signal(&mut self) -> Result<Signal> {
        let Some(mode) = self.mode else {
            return Ok(Signal::default());
        };
        self.with_chip(async |it930x, chip| {
            let mut i2c = it930x.i2c(chip.i2c_bus);
            let (locked, _) = chip.demod.lock_status(&mut i2c).await?;
            let raw = chip.demod.cnr_raw(&mut i2c).await?;
            Ok(Signal {
                locked,
                cnr_db: Some(match mode {
                    Mode::T => cnr::isdb_t(raw),
                    Mode::S => cnr::isdb_s(raw),
                }),
            })
        })
        .await
    }

    async fn stream(&mut self) -> Result<(StreamFormat, ByteStream)> {
        if self.mode.is_none() {
            return Err(Error::InvalidParams("the tuner has not been tuned"));
        }

        let (tx, rx) = mpsc::channel(STREAM_BACKLOG);
        let start = {
            let mut streams = self.shared.streams.lock().unwrap();
            streams.senders[self.index] = Some(tx);
            !std::mem::replace(&mut streams.running, true)
        };

        if start {
            let queue = async {
                let mut state = self.shared.state.lock().await;
                state.it930x.purge_psb().await?;
                Ok::<_, Error>(state.it930x.usb().bulk_in_queue(EP_STREAM)?)
            }
            .await;
            match queue {
                Ok(queue) => {
                    let shared = self.shared.clone();
                    thread::spawn(move || block_on(receive(shared, queue)));
                }
                Err(e) => {
                    let mut streams = self.shared.streams.lock().unwrap();
                    streams.running = false;
                    streams.senders[self.index] = None;
                    return Err(e);
                }
            }
        }

        Ok((StreamFormat::Ts, rx.map(Ok).boxed()))
    }
}

async fn set_lnb(shared: &Shared, index: usize, on: bool) -> Result<()> {
    let mut guard = shared.state.lock().await;
    let state = &mut *guard;
    if state.chips[index].lnb == on {
        return Ok(());
    }
    if !on {
        state.lnb_count -= 1;
    }
    if state.lnb_count == 0 {
        state.it930x.write_gpio(GPIO_LNB_POWER, on).await?;
    }
    if on {
        state.lnb_count += 1;
    }
    state.chips[index].lnb = on;
    Ok(())
}

impl Tuner for PxMltTuner {
    fn tune(&mut self, params: TuneParams) -> BoxFuture<'_, Result<()>> {
        PxMltTuner::tune(self, params).boxed()
    }

    fn signal(&mut self) -> BoxFuture<'_, Result<Signal>> {
        PxMltTuner::signal(self).boxed()
    }

    fn set_lnb(&mut self, on: bool) -> BoxFuture<'_, Result<()>> {
        set_lnb(&self.shared, self.index, on).boxed()
    }

    fn stream(&mut self) -> BoxFuture<'_, Result<(StreamFormat, ByteStream)>> {
        PxMltTuner::stream(self).boxed()
    }
}

impl Drop for PxMltTuner {
    fn drop(&mut self) {
        self.shared.streams.lock().unwrap().senders[self.index] = None;

        let shared = self.shared.clone();
        let index = self.index;
        // ponytail: released on a thread of its own, as Drop cannot wait;
        // a tuner opened right after may find it still busy.
        thread::spawn(move || {
            block_on(async {
                let _ = set_lnb(&shared, index, false).await;
                let mut guard = shared.state.lock().await;
                let state = &mut *guard;
                let chip = &mut state.chips[index];
                let mut i2c = state.it930x.i2c(chip.i2c_bus);
                let _ = chip.tuner.term(&mut i2c, &chip.demod).await;
                let _ = chip.demod.sleep(&mut i2c).await;
                chip.open = false;
                state.open_count -= 1;
                if state.open_count == 0 {
                    let _ = power_backend(&mut state.it930x, false).await;
                }
            })
        });
    }
}

/// Keeps transfers in flight on the stream endpoint, handing each tuner its
/// packets, until no tuner takes them any more.
async fn receive(shared: Arc<Shared>, mut queue: impl BulkIn) {
    for _ in 0..TRANSFERS_IN_FLIGHT {
        queue.submit(it930x::XFER_SIZE);
    }

    let mut carry = Vec::new();
    loop {
        let data = queue.next_complete().await;
        let mut streams = shared.streams.lock().unwrap();
        let Ok(data) = data else {
            // ponytail: a failed transfer ends every stream; retry it if
            // devices turn out to fail transiently.
            *streams = Streams::default();
            return;
        };
        queue.submit(it930x::XFER_SIZE);

        let mut packets: [Vec<u8>; 5] = Default::default();
        demux(&mut carry, &data, |id, packet| {
            packets[id].extend_from_slice(packet)
        });
        for (sender, packets) in streams.senders.iter_mut().zip(packets) {
            if let Some(tx) = sender
                && !packets.is_empty()
                && let Err(e) = tx.try_send(packets)
                && e.is_disconnected()
            {
                *sender = None;
            }
            // ponytail: a stream that falls STREAM_BACKLOG transfers behind
            // loses packets silently; report the drop once the daemon can.
        }

        if streams.senders.iter().all(Option::is_none) {
            streams.running = false;
            return;
        }
    }
}

/// Splits the stream of the bridge into the 188-byte packets of each input,
/// telling them apart by the sync byte, 0x17 for the first input to 0x57 for
/// the fifth, and restoring it to 0x47. `carry` keeps what is left over for
/// the next call.
fn demux(carry: &mut Vec<u8>, data: &[u8], mut out: impl FnMut(usize, &[u8])) {
    const PACKET: usize = 188;
    // Packets in a row with a sync byte it takes to lock back on the stream.
    const SYNC_COUNT: usize = 4;

    fn tagged(byte: u8) -> bool {
        byte & 0x8f == 0x07
    }

    carry.extend_from_slice(data);
    let buf = &mut carry[..];
    let mut p = 0;
    while buf.len() - p >= PACKET * SYNC_COUNT {
        if !(0..SYNC_COUNT).all(|k| tagged(buf[p + PACKET * k])) {
            p += 1;
            continue;
        }
        while buf.len() - p >= PACKET && tagged(buf[p]) {
            let id = usize::from((buf[p] & 0x70) >> 4);
            if (1..=5).contains(&id) {
                buf[p] = 0x47;
                out(id - 1, &buf[p..p + PACKET]);
            }
            p += PACKET;
        }
    }
    carry.drain(..p);
}

#[cfg(test)]
mod tests {
    use super::demux;

    fn packet(sync: u8, fill: u8) -> Vec<u8> {
        let mut p = vec![fill; 188];
        p[0] = sync;
        p
    }

    #[test]
    fn demux_splits_and_resyncs() {
        let mut stream = vec![0xaa; 5]; // garbage before the first packet
        for i in 0..10u8 {
            stream.extend(packet(((i % 5 + 1) << 4) | 0x07, i));
        }

        let mut carry = Vec::new();
        let mut got: Vec<(usize, u8)> = Vec::new();
        // Cut the stream mid-packet to exercise the carry.
        let (a, b) = stream.split_at(700);
        for part in [a, b] {
            demux(&mut carry, part, |id, p| {
                assert_eq!(p[0], 0x47);
                got.push((id, p[1]));
            });
        }

        let expected: Vec<_> = (0..10u8).map(|i| (usize::from(i % 5), i)).collect();
        assert_eq!(got, expected);
        assert!(carry.is_empty());
    }
}
