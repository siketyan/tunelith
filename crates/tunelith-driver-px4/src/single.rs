// SPDX-License-Identifier: GPL-2.0-only
//! The models with a single tuner: the PX-M1UR and Digibest ISDB2056/2056N,
//! switching between ISDB-T and ISDB-S, and the PX-S1UR and Digibest
//! ISDBT2071, receiving ISDB-T alone. Each has a TC90522 per system, with
//! an R850 and an RT710.
//!
//! Ported from `m1ur_device.c`, `isdb2056_device.c` and `s1ur_device.c` of
//! px4_drv, Copyright (c) 2018-2021 nns779.

use std::time::Duration;

use futures_timer::Delay;
use tunelith_core::usb::UsbDeviceInfo;
use tunelith_core::{Device, DeviceInfo, Error, Result, Signal, System, TuneParams};

use crate::board::{self, Board, Bridge, Lock};
use crate::cnr;
use crate::it930x::Input;
use crate::px4::{
    R850_ADDR, R850_ISDB_T, RT710_ADDR, RT710_CONFIG, TC_INIT_S, TC_INIT_T, r850_config,
    select_tsid, wait_r850, wait_rt710,
};
use crate::r850::R850;
use crate::rt710::Rt710;
use crate::tc90522::Tc90522;

const I2C_BUS: u8 = 3;
const GPIO_BACKEND_RESET: u8 = 3;
const GPIO_BACKEND_POWER: u8 = 2;

