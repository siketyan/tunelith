//! The configuration, a TOML file beside the library of the same name: where
//! tunelithd is, and the tuning spaces and their channels, which Tunelith has
//! no list of.

use std::error::Error;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tunelith::{Polarization, StreamId, System, TuneParams};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    socket: Option<PathBuf>,
    #[serde(default)]
    lnb: bool,
    #[serde(default)]
    space: Vec<SpaceEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpaceEntry {
    name: String,
    system: SystemName,
    #[serde(default)]
    channel: Vec<ChannelEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChannelEntry {
    name: String,
    /// In kHz; for a satellite, before the LNB converts it.
    frequency: u32,
    stream_id: Option<u16>,
    polarization: Option<PolarizationName>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum SystemName {
    IsdbT,
    IsdbS,
    IsdbS3,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum PolarizationName {
    Right,
    Left,
}

pub struct Config {
    /// The socket of tunelithd; that of the user's or the system's if `None`.
    pub socket: Option<PathBuf>,
    /// Powers the LNB of the antenna.
    pub lnb: bool,
    pub spaces: Vec<Space>,
}

/// The names are NUL-terminated UTF-16, as the host takes them.
pub struct Space {
    pub name: Vec<u16>,
    pub channels: Vec<Channel>,
}

pub struct Channel {
    pub name: Vec<u16>,
    pub params: TuneParams,
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, Box<dyn Error>> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()).into())
    }

    pub fn parse(text: &str) -> Result<Self, Box<dyn Error>> {
        let file: File = toml::from_str(text)?;
        let mut spaces = Vec::with_capacity(file.space.len());
        for space in file.space {
            let system = match space.system {
                SystemName::IsdbT => System::IsdbT,
                SystemName::IsdbS => System::IsdbS,
                SystemName::IsdbS3 => System::IsdbS3,
            };
            let mut channels = Vec::with_capacity(space.channel.len());
            for channel in space.channel {
                let params = TuneParams {
                    system,
                    frequency_khz: channel.frequency,
                    stream_id: channel.stream_id.map(StreamId),
                    polarization: channel.polarization.map(|p| match p {
                        PolarizationName::Right => Polarization::Right,
                        PolarizationName::Left => Polarization::Left,
                    }),
                };
                params
                    .validate()
                    .map_err(|e| format!("{} / {}: {e}", space.name, channel.name))?;
                channels.push(Channel {
                    name: wide(&channel.name),
                    params,
                });
            }
            spaces.push(Space {
                name: wide(&space.name),
                channels,
            });
        }
        Ok(Self {
            socket: file.socket,
            lnb: file.lnb,
            spaces,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        let config = Config::parse(
            r#"
            lnb = true

            [[space]]
            name = "UHF"
            system = "isdb-t"
            channel = [{ name = "13ch", frequency = 473143 }]

            [[space]]
            name = "BS"
            system = "isdb-s"
            channel = [{ name = "BS01/TS0", frequency = 11727480, stream_id = 0x4010 }]
            "#,
        )
        .unwrap();
        assert!(config.lnb);
        assert_eq!(config.spaces[1].name, wide("BS"));
        let bs = &config.spaces[1].channels[0];
        assert_eq!(bs.name, wide("BS01/TS0"));
        assert_eq!(bs.params.stream_id, Some(StreamId(0x4010)));

        // A satellite channel without its stream id.
        let missing = r#"
            [[space]]
            name = "BS"
            system = "isdb-s"
            channel = [{ name = "BS01/TS0", frequency = 11727480 }]
        "#;
        assert!(Config::parse(missing).is_err());
    }
}
