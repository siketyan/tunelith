//! Lists the devices tunelithd holds and their tuners.
//!
//! ```shell
//! cargo run -p tunelith --example list [SOCKET]
//! ```

use tunelith::{Client, DEFAULT_SOCKET};

#[tokio::main]
async fn main() -> tunelith::Result<()> {
    let socket = std::env::args().nth(1);
    let client = Client::connect(socket.as_deref().unwrap_or(DEFAULT_SOCKET)).await?;

    for device in client.list().await? {
        println!("{} ({})", device.info.name, device.info.id);
        for tuner in device.tuners {
            let busy = if tuner.busy { " busy" } else { "" };
            println!("  {} {:?}{busy}", tuner.info.id, tuner.info.systems);
        }
    }
    Ok(())
}
