//! The generic driver over BDA, the Broadcast Driver Architecture of Windows,
//! driven through Kernel Streaming alone: the pins of the tuner and capture
//! filters are created and connected as a DirectShow graph would, with no
//! graph, network provider or reference clock in between. A device model
//! plugs in what BDA leaves to the vendor through its own [`Quirks`].

mod ks;

use std::io;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};

use crate::{
    ByteStream, Device, DeviceInfo, Driver, Error, Polarization, Result, Signal, StreamFormat,
    StreamId, System, TuneParams, Tuner, TunerInfo,
};
use ks::{Handle, Medium, State};

const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// 512 packets, the frame the capture filters seen so far ask for.
const DEFAULT_FRAME: usize = 188 * 512;
const READS_IN_FLIGHT: usize = 8;
/// The frames a stream may fall behind by before its data is dropped.
const STREAM_BACKLOG: usize = 64;

/// The symbol rates of the satellite systems, in kBd.
const ISDB_S_SYMBOL_RATE: u32 = 28_860;
const ISDB_S3_SYMBOL_RATE: u32 = 33_756;
/// `BDA_MOD_ISDB_S_TMCC`, `BDA_BCC_RATE_2_3` and
/// `BDA_SPECTRAL_INVERSION_AUTOMATIC`: the modulation, as for the TMCC, of
/// both satellite systems.
const MODULATION_ISDB_S_TMCC: u32 = 35;
const INNER_FEC_RATE_2_3: u32 = 2;
const SPECTRAL_INVERSION_AUTOMATIC: u32 = 1;
/// `BDA_POLARISATION_CIRCULAR_L` and `BDA_POLARISATION_CIRCULAR_R`.
const POLARISATION_CIRCULAR_L: u32 = 3;
const POLARISATION_CIRCULAR_R: u32 = 4;
/// Above any local oscillator, so that the LNB has the one band.
const LNB_SWITCH_KHZ: u32 = 20_000_000;

/// What sets a device model apart from the others BDA drives.
pub trait Quirks: Send + Sync + 'static {
    /// Whether the device is of this model.
    fn claims(&self, device: &BdaDevice) -> bool;

    /// The systems the satellite tuners receive beyond ISDB-S.
    fn extra_systems(&self) -> &[System] {
        &[]
    }

    /// Selects the stream to take out of the transponder, through the input
    /// pin of the tuner filter, before the tune. BDA has no property of its
    /// own for it.
    fn select_stream(&self, _input: &Pin<'_>, _system: System, _id: StreamId) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the tuner has no known way to select a stream",
        ))
    }

    /// The bytes the tuner gives out after a tune that it received before,
    /// which are left out.
    fn stale_bytes(&self) -> usize {
        0
    }

    /// The carrier-to-noise ratio in dB of the signal strength the tuner
    /// reports, if it reports that.
    fn cnr_db(&self, _strength: i32) -> Option<f64> {
        None
    }

    fn lock_timeout(&self) -> Duration {
        Duration::from_secs(5)
    }
}

/// A pin of a tuner filter, for [`Quirks`] to set the vendor's properties on.
pub struct Pin<'a>(&'a Handle);

impl Pin<'_> {
    /// Sets the property `id` of the property set `set`, with `instance`
    /// following the `KSPROPERTY` of the request.
    pub fn set_property(
        &self,
        set: u128,
        id: u32,
        instance: &[u8],
        value: &[u8],
    ) -> io::Result<()> {
        let mut request = ks::request(set, id, ks::SET, &[]);
        request.extend_from_slice(instance);
        self.0.set(&request, value)
    }
}

/// A physical device, with its BDA tuners.
pub struct BdaDevice {
    /// The instance id of the device,
    /// `PCI\VEN_544D&DEV_6178&SUBSYS_00016812&REV_00\4&23F93F77&0&00E0` for
    /// instance.
    pub instance_id: String,
    name: String,
    tuners: Vec<Filters>,
}

