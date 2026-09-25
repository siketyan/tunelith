// SPDX-License-Identifier: GPL-2.0-only
//! Sony CXD2858ER, the ISDB-T/S tuner, reached through the gate of the
//! demodulator in front of it.
//!
//! Ported from `cxd2858er.c` of px4_drv, Copyright (c) 2018-2021 nns779.
//! The crystal is 16 MHz and both LNAs are on, as on every model using it.

use std::time::Duration;

use futures_timer::Delay;
use tunelith_core::{I2c, Result};

use crate::cxd2856er::{Cxd2856er, Mode};
use crate::error;

const ADDR: u8 = 0x60;

async fn write(i2c: &mut impl I2c, reg: u8, data: &[u8]) -> Result<()> {
    let mut buf = vec![reg];
    buf.extend_from_slice(data);
    i2c.write(ADDR, &buf).await
}

async fn read(i2c: &mut impl I2c, reg: u8) -> Result<u8> {
    let mut value = [0];
    i2c.write_read(ADDR, &[reg], &mut value).await?;
    Ok(value[0])
}

async fn write_mask(i2c: &mut impl I2c, reg: u8, value: u8, mask: u8) -> Result<()> {
    let current = read(i2c, reg).await?;
    write(i2c, reg, &[(current & !mask) | (value & mask)]).await
}

async fn sleep_ms(ms: u64) {
    Delay::new(Duration::from_millis(ms)).await;
}

/// Closes the gate after `result` whatever it is, keeping the first error.
async fn close(i2c: &mut impl I2c, demod: &Cxd2856er, result: Result<()>) -> Result<()> {
    let closed = demod.gate(i2c, false).await;
    result.and(closed)
}

#[derive(Default)]
pub struct Cxd2858er {
    /// The system it is set up for, if any.
    mode: Option<Mode>,
}

impl Cxd2858er {
    pub async fn init(&mut self, i2c: &mut impl I2c, demod: &Cxd2856er) -> Result<()> {
        self.mode = None;
        demod.gate(i2c, true).await?;
        let result = power_on(i2c).await;
        close(i2c, demod, result).await
    }

    pub async fn term(&mut self, i2c: &mut impl I2c, demod: &Cxd2856er) -> Result<()> {
        if self.mode.is_none() {
            return Ok(());
        }
        demod.gate(i2c, true).await?;
        let result = self.stop(i2c).await;
        close(i2c, demod, result).await
    }

    async fn stop(&mut self, i2c: &mut impl I2c) -> Result<()> {
        match self.mode.take() {
            Some(Mode::T) => {
                write(i2c, 0x74, &[0x02]).await?;
                write_mask(i2c, 0x67, 0x00, 0xfe).await?;
                write(i2c, 0x5e, &[0x15, 0x00, 0x00]).await?;
                write(i2c, 0x88, &[0x00]).await?;
                write(i2c, 0x87, &[0xc0]).await
            }
            Some(Mode::S) => {
                write(i2c, 0x15, &[0x02]).await?;
                write(i2c, 0x43, &[0x07]).await?;
                write(i2c, 0x0c, &[0x14, 0x00, 0x00]).await?;
                write(i2c, 0x01, &[0x00]).await?;
                write(i2c, 0x05, &[0x00]).await?;
                write(i2c, 0x04, &[0xc0]).await
            }
            None => Ok(()),
        }
    }

    /// Tunes to an ISDB-T frequency in kHz, 6 MHz wide.
    pub async fn tune_t(&mut self, i2c: &mut impl I2c, demod: &Cxd2856er, khz: u32) -> Result<()> {
        demod.gate(i2c, true).await?;
        let result = self.set_t(i2c, khz).await;
        close(i2c, demod, result).await
    }

