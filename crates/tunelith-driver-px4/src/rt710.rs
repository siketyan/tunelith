// SPDX-License-Identifier: GPL-2.0-only
//! Rafael Micro RT710 (and its successor RT720), the ISDB-S tuner, reached
//! through the I2C relay of the demodulator in front of it.
//!
//! Ported from `rt710.c` of px4_drv, Copyright (c) 2018-2021 nns779, but
//! for the RF signal strength, which px4_drv only logs.

use std::time::Duration;

use futures_timer::Delay;
use tunelith_core::{I2c, Result};

use crate::error;

const NUM_REGS: usize = 0x10;

const RT710_INIT_REGS: [u8; NUM_REGS] = [
    0x40, 0x1d, 0x20, 0x10, 0x41, 0x50, 0xed, 0x25, 0x07, 0x58, 0x39, 0x64, 0x38, 0xe7, 0x90, 0x35,
];

const RT720_INIT_REGS: [u8; NUM_REGS] = [
    0x00, 0x1c, 0x00, 0x10, 0x41, 0x48, 0xda, 0x4b, 0x07, 0x58, 0x38, 0x40, 0x37, 0xe7, 0x4c, 0x59,
];

const SLEEP_REGS: [u8; NUM_REGS] = [
    0xff, 0x5c, 0x88, 0x30, 0x41, 0xc8, 0xed, 0x25, 0x47, 0xfc, 0x48, 0xa2, 0x08, 0x0f, 0xf3, 0x59,
];

