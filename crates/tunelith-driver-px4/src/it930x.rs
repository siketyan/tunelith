// SPDX-License-Identifier: GPL-2.0-only
//! ITE IT930x, the USB bridge carrying the TS of every tuner and reaching
//! their chips over I2C.
//!
//! Ported from `it930x.c` and `itedtv_bus.c` of px4_drv,
//! Copyright (c) 2018-2021 nns779.

use std::time::Duration;

use futures_timer::Delay;
use tunelith_core::usb::{BulkIn, UsbTransport, with_timeout};
use tunelith_core::{I2c, Result};

use crate::error;

const CMD_REG_READ: u16 = 0x00;
const CMD_REG_WRITE: u16 = 0x01;
const CMD_QUERYINFO: u16 = 0x22;
const CMD_BOOT: u16 = 0x23;
const CMD_FW_SCATTER_WRITE: u16 = 0x29;
const CMD_I2C_READ: u16 = 0x2a;
const CMD_I2C_WRITE: u16 = 0x2b;

const EP_CTRL_OUT: u8 = 0x02;
const EP_CTRL_IN: u8 = 0x81;
pub const EP_STREAM: u8 = 0x84;

const CTRL_TIMEOUT: Duration = Duration::from_secs(3);
const PSB_PURGE_TIMEOUT: Duration = Duration::from_secs(2);
const I2C_SPEED: u8 = 0x07;
/// The size of the transfers the bridge sends the TS in.
pub const XFER_SIZE: usize = 188 * 816;

/// A TS input port, where a demodulator hands the bridge its stream.
pub struct Input {
    pub port: u8,
    pub slave: u8,
    pub i2c_bus: u8,
    pub i2c_addr: u8,
    /// Replaces 0x47 in the packets of this input, telling it apart from the
    /// others in the multiplexed stream.
    pub sync_byte: u8,
}

pub struct It930x<T> {
    usb: T,
    seq: u8,
    max_bulk_size: usize,
    gpio_out: u16,
    gpio_on: u16,
}

impl<T: UsbTransport> It930x<T> {
    /// `usb_version` is `bcdUSB`, which gives the size of the bulk packets.
    pub fn new(usb: T, usb_version: u16) -> Self {
        Self {
            usb,
            seq: 0,
            max_bulk_size: if usb_version == 0x0110 { 64 } else { 512 },
            gpio_out: 0,
            gpio_on: 0,
        }
    }

    pub fn usb(&self) -> &T {
        &self.usb
    }

    async fn ctrl(&mut self, cmd: u16, payload: &[u8]) -> Result<Vec<u8>> {
        if payload.len() > 255 - 3 - 2 {
            return Err(error("control message too long"));
        }

        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);

        let len = 4 + payload.len() + 2;
        let mut buf = Vec::with_capacity(len);
        buf.push((len - 1) as u8);
        buf.extend_from_slice(&cmd.to_be_bytes());
        buf.push(seq);
        buf.extend_from_slice(payload);
        buf.extend_from_slice(&checksum(&buf[1..]).to_be_bytes());
        self.usb.bulk_out(EP_CTRL_OUT, buf, CTRL_TIMEOUT).await?;
        Delay::new(Duration::from_millis(1)).await;

        let buf = self.usb.bulk_in(EP_CTRL_IN, 256, CTRL_TIMEOUT).await?;
        Delay::new(Duration::from_millis(1)).await;

