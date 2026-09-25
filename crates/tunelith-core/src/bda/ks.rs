//! The parts of Kernel Streaming in use, written after the Windows SDK
//! headers `ks.h`, `ksmedia.h`, `bdatypes.h` and `bdamedia.h`: the requests
//! a filter or a pin takes as I/O controls, the creation of pins and
//! allocators, and the enumeration of the filters through SetupAPI.

use std::ffi::c_void;
use std::io;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA, SPDRP_DEVICEDESC, SPDRP_FRIENDLYNAME,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceInterfaceDetailW,
    SetupDiGetDeviceRegistryPropertyW, SetupDiOpenDeviceInterfaceRegKey,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_IO_PENDING, ERROR_MORE_DATA, GENERIC_READ,
    GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Media::KernelStreaming::{KsCreateAllocator, KsCreatePin};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED,
};
use windows_sys::Win32::System::Registry::{KEY_READ, RegCloseKey, RegQueryValueExW};
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows_sys::core::GUID;

const IOCTL_KS_PROPERTY: u32 = 0x002f_0003;
const IOCTL_KS_METHOD: u32 = 0x002f_000f;
const IOCTL_KS_READ_STREAM: u32 = 0x002f_4017;

pub const GET: u32 = 0x1;
pub const SET: u32 = 0x2;
const TOPOLOGY: u32 = 0x1000_0000;

pub const KSPROPSETID_PIN: u128 = 0x8c13_4960_51ad_11cf_878a_94f8_01c1_0000;
pub const KSPROPERTY_PIN_DATAFLOW: u32 = 2;
pub const KSPROPERTY_PIN_DATARANGES: u32 = 3;
pub const KSPROPERTY_PIN_MEDIUMS: u32 = 6;
pub const KSPROPERTY_PIN_CTYPES: u32 = 1;
pub const KSPIN_DATAFLOW_IN: u32 = 1;
pub const KSPIN_DATAFLOW_OUT: u32 = 2;

const KSPROPSETID_CONNECTION: u128 = 0x1d58_c920_ac9b_11cf_a5d6_28db_04c1_0000;
const KSPROPERTY_CONNECTION_STATE: u32 = 0;
const KSPROPERTY_CONNECTION_ALLOCATORFRAMING_EX: u32 = 6;

const KSPROPSETID_STREAM: u128 = 0x65aa_ba60_98ae_11cf_a10d_0020_afd1_56e4;
const KSPROPERTY_STREAM_ALLOCATOR: u32 = 0;
const KSPROPERTY_STREAM_PIPE_ID: u32 = 10;

pub const KSPROPSETID_BDA_TOPOLOGY: u128 = 0xa14e_e835_0a23_11d3_9cc7_00c0_4f79_71e0;
pub const KSPROPERTY_BDA_NODE_DESCRIPTORS: u32 = 7;
const KSMETHODSETID_BDA_DEVICE_CONFIGURATION: u128 = 0x7198_5f45_1ca1_11d3_9cc8_00c0_4f79_71e0;
const KSMETHOD_BDA_CREATE_TOPOLOGY: u32 = 2;
const KSMETHODSETID_BDA_CHANGE_SYNC: u128 = 0xfd0a_5af3_b41d_11d2_9c95_00c0_4f79_71e0;
pub const KSMETHOD_BDA_START_CHANGES: u32 = 0;
pub const KSMETHOD_BDA_CHECK_CHANGES: u32 = 1;
pub const KSMETHOD_BDA_COMMIT_CHANGES: u32 = 2;

