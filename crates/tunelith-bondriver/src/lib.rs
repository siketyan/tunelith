//! BonDriver_Tunelith, a BonDriver (IBonDriver2) receiving through tunelithd.
//!
//! `CreateBonDriver` hands the host an object laid out as a C++ one: a pointer
//! to the vtable, then what Rust keeps. On x64 the member functions take
//! `this` first in the C calling convention, on Windows as on Linux. The
//! tuning spaces and channels come from a TOML file beside the library, of the
//! same name (`config.rs`).

// The crate is named after the file the hosts look for.
#![allow(non_snake_case)]

#[cfg(all(windows, target_arch = "x86"))]
compile_error!("the 32-bit Windows hosts call the member functions in thiscall, not supported");

mod config;
#[cfg_attr(windows, path = "msvc.rs")]
#[cfg_attr(not(windows), path = "itanium.rs")]
mod rtti;

use std::error::Error;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::runtime::{Handle, Runtime};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tunelith::{AcquireOptions, Client, Signal, Stream};

use config::{Config, wide};

type Bool = i32;
const TRUE: Bool = 1;
const FALSE: Bool = 0;

const WAIT_OBJECT_0: u32 = 0;
const WAIT_ABANDONED: u32 = 0x80;
const WAIT_TIMEOUT: u32 = 0x102;

/// The most bytes `GetTsStream` hands out at a time, which the buffer of a
/// host copying them is made for, as with the other BonDrivers.
const CHUNK: usize = 188 * 256;
/// The chunks kept for the host to read, a second or so; tunelithd drops what
/// comes beyond.
const QUEUE: usize = 128;

type This = *mut BonDriver;

/// The vtable of IBonDriver2, as C++ lays it out.
#[repr(C)]
pub struct Vtable {
    open_tuner: unsafe extern "C" fn(This) -> Bool,
    close_tuner: unsafe extern "C" fn(This),
    set_channel_legacy: unsafe extern "C" fn(This, u8) -> Bool,
    get_signal_level: unsafe extern "C" fn(This) -> f32,
    wait_ts_stream: unsafe extern "C" fn(This, u32) -> u32,
    get_ready_count: unsafe extern "C" fn(This) -> u32,
    // MSVC puts the overloads of a name in the reverse order of declaration.
    #[cfg(windows)]
    get_ts_stream_ptr: unsafe extern "C" fn(This, *mut *mut u8, *mut u32, *mut u32) -> Bool,
    get_ts_stream: unsafe extern "C" fn(This, *mut u8, *mut u32, *mut u32) -> Bool,
    #[cfg(not(windows))]
    get_ts_stream_ptr: unsafe extern "C" fn(This, *mut *mut u8, *mut u32, *mut u32) -> Bool,
    purge_ts_stream: unsafe extern "C" fn(This),
    release: unsafe extern "C" fn(This),
    get_tuner_name: unsafe extern "C" fn(This) -> *const u16,
    is_tuner_opening: unsafe extern "C" fn(This) -> Bool,
    enum_tuning_space: unsafe extern "C" fn(This, u32) -> *const u16,
    enum_channel_name: unsafe extern "C" fn(This, u32, u32) -> *const u16,
    set_channel: unsafe extern "C" fn(This, u32, u32) -> Bool,
    get_cur_space: unsafe extern "C" fn(This) -> u32,
    get_cur_channel: unsafe extern "C" fn(This) -> u32,
}

const VTABLE: Vtable = Vtable {
    open_tuner,
    close_tuner,
    set_channel_legacy,
    get_signal_level,
    wait_ts_stream,
    get_ready_count,
    get_ts_stream_ptr,
    get_ts_stream,
    purge_ts_stream,
    release,
    get_tuner_name,
    is_tuner_opening,
    enum_tuning_space,
    enum_channel_name,
    set_channel,
    get_cur_space,
    get_cur_channel,
};

#[repr(C)]
pub struct BonDriver {
    vtable: &'static Vtable,
    config: Config,
    name: Vec<u16>,
    /// The C/N in dB as the bits of an `f32`, updated as the stream goes.
    level: Arc<AtomicU32>,
    /// Locked before `data` when both are.
    control: Mutex<Control>,
    data: Mutex<Data>,
}

#[derive(Default)]
struct Control {
    runtime: Option<Runtime>,
    client: Option<Client>,
    receiver: Option<JoinHandle<()>>,
    current: Option<(u32, u32)>,
}

