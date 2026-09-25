// SPDX-License-Identifier: GPL-2.0-only
//! The PX4/PX5 series, PX-W3U4 to PX-Q3PE5: an IT930x with two ISDB-S
//! tuners (TC90522 and RT710) and two ISDB-T tuners (TC90522 and R850). The
//! Q models are two such boards on one card, powered together.
//!
//! Ported from `px4_device.c` and `px4_mldev.c` of px4_drv,
//! Copyright (c) 2018-2021 nns779.

use std::time::Duration;

use futures_timer::Delay;
use tunelith_core::usb::UsbDeviceInfo;
use tunelith_core::{Device, DeviceInfo, Error, I2c, Result, Signal, System, TuneParams};

use crate::board::{self, Board, Bridge, Lnb, Lock};
use crate::it930x::Input;
use crate::r850::{self, R850, R850Config};
use crate::rt710::{
    AgcMode, FineGain, Rt710, Rt710Config, ScanMode, SignalOutputMode, VgaAttenMode,
};
use crate::tc90522::Tc90522;
use crate::{cnr, error};

const GPIO_BACKEND_RESET: u8 = 7;
const GPIO_BACKEND_POWER: u8 = 2;

/// The demodulator addresses of the tuners of a board: two ISDB-S, then two
/// ISDB-T.
const DEMODS: [u8; 4] = [0x11, 0x13, 0x10, 0x12];

pub const RT710_ADDR: u8 = 0x7a;
pub const RT710_CONFIG: Rt710Config = Rt710Config {
    xtal: 24000,
    loop_through: false,
    clock_out: false,
    signal_output_mode: SignalOutputMode::Differential,
    agc_mode: AgcMode::Positive,
    vga_atten_mode: VgaAttenMode::Off,
    fine_gain: FineGain::Db3,
    scan_mode: ScanMode::Manual,
};

pub const R850_ADDR: u8 = 0x7c;
pub fn r850_config(loop_through: bool) -> R850Config {
    R850Config {
        xtal: 24000,
        loop_through,
        clock_out: false,
        no_imr_calibration: true,
        no_lpf_calibration: true,
    }
}
pub const R850_ISDB_T: r850::SystemConfig = r850::SystemConfig {
    system: r850::System::IsdbT,
    bandwidth: r850::Bandwidth::M6,
    if_freq: 4063,
};

pub const TC_INIT_T: &[(u8, u8)] = &[
    (0xb0, 0xa0),
    (0xb2, 0x3d),
    (0xb3, 0x25),
    (0xb4, 0x8b),
    (0xb5, 0x4b),
    (0xb6, 0x3f),
    (0xb7, 0xff),
    (0xb8, 0xc0),
];
pub const TC_INIT_S: &[(u8, u8)] = &[(0x15, 0x00), (0x1d, 0x00)];

enum Tuner {
    S(Rt710),
    T(R850),
}

struct Chip {
    demod: Tc90522,
    tuner: Tuner,
}

struct Half {
    bridge: Bridge,
    chips: Vec<Chip>,
    open_count: usize,
    lnb: Lnb,
}

struct Px4 {
    halves: Vec<Half>,
}

