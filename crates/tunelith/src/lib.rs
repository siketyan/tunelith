#![doc = include_str!("../README.md")]
#![warn(missing_docs)]

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker, ready};

use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::TokioAsyncReadCompatExt;
pub use tunelith_core::proto::DEFAULT_SOCKET;
use tunelith_core::proto::{self, envelope::Body, hello::Role};
pub use tunelith_core::{
    DeviceInfo, Error, Polarization, Result, Signal, StreamFormat, StreamId, System, TuneParams,
    TunerInfo,
};

/// Where to find tunelithd: the socket of one run for the user, in
/// `$XDG_RUNTIME_DIR/tunelith/`, if there is one, or else [`DEFAULT_SOCKET`],
/// that of the system.
pub fn default_socket() -> PathBuf {
    #[cfg(unix)]
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let user = Path::new(&dir).join("tunelith/tunelithd.sock");
        if user.exists() {
            return user;
        }
    }
    PathBuf::from(DEFAULT_SOCKET)
}

/// A connection to tunelithd.
///
/// Cloning it is cheap and shares the connection; the streams acquired
/// through it keep it open while they live.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    requests: mpsc::UnboundedSender<proto::Envelope>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Body>>>,
    /// The bytes each stream lost, as tunelithd reports them.
    drops: Mutex<HashMap<u64, Arc<AtomicU64>>>,
    /// How each stream ended, once tunelithd says.
    ends: Mutex<HashMap<u64, End>>,
    next_id: AtomicU64,
}

enum End {
    /// Not yet; the waker of the read waiting to know.
    Pending(Option<Waker>),
    /// For the error, if any.
    Ended(Option<String>),
}

impl Inner {
    /// Releases a stream, not waiting for the answer.
    fn release(&self, stream_token: u64) {
        self.forget(stream_token);
        let request = proto::ReleaseRequest {
            stream_token,
            ..Default::default()
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let _ = self
            .requests
            .send(proto::envelope(id, Body::ReleaseRequest(request)));
    }

    fn forget(&self, token: u64) {
        self.drops.lock().unwrap().remove(&token);
        self.ends.lock().unwrap().remove(&token);
    }

    fn end(&self, token: u64, error: Option<String>) {
        if let Some(end) = self.ends.lock().unwrap().get_mut(&token)
            && let End::Pending(waker) = std::mem::replace(end, End::Ended(error))
            && let Some(waker) = waker
        {
            waker.wake();
        }
    }
}

/// A device tunelithd holds, as [`Client::list`] reports it.
#[derive(Clone, Debug)]
pub struct DeviceStatus {
    /// The id and the name of the device.
    pub info: DeviceInfo,
    /// The tuners of the device.
    pub tuners: Vec<TunerStatus>,
}

/// A tuner of a [`DeviceStatus`].
#[derive(Clone, Debug)]
pub struct TunerStatus {
    /// The id of the tuner, to pass as [`AcquireOptions::tuner`], and the
    /// systems it receives.
    pub info: TunerInfo,
    /// Whether a stream holds the tuner.
    pub busy: bool,
}

/// How to acquire a stream; the default takes any tuner, with the LNB off.
#[derive(Clone, Debug, Default)]
pub struct AcquireOptions {
    /// The id of the tuner to use, as in [`TunerInfo::id`]; any free one
    /// receiving the system if `None`.
    pub tuner: Option<String>,
    /// Powers the LNB of the antenna, for a satellite system when no other
    /// equipment feeds it.
    pub lnb: bool,
}

fn remote(message: String) -> Error {
    io::Error::other(message).into()
}

fn closed() -> Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "tunelithd closed the connection").into()
}

fn unexpected() -> Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "unexpected response from tunelithd",
    )
    .into()
}

/// A connection to tunelithd: a Unix domain socket, or a named pipe on
/// Windows.
#[cfg(unix)]
type Connection = tokio::net::UnixStream;
#[cfg(windows)]
type Connection = tokio::net::windows::named_pipe::NamedPipeClient;

#[cfg(unix)]
async fn open(path: &Path) -> io::Result<Connection> {
    Connection::connect(path).await
}

#[cfg(windows)]
async fn open(path: &Path) -> io::Result<Connection> {
    use tokio::net::windows::named_pipe::ClientOptions;

    // ERROR_PIPE_BUSY: every instance of the pipe is taken for the moment.
    const PIPE_BUSY: i32 = 231;
    loop {
        match ClientOptions::new().open(path) {
            Err(e) if e.raw_os_error() == Some(PIPE_BUSY) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await
            }
            result => return result,
        }
    }
}

