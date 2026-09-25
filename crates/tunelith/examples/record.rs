//! Records a stream to a file until Ctrl-C.
//!
//! ```shell
//! cargo run -p tunelith --example record -- isdb-t 521143 out.ts
//! cargo run -p tunelith --example record -- isdb-s 11727480 0x4010 out.ts
//! cargo run -p tunelith --example record -- isdb-s3 12034360 0xB110 out.tlv
//! ```

use std::error::Error;

use tokio::io::AsyncWriteExt;
use tunelith::{AcquireOptions, Client, DEFAULT_SOCKET, StreamId, System, TuneParams};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (params, path) = match args.as_slice() {
        [system, freq, path] if system == "isdb-t" => (
            TuneParams {
                system: System::IsdbT,
                frequency_khz: freq.parse()?,
                stream_id: None,
                polarization: None,
            },
            path,
        ),
        [system, freq, id, path] => (
            TuneParams {
                system: match system.as_str() {
                    "isdb-s" => System::IsdbS,
                    "isdb-s3" => System::IsdbS3,
                    _ => return Err(format!("unknown system: {system}").into()),
                },
                frequency_khz: freq.parse()?,
                stream_id: Some(StreamId(u16::from_str_radix(
                    id.trim_start_matches("0x"),
                    16,
                )?)),
                polarization: None,
            },
            path,
        ),
        _ => return Err("usage: record SYSTEM FREQ_KHZ [STREAM_ID] PATH".into()),
    };

    let client = Client::connect(DEFAULT_SOCKET).await?;
    let mut stream = client.acquire(params, AcquireOptions::default()).await?;
    let signal = stream.signal().await?;
    eprintln!(
        "{}: locked {}, C/N {:?} dB, {:?}",
        stream.tuner(),
        signal.locked,
        signal.cnr_db,
        stream.format(),
    );

    let mut file = tokio::fs::File::create(path).await?;
    tokio::select! {
        result = tokio::io::copy(&mut stream, &mut file) => { result?; }
        _ = tokio::signal::ctrl_c() => {}
    }
    file.flush().await?;

    if stream.dropped_bytes() > 0 {
        eprintln!(
            "lost {} bytes for reading too slowly",
            stream.dropped_bytes()
        );
    }
    Ok(())
}