/// Opens the boards of a card: one, or the two of a Q model in the order of
/// their device ids.
pub async fn open(
    usbs: &[UsbDeviceInfo],
    info: DeviceInfo,
    firmware: &[u8],
) -> Result<Box<dyn Device>> {
    let inputs: Vec<_> = (0..4u8)
        .map(|i| Input {
            port: i + 1,
            slave: i,
            i2c_bus: 2,
            i2c_addr: DEMODS[usize::from(i)],
            sync_byte: ((i + 1) << 4) | 0x07,
        })
        .collect();

    let mut halves = Vec::new();
    for usb in usbs {
        let mut bridge = board::open_bridge(usb, &inputs, firmware).await?;
        bridge.gpio_output(GPIO_BACKEND_RESET).await?;
        bridge.gpio_output(GPIO_BACKEND_POWER).await?;
        bridge.write_gpio(GPIO_BACKEND_POWER, false).await?;
        bridge.write_gpio(GPIO_BACKEND_RESET, true).await?;
        bridge.gpio_output(Lnb::GPIO).await?;
        bridge.write_gpio(Lnb::GPIO, false).await?;

        let chips = DEMODS
            .iter()
            .enumerate()
            .map(|(i, &addr)| {
                let mut demod = Tc90522::new(addr);
                demod.is_secondary = i % 2 == 1;
                let tuner = if i < 2 {
                    Tuner::S(Rt710::new(RT710_ADDR, RT710_CONFIG))
                } else {
                    Tuner::T(R850::new(R850_ADDR, r850_config(i % 2 == 0)))
                };
                Chip { demod, tuner }
            })
            .collect();
        halves.push(Half {
            bridge,
            chips,
            open_count: 0,
            lnb: Lnb::new(4),
        });
    }

    let systems = (0..halves.len() * 4)
        .map(|i| {
            vec![if i % 4 < 2 {
                System::IsdbS
            } else {
                System::IsdbT
            }]
        })
        .collect();
    let tagged = vec![true; halves.len()];
    Ok(board::device(info, Px4 { halves }, systems, &tagged))
}

impl Half {
    async fn power(&mut self, on: bool) -> Result<()> {
        if on {
            self.bridge.write_gpio(GPIO_BACKEND_RESET, false).await?;
            Delay::new(Duration::from_millis(80)).await;
            self.bridge.write_gpio(GPIO_BACKEND_POWER, true).await?;
            Delay::new(Duration::from_millis(20)).await;
            Ok(())
        } else {
            let power = self.bridge.write_gpio(GPIO_BACKEND_POWER, false).await;
            let reset = self.bridge.write_gpio(GPIO_BACKEND_RESET, true).await;
            power.and(reset)
        }
    }

    /// Initialises the tuners of the board, then puts all but `index` to sleep.
    async fn init(&mut self, index: usize) -> Result<()> {
        for chip in &mut self.chips {
            let mut i2c = self.bridge.i2c(2);
            let mut bus = chip.demod.tuner_bus(&mut i2c);
            match &mut chip.tuner {
                Tuner::S(rt710) => rt710.init(&mut bus).await?,
                Tuner::T(r850) => r850.init(&mut bus).await?,
            }
        }
        for i in (0..4).filter(|&i| i != index) {
            self.sleep(i).await?;
        }
        Ok(())
    }

    async fn term(&mut self) {
        for chip in &mut self.chips {
            match &mut chip.tuner {
                Tuner::S(rt710) => {
                    let _ = rt710.term().await;
                }
                Tuner::T(r850) => r850.term(),
            }
        }
    }

    async fn sleep(&mut self, index: usize) -> Result<()> {
        let chip = &mut self.chips[index];
        let mut i2c = self.bridge.i2c(2);
        match &mut chip.tuner {
            Tuner::S(rt710) => {
                rt710.sleep(&mut chip.demod.tuner_bus(&mut i2c)).await?;
                chip.demod.sleep_s(&mut i2c, true).await
            }
            Tuner::T(r850) => {
                r850.sleep()?;
                chip.demod.sleep_t(&mut i2c, true).await
            }
        }
    }

    async fn wakeup(&mut self, index: usize) -> Result<()> {
        let chip = &mut self.chips[index];
        let mut i2c = self.bridge.i2c(2);
        match &mut chip.tuner {
            Tuner::S(_) => {
                chip.demod.write_multiple_regs(&mut i2c, TC_INIT_S).await?;
                chip.demod.write_reg(&mut i2c, 0x04, 0x02).await?;
                chip.demod.enable_ts_pins_s(&mut i2c, false).await?;
                chip.demod.sleep_s(&mut i2c, false).await
            }
            Tuner::T(r850) => {
                chip.demod.write_multiple_regs(&mut i2c, TC_INIT_T).await?;
                chip.demod.write_reg(&mut i2c, 0x1f, 0x00).await?;
                chip.demod.write_reg(&mut i2c, 0x75, 0x00).await?;
                chip.demod.enable_ts_pins_t(&mut i2c, false).await?;
                chip.demod.sleep_t(&mut i2c, false).await?;
                r850.wakeup()?;
                r850.set_system(R850_ISDB_T)
            }
        }
    }
}

