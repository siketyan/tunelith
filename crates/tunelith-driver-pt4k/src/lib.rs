//! PT4K (TBS6812), which receives ISDB-T, ISDB-S and ISDB-S3.
//!
//! On Linux the out-of-tree DVB driver of TBS drives it. The kernel lists
//! ISDB-S alone among its systems; ISDB-S3 goes through ISDB-S, the driver
//! telling the two apart by `DTV_STREAM_ID`: under 8 it is a relative TS
//! number, with 0xB or 0xC in the top 4 of the lower 16 bits a TLV stream id,
//! and otherwise a TSID. [`tunelith_core::StreamId`] is never a relative TS
//! number, so it passes as is.

#![cfg(target_os = "linux")]

use tunelith_core::System;
use tunelith_core::dvb::{DvbDevice, DvbDriver, Quirks};

pub struct Pt4k;

impl Quirks for Pt4k {
    fn claims(&self, device: &DvbDevice) -> bool {
        // The bridge is TBS's own, shared by its cards; the subsystem vendor
        // tells the model.
        device.attr("vendor") == Some(0x544d) && device.attr("subsystem_vendor") == Some(0x6812)
    }

    fn extra_systems(&self) -> &[System] {
        &[System::IsdbS3]
    }
}

pub fn driver() -> DvbDriver<Pt4k> {
    DvbDriver::new(Pt4k)
}
