// SPDX-License-Identifier: GPL-2.0-only
//! Sony CXD2856ER, the ISDB-T/S demodulator.
//!
//! Ported from `cxd2856er.c` of px4_drv, Copyright (c) 2018-2021 nns779.

use std::time::Duration;

use futures_timer::Delay;
use tunelith_core::{I2c, Result};

/// The system the demodulator runs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    T,
    S,
}

/// A register write, as the sequences below are made of.
#[derive(Clone, Copy)]
enum Op {
    /// To the system address (SLVX).
    X(u8, u8),
    /// To the demodulator address (SLVT).
    T(u8, u8),
    /// To SLVT, only the bits in the mask.
    M(u8, u8, u8),
    /// To SLVT, from the register on.
    B(u8, &'static [u8]),
    SleepMs(u64),
}

use Op::*;

const INIT: &[Op] = &[
    X(0x00, 0x00),
    X(0x10, 0x01),
    X(0x18, 0x01),
    X(0x28, 0x13),
    X(0x17, 0x01),
    X(0x1d, 0x00),
    // 24 MHz crystal.
    X(0x14, 0x01),
    X(0x1c, 0x03),
    SleepMs(4),
    X(0x50, 0x00),
    SleepMs(1),
    X(0x10, 0x00),
    SleepMs(1),
    X(0x00, 0x00),
    // The tuner is on the demodulator's I2C.
    X(0x1a, 0x01),
];

fn ts_clock(mode: Mode) -> Vec<Op> {
    vec![
        T(0x00, 0x00),
        M(0xd3, 0x01, 0x01),
        M(0xde, 0x00, 0x01),
        M(0xda, 0x00, 0x01),
        M(0xc4, 0x00, 0x03),
        M(0xd1, 0x02, 0x03),
        T(0xd9, 0x10),
        M(0x32, 0x00, 0x01),
        M(0x33, if mode == Mode::T { 0x02 } else { 0x00 }, 0x03),
        M(0x32, 0x01, 0x01),
        T(0x00, 0x10),
        M(0x66, 0x01, 0x01),
        T(0x00, 0x40),
        M(0x66, 0x01, 0x01),
    ]
}

const WAKEUP_T: &[Op] = &[
    X(0x00, 0x00),
    X(0x17, 0x06),
    T(0x00, 0x00),
    T(0xa9, 0x00),
    T(0x2c, 0x01),
    T(0x4b, 0x74),
    T(0x49, 0x00),
    X(0x18, 0x00),
    T(0x00, 0x11),
    T(0x6a, 0x50),
    T(0x00, 0x10),
    T(0xa5, 0x01),
    T(0x00, 0x00),
    B(0xce, &[0x00, 0x00]),
    T(0x00, 0x10),
    T(0x69, 0x04),
    T(0x6b, 0x03),
    T(0x9d, 0x50),
    T(0xd3, 0x06),
    T(0xed, 0x00),
    T(0xe2, 0xce),
    T(0xf2, 0x13),
    T(0xde, 0x2e),
    T(0x00, 0x15),
    T(0xde, 0x02),
    T(0x00, 0x17),
    B(0x38, &[0x00, 0x03]),
    T(0x00, 0x1e),
    T(0x73, 0x68),
    T(0x00, 0x63),
    T(0x81, 0x00),
    T(0x00, 0x11),
    B(0x33, &[0x00, 0x03, 0x3b]),
    T(0x00, 0x60),
    B(0xa8, &[0xb7, 0x1b]),
    // Bandwidth: 6 MHz.
    T(0x00, 0x10),
    B(0x9f, &[0x17, 0xa0, 0x80, 0x00, 0x00]),
    B(
        0xa6,
        &[
            0x31, 0xa8, 0x29, 0x9b, 0x27, 0x9c, 0x28, 0x9e, 0x29, 0xa4, 0x29, 0xa2, 0x29, 0xa8,
        ],
    ),
    B(0xb6, &[0x12, 0xee, 0xef]),
    T(0xd7, 0x04),
    B(0xd9, &[0x1f, 0x79]),
    T(0x00, 0x12),
    T(0x71, 0x07),
    T(0x00, 0x15),
    T(0xbe, 0x02),
    T(0x00, 0x00),
    M(0x80, 0x08, 0x1f),
];

const WAKEUP_S: &[Op] = &[
    X(0x00, 0x00),
    X(0x17, 0x0c),
    T(0x00, 0x00),
    T(0x2d, 0x00),
    T(0xa9, 0x00),
    T(0x2c, 0x01),
    X(0x28, 0x31),
    T(0x4b, 0x31),
    T(0x6a, 0x00),
    X(0x18, 0x00),
    T(0x00, 0x00),
    T(0x20, 0x01),
    B(0xce, &[0x00, 0x00]),
    T(0x00, 0xae),
    B(0x20, &[0x07, 0x37, 0x0a]),
    T(0x00, 0xa0),
    T(0xd7, 0x00),
    T(0x00, 0x00),
    M(0x80, 0x10, 0x1f),
];

const SLEEP_HEAD: &[Op] = &[T(0x00, 0x00), T(0xc3, 0x01), M(0x80, 0x1f, 0x1f)];

const SLEEP_T: &[Op] = &[
    T(0x00, 0x10),
    T(0x69, 0x05),
    T(0x6b, 0x07),
    T(0x9d, 0x14),
    T(0xd3, 0x00),
    T(0xed, 0x01),
    T(0xe2, 0x4e),
    T(0xf2, 0x03),
    T(0xde, 0x32),
    T(0x00, 0x15),
    T(0xde, 0x03),
    T(0x00, 0x17),
    B(0x38, &[0x01, 0x00]),
    T(0x00, 0x1e),
    T(0x73, 0x00),
    T(0x00, 0x63),
    T(0x81, 0x01),
    X(0x00, 0x00),
    X(0x18, 0x01),
    T(0x00, 0x00),
    T(0x49, 0x33),
    T(0x4b, 0x21),
    T(0xfe, 0x01),
    T(0x2c, 0x00),
    T(0xa9, 0x00),
    X(0x17, 0x01),
];

const SLEEP_S: &[Op] = &[
    X(0x00, 0x00),
    X(0x18, 0x01),
    T(0x00, 0x00),
    T(0x6a, 0x11),
    T(0x4b, 0x21),
    X(0x28, 0x13),
    T(0xfe, 0x01),
    T(0x2c, 0x00),
    T(0xa9, 0x00),
    T(0x2d, 0x00),
    X(0x17, 0x01),
    T(0x00, 0xa0),
    T(0xd7, 0x00),
];

const POST_TUNE: &[Op] = &[T(0x00, 0x00), T(0xfe, 0x01), T(0xc3, 0x00)];

pub struct Cxd2856er {
    slvx: u8,
    slvt: u8,
    /// The system it runs, or `None` asleep.
    mode: Option<Mode>,
}

impl Cxd2856er {
    pub fn new(i2c_addr: u8) -> Self {
        Self {
            slvx: i2c_addr + 2,
            slvt: i2c_addr,
            mode: None,
        }
    }

