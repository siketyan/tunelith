// SPDX-License-Identifier: GPL-2.0-only
//! Toshiba TC90522, the ISDB-T/S demodulator.
//!
//! Ported from `tc90522.c` of px4_drv, Copyright (c) 2018-2021 nns779.

use tunelith_core::{I2c, Result};

use crate::error;

pub struct Tc90522 {
    addr: u8,
    /// The second ISDB-S demodulator of a pair, which takes a different AGC setting.
    pub is_secondary: bool,
}

impl Tc90522 {
    pub fn new(i2c_addr: u8) -> Self {
        Self {
            addr: i2c_addr,
            is_secondary: false,
        }
    }

    pub async fn read_regs(&self, i2c: &mut impl I2c, reg: u8, buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() {
            return Err(error("empty TC90522 read"));
        }
        i2c.write_read(self.addr, &[reg], buf).await
    }

    pub async fn read_reg(&self, i2c: &mut impl I2c, reg: u8) -> Result<u8> {
        let mut b = [0];
        self.read_regs(i2c, reg, &mut b).await?;
        Ok(b[0])
    }

    pub async fn write_regs(&self, i2c: &mut impl I2c, reg: u8, data: &[u8]) -> Result<()> {
        if data.is_empty() || data.len() > 254 {
            return Err(error("bad TC90522 write length"));
        }
        let mut b = vec![reg];
        b.extend_from_slice(data);
        i2c.write(self.addr, &b).await
    }

    pub async fn write_reg(&self, i2c: &mut impl I2c, reg: u8, value: u8) -> Result<()> {
        self.write_regs(i2c, reg, &[value]).await
    }

    /// Writes `(reg, value)` pairs in order, stopping at the first error.
    pub async fn write_multiple_regs(&self, i2c: &mut impl I2c, regs: &[(u8, u8)]) -> Result<()> {
        for &(reg, value) in regs {
            self.write_reg(i2c, reg, value).await?;
        }
        Ok(())
    }

    /// Relays I2C to the tuner behind the demodulator.
    pub fn tuner_bus<'a, B: I2c>(&self, i2c: &'a mut B) -> TunerBus<'a, B> {
        TunerBus {
            i2c,
            addr: self.addr,
        }
    }

    pub async fn sleep_s(&self, i2c: &mut impl I2c, sleep: bool) -> Result<()> {
        let (a, b) = if sleep { (0x80, 0xff) } else { (0x00, 0x00) };
        self.write_multiple_regs(i2c, &[(0x13, a), (0x17, b)]).await
    }

    pub async fn set_agc_s(&self, i2c: &mut impl I2c, on: bool) -> Result<()> {
        let mut regs = [(0x0a, 0x00), (0x10, 0xb0), (0x11, 0x02), (0x03, 0x01)];
        if self.is_secondary {
            regs[1].1 = 0x30;
        }
        if on {
            regs[0].1 = 0xff;
            regs[1].1 |= 0x02;
            regs[2].1 = 0x00;
        }
        self.write_multiple_regs(i2c, &regs).await
    }

    pub async fn get_tsid_s(&self, i2c: &mut impl I2c) -> Result<u16> {
        self.read_u16(i2c, 0xe6).await
    }

    pub async fn set_tsid_s(&self, i2c: &mut impl I2c, tsid: u16) -> Result<()> {
        self.write_regs(i2c, 0x8f, &tsid.to_be_bytes()).await
    }

    pub async fn get_cn_s(&self, i2c: &mut impl I2c) -> Result<u16> {
        self.read_u16(i2c, 0xbc).await
    }

    pub async fn enable_ts_pins_s(&self, i2c: &mut impl I2c, enable: bool) -> Result<()> {
        let (a, b) = if enable { (0x00, 0x00) } else { (0x80, 0x22) };
        self.write_multiple_regs(i2c, &[(0x1c, a), (0x1f, b)]).await
    }

    pub async fn is_signal_locked_s(&self, i2c: &mut impl I2c) -> Result<bool> {
        Ok(self.read_reg(i2c, 0xc3).await? & 0x10 == 0)
    }

    pub async fn sleep_t(&self, i2c: &mut impl I2c, sleep: bool) -> Result<()> {
        self.write_reg(i2c, 0x03, if sleep { 0xf0 } else { 0x00 })
            .await
    }

    pub async fn set_agc_t(&self, i2c: &mut impl I2c, on: bool) -> Result<()> {
        let v = if on { 0x4c } else { 0x4d };
        self.write_multiple_regs(i2c, &[(0x25, 0x00), (0x20, 0x00), (0x23, v), (0x01, 0x50)])
            .await
    }

    pub async fn get_cndat_t(&self, i2c: &mut impl I2c) -> Result<u32> {
        let mut b = [0; 3];
        self.read_regs(i2c, 0x8b, &mut b).await?;
        Ok(u32::from_be_bytes([0, b[0], b[1], b[2]]))
    }

    pub async fn enable_ts_pins_t(&self, i2c: &mut impl I2c, enable: bool) -> Result<()> {
        self.write_reg(i2c, 0x1d, if enable { 0x00 } else { 0xa8 })
            .await
    }

    pub async fn is_signal_locked_t(&self, i2c: &mut impl I2c) -> Result<bool> {
        // The C driver swallows read errors here and reports "not locked".
        let Ok(b) = self.read_reg(i2c, 0x80).await else {
            return Ok(false);
        };
        if b & 0x28 != 0 {
            return Ok(false);
        }
        let Ok(b) = self.read_reg(i2c, 0xb0).await else {
            return Ok(false);
        };
        Ok(b & 0x0f >= 8)
    }

    async fn read_u16(&self, i2c: &mut impl I2c, reg: u8) -> Result<u16> {
        let mut b = [0; 2];
        self.read_regs(i2c, reg, &mut b).await?;
        Ok(u16::from_be_bytes(b))
    }
}

/// I2C to the tuner, relayed by the demodulator.
pub struct TunerBus<'a, B> {
    i2c: &'a mut B,
    addr: u8,
}

impl<B: I2c> I2c for TunerBus<'_, B> {
    async fn write(&mut self, addr: u8, data: &[u8]) -> Result<()> {
        if data.is_empty() || data.len() > 253 {
            return Err(error("bad TC90522 relay write length"));
        }
        let mut b = vec![0xfe, addr << 1];
        b.extend_from_slice(data);
        self.i2c.write(self.addr, &b).await
    }

    async fn write_read(&mut self, addr: u8, data: &[u8], buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() {
            return Err(error("empty TC90522 relay read"));
        }
        self.write(addr, data).await?;
        self.i2c
            .write_read(self.addr, &[0xfe, (addr << 1) | 0x01], buf)
            .await
    }
}