impl BdaDevice {
    /// A hexadecimal field of the hardware id in the instance id, `VEN` or
    /// `SUBSYS` for instance.
    pub fn hardware_field(&self, name: &str) -> Option<u32> {
        let hardware = self.instance_id.split('\\').nth(1)?;
        hardware.split('&').find_map(|field| {
            let value = field.strip_prefix(name)?.strip_prefix('_')?;
            u32::from_str_radix(value, 16).ok()
        })
    }

    fn id(&self) -> String {
        format!("bda:{}", self.instance_id)
    }
}

/// A tuner filter, the capture filter it feeds, and what is known of them.
#[derive(Clone)]
struct Filters {
    tuner: String,
    capture: String,
    /// The friendly name of the tuner filter, which orders the tuners.
    name: String,
    systems: Vec<System>,
    input: u32,
    output: u32,
    capture_input: u32,
    capture_output: u32,
    /// The medium connecting the tuner filter to the capture filter.
    medium: Medium,
    tuner_node: u32,
    demodulator_node: u32,
}

fn pin_factories(filter: &Handle) -> io::Result<(u32, u32)> {
    let count = filter.get_u32(&ks::request(
        ks::KSPROPSETID_PIN,
        ks::KSPROPERTY_PIN_CTYPES,
        ks::GET,
        &[],
    ))?;
    let mut input = None;
    let mut output = None;
    for pin in 0..count {
        let flow = filter.get_u32(&ks::request(
            ks::KSPROPSETID_PIN,
            ks::KSPROPERTY_PIN_DATAFLOW,
            ks::GET,
            &[pin],
        ))?;
        match flow {
            ks::KSPIN_DATAFLOW_IN => input = input.or(Some(pin)),
            ks::KSPIN_DATAFLOW_OUT => output = output.or(Some(pin)),
            _ => {}
        }
    }
    input
        .zip(output)
        .ok_or_else(|| io::Error::other("a filter without an input and an output pin"))
}

/// Finds the BDA tuners the quirks claim, with the capture filters they
/// feed, grouped by device.
fn scan(quirks: &dyn Quirks) -> io::Result<Vec<BdaDevice>> {
    let captures = ks::interfaces(ks::KSCATEGORY_BDA_RECEIVER_COMPONENT)?;
    let mut devices = Vec::<BdaDevice>::new();
    for tuner in ks::interfaces(ks::KSCATEGORY_BDA_NETWORK_TUNER)? {
        let Ok(filters) = describe(&tuner, &captures, quirks) else {
            continue;
        };
        match devices
            .iter_mut()
            .find(|d| d.instance_id == tuner.instance_id)
        {
            Some(device) => device.tuners.push(filters),
            None => devices.push(BdaDevice {
                instance_id: tuner.instance_id,
                name: tuner.device_name,
                tuners: vec![filters],
            }),
        }
    }

    devices.retain(|d| quirks.claims(d));
    for device in &mut devices {
        device.tuners.sort_by(|a, b| a.name.cmp(&b.name));
    }
    Ok(devices)
}