pub const KSPROPSETID_BDA_FREQUENCY_FILTER: u128 = 0x7198_5f47_1ca1_11d3_9cc8_00c0_4f79_71e0;
pub const KSPROPERTY_BDA_RF_TUNER_FREQUENCY: u32 = 0;
pub const KSPROPERTY_BDA_RF_TUNER_POLARITY: u32 = 1;
pub const KSPROPERTY_BDA_RF_TUNER_BANDWIDTH: u32 = 4;
pub const KSPROPERTY_BDA_RF_TUNER_FREQUENCY_MULTIPLIER: u32 = 5;
pub const KSPROPSETID_BDA_LNB_INFO: u128 = 0x992c_f102_49f9_4719_a664_c4f2_3e24_08f4;
pub const KSPROPERTY_BDA_LNB_LOF_LOW_BAND: u32 = 0;
pub const KSPROPERTY_BDA_LNB_LOF_HIGH_BAND: u32 = 1;
pub const KSPROPERTY_BDA_LNB_SWITCH_FREQUENCY: u32 = 2;
pub const KSPROPSETID_BDA_DIGITAL_DEMODULATOR: u128 = 0xef30_f379_985b_4d10_b640_a79d_5e04_e1e0;
pub const KSPROPERTY_BDA_MODULATION_TYPE: u32 = 0;
pub const KSPROPERTY_BDA_INNER_FEC_RATE: u32 = 2;
pub const KSPROPERTY_BDA_SYMBOL_RATE: u32 = 5;
pub const KSPROPERTY_BDA_SPECTRAL_INVERSION: u32 = 6;
pub const KSPROPSETID_BDA_SIGNAL_STATS: u128 = 0x1347_d106_cf3a_428a_a5cb_ac0d_9a2a_4338;
pub const KSPROPERTY_BDA_SIGNAL_STRENGTH: u32 = 0;
pub const KSPROPERTY_BDA_SIGNAL_LOCKED: u32 = 3;

pub const KSNODE_BDA_RF_TUNER: u128 = 0x7198_5f4c_1ca1_11d3_9cc8_00c0_4f79_71e0;
pub const KSNODE_BDA_QPSK_DEMODULATOR: u128 = 0x6390_c905_27c1_4d67_bdb7_77c5_0d07_9300;
pub const KSNODE_BDA_8PSK_DEMODULATOR: u128 = 0xe957_a0e7_dd98_4a3c_810b_3525_157a_b62a;
pub const KSNODE_BDA_ISDB_S_DEMODULATOR: u128 = 0xedde_230a_9086_432d_b8a5_6670_2638_07e9;
pub const KSNODE_BDA_COFDM_DEMODULATOR: u128 = 0x2dac_6e05_edbe_4b9c_b387_1b6f_ad7d_6495;
pub const KSNODE_BDA_ISDB_T_DEMODULATOR: u128 = 0xfcea_3ae3_2cb2_464d_8f5d_305c_3bb7_78a2;

pub const KSCATEGORY_BDA_NETWORK_TUNER: u128 = 0x7198_5f48_1ca1_11d3_9cc8_00c0_4f79_71e0;
pub const KSCATEGORY_BDA_RECEIVER_COMPONENT: u128 = 0xfd0a_5af4_b41d_11d2_9c95_00c0_4f79_71e0;

const KSINTERFACESETID_STANDARD: u128 = 0x1a87_66a0_62ce_11cf_a5d6_28db_04c1_0000;
pub const KSMEDIUMSETID_STANDARD: u128 = 0x4747_b320_62ce_11cf_a5d6_28db_04c1_0000;

/// `KSSTATE`.
#[derive(Clone, Copy)]
pub enum State {
    Stop = 0,
    Acquire = 1,
    Pause = 2,
    Run = 3,
}

pub fn guid(value: u128) -> GUID {
    GUID::from_u128(value)
}

fn guid_bytes(value: u128) -> [u8; 16] {
    let g = guid(value);
    let mut b = [0; 16];
    b[..4].copy_from_slice(&g.data1.to_le_bytes());
    b[4..6].copy_from_slice(&g.data2.to_le_bytes());
    b[6..8].copy_from_slice(&g.data3.to_le_bytes());
    b[8..].copy_from_slice(&g.data4);
    b
}