        let rlen = buf.len();
        if rlen < 5 {
            return Err(error("control response too short"));
        }
        if checksum(&buf[1..rlen - 2]) != u16::from_be_bytes([buf[rlen - 2], buf[rlen - 1]]) {
            return Err(error("control response checksum mismatch"));
        }
        if buf[1] != seq {
            return Err(error("control response out of sequence"));
        }
        if buf[2] != 0 {
            return Err(error("control command failed"));
        }
        Ok(buf[3..rlen - 2].to_vec())
    }

    pub async fn read_regs(&mut self, reg: u32, len: u8) -> Result<Vec<u8>> {
        let mut payload = vec![len, reg_length(reg)];
        payload.extend_from_slice(&reg.to_be_bytes());
        let mut data = self.ctrl(CMD_REG_READ, &payload).await?;
        data.truncate(len.into());
        Ok(data)
    }

    pub async fn read_reg(&mut self, reg: u32) -> Result<u8> {
        self.read_regs(reg, 1)
            .await?
            .first()
            .copied()
            .ok_or_else(|| error("empty register read"))
    }

    pub async fn write_regs(&mut self, reg: u32, data: &[u8]) -> Result<()> {
        let mut payload = vec![data.len() as u8, reg_length(reg)];
        payload.extend_from_slice(&reg.to_be_bytes());
        payload.extend_from_slice(data);
        self.ctrl(CMD_REG_WRITE, &payload).await.map(drop)
    }

    pub async fn write_reg(&mut self, reg: u32, value: u8) -> Result<()> {
        self.write_regs(reg, &[value]).await
    }

    pub async fn write_reg_mask(&mut self, reg: u32, value: u8, mask: u8) -> Result<()> {
        let value = if mask == 0xff {
            value
        } else {
            (self.read_reg(reg).await? & !mask) | (value & mask)
        };
        self.write_reg(reg, value).await
    }

    /// I2C bus 1, 2 or 3 of the bridge.
    pub fn i2c(&mut self, bus: u8) -> I2cBus<'_, T> {
        I2cBus { it930x: self, bus }
    }

    async fn firmware_version(&mut self) -> Result<u32> {
        let data = self.ctrl(CMD_QUERYINFO, &[1]).await?;
        let bytes = data
            .get(..4)
            .ok_or_else(|| error("short firmware version"))?;
        Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
    }

    /// Wakes the bridge up, which may take a few tries.
    pub async fn raise(&mut self) -> Result<()> {
        let mut result = Ok(0);
        for _ in 0..5 {
            result = self.firmware_version().await;
            if result.is_ok() {
                break;
            }
        }
        result.map(drop)
    }

    /// Loads the firmware unless the bridge has one running.
    pub async fn load_firmware(&mut self, firmware: &[u8]) -> Result<()> {
        if self.firmware_version().await? != 0 {
            return Ok(());
        }

        self.write_reg(0xf103, I2C_SPEED).await?;

        // The firmware is a run of blocks: 0x03, two bytes, the number of
        // segments, then three bytes per segment, the last of them its
        // length, then the data of the segments.
        let mut rest = firmware;
        while !rest.is_empty() {
            if rest[0] != 0x03 || rest.len() < 4 {
                return Err(error("invalid firmware block"));
            }
            let segments = usize::from(rest[3]);
            let header = 4 + segments * 3;
            let data: usize = (0..segments)
                .map(|i| rest.get(6 + i * 3).copied().map_or(0, usize::from))
                .sum();
            let (block, next) = rest
                .split_at_checked(header + data)
                .ok_or_else(|| error("truncated firmware block"))?;
            self.ctrl(CMD_FW_SCATTER_WRITE, block).await?;
            rest = next;
        }

        self.ctrl(CMD_BOOT, &[]).await?;
        if self.firmware_version().await? == 0 {
            return Err(error("the firmware did not boot"));
        }
        Ok(())
    }

    /// Sets the bridge up once the firmware runs.
    pub async fn init_warm(&mut self, inputs: &[Input]) -> Result<()> {
        for (reg, value) in [(0x4976, 0), (0x4bfb, 0), (0x4978, 0), (0x4977, 0)] {
            self.write_reg(reg, value).await?;
        }
        // Ignore sync byte: no.
        self.write_reg(0xda1a, 0).await?;
        // DVB-T interrupt: enable.
        self.write_reg_mask(0xf41f, 0x04, 0x04).await?;
        // MPEG full speed.
        self.write_reg_mask(0xda10, 0x00, 0x01).await?;
        // DVB-T mode: enable.
        self.write_reg_mask(0xf41a, 0x01, 0x01).await?;

        self.config_stream_output().await?;

        for (reg, value) in [(0xd833, 1), (0xd830, 0), (0xd831, 1), (0xd832, 0)] {
            self.write_reg(reg, value).await?;
        }

        self.config_i2c(inputs).await?;
        self.config_stream_input(inputs).await
    }

    async fn config_stream_output(&mut self) -> Result<()> {
        self.write_reg_mask(0xda1d, 0x01, 0x01).await?;
        let result = async {
            // Disable EP4, disable its NAK, enable it again.
            self.write_reg_mask(0xdd11, 0x00, 0x20).await?;
            self.write_reg_mask(0xdd13, 0x00, 0x20).await?;
            self.write_reg_mask(0xdd11, 0x20, 0x20).await?;
            let threshold = ((XFER_SIZE / 4) as u16).to_le_bytes();
            self.write_regs(0xdd88, &threshold).await?;
            self.write_reg(0xdd0c, (self.max_bulk_size / 4) as u8)
                .await?;
            self.write_reg_mask(0xda05, 0x00, 0x01).await?;
            self.write_reg_mask(0xda06, 0x00, 0x01).await
        }
        .await;
        let r1 = self.write_reg_mask(0xda1d, 0x00, 0x01).await;
        // Reverse: no.
        let r2 = self.write_reg(0xd920, 0).await;
        result.and(r1).and(r2)
    }

    async fn config_i2c(&mut self, inputs: &[Input]) -> Result<()> {
        const REGS: [(u32, u32); 5] = [
            (0x4975, 0x4971),
            (0x4974, 0x4970),
            (0x4973, 0x496f),
            (0x4972, 0x496e),
            (0x4964, 0x4963),
        ];

        self.write_reg(0xf6a7, I2C_SPEED).await?;
        self.write_reg(0xf103, I2C_SPEED).await?;
        for input in inputs {
            let (addr_reg, bus_reg) = REGS[usize::from(input.slave)];
            self.write_reg(addr_reg, input.i2c_addr << 1).await?;
            self.write_reg(bus_reg, input.i2c_bus).await?;
        }
        Ok(())
    }

    async fn config_stream_input(&mut self, inputs: &[Input]) -> Result<()> {
        for port in 0..5u8 {
            let reg = u32::from(port);
            let Some(input) = inputs.iter().find(|i| i.port == port) else {
                self.write_reg(0xda4c + reg, 0).await?;
                continue;
            };
            if port < 2 {
                // Serial, not parallel.
                self.write_reg(0xda58 + reg, 0).await?;
            }
            // Aggregation mode: sync byte.
            self.write_reg(0xda73 + reg, 1).await?;
            self.write_reg(0xda78 + reg, input.sync_byte).await?;
            self.write_reg(0xda4c + reg, 1).await?;
        }
        Ok(())
    }

    /// Makes GPIO `gpio` (1 to 16) an enabled output.
    pub async fn gpio_output(&mut self, gpio: u8) -> Result<()> {
        const MODE_REGS: [u32; 16] = [
            0xd8b0, 0xd8b8, 0xd8b4, 0xd8c0, 0xd8bc, 0xd8c8, 0xd8c4, 0xd8d0, 0xd8cc, 0xd8d8, 0xd8d4,
            0xd8e0, 0xd8dc, 0xd8e4, 0xd8e8, 0xd8ec,
        ];

        let bit = 1 << (gpio - 1);
        let reg = MODE_REGS[usize::from(gpio - 1)];
        if self.gpio_out & bit == 0 {
            self.write_reg(reg, 1).await?;
            self.gpio_out |= bit;
        }
        if self.gpio_on & bit == 0 {
            self.write_reg(reg + 1, 1).await?;
            self.gpio_on |= bit;
        }
        Ok(())
    }

    pub async fn write_gpio(&mut self, gpio: u8, high: bool) -> Result<()> {
        const OUT_REGS: [u32; 16] = [
            0xd8af, 0xd8b7, 0xd8b3, 0xd8bf, 0xd8bb, 0xd8c7, 0xd8c3, 0xd8cf, 0xd8cb, 0xd8d7, 0xd8d3,
            0xd8df, 0xd8db, 0xd8e3, 0xd8e7, 0xd8eb,
        ];

        if self.gpio_out & (1 << (gpio - 1)) == 0 {
            return Err(error("GPIO is not an output"));
        }
        self.write_reg(OUT_REGS[usize::from(gpio - 1)], high.into())
            .await
    }

    /// Drops what the bridge has buffered, before the stream starts.
    pub async fn purge_psb(&mut self) -> Result<()> {
        self.write_reg_mask(0xda1d, 0x01, 0x01).await?;
        let mut queue = self.usb.bulk_in_queue(EP_STREAM)?;
        queue.submit(1024);
        let result = with_timeout(queue.next_complete(), PSB_PURGE_TIMEOUT).await;
        drop(queue);
        // px4_drv goes on whether this fails or not.
        let _ = self.write_reg_mask(0xda1d, 0x00, 0x01).await;
        result??;
        Ok(())
    }
}