fn describe(
    tuner: &ks::Interface,
    captures: &[ks::Interface],
    quirks: &dyn Quirks,
) -> io::Result<Filters> {
    let filter = Handle::open(&tuner.path)?;
    let (input, output) = pin_factories(&filter)?;
    let medium = *ks::mediums(&filter, output)?
        .first()
        .ok_or_else(|| io::Error::other("an output pin without a medium"))?;

    // The capture filter of the same device taking what the output pin
    // gives, by its medium.
    let (capture, capture_input, capture_output) = captures
        .iter()
        .filter(|c| c.instance_id == tuner.instance_id)
        .find_map(|c| {
            let filter = Handle::open(&c.path).ok()?;
            let (input, output) = pin_factories(&filter).ok()?;
            ks::mediums(&filter, input)
                .ok()?
                .contains(&medium)
                .then(|| (c.path.clone(), input, output))
        })
        .ok_or_else(|| io::Error::other("no capture filter for the tuner"))?;

    let mut tuner_node = None;
    let mut demodulator = None;
    for (node, function) in ks::node_functions(&filter)? {
        match function {
            ks::KSNODE_BDA_RF_TUNER => tuner_node = Some(node),
            ks::KSNODE_BDA_COFDM_DEMODULATOR | ks::KSNODE_BDA_ISDB_T_DEMODULATOR => {
                demodulator = Some((node, vec![System::IsdbT]));
            }
            ks::KSNODE_BDA_QPSK_DEMODULATOR
            | ks::KSNODE_BDA_8PSK_DEMODULATOR
            | ks::KSNODE_BDA_ISDB_S_DEMODULATOR => {
                let mut systems = vec![System::IsdbS];
                systems.extend_from_slice(quirks.extra_systems());
                demodulator = Some((node, systems));
            }
            _ => {}
        }
    }
    let (Some(tuner_node), Some((demodulator_node, systems))) = (tuner_node, demodulator) else {
        return Err(io::Error::other("not an ISDB tuner"));
    };

    Ok(Filters {
        tuner: tuner.path.clone(),
        capture,
        name: tuner.name.clone(),
        systems,
        input,
        output,
        capture_input,
        capture_output,
        medium,
        tuner_node,
        demodulator_node,
    })
}

pub struct BdaDriver<Q> {
    quirks: Arc<Q>,
}

impl<Q: Quirks> BdaDriver<Q> {
    pub fn new(quirks: Q) -> Self {
        Self {
            quirks: Arc::new(quirks),
        }
    }
}

fn describe_device(device: &BdaDevice) -> (DeviceInfo, Vec<TunerInfo>) {
    let id = device.id();
    let tuners = device
        .tuners
        .iter()
        .enumerate()
        .map(|(index, filters)| TunerInfo {
            id: format!("{id}#{index}"),
            systems: filters.systems.clone(),
        })
        .collect();
    let info = DeviceInfo {
        id,
        name: device.name.clone(),
    };
    (info, tuners)
}

impl<Q: Quirks> Driver for BdaDriver<Q> {
    fn probe(&self) -> BoxFuture<'_, Result<Vec<DeviceInfo>>> {
        let quirks = self.quirks.clone();
        blocking::unblock(move || {
            Ok(scan(&*quirks)?
                .iter()
                .map(|d| describe_device(d).0)
                .collect())
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
            let (info, tuners) = describe_device(&device);
            Ok(Box::new(BdaDeviceHandle {
                info,
                opened: Arc::new(Mutex::new(vec![false; tuners.len()])),
                tuners,
                filters: device.tuners,
                quirks,
            }) as Box<dyn Device>)
        })
        .boxed()
    }
}

struct BdaDeviceHandle<Q> {
    info: DeviceInfo,
    tuners: Vec<TunerInfo>,
    filters: Vec<Filters>,
    /// Which tuners are open, which the filters do not keep from being
    /// opened again.
    opened: Arc<Mutex<Vec<bool>>>,
    quirks: Arc<Q>,
}

impl<Q: Quirks> Device for BdaDeviceHandle<Q> {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn tuners(&self) -> &[TunerInfo] {
        &self.tuners
    }

    fn open_tuner(&self, index: usize) -> BoxFuture<'_, Result<Box<dyn Tuner>>> {
        async move {
            let filters = self
                .filters
                .get(index)
                .cloned()
                .ok_or_else(|| Error::NotFound(format!("{}#{index}", self.info.id)))?;
            {
                let mut opened = self.opened.lock().unwrap();
                if std::mem::replace(&mut opened[index], true) {
                    return Err(io::Error::from(io::ErrorKind::ResourceBusy).into());
                }
            }
            let graph = blocking::unblock(move || Graph::build(filters)).await;
            let graph = match graph {
                Ok(graph) => graph,
                Err(e) => {
                    self.opened.lock().unwrap()[index] = false;
                    return Err(e.into());
                }
            };
            Ok(Box::new(BdaTuner {
                graph: Arc::new(graph),
                systems: self.tuners[index].systems.clone(),
                quirks: self.quirks.clone(),
                system: None,
                _opened: Opened(self.opened.clone(), index),
            }) as Box<dyn Tuner>)
        }
        .boxed()
    }
}