pub fn read_guid(b: &[u8]) -> u128 {
    let data1 = u32::from_le_bytes(b[..4].try_into().unwrap());
    let data2 = u16::from_le_bytes(b[4..6].try_into().unwrap());
    let data3 = u16::from_le_bytes(b[6..8].try_into().unwrap());
    let data4 = u64::from_be_bytes(b[8..16].try_into().unwrap());
    (u128::from(data1) << 96)
        | (u128::from(data2) << 80)
        | (u128::from(data3) << 64)
        | u128::from(data4)
}

pub fn read_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// A `KSPROPERTY` or `KSMETHOD`, followed by what the request carries: a pin
/// id for `KSP_PIN`, a node id for `KSP_NODE`, and so on.
pub fn request(set: u128, id: u32, flags: u32, extra: &[u32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(24 + extra.len() * 4);
    b.extend_from_slice(&guid_bytes(set));
    b.extend_from_slice(&id.to_le_bytes());
    b.extend_from_slice(&flags.to_le_bytes());
    for x in extra {
        b.extend_from_slice(&x.to_le_bytes());
    }
    // KSP_PIN and KSP_NODE end with a reserved ULONG.
    if !extra.is_empty() {
        b.extend_from_slice(&0u32.to_le_bytes());
    }
    b
}

/// A request of a node of the filter, sent through the pin controlling it.
pub fn node_request(set: u128, id: u32, flags: u32, node: u32) -> Vec<u8> {
    request(set, id, flags | TOPOLOGY, &[node])
}

/// An owned handle to a filter, a pin or an allocator, opened for
/// overlapped I/O.
pub struct Handle(HANDLE);

// SAFETY: a kernel handle may be used and closed from any thread.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and closed once.
        unsafe { CloseHandle(self.0) };
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

impl Handle {
    /// Opens a filter by the path of its device interface.
    pub fn open(path: &str) -> io::Result<Self> {
        let path = wide(path);
        // SAFETY: the path is a NUL-terminated wide string.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(handle))
    }

    /// Sends an I/O control and waits for it, returning the bytes it wrote.
    fn ioctl(&self, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<usize> {
        let event = Event::new()?;
        // SAFETY: an OVERLAPPED is plain data, zero being its initial state.
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event.0;
        let mut returned = 0;
        // SAFETY: the buffers and the OVERLAPPED outlive the request, which
        // is waited for before they go.
        let ok = unsafe {
            DeviceIoControl(
                self.0,
                code,
                input.as_ptr().cast(),
                input.len() as u32,
                if output.is_empty() {
                    null_mut()
                } else {
                    output.as_mut_ptr().cast()
                },
                output.len() as u32,
                &mut returned,
                &mut overlapped,
            )
        };
        if ok == 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(e);
            }
            // SAFETY: as above; this waits for the request to complete.
            if unsafe { GetOverlappedResult(self.0, &overlapped, &mut returned, 1) } == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(returned as usize)
    }

    pub fn get(&self, request: &[u8], value: &mut [u8]) -> io::Result<usize> {
        self.ioctl(IOCTL_KS_PROPERTY, request, value)
    }

    /// Gets a property of a size it tells first.
    pub fn get_var(&self, request: &[u8]) -> io::Result<Vec<u8>> {
        let mut size = 0;
        match self.ioctl(IOCTL_KS_PROPERTY, request, &mut []) {
            Ok(n) => size = n,
            Err(e)
                if e.raw_os_error() == Some(ERROR_MORE_DATA as i32)
                    || e.raw_os_error() == Some(ERROR_INSUFFICIENT_BUFFER as i32) => {}
            Err(e) => return Err(e),
        }
        if size == 0 {
            // The size is told through the bytes returned, which an error
            // does not carry here; ask again with room to spare.
            size = 64 * 1024;
        }
        let mut value = vec![0; size];
        let n = self.ioctl(IOCTL_KS_PROPERTY, request, &mut value)?;
        value.truncate(n);
        Ok(value)
    }

    pub fn get_u32(&self, request: &[u8]) -> io::Result<u32> {
        let mut value = [0; 4];
        self.get(request, &mut value)?;
        Ok(u32::from_le_bytes(value))
    }

    pub fn set(&self, request: &[u8], value: &[u8]) -> io::Result<()> {
        // A property to set carries its value where one to get receives it.
        let mut value = value.to_vec();
        self.ioctl(IOCTL_KS_PROPERTY, request, &mut value).map(drop)
    }

    pub fn set_u32(&self, request: &[u8], value: u32) -> io::Result<()> {
        self.set(request, &value.to_le_bytes())
    }

    pub fn method(&self, request: &[u8]) -> io::Result<()> {
        self.ioctl(IOCTL_KS_METHOD, request, &mut []).map(drop)
    }

    pub fn set_state(&self, state: State) -> io::Result<()> {
        let request = request(
            KSPROPSETID_CONNECTION,
            KSPROPERTY_CONNECTION_STATE,
            SET,
            &[],
        );
        self.set_u32(&request, state as u32)
    }

    /// The size of the frames the pin gives out, if it tells.
    pub fn frame_size(&self) -> Option<usize> {
        let request = request(
            KSPROPSETID_CONNECTION,
            KSPROPERTY_CONNECTION_ALLOCATORFRAMING_EX,
            GET,
            &[],
        );
        let b = self.get_var(&request).ok()?;
        // KSALLOCATOR_FRAMING_EX: a 24-byte header, then the first
        // KS_FRAMING_ITEM, whose KS_FRAMING_RANGE_WEIGHTED starts 68 bytes in
        // with the minimum frame size.
        let min = read_u32(b.get(..24 + 72)?, 24 + 68).try_into().ok()?;
        (read_u32(&b, 0) > 0 && min > 0).then_some(min)
    }

    /// Makes `self`, the source pin of a connection between two filters,
    /// hand its frames to `sink` through an allocator of their own.
    pub fn connect_pipe(&self, sink: &Handle) -> io::Result<Handle> {
        // KSALLOCATOR_FRAMING: system memory from the non-paged pool, one
        // frame of a page; the pins size the frames themselves.
        let framing: [u32; 6] = [2, 0, 1, 4096, 0, 0];
        let mut allocator = null_mut();
        // SAFETY: the framing is a KSALLOCATOR_FRAMING in layout.
        let r = unsafe { KsCreateAllocator(self.0, framing.as_ptr().cast(), &mut allocator) };
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r as i32));
        }
        let allocator = Handle(allocator);
        // A HANDLE, of the width of a pointer.
        let value = (allocator.0 as usize).to_le_bytes();
        for (pin, id) in [
            (self, KSPROPERTY_STREAM_PIPE_ID),
            (self, KSPROPERTY_STREAM_ALLOCATOR),
            (sink, KSPROPERTY_STREAM_PIPE_ID),
        ] {
            pin.set(&request(KSPROPSETID_STREAM, id, SET, &[]), &value)?;
        }
        Ok(allocator)
    }
}

