//! tunelithd: shares the tuners among programs over a Unix domain socket.
//!
//! A client takes a stream of what it asks to receive: a stream already tuned
//! to the same is shared, or else the first free tuner receiving the system
//! is taken, first come, first served. The tuner goes back once the last
//! client of its stream lets go of it.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use clap::Parser;
use futures::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio_util::compat::TokioAsyncReadCompatExt;
use tunelith_core::proto::{self, Envelope, envelope::Body, hello::Role};
use tunelith_core::{ByteStream, Device, Error, Registry, Result, StreamFormat, TuneParams, Tuner};

/// The chunks a client may fall behind by before its stream loses data.
const BACKLOG: usize = 256;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// The socket to listen on.
    #[arg(long, env = "TUNELITH_SOCKET", default_value = proto::DEFAULT_SOCKET)]
    socket: PathBuf,
}

type Chunk = Arc<[u8]>;

struct Daemon {
    devices: Vec<Box<dyn Device>>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The ids of the tuners streams hold.
    busy: HashSet<String>,
    sessions: Vec<Arc<Session>>,
    /// The session of each stream token.
    tokens: HashMap<u64, Arc<Session>>,
    /// The data of the streams whose data connection has yet to attach.
    unattached: HashMap<u64, mpsc::Receiver<Chunk>>,
    next_token: u64,
}

/// A tuner tuned to `params`, and the clients taking its stream.
struct Session {
    params: TuneParams,
    tuner_id: String,
    format: StreamFormat,
    tuner: tokio::sync::Mutex<Box<dyn Tuner>>,
    subscribers: Mutex<HashMap<u64, Subscriber>>,
}

struct Subscriber {
    tx: mpsc::Sender<Chunk>,
    /// The control connection of the client, for events.
    events: mpsc::UnboundedSender<Envelope>,
    /// The bytes dropped since the client last kept up.
    dropped: u64,
}

fn error(message: impl ToString) -> Body {
    Body::Error(proto::Error {
        message: message.to_string(),
        ..Default::default()
    })
}

fn is_busy(e: &Error) -> bool {
    matches!(e, Error::Io(e) if e.kind() == io::ErrorKind::ResourceBusy)
}

