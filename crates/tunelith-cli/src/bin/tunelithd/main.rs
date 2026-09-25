//! tunelithd: shares the tuners among programs over a Unix domain socket.
//!
//! A client takes a stream of what it asks to receive: a stream already tuned
//! to the same is shared, or else the first free tuner receiving the system
//! is taken, first come, first served. The tuner goes back once the last
//! client of its stream lets go of it.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::pin::pin;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use futures::StreamExt;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio_util::compat::TokioAsyncReadCompatExt;
use tunelith_core::proto::{self, Envelope, envelope::Body, hello::Role};
use tunelith_core::{
    ByteStream, Device, Driver, Error, Registry, Result, StreamFormat, TuneParams, Tuner,
};

use crate::listener::Listener;

mod listener;

/// The chunks a client may fall behind by before its stream loses data.
const BACKLOG: usize = 256;
/// How long a stream waits for the tuner it asks for to be closed.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// The socket to listen on.
    #[arg(long, env = "TUNELITH_SOCKET", default_value = proto::DEFAULT_SOCKET)]
    socket: PathBuf,
    /// The group whose members may use the tuners through the socket; empty
    /// for the group the daemon runs as. Unix only.
    #[arg(long, default_value = "video")]
    socket_group: String,
}

type Chunk = Arc<[u8]>;

struct Daemon {
    devices: Vec<Box<dyn Device>>,
    state: Mutex<State>,
    /// Notified when a tuner is closed.
    closed: Notify,
}

#[derive(Default)]
struct State {
    /// The ids of the tuners streams hold, or that are being closed.
    busy: HashSet<String>,
    /// The ids of the tuners being closed.
    closing: HashSet<String>,
    sessions: Vec<Arc<Session>>,
    /// The session of each stream token.
    tokens: HashMap<u64, Arc<Session>>,
    /// The data of the streams whose data connection has yet to attach.
    unattached: HashMap<u64, mpsc::Receiver<Chunk>>,
}