#[derive(Default)]
struct Data {
    handle: Option<Handle>,
    chunks: Option<mpsc::Receiver<Vec<u8>>>,
    /// A chunk `WaitTsStream` took.
    pending: Option<Vec<u8>>,
    /// The chunk last handed out by pointer, alive until the next one.
    last: Vec<u8>,
}

impl Data {
    fn next(&mut self) -> Option<Vec<u8>> {
        self.pending
            .take()
            .or_else(|| self.chunks.as_mut()?.try_recv().ok())
    }

    fn ready_count(&self) -> u32 {
        let queued = self.chunks.as_ref().map_or(0, |c| c.len());
        (queued + usize::from(self.pending.is_some())) as u32
    }

    fn clear(&mut self) {
        *self = Self {
            last: std::mem::take(&mut self.last),
            ..Self::default()
        };
    }
}

type Result<T, E = Box<dyn Error>> = std::result::Result<T, E>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl BonDriver {
    fn new(config: Config) -> Self {
        Self {
            vtable: &rtti::RTTI.vtable,
            config,
            name: wide("Tunelith"),
            level: Arc::default(),
            control: Mutex::default(),
            data: Mutex::default(),
        }
    }

    fn open(&self) -> Result<()> {
        let mut control = lock(&self.control);
        if control.client.is_some() {
            return Ok(());
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        let socket = (self.config.socket.clone()).unwrap_or_else(tunelith::default_socket);
        let client = runtime
            .block_on(Client::connect(&socket))
            .map_err(|e| format!("cannot reach tunelithd at {}: {e}", socket.display()))?;
        control.client = Some(client);
        control.runtime = Some(runtime);
        Ok(())
    }

    /// Leaves the channel, waiting for the stream to be released so that its
    /// tuner is free for the next.
    fn leave(&self, control: &mut Control) {
        if let (Some(runtime), Some(receiver)) = (&control.runtime, control.receiver.take()) {
            receiver.abort();
            let _ = runtime.block_on(receiver);
        }
        control.current = None;
        lock(&self.data).clear();
        self.level.store(0, Ordering::Relaxed);
    }

    fn close(&self) {
        let mut control = lock(&self.control);
        self.leave(&mut control);
        control.client = None;
        if let Some(runtime) = control.runtime.take() {
            runtime.shutdown_background();
        }
    }

    fn set_channel(&self, space: u32, channel: u32) -> Result<()> {
        let params = self
            .config
            .spaces
            .get(space as usize)
            .and_then(|s| s.channels.get(channel as usize))
            .ok_or("no such channel")?
            .params;

        let mut control = lock(&self.control);
        self.leave(&mut control);
        let (Some(runtime), Some(client)) = (&control.runtime, &control.client) else {
            return Err("the tuner is not open".into());
        };
        let options = AcquireOptions {
            tuner: None,
            lnb: self.config.lnb,
        };
        let stream = runtime.block_on(client.acquire(params, options))?;
        // The level is there once the channel is, for the hosts reading it
        // right after the tune.
        if let Ok(signal) = runtime.block_on(stream.signal()) {
            self.level.store(level_bits(&signal), Ordering::Relaxed);
        }
        let (tx, rx) = mpsc::channel(QUEUE);
        let receiver = runtime.spawn(receive(stream, tx, self.level.clone()));
        *lock(&self.data) = Data {
            handle: Some(runtime.handle().clone()),
            chunks: Some(rx),
            ..Data::default()
        };
        control.receiver = Some(receiver);
        control.current = Some((space, channel));
        Ok(())
    }

    fn wait(&self, timeout_ms: u32) -> u32 {
        let mut data = lock(&self.data);
        if data.ready_count() > 0 {
            return WAIT_OBJECT_0;
        }
        let Data {
            handle: Some(handle),
            chunks: Some(chunks),
            ..
        } = &mut *data
        else {
            return WAIT_ABANDONED;
        };
        // 0 waits for ever, as with the other BonDrivers.
        let timeout = match timeout_ms {
            0 => Duration::MAX,
            ms => Duration::from_millis(ms.into()),
        };
        // The timer is made within the runtime, which it needs.
        let received =
            handle.block_on(async { tokio::time::timeout(timeout, chunks.recv()).await });
        match received {
            Ok(Some(chunk)) => {
                data.pending = Some(chunk);
                WAIT_OBJECT_0
            }
            Ok(None) => WAIT_ABANDONED,
            Err(_) => WAIT_TIMEOUT,
        }
    }
}

/// The signal level a host is given, the C/N, as the bits of an `f32`.
fn level_bits(signal: &Signal) -> u32 {
    (signal.cnr_db.unwrap_or_default() as f32).to_bits()
}

/// Reads the stream into `tx`, and its signal into `level` every second.
async fn receive(mut stream: Stream, tx: mpsc::Sender<Vec<u8>>, level: Arc<AtomicU32>) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    let mut buf = vec![0; CHUNK];
    loop {
        tokio::select! {
            read = stream.read(&mut buf) => match read {
                Ok(n) if n > 0 => {
                    if tx.send(buf[..n].to_vec()).await.is_err() {
                        return;
                    }
                }
                _ => return,
            },
            _ = interval.tick() => {
                if let Ok(signal) = stream.signal().await {
                    level.store(level_bits(&signal), Ordering::Relaxed);
                }
            }
        }
    }
}

