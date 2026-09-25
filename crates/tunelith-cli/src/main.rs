use std::error::Error;
use std::io::{self, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use futures::StreamExt;
use futures::executor::block_on;
use tunelith_core::{Polarization, Registry, StreamId, System, TuneParams, Tuner};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Lists the devices and their tuners.
    List,
    /// Tunes and writes the stream to stdout.
    Tune(TuneArgs),
}

#[derive(clap::Args)]
struct TuneArgs {
    #[arg(long, value_enum)]
    system: SystemArg,
    /// The frequency on air in kHz; for a satellite, before the LNB converts it.
    #[arg(long)]
    freq: u32,
    /// The TSID for ISDB-S, the TLV stream id for ISDB-S3 (0x-prefixed for hex).
    #[arg(long, value_parser = parse_u16)]
    stream_id: Option<u16>,
    #[arg(long, value_enum)]
    polarization: Option<PolarizationArg>,
    /// The tuner to use, as `list` shows it; any free one if omitted.
    #[arg(long)]
    tuner: Option<String>,
    /// Powers the LNB of the satellite antenna.
    #[arg(long)]
    lnb: bool,
    /// Stops after this many seconds.
    #[arg(long)]
    duration: Option<u64>,
}

#[derive(Clone, Copy, ValueEnum)]
enum SystemArg {
    IsdbT,
    IsdbS,
    IsdbS3,
}

#[derive(Clone, Copy, ValueEnum)]
enum PolarizationArg {
    Right,
    Left,
}

fn parse_u16(s: &str) -> Result<u16, String> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u16::from_str_radix(hex, 16),
        None => s.parse(),
    }
    .map_err(|e| e.to_string())
}

fn registry() -> Registry {
    Registry::new(vec![
        Box::new(tunelith_driver_px4::driver()),
        Box::new(tunelith_driver_pt4k::driver()),
        Box::new(tunelith_core::dvb::DvbDriver::generic()),
    ])
}

async fn list() -> Result<(), Box<dyn Error>> {
    let registry = registry();
    for found in registry.probe().await? {
        println!("{} ({})", found.info.name, found.info.id);
        for tuner in registry.open(&found).await?.tuners() {
            println!("  {} {:?}", tuner.id, tuner.systems);
        }
    }
    Ok(())
}

/// Opens the tuner asked for, or else the first free one receiving the system.
async fn open_tuner(
    registry: &Registry,
    id: Option<&str>,
    system: System,
) -> Result<Box<dyn Tuner>, Box<dyn Error>> {
    for found in registry.probe().await? {
        let device = registry.open(&found).await?;
        for (index, info) in device.tuners().iter().enumerate() {
            match id {
                Some(id) if id == info.id => return Ok(device.open_tuner(index).await?),
                None if info.systems.contains(&system) => match device.open_tuner(index).await {
                    Ok(tuner) => return Ok(tuner),
                    Err(tunelith_core::Error::Io(e)) if e.kind() == io::ErrorKind::ResourceBusy => {
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                },
                _ => {}
            }
        }
    }

    Err(match id {
        Some(id) => format!("no such tuner: {id}"),
        None => format!("no free tuner receives {system:?}"),
    }
    .into())
}

async fn tune(args: TuneArgs) -> Result<(), Box<dyn Error>> {
    let params = TuneParams {
        system: match args.system {
            SystemArg::IsdbT => System::IsdbT,
            SystemArg::IsdbS => System::IsdbS,
            SystemArg::IsdbS3 => System::IsdbS3,
        },
        frequency_khz: args.freq,
        stream_id: args.stream_id.map(StreamId),
        polarization: args.polarization.map(|p| match p {
            PolarizationArg::Right => Polarization::Right,
            PolarizationArg::Left => Polarization::Left,
        }),
    };
    params.validate()?;

    let mut tuner = open_tuner(&registry(), args.tuner.as_deref(), params.system).await?;
    if args.lnb {
        tuner.set_lnb(true).await?;
    }
    tuner.tune(params).await?;

    let signal = tuner.signal().await?;
    match signal.cnr_db {
        Some(cnr) => eprintln!("locked, C/N {cnr:.2} dB"),
        None => eprintln!("locked"),
    }

    let (format, mut stream) = tuner.stream().await?;
    eprintln!("streaming {format:?}");

    let deadline = args
        .duration
        .map(|secs| Instant::now() + Duration::from_secs(secs));
    let mut stdout = io::stdout().lock();
    while let Some(chunk) = stream.next().await {
        match stdout.write_all(&chunk?) {
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => break,
            result => result?,
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let result = block_on(async {
        match Cli::parse().command {
            Command::List => list().await,
            Command::Tune(args) => tune(args).await,
        }
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
