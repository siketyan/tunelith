// SPDX-License-Identifier: GPL-2.0-only
//! The PLEX PX4/PX-MLT series and their kin, driven in user space over USB:
//! a port of [px4_drv](https://github.com/tsukumijima/px4_drv),
//! Copyright (c) 2018-2021 nns779.
//!
//! The kernel module of px4_drv, if loaded, lets go of the device when it is
//! opened here.

mod board;
mod cnr;
mod cxd2856er;
mod cxd2858er;
mod it930x;
mod px4;
mod pxmlt;
mod r850;
mod rt710;
mod single;
mod stream;
mod tc90522;

use std::io;
use std::path::PathBuf;

use futures::FutureExt;
use futures::future::BoxFuture;
use tunelith_core::usb::{UsbDeviceInfo, list_devices};
use tunelith_core::{Device, DeviceInfo, Driver, Error, Result};

const VENDOR_ID: u16 = 0x0511;
const FIRMWARE: &str = "it930x-firmware.bin";

enum Kind {
    PxMlt(pxmlt::Layout),
    /// A PX4/PX5 board; `quad` for the Q models, two boards on one card.
    Px4 {
        quad: bool,
    },
    Single(single::Model),
}

struct Model {
    product_id: u16,
    name: &'static str,
    kind: Kind,
}

const MLT5PE: pxmlt::Layout = &[
    (0x65, 3, 0),
    (0x6c, 1, 1),
    (0x64, 1, 2),
    (0x6c, 3, 3),
    (0x64, 3, 4),
];

const fn model(product_id: u16, name: &'static str, kind: Kind) -> Model {
    Model {
        product_id,
        name,
        kind,
    }
}

const MODELS: &[Model] = &[
    model(0x083f, "PLEX PX-W3U4", Kind::Px4 { quad: false }),
    model(0x023f, "PLEX PX-W3PE4", Kind::Px4 { quad: false }),
    model(0x073f, "PLEX PX-W3PE5", Kind::Px4 { quad: false }),
    model(0x084a, "PLEX PX-Q3U4", Kind::Px4 { quad: true }),
    model(0x024a, "PLEX PX-Q3PE4", Kind::Px4 { quad: true }),
    model(0x074a, "PLEX PX-Q3PE5", Kind::Px4 { quad: true }),
    model(
        0x084e,
        "PLEX PX-MLT5U",
        Kind::PxMlt(&[
            (0x65, 3, 4),
            (0x6c, 1, 3),
            (0x64, 1, 1),
            (0x6c, 3, 2),
            (0x64, 3, 0),
        ]),
    ),
    model(0x024e, "PLEX PX-MLT5PE", Kind::PxMlt(MLT5PE)),
    // A PX-MLT5PE but for the product id.
    model(0x924e, "e-better DTV02A-5TS-P", Kind::PxMlt(MLT5PE)),
    model(
        0x0252,
        "PLEX PX-MLT8PE (3 tuners)",
        Kind::PxMlt(&[(0x65, 3, 0), (0x6c, 3, 3), (0x64, 3, 4)]),
    ),
    model(
        0x0253,
        "PLEX PX-MLT8PE (5 tuners)",
        Kind::PxMlt(&[
            (0x65, 1, 0),
            (0x64, 1, 1),
            (0x6c, 1, 2),
            (0x6c, 3, 3),
            (0x64, 3, 4),
        ]),
    ),
    model(
        0x0254,
        "Digibest ISDB6014 V2.0 (4TS)",
        Kind::PxMlt(&[(0x65, 3, 0), (0x6c, 1, 1), (0x64, 1, 2), (0x64, 3, 4)]),
    ),
    model(0x0854, "PLEX PX-M1UR", Kind::Single(single::Model::M1ur)),
    model(0x0855, "PLEX PX-S1UR", Kind::Single(single::Model::S1ur)),
    model(
        0x004b,
        "Digibest ISDB2056",
        Kind::Single(single::Model::Isdb2056),
    ),
    model(
        0x084b,
        "Digibest ISDB2056N",
        Kind::Single(single::Model::Isdb2056n),
    ),
    model(
        0x0052,
        "Digibest ISDBT2071",
        Kind::Single(single::Model::Isdbt2071),
    ),
];