impl Daemon {
    fn list(&self) -> proto::ListResponse {
        let state = self.state.lock().unwrap();
        proto::ListResponse {
            devices: self
                .devices
                .iter()
                .map(|device| proto::Device {
                    id: device.info().id.clone(),
                    name: device.info().name.clone(),
                    tuners: device
                        .tuners()
                        .iter()
                        .map(|tuner| proto::Tuner {
                            id: tuner.id.clone(),
                            systems: tuner.systems.iter().map(|&s| s.into()).collect(),
                            busy: state.busy.contains(&tuner.id),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    /// Adds a client to `session`.
    fn subscribe(
        state: &mut State,
        session: &Arc<Session>,
        events: &mpsc::UnboundedSender<Envelope>,
    ) -> proto::AcquireResponse {
        state.next_token += 1;
        let token = state.next_token;
        let (tx, rx) = mpsc::channel(BACKLOG);
        session.subscribers.lock().unwrap().insert(
            token,
            Subscriber {
                tx,
                events: events.clone(),
                dropped: 0,
            },
        );
        state.tokens.insert(token, session.clone());
        state.unattached.insert(token, rx);
        proto::AcquireResponse {
            stream_token: token,
            tuner: session.tuner_id.clone(),
            format: session.format.into(),
            ..Default::default()
        }
    }

    async fn acquire(
        self: &Arc<Self>,
        request: &proto::AcquireRequest,
        events: &mpsc::UnboundedSender<Envelope>,
    ) -> Result<proto::AcquireResponse> {
        let params = TuneParams::try_from(&*request.params)?;
        params.validate()?;
        let wanted = |id: &str| request.tuner.is_empty() || request.tuner == id;

        {
            let mut state = self.state.lock().unwrap();
            let shared = state
                .sessions
                .iter()
                .find(|s| s.params == params && wanted(&s.tuner_id))
                .cloned();
            if let Some(session) = shared {
                return Ok(Self::subscribe(&mut state, &session, events));
            }
        }

        // ponytail: two clients asking for the same at once may each take a
        // tuner; the second could wait on the first instead.
        for device in &self.devices {
            for (index, info) in device.tuners().iter().enumerate() {
                if !info.systems.contains(&params.system) || !wanted(&info.id) {
                    continue;
                }
                if !self.state.lock().unwrap().busy.insert(info.id.clone()) {
                    continue;
                }

                match self
                    .start(device.as_ref(), index, params, request.lnb)
                    .await
                {
                    Ok((tuner, format, stream)) => {
                        let session = Arc::new(Session {
                            params,
                            tuner_id: info.id.clone(),
                            format,
                            tuner: tokio::sync::Mutex::new(tuner),
                            subscribers: Mutex::default(),
                        });
                        let mut state = self.state.lock().unwrap();
                        state.sessions.push(session.clone());
                        let response = Self::subscribe(&mut state, &session, events);
                        tokio::spawn(self.clone().fan_out(session, stream));
                        return Ok(response);
                    }
                    Err(e) => {
                        self.state.lock().unwrap().busy.remove(&info.id);
                        if is_busy(&e) && request.tuner.is_empty() {
                            continue;
                        }
                        return Err(e);
                    }
                }
            }
        }

        Err(if request.tuner.is_empty() {
            io::Error::other(format!("no free tuner receives {:?}", params.system)).into()
        } else {
            io::Error::other(format!("the tuner {} is busy or unknown", request.tuner)).into()
        })
    }

    async fn start(
        &self,
        device: &dyn Device,
        index: usize,
        params: TuneParams,
        lnb: bool,
    ) -> Result<(Box<dyn Tuner>, StreamFormat, ByteStream)> {
        let mut tuner = device.open_tuner(index).await?;
        if lnb {
            tuner.set_lnb(true).await?;
        }
        tuner.tune(params).await?;
        let (format, stream) = tuner.stream().await?;
        Ok((tuner, format, stream))
    }

    /// Hands each chunk of the stream to the clients of the session, until
    /// none is left or the stream ends.
    async fn fan_out(self: Arc<Self>, session: Arc<Session>, mut stream: ByteStream) {
        while let Some(Ok(chunk)) = stream.next().await {
            let chunk = Chunk::from(chunk);
            let mut subscribers = session.subscribers.lock().unwrap();
            subscribers.retain(
                |&token, subscriber| match subscriber.tx.try_send(chunk.clone()) {
                    Ok(()) => {
                        if subscriber.dropped > 0 {
                            let event = proto::DropEvent {
                                stream_token: token,
                                dropped_bytes: std::mem::take(&mut subscriber.dropped),
                                ..Default::default()
                            };
                            let _ = subscriber
                                .events
                                .send(proto::envelope(0, Body::DropEvent(event)));
                        }
                        true
                    }
                    Err(TrySendError::Full(_)) => {
                        subscriber.dropped += chunk.len() as u64;
                        true
                    }
                    Err(TrySendError::Closed(_)) => false,
                },
            );
            if subscribers.is_empty() {
                break;
            }
        }
        self.end(&session);
    }

    /// Lets go of the tuner of `session`, ending the streams of its clients.
    fn end(&self, session: &Arc<Session>) {
        let mut state = self.state.lock().unwrap();
        if let Some(i) = state.sessions.iter().position(|s| Arc::ptr_eq(s, session)) {
            state.sessions.remove(i);
            state.busy.remove(&session.tuner_id);
        }
        let tokens: Vec<_> = state
            .tokens
            .iter()
            .filter(|(_, s)| Arc::ptr_eq(s, session))
            .map(|(&t, _)| t)
            .collect();
        for token in tokens {
            state.tokens.remove(&token);
            state.unattached.remove(&token);
        }
        session.subscribers.lock().unwrap().clear();
    }

    fn release(&self, token: u64) {
        let session = {
            let mut state = self.state.lock().unwrap();
            state.unattached.remove(&token);
            state.tokens.remove(&token)
        };
        if let Some(session) = session {
            let mut subscribers = session.subscribers.lock().unwrap();
            subscribers.remove(&token);
            if subscribers.is_empty() {
                drop(subscribers);
                self.end(&session);
            }
        }
    }

    async fn signal(&self, token: u64) -> Result<proto::SignalResponse> {
        let session = self
            .state
            .lock()
            .unwrap()
            .tokens
            .get(&token)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("stream {token}")))?;
        let signal = session.tuner.lock().await.signal().await?;
        Ok(signal.into())
    }

    async fn handle(
        self: &Arc<Self>,
        body: Option<Body>,
        events: &mpsc::UnboundedSender<Envelope>,
        owned: &Mutex<HashSet<u64>>,
    ) -> Body {
        match body {
            Some(Body::ListRequest(_)) => Body::ListResponse(self.list()),
            Some(Body::AcquireRequest(request)) => match self.acquire(&request, events).await {
                Ok(response) => {
                    owned.lock().unwrap().insert(response.stream_token);
                    // The client may have gone while the tuner was tuning.
                    if events.is_closed() {
                        self.release(response.stream_token);
                    }
                    Body::AcquireResponse(response)
                }
                Err(e) => error(e),
            },
            Some(Body::ReleaseRequest(request)) => {
                owned.lock().unwrap().remove(&request.stream_token);
                self.release(request.stream_token);
                Body::ReleaseResponse(Default::default())
            }
            Some(Body::SignalRequest(request)) => match self.signal(request.stream_token).await {
                Ok(response) => Body::SignalResponse(response),
                Err(e) => error(e),
            },
            _ => error("unexpected request"),
        }
    }

    async fn connection(self: Arc<Self>, stream: UnixStream) {
        let (read, mut write) = stream.into_split();
        let mut read = read.compat();
        let role = match proto::read(&mut read).await {
            Ok(Some(Envelope {
                body: Some(Body::Hello(hello)),
                ..
            })) if hello.version == proto::VERSION => hello.role,
            _ => {
                let message = format!("expected a hello of version {}", proto::VERSION);
                let _ = send(&mut write, proto::envelope(0, error(message))).await;
                return;
            }
        };
        let hello = proto::Hello {
            version: proto::VERSION,
            ..Default::default()
        };

        match role {
            Some(Role::Control(_)) => {
                if send(&mut write, proto::envelope(0, Body::Hello(hello)))
                    .await
                    .is_ok()
                {
                    self.control(read, write).await;
                }
            }
            Some(Role::StreamToken(token)) => {
                let rx = self.state.lock().unwrap().unattached.remove(&token);
                let Some(rx) = rx else {
                    let _ = send(&mut write, proto::envelope(0, error("unknown stream"))).await;
                    return;
                };
                if send(&mut write, proto::envelope(0, Body::Hello(hello)))
                    .await
                    .is_ok()
                {
                    data(write, rx).await;
                }
                self.release(token);
            }
            _ => {}
        }
    }

    async fn control(
        self: Arc<Self>,
        mut read: impl futures::AsyncRead + Unpin,
        mut write: OwnedWriteHalf,
    ) {
        let (events, mut outgoing) = mpsc::unbounded_channel::<Envelope>();
        tokio::spawn(async move {
            while let Some(envelope) = outgoing.recv().await {
                if send(&mut write, envelope).await.is_err() {
                    break;
                }
            }
        });

        let owned = Arc::new(Mutex::new(HashSet::new()));
        while let Ok(Some(envelope)) = proto::read(&mut read).await {
            let daemon = self.clone();
            let events = events.clone();
            let owned = owned.clone();
            tokio::spawn(async move {
                let body = daemon.handle(envelope.body, &events, &owned).await;
                let _ = events.send(proto::envelope(envelope.id, body));
            });
        }

        // The client has gone: its streams go with it.
        let tokens: Vec<_> = owned.lock().unwrap().drain().collect();
        for token in tokens {
            self.release(token);
        }
    }
}

async fn send(write: &mut OwnedWriteHalf, envelope: Envelope) -> io::Result<()> {
    write.write_all(&proto::encode(&envelope)).await
}

/// Writes the chunks of a stream to its data connection until either ends.
async fn data(mut write: OwnedWriteHalf, mut rx: mpsc::Receiver<Chunk>) {
    while let Some(chunk) = rx.recv().await {
        if write.write_all(&chunk).await.is_err() {
            break;
        }
    }
}

async fn open_devices() -> Result<Vec<Box<dyn Device>>> {
    let registry = Registry::new(vec![
        Box::new(tunelith_driver_px4::driver()),
        Box::new(tunelith_driver_pt4k::driver()),
        Box::new(tunelith_core::dvb::DvbDriver::generic()),
    ]);
    let mut devices = Vec::new();
    for found in registry.probe().await? {
        match registry.open(&found).await {
            Ok(device) => {
                eprintln!(
                    "{} ({}): {} tuners",
                    found.info.name,
                    found.info.id,
                    device.tuners().len()
                );
                devices.push(device);
            }
            Err(e) => eprintln!("skipping {} ({}): {e}", found.info.name, found.info.id),
        }
    }
    Ok(devices)
}

/// Binds the socket, taking over a stale one but not one in use.
async fn bind(path: &Path) -> io::Result<UnixListener> {
    if UnixStream::connect(path).await.is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("tunelithd is already listening on {}", path.display()),
        ));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    UnixListener::bind(path)
}

async fn run(args: Args) -> Result<()> {
    let devices = open_devices().await?;
    let listener = bind(&args.socket).await?;
    eprintln!("listening on {}", args.socket.display());

    let daemon = Arc::new(Daemon {
        devices,
        state: Mutex::default(),
    });
    let mut terminate = signal(SignalKind::terminate())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    tokio::spawn(daemon.clone().connection(stream));
                }
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
        }
    }

    // ponytail: the tuners are released on threads of their own, which the
    // process may end before; a clean shutdown would wait for them.
    let _ = std::fs::remove_file(&args.socket);
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Args::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