/// The directory and name of this library, where the configuration is.
#[cfg(unix)]
fn library_path() -> Option<PathBuf> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;

    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    let address = library_path as *const libc::c_void;
    if unsafe { libc::dladdr(address, &mut info) } == 0 || info.dli_fname.is_null() {
        return None;
    }
    let name = unsafe { CStr::from_ptr(info.dli_fname) };
    Some(std::ffi::OsStr::from_bytes(name.to_bytes()).into())
}

#[cfg(windows)]
fn library_path() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        GetModuleFileNameW, GetModuleHandleExW,
    };

    let mut module = ptr::null_mut();
    let flags =
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
    let address = library_path as *const u16;
    if unsafe { GetModuleHandleExW(flags, address, &mut module) } == 0 {
        return None;
    }
    let mut buf = vec![0; 32768];
    let len = unsafe { GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) };
    (len > 0).then(|| std::ffi::OsString::from_wide(&buf[..len as usize]).into())
}

fn create() -> Result<Box<BonDriver>> {
    let path = library_path()
        .ok_or("cannot find the library")?
        .with_extension("toml");
    Ok(Box::new(BonDriver::new(Config::load(&path)?)))
}

/// Runs `f` on the object, keeping a panic from unwinding into the host.
unsafe fn with<T>(this: This, default: T, f: impl FnOnce(&BonDriver) -> T) -> T {
    let this = unsafe { &*this };
    catch_unwind(AssertUnwindSafe(|| f(this))).unwrap_or(default)
}