    async fn set_t(&mut self, i2c: &mut impl I2c, khz: u32) -> Result<()> {
        if self.mode == Some(Mode::S) {
            self.stop(i2c).await?;
        }

        write(i2c, 0x01, &[0x00]).await?;
        write(i2c, 0x74, &[0x02]).await?;
        write(i2c, 0x87, &[0xc4, 0x40]).await?;
        write(i2c, 0x91, &[0x10, 0x20]).await?;
        write(i2c, 0x9c, &[0x00, 0x00]).await?;
        write(
            i2c,
            0x5e,
            &[0xee, 0x02, 0x1e, 0x67, 0x02, 0xb4, 0x78, 0x08, 0x30],
        )
        .await?;
        write_mask(i2c, 0x67, 0x00, 0x02).await?;
        let [f0, f1, f2, _] = khz.to_le_bytes();
        write(
            i2c,
            0x68,
            &[
                0x00,
                0x88,
                0x00,
                0x0b,
                0x22,
                0x00,
                0x17,
                0x1b,
                f0,
                f1,
                f2 & 0x0f,
                0xff,
                0x01,
                0x99,
                0x00,
                0x24,
                0x87,
            ],
        )
        .await?;
        sleep_ms(50).await;
        write(i2c, 0x88, &[0x00]).await?;
        write(i2c, 0x87, &[0xc0]).await?;
        self.mode = Some(Mode::T);
        Ok(())
    }

    /// Tunes to an ISDB-S intermediate frequency in kHz.
    pub async fn tune_s(&mut self, i2c: &mut impl I2c, demod: &Cxd2856er, khz: u32) -> Result<()> {
        demod.gate(i2c, true).await?;
        let result = self.set_s(i2c, khz).await;
        close(i2c, demod, result).await
    }

    async fn set_s(&mut self, i2c: &mut impl I2c, khz: u32) -> Result<()> {
        if self.mode == Some(Mode::T) {
            self.stop(i2c).await?;
        }

        write(i2c, 0x15, &[0x02]).await?;
        write(i2c, 0x43, &[0x06]).await?;
        write(i2c, 0x6a, &[0x00, 0x00]).await?;
        write(i2c, 0x75, &[0x99]).await?;
        write(i2c, 0x9d, &[0x00]).await?;
        write(i2c, 0x61, &[0x07]).await?;
        write(i2c, 0x01, &[0x01]).await?;
        let [f0, f1, f2, _] = ((khz + 2) / 4).to_le_bytes();
        write(
            i2c,
            0x04,
            &[
                0xc4,
                0x40,
                0x02,
                0x00,
                0xb4,
                0x78,
                0x08,
                0x30,
                0xff,
                0x02,
                0x1e,
                0x16,
                f0,
                f1,
                f2 & 0x0f,
                0xff,
                0x00,
                0x01,
            ],
        )
        .await?;
        sleep_ms(10).await;
        write(i2c, 0x05, &[0x00]).await?;
        write(i2c, 0x04, &[0xc0]).await?;
        self.mode = Some(Mode::S);
        Ok(())
    }
}

async fn power_on(i2c: &mut impl I2c) -> Result<()> {
    // T mode.
    write(i2c, 0x01, &[0x00]).await?;
    write(i2c, 0x67, &[0x00]).await?;
    write(i2c, 0x43, &[0x07]).await?;
    write(i2c, 0x5e, &[0x15, 0x00, 0x00]).await?;
    write(i2c, 0x0c, &[0x14]).await?;
    write(i2c, 0x99, &[0x7a, 0x01]).await?;
    write(
        i2c,
        0x81,
        &[
            0x10, 0x84, 0xa6, 0x00, 0x00, 0x00, 0xc4, 0x40, 0x10, 0x00, 0x45, 0x75, 0x07, 0x1c,
            0x3f, 0x02, 0x10, 0x20, 0x0a, 0x00,
        ],
    )
    .await?;
    write(i2c, 0x9b, &[0x00]).await?;
    sleep_ms(10).await;
    if read(i2c, 0x1a).await? != 0x00 {
        return Err(error("CXD2858ER did not power on"));
    }
    write(i2c, 0x17, &[0x90, 0x06]).await?;
    sleep_ms(1).await;
    let value = read(i2c, 0x19).await?;
    write(i2c, 0x95, &[(value & 0xf0) >> 4]).await?;
    write(i2c, 0x74, &[0x02]).await?;
    write(i2c, 0x88, &[0x00]).await?;
    write(i2c, 0x87, &[0xc0]).await?;
    write(i2c, 0x80, &[0x01]).await?;
    write(i2c, 0x41, &[0x07, 0x00]).await
}
