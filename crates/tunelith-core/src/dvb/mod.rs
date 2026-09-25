//! The generic driver over the Linux DVB API, which takes any ISDB tuner the
//! kernel drives. A device model that needs telling apart plugs its own
//! [`Quirks`] in.

mod sys;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use futures::AsyncReadExt;
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};

use crate::{
    ByteStream, Device, DeviceInfo, Driver, Error, Result, Signal, StreamFormat, StreamId, System,
    TuneParams, Tuner, TunerInfo,
};

const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(100);
const DVR_BUFFER_SIZE: usize = 32 * 1024 * 1024;
const READ_SIZE: usize = 188 * 1024;

/// What sets a device model apart from the others the kernel drives.
pub trait Quirks: Send + Sync + 'static {
    /// Whether the device is of this model.
    fn claims(&self, device: &DvbDevice) -> bool;

    /// The systems the tuners receive beyond those the kernel lists.
    fn extra_systems(&self) -> &[System] {
        &[]
    }

    /// The value of `DTV_STREAM_ID` for the stream.
    fn stream_id(&self, _system: System, id: StreamId) -> u32 {
        id.0.into()
    }

    fn lock_timeout(&self) -> Duration {
        Duration::from_secs(5)
    }
}

/// Takes every device.
pub struct Generic;

impl Quirks for Generic {
    fn claims(&self, _device: &DvbDevice) -> bool {
        true
    }
}

/// A physical device, with its DVB adapters.
pub struct DvbDevice {
    /// Where the device is in sysfs, `/sys/devices/pci0000:00/0000:00:1c.0/0000:01:00.0`
    /// for instance.
    pub sysfs: PathBuf,
    adapters: Vec<u32>,
}

impl DvbDevice {
    /// A hexadecimal attribute of the device in sysfs, `vendor` or
    /// `subsystem_vendor` for instance.
    pub fn attr(&self, name: &str) -> Option<u16> {
        let value = fs::read_to_string(self.sysfs.join(name)).ok()?;
        u16::from_str_radix(value.trim().trim_start_matches("0x"), 16).ok()
    }

    fn id(&self) -> String {
        format!("dvb:{}", self.sysfs.display())
    }
}

pub struct DvbDriver<Q> {
    quirks: Arc<Q>,
}

impl<Q: Quirks> DvbDriver<Q> {
    pub fn new(quirks: Q) -> Self {
        Self {
            quirks: Arc::new(quirks),
        }
    }
}

impl DvbDriver<Generic> {
    pub fn generic() -> Self {
        Self::new(Generic)
    }
}

impl<Q: Quirks> Driver for DvbDriver<Q> {
    fn probe(&self) -> BoxFuture<'_, Result<Vec<DeviceInfo>>> {
        let quirks = self.quirks.clone();
        blocking::unblock(move || {
            let mut infos = Vec::new();
            for device in scan(&*quirks)? {
                infos.push(describe(&device, &*quirks)?.0);
            }
            Ok(infos)
        })
        .boxed()
    }

    fn open<'a>(&'a self, info: &'a DeviceInfo) -> BoxFuture<'a, Result<Box<dyn Device>>> {
        let quirks = self.quirks.clone();
        let id = info.id.clone();
        blocking::unblock(move || {
            let device = scan(&*quirks)?
                .into_iter()
                .find(|d| d.id() == id)
                .ok_or(Error::NotFound(id))?;
            let (info, tuners) = describe(&device, &*quirks)?;
            Ok(Box::new(DvbDeviceHandle {
                info,
                tuners,
                adapters: device.adapters,
                quirks,
            }) as Box<dyn Device>)
        })
        .boxed()
    }
}