pub struct I2cBus<'a, T> {
    it930x: &'a mut It930x<T>,
    bus: u8,
}

impl<T: UsbTransport> I2c for I2cBus<'_, T> {
    async fn write(&mut self, addr: u8, data: &[u8]) -> Result<()> {
        let mut payload = vec![data.len() as u8, self.bus, addr << 1];
        payload.extend_from_slice(data);
        self.it930x.ctrl(CMD_I2C_WRITE, &payload).await.map(drop)
    }

    async fn write_read(&mut self, addr: u8, data: &[u8], buf: &mut [u8]) -> Result<()> {
        self.write(addr, data).await?;
        let payload = [buf.len() as u8, self.bus, addr << 1];
        let read = self.it930x.ctrl(CMD_I2C_READ, &payload).await?;
        if read.len() < buf.len() {
            return Err(error("short I2C read"));
        }
        buf.copy_from_slice(&read[..buf.len()]);
        Ok(())
    }
}

fn reg_length(reg: u32) -> u8 {
    match reg {
        0x0100_0000.. => 4,
        0x0001_0000.. => 3,
        0x0000_0100.. => 2,
        _ => 1,
    }
}

/// The one's complement of the sum of big-endian 16-bit words.
fn checksum(buf: &[u8]) -> u16 {
    let sum = buf.chunks(2).fold(0u16, |sum, word| {
        sum.wrapping_add(u16::from_be_bytes([word[0], *word.get(1).unwrap_or(&0)]))
    });
    !sum
}