/// Opens a connection to tunelithd as `role`.
async fn connect(path: &Path, role: Role) -> Result<Connection> {
    let mut stream = open(path).await?;
    let hello = proto::Hello {
        version: proto::VERSION,
        role: Some(role),
        ..Default::default()
    };
    stream
        .write_all(&proto::encode(&proto::envelope(0, Body::Hello(hello))))
        .await?;
    match proto::read(&mut (&mut stream).compat())
        .await?
        .and_then(|e| e.body)
    {
        Some(Body::Hello(hello)) if hello.version == proto::VERSION => Ok(stream),
        Some(Body::Error(e)) => Err(remote(e.message)),
        _ => Err(unexpected()),
    }
}

impl Client {
    /// Connects to tunelithd listening at `path`, usually
    /// [`DEFAULT_SOCKET`]: a Unix domain socket, or a named pipe on Windows.
    ///
    /// Fails if tunelithd is not there, the user may not connect to it, or it
    /// speaks another version of the protocol.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_owned();
        let stream = connect(&path, Role::Control(Default::default())).await?;
        let (read, mut write) = tokio::io::split(stream);

        let (requests, mut outgoing) = mpsc::unbounded_channel::<proto::Envelope>();
        tokio::spawn(async move {
            while let Some(envelope) = outgoing.recv().await {
                if write.write_all(&proto::encode(&envelope)).await.is_err() {
                    break;
                }
            }
        });

        let inner = Arc::new(Inner {
            path,
            requests,
            pending: Mutex::default(),
            drops: Mutex::default(),
            ends: Mutex::default(),
            next_id: AtomicU64::new(1),
        });
        let weak = Arc::downgrade(&inner);
        tokio::spawn(async move {
            let mut read = read.compat();
            while let Ok(Some(envelope)) = proto::read(&mut read).await {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                match envelope.body {
                    Some(Body::DropEvent(event)) => {
                        if let Some(dropped) = inner.drops.lock().unwrap().get(&event.stream_token)
                        {
                            dropped.fetch_add(event.dropped_bytes, Ordering::Relaxed);
                        }
                    }
                    Some(Body::StreamEndEvent(event)) => {
                        let error = (!event.error.is_empty()).then_some(event.error);
                        inner.end(event.stream_token, error);
                    }
                    Some(body) => {
                        if let Some(tx) = inner.pending.lock().unwrap().remove(&envelope.id)
                            && let Err(Body::AcquireResponse(response)) = tx.send(body)
                        {
                            // The acquire was given up on: nobody is to
                            // release the stream but us.
                            inner.release(response.stream_token);
                        }
                    }
                    None => {}
                }
            }
            // Fails the calls and the streams left waiting.
            if let Some(inner) = weak.upgrade() {
                inner.pending.lock().unwrap().clear();
                let tokens: Vec<_> = inner.ends.lock().unwrap().keys().copied().collect();
                for token in tokens {
                    inner.end(token, Some("tunelithd closed the connection".to_owned()));
                }
            }
        });