/// Finds the devices with an ISDB frontend the quirks claim, their adapters
/// grouped by the device in sysfs.
// ponytail: frontend0 of each adapter only; an adapter with more frontends
// shares its demux between them, which needs its own handling.
fn scan(quirks: &dyn Quirks) -> io::Result<Vec<DvbDevice>> {
    let mut devices = Vec::<DvbDevice>::new();
    let entries = match fs::read_dir("/sys/class/dvb") {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(devices),
        Err(e) => return Err(e),
    };

    let mut adapters = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(adapter) = name
            .to_str()
            .and_then(|n| n.strip_prefix("dvb")?.strip_suffix(".frontend0"))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        adapters.push((adapter, fs::canonicalize(entry.path().join("device"))?));
    }
    adapters.sort();

    for (adapter, sysfs) in adapters {
        match devices.iter_mut().find(|d| d.sysfs == sysfs) {
            Some(device) => device.adapters.push(adapter),
            None => devices.push(DvbDevice {
                sysfs,
                adapters: vec![adapter],
            }),
        }
    }

    devices.retain(|d| quirks.claims(d));
    Ok(devices)
}

fn describe(device: &DvbDevice, quirks: &dyn Quirks) -> io::Result<(DeviceInfo, Vec<TunerInfo>)> {
    let id = device.id();
    let mut name = String::new();
    let mut tuners = Vec::new();
    for (index, &adapter) in device.adapters.iter().enumerate() {
        let fe = open(&frontend_path(adapter), false)?;
        if name.is_empty() {
            name = sys::frontend_name(&fe)?;
        }

        let mut systems = Vec::new();
        for &delsys in sys::get_property(&fe, sys::DTV_ENUM_DELSYS)?.buffer() {
            match u32::from(delsys) {
                sys::SYS_ISDBT => systems.push(System::IsdbT),
                sys::SYS_ISDBS => systems.push(System::IsdbS),
                _ => {}
            }
        }
        systems.extend_from_slice(quirks.extra_systems());

        tuners.push(TunerInfo {
            id: format!("{id}#{index}"),
            systems,
        });
    }

    Ok((DeviceInfo { id, name }, tuners))
}

fn frontend_path(adapter: u32) -> PathBuf {
    format!("/dev/dvb/adapter{adapter}/frontend0").into()
}

fn open(path: &Path, write: bool) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(write)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
}

struct DvbDeviceHandle<Q> {
    info: DeviceInfo,
    tuners: Vec<TunerInfo>,
    adapters: Vec<u32>,
    quirks: Arc<Q>,
}

impl<Q: Quirks> Device for DvbDeviceHandle<Q> {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn tuners(&self) -> &[TunerInfo] {
        &self.tuners
    }

    fn open_tuner(&self, index: usize) -> BoxFuture<'_, Result<Box<dyn Tuner>>> {
        async move {
            let adapter = *self
                .adapters
                .get(index)
                .ok_or_else(|| Error::NotFound(format!("{}#{index}", self.info.id)))?;
            let systems = self.tuners[index].systems.clone();
            let quirks = self.quirks.clone();
            blocking::unblock(move || {
                // The kernel lets one program open the frontend for writing,
                // which is what makes the tuner ours alone.
                let fe = open(&frontend_path(adapter), true)?;
                let demux = open(format!("/dev/dvb/adapter{adapter}/demux0").as_ref(), true)?;
                Ok(Box::new(DvbTuner {
                    fe: Arc::new(fe),
                    demux,
                    adapter,
                    systems,
                    quirks,
                    format: None,
                }) as Box<dyn Tuner>)
            })
            .await
        }
        .boxed()
    }
}

struct DvbTuner<Q> {
    fe: Arc<File>,
    demux: File,
    adapter: u32,
    systems: Vec<System>,
    quirks: Arc<Q>,
    /// The format of the system last tuned to.
    format: Option<StreamFormat>,
}

