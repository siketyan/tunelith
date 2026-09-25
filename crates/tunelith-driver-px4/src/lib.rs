// SPDX-License-Identifier: GPL-2.0-only
//! The PLEX PX4/PX-MLT series and their kin, driven in user space over USB:
//! a port of [px4_drv](https://github.com/tsukumijima/px4_drv),
//! Copyright (c) 2018-2021 nns779.
//!
//! The kernel module of px4_drv, if loaded, lets go of the device when it is
//! opened here.

mod cnr;
mod cxd2856er;
mod cxd2858er;
mod it930x;
mod pxmlt;

use std::io;
use std::path::PathBuf;

use futures::FutureExt;
use futures::future::BoxFuture;
use tunelith_core::usb::{UsbDeviceInfo, list_devices};
use tunelith_core::{Device, DeviceInfo, Driver, Error, Result};

const VENDOR_ID: u16 = 0x0511;
const FIRMWARE: &str = "it930x-firmware.bin";

struct Model {
    product_id: u16,
    name: &'static str,
    layout: pxmlt::Layout,
}

const MLT5PE: pxmlt::Layout = &[
    (0x65, 3, 0),
    (0x6c, 1, 1),
    (0x64, 1, 2),
    (0x6c, 3, 3),
    (0x64, 3, 4),
];

// ponytail: the PX-MLT family alone; the PX-W3U4/Q3U4 and PX-M1UR/S1UR use
// other chips and wait for their own port.
const MODELS: &[Model] = &[
    Model {
        product_id: 0x084e,
        name: "PLEX PX-MLT5U",
        layout: &[
            (0x65, 3, 4),
            (0x6c, 1, 3),
            (0x64, 1, 1),
            (0x6c, 3, 2),
            (0x64, 3, 0),
        ],
    },
    Model {
        product_id: 0x024e,
        name: "PLEX PX-MLT5PE",
        layout: MLT5PE,
    },
    // A PX-MLT5PE but for the product id.
    Model {
        product_id: 0x924e,
        name: "e-better DTV02A-5TS-P",
        layout: MLT5PE,
    },
    Model {
        product_id: 0x0252,
        name: "PLEX PX-MLT8PE (3 tuners)",
        layout: &[(0x65, 3, 0), (0x6c, 3, 3), (0x64, 3, 4)],
    },
    Model {
        product_id: 0x0253,
        name: "PLEX PX-MLT8PE (5 tuners)",
        layout: &[
            (0x65, 1, 0),
            (0x64, 1, 1),
            (0x6c, 1, 2),
            (0x6c, 3, 3),
            (0x64, 3, 4),
        ],
    },
    Model {
        product_id: 0x0254,
        name: "Digibest ISDB6014 V2.0 (4TS)",
        layout: &[(0x65, 3, 0), (0x6c, 1, 1), (0x64, 1, 2), (0x64, 3, 4)],
    },
];

fn error(message: &str) -> Error {
    io::Error::other(message).into()
}

pub struct Px4Driver;

pub fn driver() -> Px4Driver {
    Px4Driver
}

/// The devices of the models known here.
async fn scan() -> Result<Vec<(UsbDeviceInfo, &'static Model, DeviceInfo)>> {
    Ok(list_devices()
        .await?
        .into_iter()
        .filter(|d| d.vendor_id == VENDOR_ID)
        .filter_map(|d| {
            let model = MODELS.iter().find(|m| m.product_id == d.product_id)?;
            let info = DeviceInfo {
                id: format!("px4:{}", d.serial.as_deref().unwrap_or(&d.port_path)),
                name: model.name.to_owned(),
            };
            Some((d, model, info))
        })
        .collect())
}

impl Driver for Px4Driver {
    fn probe(&self) -> BoxFuture<'_, Result<Vec<DeviceInfo>>> {
        async { Ok(scan().await?.into_iter().map(|(_, _, info)| info).collect()) }.boxed()
    }

    fn open<'a>(&'a self, info: &'a DeviceInfo) -> BoxFuture<'a, Result<Box<dyn Device>>> {
        async move {
            let (usb, model, info) = scan()
                .await?
                .into_iter()
                .find(|(_, _, i)| i.id == info.id)
                .ok_or_else(|| Error::NotFound(info.id.clone()))?;
            pxmlt::open(&usb, info, model.layout, &firmware()?).await
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
