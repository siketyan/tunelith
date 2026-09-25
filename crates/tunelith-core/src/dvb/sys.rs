//! The parts of the Linux DVB API in use, written after the uapi headers
//! `linux/dvb/frontend.h` and `linux/dvb/dmx.h`.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

pub const DTV_TUNE: u32 = 1;
pub const DTV_CLEAR: u32 = 2;
pub const DTV_FREQUENCY: u32 = 3;
pub const DTV_BANDWIDTH_HZ: u32 = 5;
pub const DTV_DELIVERY_SYSTEM: u32 = 17;
pub const DTV_GUARD_INTERVAL: u32 = 38;
pub const DTV_TRANSMISSION_MODE: u32 = 39;
pub const DTV_ISDBT_LAYER_ENABLED: u32 = 41;
pub const DTV_STREAM_ID: u32 = 42;
pub const DTV_ENUM_DELSYS: u32 = 44;
pub const DTV_STAT_CNR: u32 = 63;

pub const SYS_ISDBT: u32 = 8;
pub const SYS_ISDBS: u32 = 9;
pub const TRANSMISSION_MODE_AUTO: u32 = 2;
pub const GUARD_INTERVAL_AUTO: u32 = 4;

/// Feeds the LNB; the ISDB-S frontends give it the 15 V it takes in Japan.
pub const SEC_VOLTAGE_18: u32 = 1;
pub const SEC_VOLTAGE_OFF: u32 = 2;

pub const FE_HAS_LOCK: u32 = 0x10;
pub const FE_SCALE_DECIBEL: u8 = 1;

// ponytail: request numbers as on 64-bit targets, whose `struct dtv_properties`
// is 16 bytes; a 32-bit target needs its own.
const FE_GET_INFO: u64 = 0x80a8_6f3d;
const FE_READ_STATUS: u64 = 0x8004_6f45;
const FE_SET_PROPERTY: u64 = 0x4010_6f52;
const FE_GET_PROPERTY: u64 = 0x8010_6f53;
const FE_SET_VOLTAGE: u64 = 0x6f43;
const DMX_SET_PES_FILTER: u64 = 0x4014_6f2c;
const DMX_SET_BUFFER_SIZE: u64 = 0x6f2d;

/// `struct dtv_property`, with its union as bytes.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct DtvProperty {
    pub cmd: u32,
    reserved: [u32; 3],
    pub u: [u8; 56],
    pub result: i32,
}

impl DtvProperty {
    pub fn new(cmd: u32, data: u32) -> Self {
        let mut u = [0; 56];
        u[..4].copy_from_slice(&data.to_ne_bytes());
        Self {
            cmd,
            reserved: [0; 3],
            u,
            result: 0,
        }
    }

    /// `u.buffer.data[..u.buffer.len]`.
    pub fn buffer(&self) -> &[u8] {
        let len = u32::from_ne_bytes(self.u[32..36].try_into().unwrap()) as usize;
        &self.u[..len.min(32)]
    }

    /// The scale and value of `u.st.stat[0]`, if there is one.
    pub fn stat(&self) -> Option<(u8, i64)> {
        (self.u[0] > 0).then(|| {
            (
                self.u[1],
                i64::from_ne_bytes(self.u[2..10].try_into().unwrap()),
            )
        })
    }
}

#[repr(C)]
struct DtvProperties {
    num: u32,
    props: *mut DtvProperty,
}

#[repr(C)]
struct DmxPesFilterParams {
    pid: u16,
    input: u32,
    output: u32,
    pes_type: u32,
    flags: u32,
}

fn ioctl(file: &File, request: u64, arg: usize) -> io::Result<()> {
    // SAFETY: each caller passes the argument its request takes.
    if unsafe { libc::ioctl(file.as_raw_fd(), request as _, arg) } < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

pub fn set_properties(fe: &File, props: &mut [DtvProperty]) -> io::Result<()> {
    let mut arg = DtvProperties {
        num: props.len() as u32,
        props: props.as_mut_ptr(),
    };
    ioctl(fe, FE_SET_PROPERTY, &raw mut arg as usize)
}

pub fn get_property(fe: &File, cmd: u32) -> io::Result<DtvProperty> {
    let mut prop = DtvProperty::new(cmd, 0);
    let mut arg = DtvProperties {
        num: 1,
        props: &raw mut prop,
    };
    ioctl(fe, FE_GET_PROPERTY, &raw mut arg as usize)?;
    Ok(prop)
}

/// The name in `struct dvb_frontend_info`.
pub fn frontend_name(fe: &File) -> io::Result<String> {
    let mut info = [0u8; 168];
    ioctl(fe, FE_GET_INFO, info.as_mut_ptr() as usize)?;
    let name = &info[..128];
    let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    Ok(String::from_utf8_lossy(&name[..len]).into_owned())
}

pub fn read_status(fe: &File) -> io::Result<u32> {
    let mut status = 0u32;
    ioctl(fe, FE_READ_STATUS, &raw mut status as usize)?;
    Ok(status)
}

pub fn set_voltage(fe: &File, voltage: u32) -> io::Result<()> {
    ioctl(fe, FE_SET_VOLTAGE, voltage as usize)
}

/// Passes every packet from the frontend on to the DVR device.
pub fn tap_all_pids(demux: &File) -> io::Result<()> {
    let mut params = DmxPesFilterParams {
        pid: 0x2000,
        input: 0,     // DMX_IN_FRONTEND
        output: 2,    // DMX_OUT_TS_TAP
        pes_type: 20, // DMX_PES_OTHER
        flags: 4,     // DMX_IMMEDIATE_START
    };
    ioctl(demux, DMX_SET_PES_FILTER, &raw mut params as usize)
}

pub fn set_buffer_size(file: &File, size: usize) -> io::Result<()> {
    ioctl(file, DMX_SET_BUFFER_SIZE, size)
}

#[cfg(test)]
mod tests {
    #[test]
    fn layouts() {
        assert_eq!(size_of::<super::DtvProperty>(), 76);
        assert_eq!(size_of::<super::DmxPesFilterParams>(), 20);
    }
}