impl Px4 {
    fn half(&mut self, index: usize) -> (&mut Half, usize) {
        (&mut self.halves[index / 4], index % 4)
    }

    fn open_count(&self) -> usize {
        self.halves.iter().map(|h| h.open_count).sum()
    }

    /// Powers every board: those of a Q model go on and off together.
    async fn power(&mut self, on: bool) -> Result<()> {
        let mut result = Ok(());
        for half in &mut self.halves {
            result = result.and(half.power(on).await);
        }
        result
    }

    async fn try_open(&mut self, index: usize) -> Result<()> {
        let (half, i) = self.half(index);
        let first = half.open_count == 0;
        if first {
            half.init(i).await?;
        }
        half.wakeup(i).await?;
        if first {
            let mut bus = half.bridge.i2c(2);
            // The first demodulator of each system: S0 and T0.
            half.chips[0]
                .demod
                .write_multiple_regs(&mut bus, &[(0x07, 0x31), (0x08, 0x77)])
                .await?;
            half.chips[2]
                .demod
                .write_multiple_regs(&mut bus, &[(0x0e, 0x77), (0x0f, 0x13)])
                .await?;
        }
        Ok(())
    }
}

/// Picks the stream with `tsid` out of the transponder, and waits for the
/// demodulator to report it. px4_drv also takes a relative TS number below
/// 12 and looks it up in the TMCC; a [`tunelith_core::StreamId`] never is one.
pub async fn select_tsid(demod: &Tc90522, i2c: &mut impl I2c, tsid: u16) -> Result<()> {
    demod.set_tsid_s(i2c, tsid).await?;
    for _ in 0..100 {
        if demod.get_tsid_s(i2c).await.ok() == Some(tsid) {
            return Ok(());
        }
        Delay::new(Duration::from_millis(10)).await;
    }
    Err(error("the transponder does not carry the TSID"))
}

/// Waits for the PLL of the R850 to lock.
pub async fn wait_r850(r850: &mut R850, bus: &mut impl I2c) -> Result<()> {
    // A failed read is tried again; the last one's error is what counts.
    let mut last = Ok(false);
    for _ in 0..50 {
        last = r850.is_pll_locked(bus).await;
        if let Ok(true) = last {
            return Ok(());
        }
        Delay::new(Duration::from_millis(10)).await;
    }
    last?;
    Err(error("the R850 PLL did not lock"))
}

/// Waits for the PLL of the RT710 to lock.
pub async fn wait_rt710(rt710: &mut Rt710, bus: &mut impl I2c) -> Result<()> {
    // A failed read is tried again; the last one's error is what counts.
    let mut last = Ok(false);
    for _ in 0..50 {
        last = rt710.is_pll_locked(bus).await;
        if let Ok(true) = last {
            return Ok(());
        }
        Delay::new(Duration::from_millis(10)).await;
    }
    last?;
    Err(error("the RT710 PLL did not lock"))
}

impl Board for Px4 {
    fn bridge(&mut self, bridge: usize) -> &mut Bridge {
        &mut self.halves[bridge].bridge
    }

    fn source(&self, index: usize) -> (usize, usize) {
        (index / 4, index % 4)
    }

    async fn open(&mut self, index: usize) -> Result<()> {
        let first = self.open_count() == 0;
        if first {
            self.power(true).await?;
        }
        let result = self.try_open(index).await;
        let (half, _) = self.half(index);
        match result {
            Ok(()) => half.open_count += 1,
            Err(_) if half.open_count == 0 => half.term().await,
            Err(_) => {}
        }
        if result.is_err() && first {
            let _ = self.power(false).await;
        }
        result
    }

    async fn close(&mut self, index: usize) {
        let (half, i) = self.half(index);
        half.open_count -= 1;
        if half.open_count == 0 {
            half.term().await;
        } else {
            let _ = half.sleep(i).await;
        }
        if self.open_count() == 0 {
            let _ = self.power(false).await;
        }
    }