/// A medium of a pin, which says what it connects to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Medium {
    pub set: u128,
    pub id: u32,
}

impl Medium {
    pub const STANDARD: Self = Self {
        set: KSMEDIUMSETID_STANDARD,
        id: 0,
    };
}

/// The mediums of a pin factory of `filter`.
pub fn mediums(filter: &Handle, pin: u32) -> io::Result<Vec<Medium>> {
    let b = filter.get_var(&request(
        KSPROPSETID_PIN,
        KSPROPERTY_PIN_MEDIUMS,
        GET,
        &[pin],
    ))?;
    // KSMULTIPLE_ITEM, then KSPIN_MEDIUMs of 24 bytes.
    let count = read_u32(&b, 4) as usize;
    Ok((0..count)
        .map(|i| {
            let m = &b[8 + i * 24..];
            Medium {
                set: read_guid(m),
                id: read_u32(m, 16),
            }
        })
        .collect())
}

/// The first data range of a pin factory of `filter`, a `KSDATARANGE`
/// followed by what the format adds, which serves as its data format.
pub fn first_data_range(filter: &Handle, pin: u32) -> io::Result<Vec<u8>> {
    let b = filter.get_var(&request(
        KSPROPSETID_PIN,
        KSPROPERTY_PIN_DATARANGES,
        GET,
        &[pin],
    ))?;
    let size = read_u32(&b, 8) as usize;
    let mut range = b
        .get(8..8 + size)
        .ok_or_else(|| io::Error::other("short data range"))?
        .to_vec();
    // A KSDATAFORMAT has flags where a KSDATARANGE does; none are asked for.
    range[4..8].copy_from_slice(&0u32.to_le_bytes());
    Ok(range)
}