/// Marks a tuner free again once its [`BdaTuner`] goes.
struct Opened(Arc<Mutex<Vec<bool>>>, usize);

impl Drop for Opened {
    fn drop(&mut self) {
        self.0.lock().unwrap()[self.1] = false;
    }
}

/// Where the frames of a running graph go.
type Sink = Arc<Mutex<SinkState>>;

#[derive(Default)]
struct SinkState {
    /// The stream taking the frames; nowhere until there is one.
    tx: Option<mpsc::Sender<Vec<u8>>>,
    /// The bytes still to be left out as received before the last tune.
    stale: usize,
}

/// The pins of a tuner filter and its capture filter, connected and
/// running. Declared in the order they are to be closed in.
struct Graph {
    filters: Filters,
    sink: Sink,
    antenna: Handle,
    tuner_output: Handle,
    capture_input: Handle,
    capture_output: Arc<Handle>,
    _allocator: Handle,
    tuner: Handle,
    _capture: Handle,
}

impl Graph {
    fn build(filters: Filters) -> io::Result<Self> {
        let tuner = Handle::open(&filters.tuner)?;
        let capture = Handle::open(&filters.capture)?;
        ks::create_topology(&tuner, filters.input, filters.output)?;

        let antenna = ks::create_pin(
            &tuner,
            filters.input,
            Medium::STANDARD,
            &ks::first_data_range(&tuner, filters.input)?,
            None,
        )?;
        let capture_input = ks::create_pin(
            &capture,
            filters.capture_input,
            filters.medium,
            &ks::first_data_range(&capture, filters.capture_input)?,
            None,
        )?;
        let tuner_output = ks::create_pin(
            &tuner,
            filters.output,
            filters.medium,
            &ks::first_data_range(&tuner, filters.output)?,
            Some(&capture_input),
        )?;
        let capture_output = ks::create_pin(
            &capture,
            filters.capture_output,
            Medium::STANDARD,
            &ks::first_data_range(&capture, filters.capture_output)?,
            None,
        )?;

        capture_input.set_state(State::Acquire)?;
        let allocator = tuner_output.connect_pipe(&capture_input)?;
        for state in [State::Acquire, State::Pause, State::Run] {
            for pin in [&capture_input, &capture_output, &antenna, &tuner_output] {
                pin.set_state(state)?;
            }
        }

        // Every frame is taken as it comes, so that the device keeps none
        // back to hand out late; until a stream takes them they go.
        let capture_output = Arc::new(capture_output);
        let frame = capture_output.frame_size().unwrap_or(DEFAULT_FRAME);
        let mut reader = ks::Reader::new(capture_output.clone(), frame, READS_IN_FLIGHT)?;
        let sink = Sink::default();
        let frames = sink.clone();
        thread::spawn(move || {
            // Ends once the pins stop.
            while let Ok(frame) = reader.next() {
                let mut sink = frames.lock().unwrap();
                if sink.stale > 0 {
                    sink.stale = sink.stale.saturating_sub(frame.len());
                    continue;
                }
                if let Some(tx) = sink.tx.as_mut()
                    && let Err(e) = tx.try_send(frame)
                    && e.is_disconnected()
                {
                    sink.tx = None;
                }
                // ponytail: a stream falling STREAM_BACKLOG frames behind
                // loses them silently, as the USB tuners' do.
            }
        });

        Ok(Self {
            filters,
            sink,
            antenna,
            tuner_output,
            capture_input,
            capture_output,
            _allocator: allocator,
            tuner,
            _capture: capture,
        })
    }

    fn tuner_request(&self, set: u128, id: u32, flags: u32) -> Vec<u8> {
        ks::node_request(set, id, flags, self.filters.tuner_node)
    }