impl<Q: Quirks> DvbTuner<Q> {
    fn properties(&self, params: &TuneParams) -> Result<Vec<sys::DtvProperty>> {
        use sys::DtvProperty as P;

        let mut props = vec![P::new(sys::DTV_CLEAR, 0)];
        match params.stream_id {
            None => props.extend([
                P::new(sys::DTV_DELIVERY_SYSTEM, sys::SYS_ISDBT),
                P::new(sys::DTV_FREQUENCY, params.frequency_khz * 1000),
                P::new(sys::DTV_BANDWIDTH_HZ, 6_000_000),
                P::new(sys::DTV_TRANSMISSION_MODE, sys::TRANSMISSION_MODE_AUTO),
                P::new(sys::DTV_GUARD_INTERVAL, sys::GUARD_INTERVAL_AUTO),
                P::new(sys::DTV_ISDBT_LAYER_ENABLED, 0b111),
            ]),
            // The kernel has no delivery system of its own for ISDB-S3: a 4K
            // tuner takes it as ISDB-S, telling the two apart by the stream id.
            Some(stream_id) => props.extend([
                P::new(sys::DTV_DELIVERY_SYSTEM, sys::SYS_ISDBS),
                P::new(sys::DTV_FREQUENCY, params.if_frequency_khz()?),
                P::new(
                    sys::DTV_STREAM_ID,
                    self.quirks.stream_id(params.system, stream_id),
                ),
            ]),
        }
        props.push(P::new(sys::DTV_TUNE, 0));
        Ok(props)
    }
}

impl<Q: Quirks> Tuner for DvbTuner<Q> {
    fn tune(&mut self, params: TuneParams) -> BoxFuture<'_, Result<()>> {
        async move {
            params.validate()?;
            if !self.systems.contains(&params.system) {
                return Err(Error::Unsupported(params.system));
            }

            let mut props = self.properties(&params)?;
            let fe = self.fe.clone();
            let timeout = self.quirks.lock_timeout();
            blocking::unblock(move || {
                sys::set_properties(&fe, &mut props)?;
                let deadline = Instant::now() + timeout;
                while sys::read_status(&fe)? & sys::FE_HAS_LOCK == 0 {
                    if Instant::now() >= deadline {
                        return Err(Error::NoLock);
                    }
                    thread::sleep(LOCK_POLL_INTERVAL);
                }
                Ok(())
            })
            .await?;

            self.format = Some(params.stream_format());
            Ok(())
        }
        .boxed()
    }

    fn signal(&mut self) -> BoxFuture<'_, Result<Signal>> {
        let fe = self.fe.clone();
        blocking::unblock(move || {
            let locked = sys::read_status(&fe)? & sys::FE_HAS_LOCK != 0;
            let cnr_db = match sys::get_property(&fe, sys::DTV_STAT_CNR)?.stat() {
                Some((sys::FE_SCALE_DECIBEL, value)) => Some(value as f64 / 1000.0),
                _ => None,
            };
            Ok(Signal { locked, cnr_db })
        })
        .boxed()
    }

    fn set_lnb(&mut self, on: bool) -> BoxFuture<'_, Result<()>> {
        let fe = self.fe.clone();
        let voltage = if on {
            sys::SEC_VOLTAGE_18
        } else {
            sys::SEC_VOLTAGE_OFF
        };
        blocking::unblock(move || Ok(sys::set_voltage(&fe, voltage)?)).boxed()
    }

    fn stream(&mut self) -> BoxFuture<'_, Result<(StreamFormat, ByteStream)>> {
        async move {
            let format = self
                .format
                .ok_or(Error::InvalidParams("the tuner has not been tuned"))?;
            let dvr = open(
                format!("/dev/dvb/adapter{}/dvr0", self.adapter).as_ref(),
                false,
            )?;
            sys::set_buffer_size(&dvr, DVR_BUFFER_SIZE)?;
            sys::tap_all_pids(&self.demux)?;

            let reader = blocking::Unblock::with_capacity(READ_SIZE * 4, DvrReader(dvr));
            let stream = futures::stream::try_unfold(reader, |mut reader| async move {
                let mut buf = vec![0; READ_SIZE];
                let n = reader.read(&mut buf).await?;
                if n == 0 {
                    return Ok(None);
                }
                buf.truncate(n);
                Ok(Some((buf, reader)))
            });

            Ok((format, stream.boxed()))
        }
        .boxed()
    }
}

/// Reads the DVR device, going on past an overflow of the kernel's buffer.
struct DvrReader(File);

impl Read for DvrReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.0.read(buf) {
                // ponytail: the packets lost to an overflow go unreported;
                // tell the reader once there is a way to.
                Err(e) if e.raw_os_error() == Some(libc::EOVERFLOW) => continue,
                result => return result,
            }
        }
    }
}