/// Creates a pin of `filter` in `format`, connected to `to` if the pin
/// sends its frames to another filter's.
pub fn create_pin(
    filter: &Handle,
    pin: u32,
    medium: Medium,
    format: &[u8],
    to: Option<&Handle>,
) -> io::Result<Handle> {
    // KSPIN_CONNECT: the interface and the medium as KSIDENTIFIERs, the pin
    // id, the handle of the pin to connect to, and the priority; then the
    // KSDATAFORMAT.
    let mut connect = Vec::with_capacity(72 + format.len());
    connect.extend_from_slice(&guid_bytes(KSINTERFACESETID_STANDARD));
    connect.extend_from_slice(&[0; 8]);
    connect.extend_from_slice(&guid_bytes(medium.set));
    connect.extend_from_slice(&medium.id.to_le_bytes());
    connect.extend_from_slice(&0u32.to_le_bytes());
    connect.extend_from_slice(&pin.to_le_bytes());
    // The handle is aligned to its size, which leaves a gap on 64-bit
    // targets.
    if cfg!(target_pointer_width = "64") {
        connect.extend_from_slice(&0u32.to_le_bytes());
    }
    connect.extend_from_slice(&(to.map_or(null_mut(), |h| h.0) as usize).to_le_bytes());
    // KSPRIORITY_NORMAL.
    connect.extend_from_slice(&0x4000_0000u32.to_le_bytes());
    connect.extend_from_slice(&1u32.to_le_bytes());
    connect.extend_from_slice(format);

    let mut handle = null_mut();
    // SAFETY: `connect` is a KSPIN_CONNECT followed by its KSDATAFORMAT.
    let r = unsafe {
        KsCreatePin(
            filter.0,
            connect.as_ptr().cast(),
            GENERIC_READ | GENERIC_WRITE,
            &mut handle,
        )
    };
    if r != 0 {
        return Err(io::Error::from_raw_os_error(r as i32));
    }
    Ok(Handle(handle))
}

/// Creates the topology between the input and the output pin factories of
/// a BDA filter, joining its nodes up.
pub fn create_topology(filter: &Handle, input: u32, output: u32) -> io::Result<()> {
    let mut request = request(
        KSMETHODSETID_BDA_DEVICE_CONFIGURATION,
        KSMETHOD_BDA_CREATE_TOPOLOGY,
        0,
        &[],
    );
    request.extend_from_slice(&input.to_le_bytes());
    request.extend_from_slice(&output.to_le_bytes());
    filter.method(&request)
}

pub fn change(filter: &Handle, method: u32) -> io::Result<()> {
    filter.method(&request(KSMETHODSETID_BDA_CHANGE_SYNC, method, 0, &[]))
}

/// The function of each node type of a BDA filter, by node type.
pub fn node_functions(filter: &Handle) -> io::Result<Vec<(u32, u128)>> {
    let b = filter.get_var(&request(
        KSPROPSETID_BDA_TOPOLOGY,
        KSPROPERTY_BDA_NODE_DESCRIPTORS,
        GET,
        &[],
    ))?;
    // BDANODE_DESCRIPTORs: the node type, then the function and the name.
    Ok(b.as_chunks::<36>()
        .0
        .iter()
        .map(|d| (read_u32(d, 0), read_guid(&d[4..])))
        .collect())
}

/// A manual-reset event.
struct Event(HANDLE);