        Ok(Self { inner })
    }

    async fn call(&self, body: Body) -> Result<Body> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);
        self.inner
            .requests
            .send(proto::envelope(id, body))
            .map_err(|_| closed())?;
        match rx.await.map_err(|_| closed())? {
            Body::Error(e) => Err(remote(e.message)),
            body => Ok(body),
        }
    }

    /// Lists the devices tunelithd holds, with their tuners and whether each
    /// is in use.
    pub async fn list(&self) -> Result<Vec<DeviceStatus>> {
        let Body::ListResponse(response) = self.call(Body::ListRequest(Default::default())).await?
        else {
            return Err(unexpected());
        };
        Ok(response
            .devices
            .into_iter()
            .map(|device| DeviceStatus {
                tuners: device
                    .tuners
                    .into_iter()
                    .map(|tuner| TunerStatus {
                        info: TunerInfo {
                            id: tuner.id,
                            systems: tuner
                                .systems
                                .into_iter()
                                .filter_map(proto::system)
                                .collect(),
                        },
                        busy: tuner.busy,
                    })
                    .collect(),
                info: DeviceInfo {
                    id: device.id,
                    name: device.name,
                },
            })
            .collect())
    }

    /// Takes a stream of what `params` tunes to.
    ///
    /// tunelithd shares the tuner already receiving the same `params` for
    /// another client if there is one, and otherwise tunes a free tuner
    /// receiving `params.system`, or the one `options.tuner` names. The
    /// stream holds the tuner until dropped.
    ///
    /// # Errors
    ///
    /// Fails if `params` is not valid (see [`TuneParams::validate`]), no
    /// tuner is free, or the tuner cannot lock on the signal.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # async fn example(client: tunelith::Client) -> tunelith::Result<()> {
    /// use tunelith::{AcquireOptions, System, TuneParams};
    ///
    /// let params = TuneParams {
    ///     system: System::IsdbT,
    ///     frequency_khz: 521_143,
    ///     stream_id: None,
    ///     polarization: None,
    /// };
    /// let options = AcquireOptions {
    ///     tuner: Some("0000000001#0".to_owned()),
    ///     ..Default::default()
    /// };
    /// let stream = client.acquire(params, options).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn acquire(&self, params: TuneParams, options: AcquireOptions) -> Result<Stream> {
        params.validate()?;
        let request = proto::AcquireRequest {
            params: Some(params.into()).into(),
            tuner: options.tuner.unwrap_or_default(),
            lnb: options.lnb,
            ..Default::default()
        };
        let Body::AcquireResponse(response) = self.call(Body::AcquireRequest(request)).await?
        else {
            return Err(unexpected());
        };

        // Kept from now on, as tunelithd may report on the stream before its
        // data connection is up.
        let token = response.stream_token;
        // Releases the stream if this is given up on before it is handed out.
        let held = Held(&self.inner, token);
        let dropped = Arc::new(AtomicU64::new(0));
        self.inner
            .drops
            .lock()
            .unwrap()
            .insert(token, dropped.clone());
        self.inner
            .ends
            .lock()
            .unwrap()
            .insert(token, End::Pending(None));

        let attached = async {
            let format = proto::stream_format(response.format).ok_or_else(unexpected)?;
            let data = connect(&self.inner.path, Role::StreamToken(token)).await?;
            Ok::<_, Error>((format, data))
        }
        .await;
        let (format, data) = attached?;
        // The stream releases itself from now on.
        std::mem::forget(held);

        Ok(Stream {
            client: self.clone(),
            token,
            tuner: response.tuner,
            format,
            dropped,
            data,
        })
    }
}

struct Held<'a>(&'a Inner, u64);

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.0.release(self.1);
    }
}

/// What a tuner receives, as bytes to read, until dropped.
///
/// The bytes are in the [`StreamFormat`] of the system: MPEG-2 TS for ISDB-T
/// and ISDB-S, TLV for ISDB-S3. A read returns 0 bytes at the end of the
/// stream, and fails if the stream ended on an error.
///
/// tunelithd drops the bytes a stream is too slow to take rather than holding
/// up the other clients on the tuner; [`Stream::dropped_bytes`] counts them.
/// Dropping the stream releases the tuner.
pub struct Stream {
    client: Client,
    token: u64,
    tuner: String,
    format: StreamFormat,
    dropped: Arc<AtomicU64>,
    data: Connection,
}

impl Stream {
    /// The tuner the stream comes from.
    pub fn tuner(&self) -> &str {
        &self.tuner
    }

    /// The format of the bytes the stream gives out.
    pub fn format(&self) -> StreamFormat {
        self.format
    }

    /// The bytes lost so far for having read too slowly.
    pub fn dropped_bytes(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Asks tunelithd for the signal the tuner receives now.
    pub async fn signal(&self) -> Result<Signal> {
        let request = proto::SignalRequest {
            stream_token: self.token,
            ..Default::default()
        };
        match self.client.call(Body::SignalRequest(request)).await? {
            Body::SignalResponse(signal) => Ok((&signal).into()),
            _ => Err(unexpected()),
        }
    }
}

impl AsyncRead for Stream {
    /// Fails the read that finds the end of a stream that failed.
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let filled = buf.filled().len();
        ready!(Pin::new(&mut this.data).poll_read(cx, buf))?;
        if buf.filled().len() > filled || buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        // The end: tunelithd tells how it came on the control connection,
        // which may not be there yet.
        let mut ends = this.client.inner.ends.lock().unwrap();
        match ends.get_mut(&this.token) {
            Some(End::Pending(waker)) => {
                *waker = Some(cx.waker().clone());
                Poll::Pending
            }
            Some(End::Ended(Some(error))) => Poll::Ready(Err(io::Error::other(error.clone()))),
            Some(End::Ended(None)) | None => Poll::Ready(Ok(())),
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.client.inner.release(self.token);
    }
}
