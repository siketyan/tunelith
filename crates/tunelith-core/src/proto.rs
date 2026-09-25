//! The protocol between tunelithd and its clients, as
//! `proto/tunelith/v1/tunelith.proto` defines it: frames of a big-endian u32
//! length and an [`Envelope`].

use std::io;

use futures::{AsyncRead, AsyncReadExt};
use protobuf::{EnumOrUnknown, Message};

use crate::Result;

mod out {
    include!(concat!(env!("OUT_DIR"), "/proto/mod.rs"));
}

pub use generated::*;
use out::tunelith as generated;

/// The version of the protocol, exchanged in [`Hello`].
pub const VERSION: u32 = 1;

/// Where tunelithd listens unless told otherwise: a Unix domain socket, or a
/// named pipe on Windows.
#[cfg(not(windows))]
pub const DEFAULT_SOCKET: &str = "/run/tunelith/tunelithd.sock";
#[cfg(windows)]
pub const DEFAULT_SOCKET: &str = r"\\.\pipe\tunelith";

/// The largest frame taken, keeping a peer from making us allocate at will.
pub const MAX_FRAME: usize = 1 << 20;

/// The frame of `envelope`.
pub fn encode(envelope: &Envelope) -> Vec<u8> {
    let body = envelope
        .write_to_bytes()
        .expect("an envelope always serialises");
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    frame
}

/// Reads a frame, or `None` if the connection ends before one starts.
pub async fn read(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<Envelope>> {
    // Only an end before the first byte is a clean one; within the length,
    // it is a truncated frame.
    let mut len = [0; 4];
    if reader.read(&mut len[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut len[1..]).await?;

    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body).await?;
    Envelope::parse_from_bytes(&body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// An envelope of `id` around `body`.
pub fn envelope(id: u64, body: envelope::Body) -> Envelope {
    Envelope {
        id,
        body: Some(body),
        ..Default::default()
    }
}

impl From<crate::System> for EnumOrUnknown<generated::System> {
    fn from(system: crate::System) -> Self {
        EnumOrUnknown::new(match system {
            crate::System::IsdbT => generated::System::SYSTEM_ISDB_T,
            crate::System::IsdbS => generated::System::SYSTEM_ISDB_S,
            crate::System::IsdbS3 => generated::System::SYSTEM_ISDB_S3,
        })
    }
}

/// The system `system` stands for, if any.
pub fn system(system: EnumOrUnknown<generated::System>) -> Option<crate::System> {
    match system.enum_value() {
        Ok(generated::System::SYSTEM_ISDB_T) => Some(crate::System::IsdbT),
        Ok(generated::System::SYSTEM_ISDB_S) => Some(crate::System::IsdbS),
        Ok(generated::System::SYSTEM_ISDB_S3) => Some(crate::System::IsdbS3),
        _ => None,
    }
}

impl From<crate::StreamFormat> for EnumOrUnknown<generated::StreamFormat> {
    fn from(format: crate::StreamFormat) -> Self {
        EnumOrUnknown::new(match format {
            crate::StreamFormat::Ts => generated::StreamFormat::STREAM_FORMAT_TS,
            crate::StreamFormat::Tlv => generated::StreamFormat::STREAM_FORMAT_TLV,
        })
    }
}

pub fn stream_format(
    format: EnumOrUnknown<generated::StreamFormat>,
) -> Option<crate::StreamFormat> {
    match format.enum_value() {
        Ok(generated::StreamFormat::STREAM_FORMAT_TS) => Some(crate::StreamFormat::Ts),
        Ok(generated::StreamFormat::STREAM_FORMAT_TLV) => Some(crate::StreamFormat::Tlv),
        _ => None,
    }
}

impl From<crate::TuneParams> for generated::TuneParams {
    fn from(params: crate::TuneParams) -> Self {
        Self {
            system: params.system.into(),
            frequency_khz: params.frequency_khz,
            stream_id: params.stream_id.map(|id| id.0.into()),
            polarization: EnumOrUnknown::new(match params.polarization {
                None => generated::Polarization::POLARIZATION_UNSPECIFIED,
                Some(crate::Polarization::Right) => generated::Polarization::POLARIZATION_RIGHT,
                Some(crate::Polarization::Left) => generated::Polarization::POLARIZATION_LEFT,
            }),
            ..Default::default()
        }
    }
}

impl TryFrom<&generated::TuneParams> for crate::TuneParams {
    type Error = crate::Error;

    fn try_from(params: &generated::TuneParams) -> Result<Self> {
        let stream_id = params
            .stream_id
            .map(|id| u16::try_from(id).map(crate::StreamId))
            .transpose()
            .map_err(|_| crate::Error::InvalidParams("the stream id is out of range"))?;
        Ok(Self {
            system: system(params.system).ok_or(crate::Error::InvalidParams("unknown system"))?,
            frequency_khz: params.frequency_khz,
            stream_id,
            polarization: match params.polarization.enum_value() {
                Ok(generated::Polarization::POLARIZATION_RIGHT) => Some(crate::Polarization::Right),
                Ok(generated::Polarization::POLARIZATION_LEFT) => Some(crate::Polarization::Left),
                Ok(generated::Polarization::POLARIZATION_UNSPECIFIED) => None,
                Err(_) => return Err(crate::Error::InvalidParams("unknown polarization")),
            },
        })
    }
}

impl From<crate::Signal> for SignalResponse {
    fn from(signal: crate::Signal) -> Self {
        Self {
            locked: signal.locked,
            cnr_db: signal.cnr_db,
            ..Default::default()
        }
    }
}

impl From<&SignalResponse> for crate::Signal {
    fn from(signal: &SignalResponse) -> Self {
        Self {
            locked: signal.locked,
            cnr_db: signal.cnr_db,
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;

    use super::*;
    use crate::{Polarization, StreamId, System};

    #[test]
    fn frames_round_trip() {
        let params = crate::TuneParams {
            system: System::IsdbS,
            frequency_khz: 11_727_480,
            stream_id: Some(StreamId(0x4010)),
            polarization: Some(Polarization::Left),
        };
        let request = AcquireRequest {
            params: Some(params.into()).into(),
            ..Default::default()
        };
        let mut frames = encode(&envelope(7, envelope::Body::AcquireRequest(request)));
        frames.extend(encode(&envelope(
            8,
            envelope::Body::ListRequest(Default::default()),
        )));

        let mut reader = frames.as_slice();
        let first = block_on(read(&mut reader)).unwrap().unwrap();
        assert_eq!(first.id, 7);
        let Some(envelope::Body::AcquireRequest(request)) = first.body else {
            panic!("not an acquire request");
        };
        assert_eq!(
            crate::TuneParams::try_from(&*request.params).unwrap(),
            params
        );
        assert_eq!(block_on(read(&mut reader)).unwrap().unwrap().id, 8);
        assert!(block_on(read(&mut reader)).unwrap().is_none());
    }

    #[test]
    fn rejects_truncated_frames() {
        let frame = encode(&envelope(
            1,
            envelope::Body::ListRequest(Default::default()),
        ));
        for len in [2, frame.len() - 1] {
            assert!(block_on(read(&mut &frame[..len])).is_err());
        }
    }

    #[test]
    fn rejects_large_frames() {
        let frame = ((MAX_FRAME + 1) as u32).to_be_bytes();
        assert!(block_on(read(&mut frame.as_slice())).is_err());
    }
}
