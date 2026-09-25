use crate::{Error, Result};

/// A broadcasting system.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum System {
    /// Terrestrial digital broadcasting.
    IsdbT,
    /// BS / CS110 digital broadcasting (2K).
    IsdbS,
    /// Advanced BS / CS110 digital broadcasting (4K/8K).
    IsdbS3,
}

impl System {
    /// Whether the system is received through a satellite dish and an LNB.
    pub fn is_satellite(self) -> bool {
        !matches!(self, Self::IsdbT)
    }
}

/// The stream to take out of a satellite transponder: the TSID for ISDB-S, the
/// TLV stream id for ISDB-S3. Which of the two it is follows from the
/// [`System`], never from the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StreamId(pub u16);

/// The polarization of a satellite signal. The 2K broadcasts are all
/// right-hand circular; some 4K/8K ones are left-hand.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Polarization {
    #[default]
    Right,
    Left,
}

/// What to tune to. There is no channel number: a channel list is the
/// business of the layer above.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TuneParams {
    pub system: System,
    /// The frequency on air in kHz. For a satellite this is the downlink
    /// frequency before the LNB converts it, 11_727_480 for BS-1 for instance.
    pub frequency_khz: u32,
    /// Required for a satellite, and absent for ISDB-T, which carries one
    /// stream per frequency.
    pub stream_id: Option<StreamId>,
    /// The polarization of a satellite signal; `None` is right-hand.
    pub polarization: Option<Polarization>,
}

/// The local oscillator frequencies of the Japanese dual circular LNB.
const LO_RIGHT_KHZ: u32 = 10_678_000;
const LO_LEFT_KHZ: u32 = 9_505_000;

impl TuneParams {
    /// Checks that a satellite system has a stream id and ISDB-T has none.
    pub fn validate(&self) -> Result<()> {
        match (self.system.is_satellite(), self.stream_id) {
            (true, None) => Err(Error::InvalidParams("a satellite stream needs a stream id")),
            (false, Some(_)) => Err(Error::InvalidParams("ISDB-T takes no stream id")),
            _ => Ok(()),
        }
    }

    /// The intermediate frequency the LNB hands the tuner, in kHz.
    pub fn if_frequency_khz(&self) -> Result<u32> {
        let lo = match self.polarization.unwrap_or_default() {
            Polarization::Right => LO_RIGHT_KHZ,
            Polarization::Left => LO_LEFT_KHZ,
        };

        self.frequency_khz
            .checked_sub(lo)
            .ok_or(Error::InvalidParams("the frequency is below the LNB's"))
    }

    /// The same, with the polarization that goes without saying said: right
    /// for a satellite, none for ISDB-T. Equal once normalised, two tune to the
    /// same.
    pub fn normalized(self) -> Self {
        Self {
            polarization: self
                .system
                .is_satellite()
                .then(|| self.polarization.unwrap_or_default()),
            ..self
        }
    }

    /// The format [`Tuner::stream`](crate::Tuner::stream) gives out for this
    /// system.
    pub fn stream_format(&self) -> StreamFormat {
        match self.system {
            System::IsdbS3 => StreamFormat::Tlv,
            _ => StreamFormat::Ts,
        }
    }
}

/// The format of the bytes a tuner gives out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StreamFormat {
    /// MPEG-2 TS in 188-byte packets (ISDB-T and ISDB-S).
    Ts,
    /// TLV packets carrying MMT (ISDB-S3).
    Tlv,
}

/// The signal a tuner receives.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Signal {
    /// Whether the tuner is locked on the signal.
    pub locked: bool,
    /// The carrier-to-noise ratio in dB, if the tuner reports one.
    pub cnr_db: Option<f64>,
}

/// A device, holding one or more tuners.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DeviceInfo {
    /// Stable across reboots: a serial number, or the path of the device on
    /// its bus when it has none.
    pub id: String,
    /// The model of the device, for people to read.
    pub name: String,
}

/// A tuner of a device.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TunerInfo {
    /// The id of the device, then `#` and the index of the tuner.
    pub id: String,
    /// The systems the tuner receives.
    pub systems: Vec<System>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn satellite(frequency_khz: u32, polarization: Option<Polarization>) -> TuneParams {
        TuneParams {
            system: System::IsdbS,
            frequency_khz,
            stream_id: Some(StreamId(0x4010)),
            polarization,
        }
    }

    #[test]
    fn if_frequency() {
        assert_eq!(
            satellite(11_996_000, None).if_frequency_khz().unwrap(),
            1_318_000
        );
        let left = satellite(11_996_480, Some(Polarization::Left));
        assert_eq!(left.if_frequency_khz().unwrap(), 2_491_480);
        assert!(satellite(1_318_000, None).if_frequency_khz().is_err());
    }

    #[test]
    fn normalized() {
        let right = satellite(11_996_000, Some(Polarization::Right));
        assert_eq!(satellite(11_996_000, None).normalized(), right.normalized());
        assert_ne!(
            right.normalized(),
            satellite(11_996_000, Some(Polarization::Left)).normalized()
        );
        let mut terrestrial = right;
        terrestrial.system = System::IsdbT;
        terrestrial.stream_id = None;
        assert_eq!(terrestrial.normalized().polarization, None);
    }

    #[test]
    fn validate() {
        assert!(satellite(11_996_000, None).validate().is_ok());
        let mut params = satellite(11_996_000, None);
        params.stream_id = None;
        assert!(params.validate().is_err());
        params.system = System::IsdbT;
        assert!(params.validate().is_ok());
        params.stream_id = Some(StreamId(0));
        assert!(params.validate().is_err());
    }
}