    fn set_tuner(&self, set: u128, id: u32, value: u32) -> io::Result<()> {
        self.antenna
            .set_u32(&self.tuner_request(set, id, ks::SET), value)
    }

    fn set_demodulator(&self, id: u32, value: u32) -> io::Result<()> {
        let request = ks::node_request(
            ks::KSPROPSETID_BDA_DIGITAL_DEMODULATOR,
            id,
            ks::SET,
            self.filters.demodulator_node,
        );
        self.tuner_output.set_u32(&request, value)
    }

    fn signal(&self, id: u32) -> io::Result<u32> {
        self.antenna
            .get_u32(&self.tuner_request(ks::KSPROPSETID_BDA_SIGNAL_STATS, id, ks::GET))
    }

    fn locked(&self) -> io::Result<bool> {
        Ok(self.signal(ks::KSPROPERTY_BDA_SIGNAL_LOCKED)? != 0)
    }

    /// Sends the tuning of `params` down to the tuner, not waiting for it to
    /// lock.
    fn tune(&self, params: &TuneParams, quirks: &dyn Quirks) -> Result<()> {
        use ks::*;

        self.sink.lock().unwrap().stale = quirks.stale_bytes();

        if let Some(id) = params.stream_id {
            quirks.select_stream(&Pin(&self.antenna), params.system, id)?;
        }

        // What is set between the start and the commit goes down together at
        // the commit; set after it, it waits for the next one.
        change(&self.tuner, KSMETHOD_BDA_START_CHANGES)?;
        let freq = KSPROPSETID_BDA_FREQUENCY_FILTER;
        // In kHz.
        self.set_tuner(freq, KSPROPERTY_BDA_RF_TUNER_FREQUENCY_MULTIPLIER, 1000)?;
        self.set_tuner(
            freq,
            KSPROPERTY_BDA_RF_TUNER_FREQUENCY,
            params.frequency_khz,
        )?;
        if params.system.is_satellite() {
            let polarity = match params.polarization.unwrap_or_default() {
                Polarization::Right => POLARISATION_CIRCULAR_R,
                Polarization::Left => POLARISATION_CIRCULAR_L,
            };
            self.set_tuner(freq, KSPROPERTY_BDA_RF_TUNER_POLARITY, polarity)?;
            let lo = params.frequency_khz - params.if_frequency_khz()?;
            let lnb = KSPROPSETID_BDA_LNB_INFO;
            self.set_tuner(lnb, KSPROPERTY_BDA_LNB_LOF_LOW_BAND, lo)?;
            self.set_tuner(lnb, KSPROPERTY_BDA_LNB_LOF_HIGH_BAND, lo)?;
            self.set_tuner(lnb, KSPROPERTY_BDA_LNB_SWITCH_FREQUENCY, LNB_SWITCH_KHZ)?;

            let symbol_rate = match params.system {
                System::IsdbS3 => ISDB_S3_SYMBOL_RATE,
                _ => ISDB_S_SYMBOL_RATE,
            };
            self.set_demodulator(KSPROPERTY_BDA_MODULATION_TYPE, MODULATION_ISDB_S_TMCC)?;
            self.set_demodulator(KSPROPERTY_BDA_INNER_FEC_RATE, INNER_FEC_RATE_2_3)?;
            self.set_demodulator(KSPROPERTY_BDA_SYMBOL_RATE, symbol_rate)?;
            self.set_demodulator(
                KSPROPERTY_BDA_SPECTRAL_INVERSION,
                SPECTRAL_INVERSION_AUTOMATIC,
            )?;
        } else {
            // In MHz.
            self.set_tuner(freq, KSPROPERTY_BDA_RF_TUNER_BANDWIDTH, 6)?;
        }
        change(&self.tuner, KSMETHOD_BDA_CHECK_CHANGES)?;
        change(&self.tuner, KSMETHOD_BDA_COMMIT_CHANGES)?;
        Ok(())
    }
}