    async fn run(&self, i2c: &mut impl I2c, ops: &[Op]) -> Result<()> {
        for &op in ops {
            match op {
                X(reg, value) => i2c.write(self.slvx, &[reg, value]).await?,
                T(reg, value) => self.write(i2c, reg, &[value]).await?,
                M(reg, value, mask) => self.write_mask(i2c, reg, value, mask).await?,
                B(reg, data) => self.write(i2c, reg, data).await?,
                SleepMs(ms) => Delay::new(Duration::from_millis(ms)).await,
            }
        }
        Ok(())
    }

    async fn write(&self, i2c: &mut impl I2c, reg: u8, data: &[u8]) -> Result<()> {
        let mut buf = vec![reg];
        buf.extend_from_slice(data);
        i2c.write(self.slvt, &buf).await
    }

    async fn read(&self, i2c: &mut impl I2c, reg: u8, buf: &mut [u8]) -> Result<()> {
        i2c.write_read(self.slvt, &[reg], buf).await
    }

    async fn write_mask(&self, i2c: &mut impl I2c, reg: u8, value: u8, mask: u8) -> Result<()> {
        let value = if mask == 0xff {
            value
        } else {
            let mut current = [0];
            self.read(i2c, reg, &mut current).await?;
            (current[0] & !mask) | (value & mask)
        };
        self.write(i2c, reg, &[value]).await
    }