/// A tuner tuned to `params`, and the clients taking its stream.
struct Session {
    params: TuneParams,
    tuner_id: String,
    format: StreamFormat,
    /// Taken once the session ends.
    tuner: tokio::sync::Mutex<Option<Box<dyn Tuner>>>,
    subscribers: Mutex<HashMap<u64, Subscriber>>,
    /// Stops the fan-out once the last client has gone.
    stop: Notify,
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
    ) -> Result<proto::AcquireResponse> {
        // The token is all a data connection shows to take the stream, so it
        // must not be guessed by the other users of the socket.
        let token = loop {
            let token = getrandom::u64().map_err(io::Error::other)?;
            if token != 0 && !state.tokens.contains_key(&token) {
                break token;
            }
        };
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
        Ok(proto::AcquireResponse {
            stream_token: token,
            tuner: session.tuner_id.clone(),
            format: session.format.into(),
            ..Default::default()
        })
    }

    async fn acquire(
        self: &Arc<Self>,
        request: &proto::AcquireRequest,
        events: &mpsc::UnboundedSender<Envelope>,
    ) -> Result<proto::AcquireResponse> {
        // Normalised, so that asking for the same in other words shares it.
        let params = TuneParams::try_from(&*request.params)?.normalized();
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
                return Self::subscribe(&mut state, &session, events);
            }
        }

        // ponytail: two clients asking for the same at once may each take a
        // tuner; the second could wait on the first instead.
        let candidates: Vec<_> = self
            .devices
            .iter()
            .flat_map(|device| {
                device
                    .tuners()
                    .iter()
                    .enumerate()
                    .map(move |(index, info)| (device.as_ref(), index, info))
            })
            .filter(|(_, _, info)| info.systems.contains(&params.system) && wanted(&info.id))
            .collect();
        if candidates.is_empty() {
            let known = self
                .devices
                .iter()
                .any(|device| device.tuners().iter().any(|info| info.id == request.tuner));
            return Err(if request.tuner.is_empty() || known {
                Error::Unsupported(params.system)
            } else {
                Error::NotFound(request.tuner.clone())
            });
        }

        // A tuner being closed is waited for only once no other is free.
        for wait in [false, true] {
            for &(device, index, info) in &candidates {
                if !self.reserve(&info.id, wait).await {
                    continue;
                }

                match self.start(device, index, params, request.lnb).await {
                    Ok((tuner, format, stream)) => {
                        let session = Arc::new(Session {
                            params,
                            tuner_id: info.id.clone(),
                            format,
                            tuner: tokio::sync::Mutex::new(Some(tuner)),
                            subscribers: Mutex::default(),
                            stop: Notify::new(),
                        });
                        let mut state = self.state.lock().unwrap();
                        state.sessions.push(session.clone());
                        let response = Self::subscribe(&mut state, &session, events);
                        tokio::spawn(self.clone().fan_out(session, stream));
                        return response;
                    }
                    Err(e) => {
                        self.state.lock().unwrap().busy.remove(&info.id);
                        if e.is_busy() && request.tuner.is_empty() {
                            continue;
                        }
                        return Err(e);
                    }
                }
            }
        }

        let message = if request.tuner.is_empty() {
            format!("no free tuner receives {:?}", params.system)
        } else {
            format!("the tuner {} is busy", request.tuner)
        };
        Err(io::Error::new(io::ErrorKind::ResourceBusy, message).into())
    }

    /// Takes the tuner for a new stream, if `wait` waiting for it to be
    /// closed if it is being; whether it could.
    async fn reserve(&self, id: &str, wait: bool) -> bool {
        let reserved = async {
            loop {
                let mut closed = pin!(self.closed.notified());
                closed.as_mut().enable();
                {
                    let mut state = self.state.lock().unwrap();
                    if !state.closing.contains(id) {
                        return state.busy.insert(id.to_owned());
                    }
                    if !wait {
                        return false;
                    }
                }
                closed.await;
            }
        };
        tokio::time::timeout(CLOSE_TIMEOUT, reserved)
            .await
            .unwrap_or(false)
    }

    async fn start(
        &self,
        device: &dyn Device,
        index: usize,
        params: TuneParams,
        lnb: bool,
    ) -> Result<(Box<dyn Tuner>, StreamFormat, ByteStream)> {
        let mut tuner = device.open_tuner(index).await?;
        let started = async {
            if lnb {
                tuner.set_lnb(true).await?;
            }
            tuner.tune(params).await?;
            tuner.stream().await
        }
        .await;
        match started {
            Ok((format, stream)) => Ok((tuner, format, stream)),
            Err(e) => {
                // Closed before it is marked free again.
                tuner.close().await;
                Err(e)
            }
        }
    }

    /// Hands each chunk of the stream to the clients of the session, until
    /// none is left or the stream ends.
    async fn fan_out(self: Arc<Self>, session: Arc<Session>, mut stream: ByteStream) {
        let mut error = None;
        loop {
            let chunk = tokio::select! {
                chunk = stream.next() => chunk,
                () = session.stop.notified() => break,
            };
            let chunk = match chunk {
                Some(Ok(chunk)) => chunk,
                Some(Err(e)) => {
                    error = Some(e.to_string());
                    break;
                }
                None => break,
            };
            let chunk = Chunk::from(chunk);
            let mut subscribers = session.subscribers.lock().unwrap();
            subscribers.retain(
                |&token, subscriber| match subscriber.tx.try_send(chunk.clone()) {
                    Ok(()) => {
                        flush_drops(token, subscriber);
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
                drop(subscribers);
                if self.close_if_idle(&session) {
                    break;
                }
            }
        }
        drop(stream);
        self.end(&session, error).await;
    }

    /// Takes `session` out of the running ones if it has no client left, so
    /// that no one joins it any more; whether it did.
    fn close_if_idle(&self, session: &Arc<Session>) -> bool {
        let mut state = self.state.lock().unwrap();
        let idle = session.subscribers.lock().unwrap().is_empty();
        if idle {
            state.sessions.retain(|s| !Arc::ptr_eq(s, session));
        }
        idle
    }

    /// Ends the streams of the clients of `session`, for `error` if any,
    /// then lets go of its tuner.
    async fn end(&self, session: &Arc<Session>, error: Option<String>) {
        {
            let mut state = self.state.lock().unwrap();
            let State {
                sessions,
                tokens,
                unattached,
                closing,
                ..
            } = &mut *state;
            sessions.retain(|s| !Arc::ptr_eq(s, session));
            closing.insert(session.tuner_id.clone());
            tokens.retain(|token, s| {
                let ours = Arc::ptr_eq(s, session);
                if ours {
                    unattached.remove(token);
                }
                !ours
            });
            for (token, mut subscriber) in session.subscribers.lock().unwrap().drain() {
                flush_drops(token, &mut subscriber);
                let event = proto::StreamEndEvent {
                    stream_token: token,
                    error: error.clone().unwrap_or_default(),
                    ..Default::default()
                };
                let _ = subscriber
                    .events
                    .send(proto::envelope(0, Body::StreamEndEvent(event)));
            }
        }

        // Only once the tuner is closed is it free for another stream.
        let tuner = session.tuner.lock().await.take();
        if let Some(tuner) = tuner {
            tuner.close().await;
        }
        let mut state = self.state.lock().unwrap();
        state.busy.remove(&session.tuner_id);
        state.closing.remove(&session.tuner_id);
        self.closed.notify_waiters();
    }

    fn release(&self, token: u64) {
        let mut state = self.state.lock().unwrap();
        state.unattached.remove(&token);
        let Some(session) = state.tokens.remove(&token) else {
            return;
        };
        let mut subscribers = session.subscribers.lock().unwrap();
        subscribers.remove(&token);
        if subscribers.is_empty() {
            // Out of the running ones at once, so that no one joins it while
            // its fan-out ends it.
            state.sessions.retain(|s| !Arc::ptr_eq(s, &session));
            session.stop.notify_one();
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
        let mut tuner = session.tuner.lock().await;
        let tuner = tuner
            .as_mut()
            .ok_or_else(|| Error::NotFound(format!("stream {token}")))?;
        let signal = tuner.signal().await?;
        Ok(signal.into())
    }

    async fn handle(
        self: &Arc<Self>,
        body: Option<Body>,
        events: &mpsc::UnboundedSender<Envelope>,
        owned: &Mutex<Option<HashSet<u64>>>,
    ) -> Body {
        match body {
            Some(Body::ListRequest(_)) => Body::ListResponse(self.list()),
            Some(Body::AcquireRequest(request)) => match self.acquire(&request, events).await {
                Ok(response) => {
                    let kept = match owned.lock().unwrap().as_mut() {
                        Some(owned) => owned.insert(response.stream_token),
                        None => false,
                    };
                    // The client went while the tuner was tuning.
                    if !kept {
                        self.release(response.stream_token);
                    }
                    Body::AcquireResponse(response)
                }
                Err(e) => Body::Error((&e).into()),
            },
            Some(Body::ReleaseRequest(request)) => {
                if let Some(owned) = owned.lock().unwrap().as_mut() {
                    owned.remove(&request.stream_token);
                }
                self.release(request.stream_token);
                Body::ReleaseResponse(Default::default())
            }
            Some(Body::SignalRequest(request)) => match self.signal(request.stream_token).await {
                Ok(response) => Body::SignalResponse(response),
                Err(e) => Body::Error((&e).into()),
            },
            _ => error("unexpected request"),
        }
    }

    async fn connection(self: Arc<Self>, stream: listener::Connection) {
        let (read, mut write) = tokio::io::split(stream);
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
        mut write: impl AsyncWrite + Unpin + Send + 'static,
    ) {
        let (events, mut outgoing) = mpsc::unbounded_channel::<Envelope>();
        tokio::spawn(async move {
            while let Some(envelope) = outgoing.recv().await {
                if send(&mut write, envelope).await.is_err() {
                    break;
                }
            }
        });

        // The streams of the client, `None` once it has gone.
        let owned = Arc::new(Mutex::new(Some(HashSet::new())));
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
        let tokens = owned.lock().unwrap().take().unwrap_or_default();
        for token in tokens {
            self.release(token);
        }
    }
}

/// Tells the client what it lost since it last kept up, if anything.
fn flush_drops(token: u64, subscriber: &mut Subscriber) {
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
}

async fn send(write: &mut (impl AsyncWrite + Unpin), envelope: Envelope) -> io::Result<()> {
    write.write_all(&proto::encode(&envelope)).await
}

/// Writes the chunks of a stream to its data connection until either ends.
async fn data(mut write: impl AsyncWrite + Unpin, mut rx: mpsc::Receiver<Chunk>) {
    while let Some(chunk) = rx.recv().await {
        if write.write_all(&chunk).await.is_err() {
            break;
        }
    }
}

async fn open_devices() -> Result<Vec<Box<dyn Device>>> {
    let drivers: Vec<Box<dyn Driver>> = vec![
        Box::new(tunelith_driver_px4::driver()),
        #[cfg(target_os = "linux")]
        Box::new(tunelith_driver_pt4k::driver()),
        #[cfg(target_os = "linux")]
        Box::new(tunelith_core::dvb::DvbDriver::generic()),
    ];
    let registry = Registry::new(drivers);
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

async fn run(args: Args) -> Result<()> {
    let devices = open_devices().await?;
    let mut listener = Listener::bind(&args.socket, &args.socket_group).await?;
    eprintln!("listening on {}", args.socket.display());

    let daemon = Arc::new(Daemon {
        devices,
        state: Mutex::default(),
        closed: Notify::new(),
    });
    let mut shutdown = pin!(listener::shutdown());
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok(stream) = accepted {
                    tokio::spawn(daemon.clone().connection(stream));
                }
            }
            result = &mut shutdown => {
                result?;
                break;
            }
        }
    }

    // ponytail: the tuners are released on threads of their own, which the
    // process may end before; a clean shutdown would wait for them.
    listener.close();
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