impl Drop for Graph {
    fn drop(&mut self) {
        for pin in [
            &self.tuner_output,
            &self.antenna,
            &*self.capture_output,
            &self.capture_input,
        ] {
            let _ = pin.set_state(State::Stop);
        }
    }
}

struct BdaTuner<Q> {
    graph: Arc<Graph>,
    systems: Vec<System>,
    quirks: Arc<Q>,
    /// The system last tuned to.
    system: Option<System>,
    _opened: Opened,
}

impl<Q: Quirks> Tuner for BdaTuner<Q> {
    fn tune(&mut self, params: TuneParams) -> BoxFuture<'_, Result<()>> {
        async move {
            params.validate()?;
            if !self.systems.contains(&params.system) {
                return Err(Error::Unsupported(params.system));
            }

            let graph = self.graph.clone();
            let quirks = self.quirks.clone();
            self.system = None;
            blocking::unblock(move || {
                graph.tune(&params, &*quirks)?;
                let deadline = Instant::now() + quirks.lock_timeout();
                while !graph.locked()? {
                    if Instant::now() >= deadline {
                        return Err(Error::NoLock);
                    }
                    thread::sleep(LOCK_POLL_INTERVAL);
                }
                Ok(())
            })
            .await?;

            self.system = Some(params.system);
            Ok(())
        }
        .boxed()
    }

    fn signal(&mut self) -> BoxFuture<'_, Result<Signal>> {
        let graph = self.graph.clone();
        let quirks = self.quirks.clone();
        blocking::unblock(move || {
            let locked = graph.locked()?;
            let strength = graph.signal(ks::KSPROPERTY_BDA_SIGNAL_STRENGTH)? as i32;
            Ok(Signal {
                locked,
                cnr_db: quirks.cnr_db(strength),
            })
        })
        .boxed()
    }

    fn set_lnb(&mut self, on: bool) -> BoxFuture<'_, Result<()>> {
        async move {
            if on {
                // ponytail: BDA has no property for the LNB supply, and the
                // vendors' ways are not known yet.
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "powering the LNB is not supported through BDA yet",
                )
                .into());
            }
            Ok(())
        }
        .boxed()
    }

    fn stream(&mut self) -> BoxFuture<'_, Result<(StreamFormat, ByteStream)>> {
        async move {
            let format = self
                .system
                .map(|system| match system {
                    System::IsdbS3 => StreamFormat::Tlv,
                    _ => StreamFormat::Ts,
                })
                .ok_or(Error::InvalidParams("the tuner has not been tuned"))?;
            let (tx, rx) = mpsc::channel(STREAM_BACKLOG);
            self.graph.sink.lock().unwrap().tx = Some(tx);
            let mut aligner = Aligner::new(format);
            // The pins stopping ends the stream.
            let stream = rx
                .map(move |frame| aligner.feed(frame))
                .filter(|chunk| futures::future::ready(!chunk.is_empty()))
                .map(Ok);

            Ok((format, stream.boxed()))
        }
        .boxed()
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        // Stopping and closing the pins is all it takes, which the drop of
        // the graph does once the stream lets go of it as well.
        async move {
            blocking::unblock(move || drop(self)).await;
        }
        .boxed()
    }
}

const TS_PACKET: usize = 188;

/// Gives out the stream in whole packets. The first frame is left out, the
/// device having started it anywhere, then everything before the first
/// packet. A TS keeps being cut into its packets, the terrestrial tuner
/// slipping a few bytes in its first frames.
struct Aligner {
    format: StreamFormat,
    frames: usize,
    aligned: bool,
    /// The bytes of a TS packet yet to be completed.
    carry: Vec<u8>,
}

impl Aligner {
    fn new(format: StreamFormat) -> Self {
        Self {
            format,
            frames: 0,
            aligned: false,
            carry: Vec::new(),
        }
    }

    fn feed(&mut self, data: Vec<u8>) -> Vec<u8> {
        self.frames += 1;
        if self.frames == 1 {
            return Vec::new();
        }
        match self.format {
            StreamFormat::Ts => self.feed_ts(data),
            StreamFormat::Tlv => self.feed_tlv(data),
        }
    }