    /// Writes the SLVT registers the device model sets up after [`init`](Self::init).
    pub async fn setup(&self, i2c: &mut impl I2c, ops: &[(u8, u8, u8)]) -> Result<()> {
        for &(reg, value, mask) in ops {
            self.write_mask(i2c, reg, value, mask).await?;
        }
        Ok(())
    }

    pub async fn init(&mut self, i2c: &mut impl I2c) -> Result<()> {
        self.mode = None;
        self.run(i2c, INIT).await
    }

    /// Opens or closes the way to the tuner behind the demodulator.
    pub async fn gate(&self, i2c: &mut impl I2c, open: bool) -> Result<()> {
        i2c.write(self.slvx, &[0x08, open.into()]).await
    }

    async fn set_ts_pins(&self, i2c: &mut impl I2c, on: bool) -> Result<()> {
        self.write(i2c, 0x00, &[0x00]).await?;
        let mut c4 = [0];
        self.read(i2c, 0xc4, &mut c4).await?;
        let mask = match c4[0] & 0x88 {
            0x80 => 0x01,
            0x88 => 0x80,
            _ => 0xff,
        };
        self.write(i2c, 0x00, &[0x00]).await?;
        self.write_mask(i2c, 0x81, if on { 0x00 } else { 0xff }, mask)
            .await
    }

    pub async fn sleep(&mut self, i2c: &mut impl I2c) -> Result<()> {
        let Some(mode) = self.mode.take() else {
            return Ok(());
        };
        self.run(i2c, SLEEP_HEAD).await?;
        self.set_ts_pins(i2c, false).await?;
        self.run(i2c, if mode == Mode::T { SLEEP_T } else { SLEEP_S })
            .await
    }

    pub async fn wakeup(&mut self, i2c: &mut impl I2c, mode: Mode) -> Result<()> {
        self.sleep(i2c).await?;
        self.run(i2c, &ts_clock(mode)).await?;
        self.run(i2c, if mode == Mode::T { WAKEUP_T } else { WAKEUP_S })
            .await?;
        self.set_ts_pins(i2c, true).await?;
        self.mode = Some(mode);
        Ok(())
    }

    pub async fn post_tune(&self, i2c: &mut impl I2c) -> Result<()> {
        self.run(i2c, POST_TUNE).await
    }

    pub async fn set_tsid(&self, i2c: &mut impl I2c, tsid: u16) -> Result<()> {
        self.write(i2c, 0x00, &[0xc0]).await?;
        let [hi, lo] = tsid.to_be_bytes();
        self.write(i2c, 0xe9, &[hi, lo, 0]).await
    }

    /// Whether the TS has locked, and for ISDB-T whether the demodulator
    /// has given up on it.
    pub async fn lock_status(&self, i2c: &mut impl I2c) -> Result<(bool, bool)> {
        let mut status = [0];
        if self.mode == Some(Mode::T) {
            self.write(i2c, 0x00, &[0x60]).await?;
            self.read(i2c, 0x10, &mut status).await?;
            Ok((status[0] & 0x01 != 0, status[0] & 0x10 != 0))
        } else {
            self.write(i2c, 0x00, &[0xa0]).await?;
            self.read(i2c, 0x12, &mut status).await?;
            Ok((status[0] & 0x40 != 0, false))
        }
    }

    /// The raw C/N value the device model converts.
    pub async fn cnr_raw(&self, i2c: &mut impl I2c) -> Result<u16> {
        if self.mode == Some(Mode::T) {
            let mut raw = [0; 2];
            self.write(i2c, 0x01, &[0x01]).await?;
            self.write(i2c, 0x00, &[0x60]).await?;
            self.read(i2c, 0x28, &mut raw).await?;
            self.write(i2c, 0x01, &[0x00]).await?;
            Ok(u16::from_be_bytes(raw))
        } else {
            let mut raw = [0; 3];
            self.write(i2c, 0x00, &[0xa1]).await?;
            self.read(i2c, 0x10, &mut raw).await?;
            // px4_drv masks the high byte after shifting it, which leaves
            // nothing of it; this takes the 13 bits the table ranges over.
            Ok(if raw[0] & 0x01 != 0 {
                u16::from(raw[1] & 0x1f) << 8 | u16::from(raw[2])
            } else {
                0x5af
            })
        }
    }
}