/// Upper bandwidth in kHz and its (coarse, fine) setting, for the RT710.
const BANDWIDTH_PARAMS: [(u32, u8, u8); 26] = [
    (50000, 0, 0),
    (73000, 0, 1),
    (96000, 1, 0),
    (104000, 1, 1),
    (116000, 2, 0),
    (126000, 2, 1),
    (134000, 3, 0),
    (146000, 3, 1),
    (158000, 4, 0),
    (170000, 4, 1),
    (178000, 5, 0),
    (190000, 5, 1),
    (202000, 6, 0),
    (212000, 6, 1),
    (218000, 7, 0),
    (234000, 7, 1),
    (244000, 9, 1),
    (246000, 10, 0),
    (262000, 10, 1),
    (266000, 11, 0),
    (282000, 11, 1),
    (298000, 12, 1),
    (318000, 13, 1),
    (340000, 14, 1),
    (358000, 15, 1),
    (379999, 16, 1),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChipType {
    Rt710,
    Rt720,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
// The chip's settings, whether a model uses them or not.
#[allow(dead_code)]
pub enum SignalOutputMode {
    Single,
    Differential,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
// The chip's settings, whether a model uses them or not.
#[allow(dead_code)]
pub enum AgcMode {
    Negative,
    Positive,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
// The chip's settings, whether a model uses them or not.
#[allow(dead_code)]
pub enum VgaAttenMode {
    Off,
    On,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
// The chip's settings, whether a model uses them or not.
#[allow(dead_code)]
pub enum FineGain {
    Db3 = 0,
    Db2 = 1,
    Db1 = 2,
    Db0 = 3,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
// The chip's settings, whether a model uses them or not.
#[allow(dead_code)]
pub enum ScanMode {
    Manual,
    Auto,
}

#[derive(Clone, Copy, Debug)]
pub struct Rt710Config {
    /// Crystal frequency in kHz.
    pub xtal: u32,
    pub loop_through: bool,
    pub clock_out: bool,
    pub signal_output_mode: SignalOutputMode,
    pub agc_mode: AgcMode,
    pub vga_atten_mode: VgaAttenMode,
    pub fine_gain: FineGain,
    /// Only for the RT720.
    pub scan_mode: ScanMode,
}

pub struct Rt710 {
    i2c_addr: u8,
    config: Rt710Config,
    /// The detected chip, `None` until initialised.
    chip: Option<ChipType>,
    /// The frequency in kHz the PLL is set to, 0 if none.
    freq: u32,
}

async fn sleep_ms(ms: u64) {
    Delay::new(Duration::from_millis(ms)).await;
}

impl Rt710 {
    pub fn new(i2c_addr: u8, config: Rt710Config) -> Self {
        Self {
            i2c_addr,
            config,
            chip: None,
            freq: 0,
        }
    }

    fn chip_or_err(&self) -> Result<ChipType> {
        self.chip.ok_or_else(|| error("RT710 not initialised"))
    }

    /// Reads always start at register 0 and come with their bits reversed.
    async fn read_regs(&self, i2c: &mut impl I2c, reg: u8, buf: &mut [u8]) -> Result<()> {
        let (reg, len) = (reg as usize, buf.len());
        if len == 0 || len > NUM_REGS.saturating_sub(reg) {
            return Err(error("invalid RT710 register read"));
        }
        let mut b = [0u8; NUM_REGS];
        i2c.write_read(self.i2c_addr, &[0x00], &mut b[..reg + len])
            .await?;
        for (dst, src) in buf.iter_mut().zip(&b[reg..reg + len]) {
            *dst = src.reverse_bits();
        }
        Ok(())
    }

    async fn read_reg(&self, i2c: &mut impl I2c, reg: u8) -> Result<u8> {
        let mut v = [0];
        self.read_regs(i2c, reg, &mut v).await?;
        Ok(v[0])
    }

    async fn write_regs(&self, i2c: &mut impl I2c, reg: u8, data: &[u8]) -> Result<()> {
        if data.is_empty() || data.len() > NUM_REGS.saturating_sub(reg as usize) {
            return Err(error("invalid RT710 register write"));
        }
        let mut b = vec![reg];
        b.extend_from_slice(data);
        i2c.write(self.i2c_addr, &b).await
    }

    async fn write_reg(&self, i2c: &mut impl I2c, regs: &[u8; NUM_REGS], reg: u8) -> Result<()> {
        self.write_regs(i2c, reg, &regs[reg as usize..=reg as usize])
            .await
    }

    async fn set_pll(
        &mut self,
        i2c: &mut impl I2c,
        chip: ChipType,
        regs: &mut [u8; NUM_REGS],
        freq: u32,
    ) -> Result<()> {
        let xtal = self.config.xtal;
        let vco_min = 2350000;
        let vco_max = vco_min * 2;
        let mut mix_div: u32 = 2;
        let mut vco_freq = freq * mix_div;

        self.freq = 0;

        while mix_div <= 16 {
            if vco_freq >= vco_min && vco_freq <= vco_max {
                break;
            }
            mix_div *= 2;
            vco_freq = freq * mix_div;
        }

        let div_num: u8 = match mix_div {
            2 => 1,
            4 => 0,
            8 => 2,
            16 => 3,
            _ => 0,
        };

        regs[0x04] &= 0xfe;
        regs[0x04] |= div_num & 0x01;
        self.write_reg(i2c, regs, 0x04).await?;

        if chip == ChipType::Rt720 {
            regs[0x08] &= 0xef;
            regs[0x08] |= (div_num << 3) & 0x10;
            self.write_reg(i2c, regs, 0x08).await?;

            regs[0x04] &= 0x3f;
            if div_num <= 1 {
                regs[0x04] |= 0x40;
                regs[0x0c] |= 0x10;
            } else {
                regs[0x04] |= 0x80;
                regs[0x0c] &= 0xef;
            }
            self.write_reg(i2c, regs, 0x04).await?;
            self.write_reg(i2c, regs, 0x0c).await?;
        }

        // C keeps these in u8/u16; truncate the same way.
        let mut nint = ((vco_freq / 2) / xtal) as u8;
        let mut vco_fra = vco_freq.wrapping_sub(xtal * 2 * nint as u32) as u16;
        let fra = vco_fra as u32;

        if fra < xtal / 64 {
            vco_fra = 0;
        } else if fra > xtal * 127 / 64 {
            vco_fra = 0;
            nint = nint.wrapping_add(1);
        } else if fra > xtal * 127 / 128 && fra < xtal {
            vco_fra = (xtal * 127 / 128) as u16;
        } else if fra > xtal && fra < xtal * 129 / 128 {
            vco_fra = (xtal * 129 / 128) as u16;
        }

        let ni = nint.wrapping_sub(13) / 4;
        let si = nint.wrapping_sub(ni * 4).wrapping_sub(13);

        regs[0x05] = (ni & 0x3f) | ((si << 6) & 0xc0);
        self.write_reg(i2c, regs, 0x05).await?;

        if vco_fra == 0 {
            regs[0x04] |= 0x02;
        }
        self.write_reg(i2c, regs, 0x04).await?;

        let mut nsdm: u16 = 2;
        let mut sdm: u16 = 0;
        while vco_fra > 1 {
            // C would divide by zero if nsdm overflowed, which cannot happen
            // with a 24 MHz crystal: the step reaches 1 at nsdm 0x8000.
            let t = (xtal * 2) / nsdm as u32;
            if vco_fra as u32 > t {
                sdm = sdm.wrapping_add(0x8000 / (nsdm / 2));
                vco_fra = (vco_fra as u32 - t) as u16;
                if nsdm >= 0x8000 {
                    break;
                }
            }
            nsdm = nsdm.wrapping_mul(2);
        }

        regs[0x07] = (sdm >> 8) as u8;
        regs[0x06] = sdm as u8;
        self.write_reg(i2c, regs, 0x07).await?;
        self.write_reg(i2c, regs, 0x06).await?;

        self.freq = freq;
        Ok(())
    }

    pub async fn init(&mut self, i2c: &mut impl I2c) -> Result<()> {
        self.chip = None;
        self.freq = 0;

        let tmp = self.read_reg(i2c, 0x03).await?;
        self.chip = Some(if tmp & 0xf0 == 0x70 {
            ChipType::Rt710
        } else {
            ChipType::Rt720
        });
        Ok(())
    }

    pub async fn term(&mut self) -> Result<()> {
        self.chip = None;
        Ok(())
    }

    pub async fn sleep(&mut self, i2c: &mut impl I2c) -> Result<()> {
        let chip = self.chip_or_err()?;
        let mut regs = SLEEP_REGS;

        if chip == ChipType::Rt720 {
            regs[0x01] = 0x5e;
            regs[0x03] |= 0x20;
        } else if self.config.clock_out {
            regs[0x03] = 0x20;
        }

        self.write_regs(i2c, 0x00, &regs).await
    }

    /// `freq` and `symbol_rate` in kHz and ksps; `rolloff` 0 to 5.
    pub async fn set_params(
        &mut self,
        i2c: &mut impl I2c,
        freq: u32,
        mut symbol_rate: u32,
        rolloff: u32,
    ) -> Result<()> {
        let chip = self.chip_or_err()?;
        if rolloff > 5 {
            return Err(error("invalid RT710 rolloff"));
        }

        let config = self.config;
        let mut regs = match chip {
            ChipType::Rt710 => RT710_INIT_REGS,
            ChipType::Rt720 => RT720_INIT_REGS,
        };

        if config.loop_through {
            regs[0x01] &= 0xfb;
        } else {
            regs[0x01] |= 0x04;
        }

        if config.clock_out {
            regs[0x03] &= 0xef;
        } else {
            regs[0x03] |= 0x10;
        }

        match config.signal_output_mode {
            SignalOutputMode::Differential => regs[0x0b] &= 0xef,
            SignalOutputMode::Single => regs[0x0b] |= 0x10,
        }

        match config.agc_mode {
            AgcMode::Positive => regs[0x0d] |= 0x10,
            AgcMode::Negative => regs[0x0d] &= 0xef,
        }

        match config.vga_atten_mode {
            VgaAttenMode::On => regs[0x0b] |= 0x08,
            VgaAttenMode::Off => regs[0x0b] &= 0xf7,
        }

        if chip == ChipType::Rt710 {
            regs[0x0e] &= 0xfc;
            regs[0x0e] |= config.fine_gain as u8 & 0x03;
        } else {
            if matches!(config.fine_gain, FineGain::Db3 | FineGain::Db2) {
                regs[0x0e] &= 0xfe;
            } else {
                regs[0x0e] |= 0x01;
            }
            regs[0x03] &= 0xf0;
        }

        self.write_regs(i2c, 0x00, &regs).await?;
        self.set_pll(i2c, chip, &mut regs, freq).await?;

        sleep_ms(10).await;

        if chip == ChipType::Rt710 {
            if freq.wrapping_sub(1600000) >= 350000 {
                regs[0x02] &= 0xbf;
                regs[0x08] &= 0x7f;
                if freq >= 1950000 {
                    regs[0x0a] = 0x38;
                }
            } else {
                regs[0x02] |= 0x40;
                regs[0x08] |= 0x80;
            }

            self.write_reg(i2c, &regs, 0x0a).await?;
            self.write_reg(i2c, &regs, 0x02).await?;
            self.write_reg(i2c, &regs, 0x08).await?;

            regs[0x0e] &= 0xf3;
            if freq >= 2000000 {
                regs[0x0e] |= 0x08;
            }
            self.write_reg(i2c, &regs, 0x0e).await?;
        } else {
            match config.scan_mode {
                ScanMode::Auto => {
                    regs[0x0b] |= 0x02;
                    symbol_rate += 10000;
                }
                ScanMode::Manual => {
                    regs[0x0b] &= 0xfc;
                    if symbol_rate >= 15000 {
                        symbol_rate += 6000;
                    }
                }
            }
            self.write_reg(i2c, &regs, 0x0b).await?;
        }

        regs[0x0f] = bandwidth_reg(chip, symbol_rate, rolloff)
            .ok_or_else(|| error("RT710 bandwidth is zero"))?;
        self.write_reg(i2c, &regs, 0x0f).await
    }

    pub async fn is_pll_locked(&mut self, i2c: &mut impl I2c) -> Result<bool> {
        self.chip_or_err()?;
        Ok(self.read_reg(i2c, 0x02).await? & 0x80 != 0)
    }
}

/// The value of register 0x0f for the filter bandwidth, `None` if it is zero.
fn bandwidth_reg(chip: ChipType, symbol_rate: u32, rolloff: u32) -> Option<u8> {
    let mut bandwidth = (symbol_rate * (115 + rolloff * 5)) / 10;
    if bandwidth == 0 {
        return None;
    }

    let (mut coarse, mut fine) = (0u8, 0u8);

    if chip == ChipType::Rt710 {
        if bandwidth >= 380000 {
            bandwidth -= 380000;
            if !bandwidth.is_multiple_of(17400) {
                coarse += 1;
            }
            coarse = coarse.wrapping_add((((bandwidth / 17400) & 0xff) as u8).wrapping_add(0x10));
            fine = 1;
        } else if let Some(&(_, c, f)) = BANDWIDTH_PARAMS.iter().find(|p| bandwidth <= p.0) {
            (coarse, fine) = (c, f);
        }
    } else {
        fine = if rolloff > 1 { 1 } else { 0 };
        let range = fine as u32 * 20000;
        let s = symbol_rate * 12;

        // C then adds 1000-3000 to the symbol rate, but after `s` was taken
        // from it, so that has no effect and is left out.

        if s <= 88000 + range {
            coarse = 0;
        } else if s <= 368000 + range {
            coarse = ((s - 88000 - range) / 20000) as u8;
            if !(s - 88000 - range).is_multiple_of(20000) {
                coarse += 1;
            }
            if coarse > 6 {
                coarse += 1;
            }
        } else if s <= 764000 + range {
            coarse = ((s - 368000 - range) / 20000) as u8 + 15;
            // Differs from the division above; kept as in C.
            if !(s + 25216 - range).is_multiple_of(20000) {
                coarse += 1;
            }
            if coarse >= 33 {
                coarse += 3;
            } else if coarse >= 29 {
                coarse += 2;
            } else if coarse >= 27 {
                coarse += 3;
            } else if coarse >= 24 {
                coarse += 2;
            } else if coarse >= 19 {
                coarse += 1;
            }
        } else {
            coarse = 42;
        }
    }

    Some(((coarse << 2) & 0xfc) | (fine & 0x03))
}

#[cfg(test)]
mod tests {
    use super::{ChipType, bandwidth_reg};

    #[test]
    fn bandwidth() {
        // 28.86 Msps, rolloff 4 as every caller uses: 389610 kHz.
        assert_eq!(bandwidth_reg(ChipType::Rt710, 28860, 4), Some(0x45));
        // 98400 kHz falls in the (1, 1) row.
        assert_eq!(bandwidth_reg(ChipType::Rt710, 8200, 1), Some(0x05));
        assert_eq!(bandwidth_reg(ChipType::Rt710, 0, 4), None);
        // RT720 manual scan: 28860 + 6000.
        assert_eq!(bandwidth_reg(ChipType::Rt720, 34860, 4), Some(0x45));
    }
}