impl Event {
    fn new() -> io::Result<Self> {
        // SAFETY: no name or security attributes.
        let event = unsafe { CreateEventW(null(), 1, 0, null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(event))
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the event is ours.
        unsafe { CloseHandle(self.0) };
    }
}

/// `KSSTREAM_HEADER`.
#[repr(C)]
struct StreamHeader {
    size: u32,
    type_specific_flags: u32,
    presentation_time: [u64; 2],
    duration: i64,
    frame_extent: u32,
    data_used: u32,
    data: *mut c_void,
    options_flags: u32,
    #[cfg(target_pointer_width = "64")]
    reserved: u32,
}

/// A read of a frame from a pin, in flight or done.
struct Read {
    header: Box<StreamHeader>,
    data: Box<[u8]>,
    overlapped: Box<OVERLAPPED>,
    event: Event,
}

impl Read {
    fn new(size: usize) -> io::Result<Self> {
        let mut data = vec![0; size].into_boxed_slice();
        let header = Box::new(StreamHeader {
            size: size_of::<StreamHeader>() as u32,
            type_specific_flags: 0,
            presentation_time: [0; 2],
            duration: 0,
            frame_extent: size as u32,
            data_used: 0,
            data: data.as_mut_ptr().cast(),
            options_flags: 0,
            #[cfg(target_pointer_width = "64")]
            reserved: 0,
        });
        let event = Event::new()?;
        // SAFETY: an OVERLAPPED is plain data.
        let mut overlapped: Box<OVERLAPPED> = Box::new(unsafe { std::mem::zeroed() });
        overlapped.hEvent = event.0;
        Ok(Self {
            header,
            data,
            overlapped,
            event,
        })
    }

