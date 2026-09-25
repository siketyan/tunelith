use std::error::Error;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tunelith::{AcquireOptions, Client};
use tunelith_core::{Driver, Polarization, Registry, StreamId, System, TuneParams, Tuner};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Opens the devices directly rather than through tunelithd.
    #[arg(long, global = true)]
    direct: bool,
    /// The socket tunelithd listens on.
    #[arg(long, global = true, env = "TUNELITH_SOCKET", default_value = tunelith::DEFAULT_SOCKET)]
    socket: PathBuf,
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

impl TuneArgs {
    fn params(&self) -> TuneParams {
        TuneParams {
            system: match self.system {
                SystemArg::IsdbT => System::IsdbT,
                SystemArg::IsdbS => System::IsdbS,
                SystemArg::IsdbS3 => System::IsdbS3,
            },
            frequency_khz: self.freq,
            stream_id: self.stream_id.map(StreamId),
            polarization: self.polarization.map(|p| match p {
                PolarizationArg::Right => Polarization::Right,
                PolarizationArg::Left => Polarization::Left,
            }),
        }
    }

    fn deadline(&self) -> Option<Instant> {
        self.duration
            .map(|secs| Instant::now() + Duration::from_secs(secs))
    }
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

type Result<T, E = Box<dyn Error>> = std::result::Result<T, E>;

async fn connect(cli: &Cli) -> Result<Client> {
    Client::connect(&cli.socket).await.map_err(|e| {
        format!(
            "cannot reach tunelithd at {}: {e} (start it, or pass --direct)",
            cli.socket.display()
        )
        .into()
    })
}

async fn list(cli: &Cli) -> Result<()> {
    for device in connect(cli).await?.list().await? {
        println!("{} ({})", device.info.name, device.info.id);
        for tuner in device.tuners {
            let busy = if tuner.busy { " busy" } else { "" };
            println!("  {} {:?}{busy}", tuner.info.id, tuner.info.systems);
        }
    }
    Ok(())
}

async fn tune(cli: &Cli, args: &TuneArgs) -> Result<()> {
    let options = AcquireOptions {
        tuner: args.tuner.clone(),
        lnb: args.lnb,
    };
    let mut stream = connect(cli).await?.acquire(args.params(), options).await?;
    let signal = stream.signal().await?;
    match signal.cnr_db {
        Some(cnr) => eprintln!("{}: locked, C/N {cnr:.2} dB", stream.tuner()),
        None => eprintln!("{}: locked", stream.tuner()),
    }
    eprintln!("streaming {:?}", stream.format());

    let deadline = args.deadline();
    let mut stdout = tokio::io::stdout();
    let mut buf = vec![0; 188 * 1024];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        match stdout.write_all(&buf[..n]).await {
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => break,
            result => result?,
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
    }
    stdout.flush().await?;

    if stream.dropped_bytes() > 0 {
        eprintln!(
            "lost {} bytes for reading too slowly",
            stream.dropped_bytes()
        );
    }
    Ok(())
}

fn registry() -> Registry {
    let drivers: Vec<Box<dyn Driver>> = vec![
        Box::new(tunelith_driver_px4::driver()),
        #[cfg(target_os = "linux")]
        Box::new(tunelith_driver_pt4k::driver()),
        #[cfg(target_os = "linux")]
        Box::new(tunelith_core::dvb::DvbDriver::generic()),
    ];
    Registry::new(drivers)
}

async fn list_direct() -> Result<()> {
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
async fn open_tuner(id: Option<&str>, system: System) -> Result<Box<dyn Tuner>> {
    let registry = registry();
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

async fn tune_direct(args: &TuneArgs) -> Result<()> {
    let params = args.params();
    params.validate()?;

    let mut tuner = open_tuner(args.tuner.as_deref(), params.system).await?;
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

    let deadline = args.deadline();
    let result: Result<()> = async {
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
    .await;

    // Waits for the tuner to go back, which the end of the process would cut.
    drop(stream);
    tuner.close().await;
    result
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match (&cli.command, cli.direct) {
        (Command::List, false) => list(&cli).await,
        (Command::List, true) => list_direct().await,
        (Command::Tune(args), false) => tune(&cli, args).await,
        (Command::Tune(args), true) => tune_direct(args).await,
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