const TC_INIT_ISDBT2071: &[(u8, u8)] = &[
    (0x04, 0x00),
    (0x10, 0x00),
    (0x11, 0x2d),
    (0x12, 0x02),
    (0x13, 0x62),
    (0x14, 0x60),
    (0x15, 0x00),
    (0x16, 0x00),
    (0x1d, 0x05),
    (0x1e, 0x15),
    (0x1f, 0x40),
    (0x30, 0x20),
    (0x31, 0x0b),
    (0x32, 0x8f),
    (0x34, 0x0f),
    (0x38, 0x01),
    (0x39, 0x1c),
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Model {
    M1ur,
    Isdb2056,
    Isdb2056n,
    S1ur,
    Isdbt2071,
}

struct Single {
    model: Model,
    bridge: Bridge,
    demod_t: Tc90522,
    /// Absent on the ISDBT2071.
    demod_s: Option<Tc90522>,
    /// The second ISDB-S demodulator of the ISDB2056N.
    demod_s0: Option<Tc90522>,
    r850: R850,
    /// Absent on the ISDB-T models.
    rt710: Option<Rt710>,
    /// The system last tuned to.
    system: Option<System>,
}

pub async fn open(
    usb: &UsbDeviceInfo,
    info: DeviceInfo,
    model: Model,
    firmware: &[u8],
) -> Result<Box<dyn Device>> {
    let (port, addr_t) = match model {
        Model::Isdbt2071 => (4, 0x18),
        _ => (0, 0x10),
    };
    let input = Input {
        port,
        slave: 0,
        i2c_bus: I2C_BUS,
        i2c_addr: addr_t,
        sync_byte: 0x47,
    };
    let mut bridge = board::open_bridge(usb, &[input], firmware).await?;
    bridge.gpio_output(GPIO_BACKEND_RESET).await?;
    bridge.write_gpio(GPIO_BACKEND_RESET, true).await?;
    bridge.gpio_output(GPIO_BACKEND_POWER).await?;
    bridge.write_gpio(GPIO_BACKEND_POWER, false).await?;

    let demod_s = match model {
        Model::Isdbt2071 => None,
        Model::Isdb2056n => {
            let mut demod = Tc90522::new(0x13);
            demod.is_secondary = true;
            Some(demod)
        }
        _ => Some(Tc90522::new(0x11)),
    };
    let satellite = matches!(model, Model::M1ur | Model::Isdb2056 | Model::Isdb2056n);
    let board = Single {
        model,
        bridge,
        demod_t: Tc90522::new(addr_t),
        demod_s,
        demod_s0: (model == Model::Isdb2056n).then(|| Tc90522::new(0x11)),
        r850: R850::new(R850_ADDR, r850_config(false)),
        rt710: satellite.then(|| Rt710::new(RT710_ADDR, RT710_CONFIG)),
        system: None,
    };
    let systems = if satellite {
        vec![System::IsdbT, System::IsdbS]
    } else {
        vec![System::IsdbT]
    };
    Ok(board::device(info, board, vec![systems], &[false]))
}

impl Single {
    fn satellite(&self) -> bool {
        self.rt710.is_some()
    }

    async fn power(&mut self, on: bool) -> Result<()> {
        if on {
            self.bridge.write_gpio(GPIO_BACKEND_RESET, false).await?;
            Delay::new(Duration::from_millis(100)).await;
            self.bridge.write_gpio(GPIO_BACKEND_POWER, true).await?;
            Delay::new(Duration::from_millis(20)).await;
            Ok(())
        } else {
            let power = self.bridge.write_gpio(GPIO_BACKEND_POWER, false).await;
            let reset = self.bridge.write_gpio(GPIO_BACKEND_RESET, true).await;
            power.and(reset)
        }
    }

    async fn term(&mut self) {
        self.r850.term();
        if let Some(rt710) = &mut self.rt710 {
            let _ = rt710.term().await;
        }
    }

    async fn init(&mut self) -> Result<()> {
        let satellite = self.satellite();
        let mut i2c = self.bridge.i2c(I2C_BUS);
        self.r850
            .init(&mut self.demod_t.tuner_bus(&mut i2c))
            .await?;
        if let (Some(rt710), Some(demod_s)) = (&mut self.rt710, &self.demod_s) {
            rt710.init(&mut demod_s.tuner_bus(&mut i2c)).await?;
        }

        let init_t = if self.model == Model::Isdbt2071 {
            TC_INIT_ISDBT2071
        } else {
            TC_INIT_T
        };
        self.demod_t.write_multiple_regs(&mut i2c, init_t).await?;
        self.demod_t.enable_ts_pins_t(&mut i2c, false).await?;
        if satellite {
            // Asleep until tuned to ISDB-T.
            self.demod_t.sleep_t(&mut i2c, true).await?;
        } else {
            self.demod_t.sleep_t(&mut i2c, false).await?;
            self.r850.wakeup()?;
        }
        self.r850.set_system(R850_ISDB_T)?;

        if let Some(demod_s) = &self.demod_s {
            if satellite {
                demod_s.write_multiple_regs(&mut i2c, TC_INIT_S).await?;
            }
            demod_s.enable_ts_pins_s(&mut i2c, false).await?;
            demod_s.sleep_s(&mut i2c, true).await?;
        }
        Ok(())
    }

    async fn tune_t(&mut self, khz: u32) -> Result<()> {
        let satellite = self.satellite();
        let mut i2c = self.bridge.i2c(I2C_BUS);
        let demod = &self.demod_t;
        demod.write_reg(&mut i2c, 0x47, 0x30).await?;
        demod.set_agc_t(&mut i2c, false).await?;
        let regs: &[(u8, u8)] = match self.model {
            Model::S1ur => &[
                (0x0e, 0x77),
                (0x0f, 0x10),
                (0x71, 0x20),
                (0x76, 0x0c),
                (0x1f, 0x30),
            ],
            Model::Isdbt2071 => &[
                (0x76, 0x03),
                (0x77, 0x01),
                (0x3b, 0x10),
                (0x3c, 0x10),
                (0x3d, 0x24),
            ],
            _ => {
                if let Some(demod_s) = &self.demod_s {
                    demod_s.sleep_s(&mut i2c, true).await?;
                }
                demod
                    .write_multiple_regs(&mut i2c, &[(0x0e, 0x77), (0x0f, 0x10), (0x71, 0x20)])
                    .await?;
                demod.sleep_t(&mut i2c, false).await?;
                &[(0x76, 0x0c), (0x1f, 0x30)]
            }
        };
        demod.write_multiple_regs(&mut i2c, regs).await?;
        if satellite {
            self.r850.wakeup()?;
        }

        let r850 = &mut self.r850;
        let mut bus = demod.tuner_bus(&mut i2c);
        r850.set_frequency(&mut bus, khz).await?;
        wait_r850(r850, &mut bus).await?;

        demod.set_agc_t(&mut i2c, true).await?;
        if self.model != Model::Isdbt2071 {
            demod
                .write_multiple_regs(&mut i2c, &[(0x71, 0x01), (0x72, 0x25), (0x75, 0x00)])
                .await?;
        }
        Delay::new(Duration::from_millis(100)).await;
        Ok(())
    }

    async fn tune_s(&mut self, khz: u32) -> Result<()> {
        let (Some(demod_s), Some(rt710)) = (&self.demod_s, &mut self.rt710) else {
            return Err(Error::Unsupported(System::IsdbS));
        };
        let mut i2c = self.bridge.i2c(I2C_BUS);
        demod_s.set_agc_s(&mut i2c, false).await?;
        self.demod_t
            .write_multiple_regs(&mut i2c, &[(0x0e, 0x11), (0x0f, 0x70)])
            .await?;
        self.demod_t.sleep_t(&mut i2c, true).await?;
        match &self.demod_s0 {
            Some(demod_s0) => {
                demod_s0
                    .write_multiple_regs(&mut i2c, &[(0x07, 0x77), (0x08, 0x37)])
                    .await?
            }
            None => {
                demod_s
                    .write_multiple_regs(&mut i2c, &[(0x07, 0x77), (0x08, 0x10)])
                    .await?
            }
        }
        demod_s.sleep_s(&mut i2c, false).await?;
        demod_s
            .write_multiple_regs(&mut i2c, &[(0x04, 0x02), (0x8e, 0x02)])
            .await?;
        self.demod_t.write_reg(&mut i2c, 0x1f, 0x20).await?;

        let mut bus = demod_s.tuner_bus(&mut i2c);
        rt710.set_params(&mut bus, khz, 28860, 4).await?;
        wait_rt710(rt710, &mut bus).await?;

        demod_s.set_agc_s(&mut i2c, true).await
    }
}

impl Board for Single {
    fn bridge(&mut self, _bridge: usize) -> &mut Bridge {
        &mut self.bridge
    }

    fn source(&self, _index: usize) -> (usize, usize) {
        (0, 0)
    }

    async fn open(&mut self, _index: usize) -> Result<()> {
        self.power(true).await?;
        let result = self.init().await;
        if result.is_err() {
            self.term().await;
            let _ = self.power(false).await;
        }
        result
    }

    async fn close(&mut self, _index: usize) {
        self.term().await;
        let _ = self.power(false).await;
        self.system = None;
    }

    async fn tune(&mut self, _index: usize, params: &TuneParams) -> Result<()> {
        self.system = None;
        match params.system {
            System::IsdbT => self.tune_t(params.frequency_khz).await?,
            System::IsdbS => self.tune_s(params.if_frequency_khz()?).await?,
            System::IsdbS3 => return Err(Error::Unsupported(params.system)),
        }
        self.system = Some(params.system);
        Ok(())
    }

    async fn lock(&mut self, _index: usize, system: System) -> Result<Lock> {
        let mut i2c = self.bridge.i2c(I2C_BUS);
        let locked = match system {
            System::IsdbT => self.demod_t.is_signal_locked_t(&mut i2c).await?,
            _ => {
                let demod_s = self.demod_s.as_ref().ok_or(Error::Unsupported(system))?;
                demod_s.is_signal_locked_s(&mut i2c).await?
            }
        };
        Ok(if locked { Lock::Locked } else { Lock::Waiting })
    }

    async fn select(&mut self, _index: usize, params: &TuneParams) -> Result<()> {
        let Some(stream_id) = params.stream_id else {
            return Ok(());
        };
        let mut i2c = self.bridge.i2c(I2C_BUS);
        let demod_s = self
            .demod_s
            .as_ref()
            .ok_or(Error::Unsupported(System::IsdbS))?;
        select_tsid(demod_s, &mut i2c, stream_id.0).await
    }

    async fn signal(&mut self, _index: usize, system: System) -> Result<Signal> {
        let mut i2c = self.bridge.i2c(I2C_BUS);
        Ok(match system {
            System::IsdbT => Signal {
                locked: self.demod_t.is_signal_locked_t(&mut i2c).await?,
                cnr_db: Some(cnr::isdb_t(self.demod_t.get_cndat_t(&mut i2c).await?)),
            },
            _ => {
                let demod_s = self.demod_s.as_ref().ok_or(Error::Unsupported(system))?;
                Signal {
                    locked: demod_s.is_signal_locked_s(&mut i2c).await?,
                    cnr_db: Some(cnr::isdb_s(demod_s.get_cn_s(&mut i2c).await?)),
                }
            }
        })
    }

    async fn set_capture(&mut self, _index: usize, on: bool) -> Result<()> {
        let mut i2c = self.bridge.i2c(I2C_BUS);
        match self.system {
            Some(System::IsdbT) => self.demod_t.enable_ts_pins_t(&mut i2c, on).await,
            Some(_) => {
                let demod_s = self
                    .demod_s
                    .as_ref()
                    .ok_or(Error::Unsupported(System::IsdbS))?;
                demod_s.enable_ts_pins_s(&mut i2c, on).await
            }
            None => Ok(()),
        }
    }
}