fn error(message: &str) -> Error {
    io::Error::other(message).into()
}

/// Runs `future` apart from the caller: on a thread of its own, or in the
/// browser as a task of its event loop.
fn spawn(future: impl Future<Output = ()> + Send + 'static) {
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_futures::spawn_local(future);
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::spawn(move || futures::executor::block_on(future));
}

pub struct Px4Driver;

pub fn driver() -> Px4Driver {
    Px4Driver
}

/// A device as the driver sees it: the USB devices of its bridges.
struct Found {
    usbs: Vec<UsbDeviceInfo>,
    model: &'static Model,
    info: DeviceInfo,
}

/// The devices of the models known here. The two boards of a Q model share
/// a serial number but for its last digit, 1 or 2, and make one device.
async fn scan() -> Result<Vec<Found>> {
    let mut found = Vec::<Found>::new();
    for usb in list_devices().await? {
        let Some(model) = MODELS
            .iter()
            .find(|m| usb.vendor_id == VENDOR_ID && m.product_id == usb.product_id)
        else {
            continue;
        };

        let serial = usb.serial.clone().unwrap_or_else(|| usb.port_path.clone());
        let id = match model.kind {
            Kind::Px4 { quad: true } if serial.len() == 15 => format!("px4:{}", &serial[..14]),
            _ => format!("px4:{serial}"),
        };
        match found.iter_mut().find(|f| f.info.id == id) {
            Some(f) => {
                f.usbs.push(usb);
                f.usbs.sort_by(|a, b| a.serial.cmp(&b.serial));
            }
            None => found.push(Found {
                usbs: vec![usb],
                model,
                info: DeviceInfo {
                    id,
                    name: model.name.to_owned(),
                },
            }),
        }
    }
    Ok(found)
}

impl Driver for Px4Driver {
    fn probe(&self) -> BoxFuture<'_, Result<Vec<DeviceInfo>>> {
        async { Ok(scan().await?.into_iter().map(|f| f.info).collect()) }.boxed()
    }

    fn open<'a>(&'a self, info: &'a DeviceInfo) -> BoxFuture<'a, Result<Box<dyn Device>>> {
        async move {
            let Found { usbs, model, info } = scan()
                .await?
                .into_iter()
                .find(|f| f.info.id == info.id)
                .ok_or_else(|| Error::NotFound(info.id.clone()))?;
            let firmware = firmware()?;
            match model.kind {
                Kind::PxMlt(layout) => pxmlt::open(&usbs[0], info, layout, &firmware).await,
                Kind::Px4 { .. } => px4::open(&usbs, info, &firmware).await,
                Kind::Single(m) => single::open(&usbs[0], info, m, &firmware).await,
            }
        }
        .boxed()
    }
}

/// Reads the firmware of the IT930x, which is not ours to ship, from where
/// the user put it.
fn firmware() -> Result<Vec<u8>> {
    let mut dirs = Vec::<PathBuf>::new();
    if cfg!(windows) {
        if let Some(data) = std::env::var_os("ProgramData") {
            dirs.push(PathBuf::from(data).join(r"tunelith\firmware"));
        }
    } else {
        dirs.push("/lib/firmware".into());
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::home_dir().map(|home| home.join(".local/share")));
        if let Some(data) = data {
            dirs.push(data.join("tunelith/firmware"));
        }
    }

    for dir in &dirs {
        if let Ok(firmware) = std::fs::read(dir.join(FIRMWARE)) {
            return Ok(firmware);
        }
    }

    let searched: Vec<_> = dirs.iter().map(|d| d.display().to_string()).collect();
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{FIRMWARE} not found in {}", searched.join(", ")),
    )
    .into())
}
