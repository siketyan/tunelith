// SPDX-License-Identifier: GPL-2.0-only
//! The PX-MLT series and its kin: an IT930x with up to five CXD2856ER and
//! CXD2858ER pairs, every tuner receiving both ISDB-T and ISDB-S.
//!
//! Ported from `pxmlt_device.c` of px4_drv, Copyright (c) 2018-2021 nns779.

use std::time::Duration;

use futures_timer::Delay;
use tunelith_core::usb::UsbDeviceInfo;
use tunelith_core::{Device, DeviceInfo, Error, Result, Signal, System, TuneParams};

use crate::board::{self, Board, Bridge, Lnb, Lock};
use crate::cnr;
use crate::cxd2856er::{Cxd2856er, Mode};
use crate::cxd2858er::Cxd2858er;
use crate::it930x::Input;

/// The address, I2C bus and TS port of the demodulator of each tuner.
pub type Layout = &'static [(u8, u8, u8)];

const GPIO_BACKEND_RESET: u8 = 7;
const GPIO_BACKEND_POWER: u8 = 2;

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

struct Chip {
    demod: Cxd2856er,
    tuner: Cxd2858er,
    i2c_bus: u8,
}

struct PxMlt {
    bridge: Bridge,
    chips: Vec<Chip>,
    open_count: usize,
    lnb: Lnb,
}

pub async fn open(
    usb: &UsbDeviceInfo,
    info: DeviceInfo,
    layout: Layout,
    firmware: &[u8],
) -> Result<Box<dyn Device>> {
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
    let mut bridge = board::open_bridge(usb, &inputs, firmware).await?;

    bridge.gpio_output(GPIO_BACKEND_RESET).await?;
    bridge.write_gpio(GPIO_BACKEND_RESET, true).await?;
    bridge.gpio_output(GPIO_BACKEND_POWER).await?;
    bridge.write_gpio(GPIO_BACKEND_POWER, false).await?;
    bridge.gpio_output(Lnb::GPIO).await?;
    bridge.write_gpio(Lnb::GPIO, false).await?;

    let chips = layout
        .iter()
        .map(|&(i2c_addr, i2c_bus, _)| Chip {
            demod: Cxd2856er::new(i2c_addr),
            tuner: Cxd2858er::default(),
            i2c_bus,
        })
        .collect();
    let board = PxMlt {
        bridge,
        chips,
        open_count: 0,
        lnb: Lnb::new(layout.len()),
    };
    let systems = vec![vec![System::IsdbT, System::IsdbS]; layout.len()];
    Ok(board::device(info, board, systems, &[true]))
}

impl PxMlt {
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

    async fn init_chip(&mut self, index: usize) -> Result<()> {
        let chip = &mut self.chips[index];
        let mut i2c = self.bridge.i2c(chip.i2c_bus);
        chip.demod.init(&mut i2c).await?;
        chip.tuner.init(&mut i2c, &chip.demod).await?;
        chip.demod.setup(&mut i2c, DEMOD_SETUP).await
    }
}

fn mode(system: System) -> Result<Mode> {
    match system {
        System::IsdbT => Ok(Mode::T),
        System::IsdbS => Ok(Mode::S),
        System::IsdbS3 => Err(Error::Unsupported(system)),
    }
}

impl Board for PxMlt {
    fn bridge(&mut self, _bridge: usize) -> &mut Bridge {
        &mut self.bridge
    }

    fn source(&self, index: usize) -> (usize, usize) {
        (0, index)
    }

    async fn open(&mut self, index: usize) -> Result<()> {
        if self.open_count == 0 {
            self.power(true).await?;
        }
        let result = self.init_chip(index).await;
        if result.is_err() && self.open_count == 0 {
            let _ = self.power(false).await;
        }
        result?;
        self.open_count += 1;
        Ok(())
    }

    async fn close(&mut self, index: usize) {
        let chip = &mut self.chips[index];
        let mut i2c = self.bridge.i2c(chip.i2c_bus);
        let _ = chip.tuner.term(&mut i2c, &chip.demod).await;
        let _ = chip.demod.sleep(&mut i2c).await;
        self.open_count -= 1;
        if self.open_count == 0 {
            let _ = self.power(false).await;
        }
    }

    async fn tune(&mut self, index: usize, params: &TuneParams) -> Result<()> {
        let mode = mode(params.system)?;
        let khz = match mode {
            Mode::T => params.frequency_khz,
            Mode::S => params.if_frequency_khz()?,
        };

        let chip = &mut self.chips[index];
        let mut i2c = self.bridge.i2c(chip.i2c_bus);
        // The demodulator takes the TSID before the tuning.
        if let Some(stream_id) = params.stream_id {
            chip.demod.set_tsid(&mut i2c, stream_id.0).await?;
        }
        chip.demod.wakeup(&mut i2c, mode).await?;
        match mode {
            Mode::T => chip.tuner.tune_t(&mut i2c, &chip.demod, khz).await?,
            Mode::S => chip.tuner.tune_s(&mut i2c, &chip.demod, khz).await?,
        }
        chip.demod.post_tune(&mut i2c).await
    }

    async fn lock(&mut self, index: usize, _system: System) -> Result<Lock> {
        let chip = &self.chips[index];
        let mut i2c = self.bridge.i2c(chip.i2c_bus);
        Ok(match chip.demod.lock_status(&mut i2c).await? {
            (true, _) => Lock::Locked,
            (false, true) => Lock::Failed,
            (false, false) => Lock::Waiting,
        })
    }

    async fn signal(&mut self, index: usize, system: System) -> Result<Signal> {
        let chip = &self.chips[index];
        let mut i2c = self.bridge.i2c(chip.i2c_bus);
        let (locked, _) = chip.demod.lock_status(&mut i2c).await?;
        let raw = chip.demod.cnr_raw(&mut i2c).await?;
        Ok(Signal {
            locked,
            cnr_db: Some(match mode(system)? {
                Mode::T => cnr::cxd_isdb_t(raw),
                Mode::S => cnr::cxd_isdb_s(raw),
            }),
        })
    }

    async fn set_lnb(&mut self, index: usize, on: bool) -> Result<()> {
        self.lnb.set(&mut self.bridge, index, on).await
    }
}
