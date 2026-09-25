# tunelith

The client of tunelithd, the daemon of [Tunelith](https://github.com/siketyan/tunelith)
that shares the tuners for the Japanese digital broadcasting systems (ISDB-T,
ISDB-S and ISDB-S3) among programs.

A program connects a `Client` to tunelithd and acquires a `Stream` of what to
receive; tunelithd picks a free tuner for it, or shares the tuner another
program is already receiving the same on. The `Stream` is an `AsyncRead` of
the bytes the tuner gives out, and releases the tuner when dropped.

Tunelith tunes by broadcasting system, frequency and stream id, and keeps no
channel list: channel lists, scanning, EPG and descrambling belong to the
program using this crate.

## Usage

```toml
[dependencies]
tunelith = "0.1"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "io-util", "fs"] }
```

The client needs a Tokio runtime, and tunelithd running (see the
[README of Tunelith](https://github.com/siketyan/tunelith#tunelithd)).

```rust,no_run
use tunelith::{AcquireOptions, Client, DEFAULT_SOCKET, StreamId, System, TuneParams};

#[tokio::main]
async fn main() -> tunelith::Result<()> {
    let client = Client::connect(DEFAULT_SOCKET).await?;

    // BS-1 (11727.48 MHz downlink), TSID 0x4010.
    let params = TuneParams {
        system: System::IsdbS,
        frequency_khz: 11_727_480,
        stream_id: Some(StreamId(0x4010)),
        polarization: None,
    };
    let mut stream = client.acquire(params, AcquireOptions::default()).await?;
    println!("{} gives out {:?}", stream.tuner(), stream.format());

    let mut file = tokio::fs::File::create("out.ts").await?;
    tokio::io::copy(&mut stream, &mut file).await?;
    Ok(())
}
```

### What to tune to

| System | `frequency_khz` | `stream_id` | Stream format |
|---|---|---|---|
| `System::IsdbT` | The frequency on air, e.g. `521_143` | `None` | MPEG-2 TS |
| `System::IsdbS` | The downlink frequency before the LNB, e.g. `11_727_480` | The TSID | MPEG-2 TS |
| `System::IsdbS3` | The downlink frequency before the LNB, e.g. `12_034_360` | The TLV stream id | TLV |

A `StreamId` is never a relative TS number. A left-hand circular broadcast
(some of ISDB-S3) takes `polarization: Some(Polarization::Left)`.

### Reading the stream

- Read steadily: tunelithd drops the bytes a client is too slow to take rather
  than holding up the others sharing the tuner. `Stream::dropped_bytes` tells
  how many were lost.
- The read returns 0 bytes when the stream ends, and fails if it ended on an
  error (the device went away, tunelithd stopped, …).
- `Stream::signal` reports the lock and the C/N of the tuner.

## Examples

- [`list`](examples/list.rs): lists the devices and their tuners.
- [`record`](examples/record.rs): records a stream to a file.

```shell
cargo run -p tunelith --example list
cargo run -p tunelith --example record -- isdb-t 521143 out.ts
```

## License

MIT OR Apache-2.0.