/// Reports a failure where it can be seen, the host having no way to.
fn report(result: Result<()>) -> Bool {
    match result {
        Ok(()) => TRUE,
        Err(e) => {
            eprintln!("BonDriver_Tunelith: {e}");
            FALSE
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn CreateBonDriver() -> This {
    let created = catch_unwind(create).unwrap_or_else(|_| Err("panicked".into()));
    match created {
        Ok(driver) => Box::into_raw(driver),
        Err(e) => {
            eprintln!("BonDriver_Tunelith: {e}");
            ptr::null_mut()
        }
    }
}

unsafe extern "C" fn open_tuner(this: This) -> Bool {
    unsafe { with(this, FALSE, |d| report(d.open())) }
}

unsafe extern "C" fn close_tuner(this: This) {
    unsafe { with(this, (), BonDriver::close) }
}

/// The channel alone, of IBonDriver: the spaces go without saying for none.
unsafe extern "C" fn set_channel_legacy(_: This, _: u8) -> Bool {
    FALSE
}

unsafe extern "C" fn get_signal_level(this: This) -> f32 {
    unsafe {
        with(this, 0.0, |d| {
            f32::from_bits(d.level.load(Ordering::Relaxed))
        })
    }
}

unsafe extern "C" fn wait_ts_stream(this: This, timeout_ms: u32) -> u32 {
    unsafe { with(this, WAIT_ABANDONED, |d| d.wait(timeout_ms)) }
}

unsafe extern "C" fn get_ready_count(this: This) -> u32 {
    unsafe { with(this, 0, |d| lock(&d.data).ready_count()) }
}

/// Copies a chunk into `dst`, which holds [`CHUNK`] bytes.
unsafe extern "C" fn get_ts_stream(
    this: This,
    dst: *mut u8,
    size: *mut u32,
    remain: *mut u32,
) -> Bool {
    unsafe {
        with(this, FALSE, |d| {
            let mut data = lock(&d.data);
            let chunk = data.next().unwrap_or_default();
            ptr::copy_nonoverlapping(chunk.as_ptr(), dst, chunk.len());
            *size = chunk.len() as u32;
            *remain = data.ready_count();
            TRUE
        })
    }
}

unsafe extern "C" fn get_ts_stream_ptr(
    this: This,
    dst: *mut *mut u8,
    size: *mut u32,
    remain: *mut u32,
) -> Bool {
    unsafe {
        with(this, FALSE, |d| {
            let mut data = lock(&d.data);
            data.last = data.next().unwrap_or_default();
            *dst = data.last.as_mut_ptr();
            *size = data.last.len() as u32;
            *remain = data.ready_count();
            TRUE
        })
    }
}

unsafe extern "C" fn purge_ts_stream(this: This) {
    unsafe {
        with(this, (), |d| {
            let mut data = lock(&d.data);
            while data.next().is_some() {}
        })
    }
}

unsafe extern "C" fn release(this: This) {
    unsafe {
        with(this, (), BonDriver::close);
        drop(Box::from_raw(this));
    }
}

unsafe extern "C" fn get_tuner_name(this: This) -> *const u16 {
    unsafe { with(this, ptr::null(), |d| d.name.as_ptr()) }
}

unsafe extern "C" fn is_tuner_opening(this: This) -> Bool {
    unsafe { with(this, FALSE, |d| lock(&d.control).client.is_some().into()) }
}

unsafe extern "C" fn enum_tuning_space(this: This, space: u32) -> *const u16 {
    unsafe {
        with(this, ptr::null(), |d| {
            let space = d.config.spaces.get(space as usize);
            space.map_or(ptr::null(), |s| s.name.as_ptr())
        })
    }
}

unsafe extern "C" fn enum_channel_name(this: This, space: u32, channel: u32) -> *const u16 {
    unsafe {
        with(this, ptr::null(), |d| {
            let space = d.config.spaces.get(space as usize);
            let channel = space.and_then(|s| s.channels.get(channel as usize));
            channel.map_or(ptr::null(), |c| c.name.as_ptr())
        })
    }
}

unsafe extern "C" fn set_channel(this: This, space: u32, channel: u32) -> Bool {
    unsafe { with(this, FALSE, |d| report(d.set_channel(space, channel))) }
}

unsafe extern "C" fn get_cur_space(this: This) -> u32 {
    unsafe {
        with(this, u32::MAX, |d| {
            lock(&d.control).current.map_or(u32::MAX, |c| c.0)
        })
    }
}

unsafe extern "C" fn get_cur_channel(this: This) -> u32 {
    unsafe {
        with(this, u32::MAX, |d| {
            lock(&d.control).current.map_or(u32::MAX, |c| c.1)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goes through the vtable as a host would, short of tunelithd.
    #[test]
    fn vtable() {
        let config = Config::parse(
            r#"
            socket = "/nonexistent/tunelithd.sock"

            [[space]]
            name = "UHF"
            system = "isdb-t"
            channel = [{ name = "13ch", frequency = 473143 }]
            "#,
        )
        .unwrap();
        let this = Box::into_raw(Box::new(BonDriver::new(config)));
        unsafe {
            let vtable = (*this).vtable;
            assert_eq!(*(vtable.enum_tuning_space)(this, 0), 'U' as u16);
            assert!((vtable.enum_tuning_space)(this, 1).is_null());
            assert_eq!(*(vtable.enum_channel_name)(this, 0, 0), '1' as u16);
            assert_eq!((vtable.get_cur_space)(this), u32::MAX);
            assert_eq!((vtable.open_tuner)(this), FALSE);
            assert_eq!((vtable.set_channel)(this, 0, 0), FALSE);

            let (mut size, mut remain) = (1, 1);
            let mut dst = ptr::null_mut();
            let got = (vtable.get_ts_stream_ptr)(this, &mut dst, &mut size, &mut remain);
            assert_eq!((got, size, remain), (TRUE, 0, 0));
            assert_eq!((vtable.wait_ts_stream)(this, 10), WAIT_ABANDONED);
            (vtable.release)(this);
        }
    }
}
