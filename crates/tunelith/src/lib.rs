//! The client of tunelithd, the daemon that shares the tuners among programs.
//!
//! A [`Client`] asks for a [`Stream`] of what to receive; tunelithd picks a
//! free tuner for it, or shares the one another client is receiving the same
//! on. The client needs a Tokio runtime.

// ponytail: Unix domain sockets alone; Windows is to take a named pipe.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::TokioAsyncReadCompatExt;
pub use tunelith_core::proto::DEFAULT_SOCKET;
use tunelith_core::proto::{self, envelope::Body, hello::Role};
pub use tunelith_core::{
    DeviceInfo, Error, Polarization, Result, Signal, StreamFormat, StreamId, System, TuneParams,
    TunerInfo,
};

/// A connection to tunelithd.
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
    next_id: AtomicU64,
}

#[derive(Clone, Debug)]
pub struct DeviceStatus {
    pub info: DeviceInfo,
    pub tuners: Vec<TunerStatus>,
}

#[derive(Clone, Debug)]
pub struct TunerStatus {
    pub info: TunerInfo,
    /// Whether a stream holds the tuner.
    pub busy: bool,
}

#[derive(Clone, Debug, Default)]
pub struct AcquireOptions {
    /// The tuner to use; any free one receiving the system if `None`.
    pub tuner: Option<String>,
    /// Powers the LNB of the antenna.
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

/// Opens a connection to tunelithd as `role`.
async fn connect(path: &Path, role: Role) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(path).await?;
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
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_owned();
        let stream = connect(&path, Role::Control(Default::default())).await?;
        let (read, mut write) = stream.into_split();

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
                    Some(body) => {
                        if let Some(tx) = inner.pending.lock().unwrap().remove(&envelope.id) {
                            let _ = tx.send(body);
                        }
                    }
                    None => {}
                }
            }
            // Fails the calls left waiting.
            if let Some(inner) = weak.upgrade() {
                inner.pending.lock().unwrap().clear();
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

    /// Releases a stream, not waiting for the answer.
    fn release(&self, stream_token: u64) {
        let request = proto::ReleaseRequest {
            stream_token,
            ..Default::default()
        };
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let _ = self
            .inner
            .requests
            .send(proto::envelope(id, Body::ReleaseRequest(request)));
    }

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

        let token = response.stream_token;
        let attached = async {
            let format = proto::stream_format(response.format).ok_or_else(unexpected)?;
            let data = connect(&self.inner.path, Role::StreamToken(token)).await?;
            Ok::<_, Error>((format, data))
        }
        .await;
        let (format, data) = match attached {
            Ok(attached) => attached,
            Err(e) => {
                self.release(token);
                return Err(e);
            }
        };

        let dropped = Arc::new(AtomicU64::new(0));
        self.inner
            .drops
            .lock()
            .unwrap()
            .insert(token, dropped.clone());
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

/// What a tuner receives, as bytes to read, until dropped.
pub struct Stream {
    client: Client,
    token: u64,
    tuner: String,
    format: StreamFormat,
    dropped: Arc<AtomicU64>,
    data: UnixStream,
}

impl Stream {
    /// The tuner the stream comes from.
    pub fn tuner(&self) -> &str {
        &self.tuner
    }

    pub fn format(&self) -> StreamFormat {
        self.format
    }

    /// The bytes lost so far for having read too slowly.
    pub fn dropped_bytes(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

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
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().data).poll_read(cx, buf)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.client.inner.drops.lock().unwrap().remove(&self.token);
        self.client.release(self.token);
    }
}