    async fn tune(&mut self, index: usize, params: &TuneParams) -> Result<()> {
        let (half, i) = self.half(index);
        let chip = &mut half.chips[i];
        let demod = &chip.demod;
        let mut i2c = half.bridge.i2c(2);
        match &mut chip.tuner {
            Tuner::T(r850) => {
                demod.write_reg(&mut i2c, 0x47, 0x30).await?;
                demod.set_agc_t(&mut i2c, false).await?;
                demod.write_reg(&mut i2c, 0x76, 0x0c).await?;
                let mut bus = demod.tuner_bus(&mut i2c);
                r850.set_frequency(&mut bus, params.frequency_khz).await?;
                wait_r850(r850, &mut bus).await?;
                demod.set_agc_t(&mut i2c, true).await?;
                demod.write_reg(&mut i2c, 0x71, 0x21).await?;
                demod.write_reg(&mut i2c, 0x72, 0x25).await?;
                demod.write_reg(&mut i2c, 0x75, 0x08).await
            }
            Tuner::S(rt710) => {
                demod.set_agc_s(&mut i2c, false).await?;
                demod.write_reg(&mut i2c, 0x8e, 0x06).await?;
                demod.write_reg(&mut i2c, 0xa3, 0xf7).await?;
                let mut bus = demod.tuner_bus(&mut i2c);
                rt710
                    .set_params(&mut bus, params.if_frequency_khz()?, 28860, 4)
                    .await?;
                wait_rt710(rt710, &mut bus).await?;
                demod.set_agc_s(&mut i2c, true).await
            }
        }
    }

    async fn lock(&mut self, index: usize, _system: System) -> Result<Lock> {
        let (half, i) = self.half(index);
        let chip = &half.chips[i];
        let mut i2c = half.bridge.i2c(2);
        let locked = match chip.tuner {
            Tuner::T(_) => chip.demod.is_signal_locked_t(&mut i2c).await?,
            Tuner::S(_) => chip.demod.is_signal_locked_s(&mut i2c).await?,
        };
        Ok(if locked { Lock::Locked } else { Lock::Waiting })
    }

    async fn select(&mut self, index: usize, params: &TuneParams) -> Result<()> {
        let Some(stream_id) = params.stream_id else {
            return Ok(());
        };
        let (half, i) = self.half(index);
        let mut i2c = half.bridge.i2c(2);
        select_tsid(&half.chips[i].demod, &mut i2c, stream_id.0).await
    }

    async fn signal(&mut self, index: usize, _system: System) -> Result<Signal> {
        let (half, i) = self.half(index);
        let chip = &half.chips[i];
        let mut i2c = half.bridge.i2c(2);
        Ok(match chip.tuner {
            Tuner::T(_) => Signal {
                locked: chip.demod.is_signal_locked_t(&mut i2c).await?,
                cnr_db: Some(cnr::isdb_t(chip.demod.get_cndat_t(&mut i2c).await?)),
            },
            Tuner::S(_) => Signal {
                locked: chip.demod.is_signal_locked_s(&mut i2c).await?,
                cnr_db: Some(cnr::isdb_s(chip.demod.get_cn_s(&mut i2c).await?)),
            },
        })
    }

    async fn set_lnb(&mut self, index: usize, on: bool) -> Result<()> {
        let (half, i) = self.half(index);
        if let Tuner::T(_) = half.chips[i].tuner {
            return Err(Error::Unsupported(System::IsdbT));
        }
        half.lnb.set(&mut half.bridge, i, on).await
    }

    async fn set_capture(&mut self, index: usize, on: bool) -> Result<()> {
        let (half, i) = self.half(index);
        let chip = &half.chips[i];
        let mut i2c = half.bridge.i2c(2);
        match chip.tuner {
            Tuner::T(_) => chip.demod.enable_ts_pins_t(&mut i2c, on).await,
            Tuner::S(_) => chip.demod.enable_ts_pins_s(&mut i2c, on).await,
        }
    }
}