    fn submit(&mut self, pin: &Handle) -> io::Result<()> {
        self.header.data_used = 0;
        // SAFETY: the header, its data and the OVERLAPPED are boxed and
        // outlive the request, which the reader waits for or cancels before
        // they go.
        let ok = unsafe {
            DeviceIoControl(
                pin.0,
                IOCTL_KS_READ_STREAM,
                null(),
                0,
                (&mut *self.header as *mut StreamHeader).cast(),
                size_of::<StreamHeader>() as u32,
                null_mut(),
                &mut *self.overlapped,
            )
        };
        if ok == 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(e);
            }
        }
        Ok(())
    }

    /// Waits for the read, returning the bytes it got.
    fn complete(&mut self, pin: &Handle) -> io::Result<&[u8]> {
        let mut returned = 0;
        // SAFETY: the event belongs to the request being waited for.
        unsafe { WaitForSingleObject(self.event.0, INFINITE) };
        // SAFETY: as in `submit`.
        if unsafe { GetOverlappedResult(pin.0, &*self.overlapped, &mut returned, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(&self.data[..self.header.data_used as usize])
    }
}

/// Reads the frames of a pin, keeping several reads in flight so that the
/// device has somewhere to put what it receives while one is being taken.
pub struct Reader {
    pin: std::sync::Arc<Handle>,
    reads: std::collections::VecDeque<Read>,
}

// SAFETY: the reads hold kernel handles and pointers into buffers they own,
// which stay put wherever the reader is moved to.
unsafe impl Send for Reader {}

impl Reader {
    pub fn new(pin: std::sync::Arc<Handle>, frame: usize, in_flight: usize) -> io::Result<Self> {
        // Each read goes in the reader as soon as it is in flight, so that
        // failing to start the next one cancels it before its buffers go.
        let mut reader = Self {
            pin,
            reads: std::collections::VecDeque::new(),
        };
        for _ in 0..in_flight {
            let mut read = Read::new(frame)?;
            read.submit(&reader.pin)?;
            reader.reads.push_back(read);
        }
        Ok(reader)
    }

    /// The next frame; an error once the pin stops.
    pub fn next(&mut self) -> io::Result<Vec<u8>> {
        let mut read = self.reads.pop_front().expect("reads in flight");
        let result = read.complete(&self.pin).map(<[u8]>::to_vec);
        if result.is_ok() {
            read.submit(&self.pin)?;
        }
        self.reads.push_back(read);
        result
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        // The buffers go with the reads, which the device must let go of
        // first.
        // SAFETY: cancels the requests of this thread's reads on the pin.
        unsafe { CancelIoEx(self.pin.0, null()) };
        for read in &mut self.reads {
            let _ = read.complete(&self.pin);
        }
    }
}

/// A device interface of a category, with the device it belongs to.
pub struct Interface {
    /// The path to open the filter by.
    pub path: String,
    /// The `FriendlyName` of the interface.
    pub name: String,
    /// The instance id of the device.
    pub instance_id: String,
    /// The friendly name or else the description of the device.
    pub device_name: String,
}

/// The device interfaces of a category that are present.
pub fn interfaces(category: u128) -> io::Result<Vec<Interface>> {
    let category = guid(category);
    // SAFETY: SetupAPI calls on a device information set this function owns
    // and destroys, with buffers of the sizes they report.
    unsafe {
        let set = SetupDiGetClassDevsW(
            &category,
            null(),
            null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        );
        if set as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        let mut found = Vec::new();
        for index in 0.. {
            let mut data: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            data.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(set, null(), &category, index, &mut data) == 0 {
                break;
            }

            let mut needed = 0;
            SetupDiGetDeviceInterfaceDetailW(set, &data, null_mut(), 0, &mut needed, null_mut());
            // No size, as when the interface has gone since, leaves nothing
            // to write the header of the detail into.
            if (needed as usize) < size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() {
                continue;
            }
            // A buffer aligned for the structure the size is of.
            let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
            let detail = buf.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            let mut device: SP_DEVINFO_DATA = std::mem::zeroed();
            device.cbSize = size_of::<SP_DEVINFO_DATA>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(set, &data, detail, needed, null_mut(), &mut device)
                == 0
            {
                continue;
            }
            let path = from_wide(std::ptr::addr_of!((*detail).DevicePath).cast());

            let key = SetupDiOpenDeviceInterfaceRegKey(set, &data, 0, KEY_READ);
            let name = if key as isize == -1 {
                String::new()
            } else {
                let name = reg_string(key, "FriendlyName");
                RegCloseKey(key);
                name
            };

            let mut id = [0u16; 512];
            SetupDiGetDeviceInstanceIdW(set, &device, id.as_mut_ptr(), 512, null_mut());
            let device_name = [SPDRP_FRIENDLYNAME, SPDRP_DEVICEDESC]
                .into_iter()
                .map(|property| {
                    let mut value = [0u16; 512];
                    SetupDiGetDeviceRegistryPropertyW(
                        set,
                        &device,
                        property,
                        null_mut(),
                        value.as_mut_ptr().cast(),
                        1024,
                        null_mut(),
                    );
                    from_wide(value.as_ptr())
                })
                .find(|n| !n.is_empty())
                .unwrap_or_default();

            found.push(Interface {
                path,
                name,
                instance_id: from_wide(id.as_ptr()),
                device_name,
            });
        }
        SetupDiDestroyDeviceInfoList(set);
        Ok(found)
    }
}

/// A string value of an open registry key, or an empty one.
unsafe fn reg_string(key: windows_sys::Win32::System::Registry::HKEY, name: &str) -> String {
    let name = wide(name);
    let mut value = [0u16; 512];
    let mut size = 1024u32;
    // SAFETY: the buffer holds `size` bytes.
    let r = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            null(),
            null_mut(),
            value.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if r != 0 {
        return String::new();
    }
    // SAFETY: the buffer was zeroed, so the value ends with a NUL.
    unsafe { from_wide(value.as_ptr()) }
}

/// A NUL-terminated wide string.
unsafe fn from_wide(p: *const u16) -> String {
    // SAFETY: the caller hands a NUL-terminated string.
    unsafe {
        let len = (0..).take_while(|&i| *p.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
    }
}
