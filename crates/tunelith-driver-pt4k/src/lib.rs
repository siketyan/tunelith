//! PT4K (TBS6812), which receives ISDB-T, ISDB-S and ISDB-S3.
//!
//! On Linux the out-of-tree DVB driver of TBS drives it, and on Windows its
//! BDA driver.

#[cfg(target_os = "linux")]
mod linux {
    //! The kernel lists ISDB-S alone among the systems; ISDB-S3 goes through
    //! ISDB-S, the driver telling the two apart by `DTV_STREAM_ID`: under 8 it
    //! is a relative TS number, with 0xB or 0xC in the top 4 of the lower 16
    //! bits a TLV stream id, and otherwise a TSID. [`tunelith_core::StreamId`]
    //! is never a relative TS number, so it passes as is.

    use tunelith_core::System;
    use tunelith_core::dvb::{DvbDevice, DvbDriver, Quirks};

    pub struct Pt4k;

    impl Quirks for Pt4k {
        fn claims(&self, device: &DvbDevice) -> bool {
            // The bridge is TBS's own, shared by its cards; the subsystem
            // vendor tells the model.
            device.attr("vendor") == Some(0x544d) && device.attr("subsystem_vendor") == Some(0x6812)
        }

        fn extra_systems(&self) -> &[System] {
            &[System::IsdbS3]
        }
    }

    pub fn driver() -> DvbDriver<Pt4k> {
        DvbDriver::new(Pt4k)
    }
}

#[cfg(windows)]
mod windows {
    //! The driver shows a tuner filter for ISDB-T and one for ISDB-S and
    //! ISDB-S3. The stream to take out of a satellite transponder, TSID or TLV
    //! stream id alike, goes in a property of TBS's own on the input pin.

    use std::io;
    use std::time::Duration;

    use tunelith_core::bda::{BdaDevice, BdaDriver, Pin, Quirks};
    use tunelith_core::{StreamId, System};

    /// `KSPROPSETID_BdaTunerExtensionProperties` of TBS.
    const TBS_EXTENSION: u128 = 0xfaa8_f3e5_31d4_4e41_88ef_d9eb_716f_6ec9;
    /// The stream id to select, as a ULONG both in the request and as the
    /// value.
    const TBS_STREAM_ID: u32 = 96;
    /// A command, got rather than set: 188 bytes in the request and as many
    /// back.
    const TBS_COMMAND: u32 = 0;
    const TBS_COMMAND_SIZE: usize = 188;
    /// Where the command takes the LNB supply, as a ULONG: 1 for on.
    const TBS_COMMAND_LNB_POWER: usize = 184;

    pub struct Pt4k;

    impl Quirks for Pt4k {
        fn claims(&self, device: &BdaDevice) -> bool {
            // SUBSYS is the subsystem id, then the subsystem vendor.
            device.hardware_field("VEN") == Some(0x544d)
                && device.hardware_field("SUBSYS").map(|s| s & 0xffff) == Some(0x6812)
        }

        fn extra_systems(&self) -> &[System] {
            &[System::IsdbS3]
        }

        fn select_stream(&self, input: &Pin<'_>, _system: System, id: StreamId) -> io::Result<()> {
            let id = u32::from(id.0).to_le_bytes();
            input.set_property(TBS_EXTENSION, TBS_STREAM_ID, &id, &id)
        }

        // ponytail: 0 for off is inferred from 1 for on, which is all that
        // was seen of the command; the supply could not be measured here.
        fn set_lnb(&self, input: &Pin<'_>, on: bool) -> io::Result<()> {
            let mut command = [0; TBS_COMMAND_SIZE];
            command[TBS_COMMAND_LNB_POWER..][..4].copy_from_slice(&u32::from(on).to_le_bytes());
            let mut reply = [0; TBS_COMMAND_SIZE];
            input.get_property(TBS_EXTENSION, TBS_COMMAND, &command, &mut reply)?;
            Ok(())
        }

        /// Once it starts to receive, the tuner gives out a round of its
        /// buffers as they were, some 0.7 to 1.1 MiB from whatever it
        /// received last, then what it receives.
        fn stale_bytes(&self) -> usize {
            5 << 18
        }

        /// The strength the driver reports is the C/N in 0.001 dB.
        fn cnr_db(&self, strength: i32) -> Option<f64> {
            Some(f64::from(strength) / 1000.0)
        }

        fn lock_timeout(&self) -> Duration {
            Duration::from_secs(5)
        }
    }

    pub fn driver() -> BdaDriver<Pt4k> {
        BdaDriver::new(Pt4k)
    }
}

#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(windows)]
pub use windows::*;