    fn feed_tlv(&mut self, mut data: Vec<u8>) -> Vec<u8> {
        if self.aligned {
            return data;
        }
        match packet_start(StreamFormat::Tlv, &data) {
            Some(start) => {
                self.aligned = true;
                data.drain(..start);
                data
            }
            None => Vec::new(),
        }
    }

    fn feed_ts(&mut self, data: Vec<u8>) -> Vec<u8> {
        self.carry.extend_from_slice(&data);
        let buf = &self.carry;
        let mut out = Vec::with_capacity(buf.len());
        let mut at = 0;
        while buf.len() - at >= TS_PACKET {
            if !self.aligned {
                if buf.len() - at < TS_PACKET * 3 {
                    break;
                }
                match packet_start(StreamFormat::Ts, &buf[at..]) {
                    Some(start) => {
                        at += start;
                        self.aligned = true;
                    }
                    None => {
                        at = buf.len() - (TS_PACKET * 3 - 1);
                        break;
                    }
                }
                continue;
            }
            if buf[at] != 0x47 {
                self.aligned = false;
                continue;
            }
            out.extend_from_slice(&buf[at..at + TS_PACKET]);
            at += TS_PACKET;
        }
        self.carry.drain(..at);
        out
    }
}

/// Where the first of three packets in a row starts.
fn packet_start(format: StreamFormat, data: &[u8]) -> Option<usize> {
    (0..data.len()).find(|&start| {
        let mut at = start;
        (0..3).all(|_| {
            let Some(&head) = data.get(at) else {
                return false;
            };
            match format {
                StreamFormat::Ts if head == 0x47 => {
                    at += 188;
                    true
                }
                StreamFormat::Tlv if head == 0x7f => match data.get(at + 2..at + 4) {
                    Some(len) => {
                        at += 4 + usize::from(u16::from_be_bytes([len[0], len[1]]));
                        true
                    }
                    None => false,
                },
                _ => false,
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(instance_id: &str) -> BdaDevice {
        BdaDevice {
            instance_id: instance_id.into(),
            name: String::new(),
            tuners: Vec::new(),
        }
    }

    #[test]
    fn hardware_field() {
        let d = device(r"PCI\VEN_544D&DEV_6178&SUBSYS_00016812&REV_00\4&23F93F77&0&00E0");
        assert_eq!(d.hardware_field("VEN"), Some(0x544d));
        assert_eq!(d.hardware_field("SUBSYS"), Some(0x0001_6812));
        assert_eq!(d.hardware_field("FOO"), None);
    }

    fn packets(n: usize) -> Vec<u8> {
        let mut data = Vec::new();
        for i in 0..n {
            data.push(0x47);
            data.extend_from_slice(&[i as u8; 187]);
        }
        data
    }

    #[test]
    fn aligns_ts() {
        let mut aligner = Aligner::new(StreamFormat::Ts);
        assert!(aligner.feed(packets(4)).is_empty());

        let mut frame = vec![0; 5];
        frame.extend_from_slice(&packets(3));
        frame.extend_from_slice(&packets(1)[..100]);
        assert_eq!(aligner.feed(frame), packets(3));

        // The rest of the packet, then a few bytes slipped in.
        let mut frame = packets(1)[100..].to_vec();
        frame.extend_from_slice(&[0xff; 7]);
        frame.extend_from_slice(&packets(4));
        let out = aligner.feed(frame);
        assert_eq!(&out[..188], &packets(1)[..]);
        assert_eq!(&out[188..], &packets(4)[..]);
    }

    #[test]
    fn aligns_tlv() {
        let packet = [0x7f, 0x03, 0x00, 0x02, 0xaa, 0xbb];
        let mut frame = vec![0x7f, 0x00];
        for _ in 0..3 {
            frame.extend_from_slice(&packet);
        }
        assert_eq!(packet_start(StreamFormat::Tlv, &frame), Some(2));
    }
}
