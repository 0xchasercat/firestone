use std::{
    convert::Infallible,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt},
        net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream},
    },
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    http::{Request, Response, StatusCode, header},
};
use firestone_core::{
    Action, DispatchFuture, Dispatcher, ErrorKind, EventSink, FirestoneError, GlobalConfig, Paths,
};
use futures_util::FutureExt as _;
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use nix::{
    errno::Errno,
    fcntl::{AtFlags, Flock, FlockArg, OFlag, openat},
    sys::{
        socket::UnixAddr,
        stat::{FileStat, Mode, SFlag, fstatat, umask},
    },
    unistd::{UnlinkatFlags, unlinkat},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream, UnixListener, UnixStream},
    sync::watch,
    task::JoinSet,
};
use tower::ServiceExt as _;

use crate::{
    api,
    ui::auth::{self, UiAuth},
};

const GRACEFUL_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const ACTION_SAFE_POINT_TIMEOUT: Duration = Duration::from_secs(3_610);
const RUNTIME_TASK_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const INTERNAL_ERROR_BODY: &str = concat!(
    "{\"error\":{\"kind\":\"generic\",\"message\":\"internal server error\",",
    "\"hint\":\"retry the request; if it fails again, report a Firestone bug\"}}"
);

/// Where the stateless adapter should publish itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeListener {
    /// A securely published Unix socket inside the private runtime directory.
    Unix(PathBuf),
    /// A loopback TCP port. Only `127.0.0.1` and `::1` are bindable.
    Loopback { addr: SocketAddr },
}

/// The listener as it actually exists after a successful bind.
///
/// A value of this type can only be produced by a bind that succeeded, so the
/// `ready` callback can never announce a URL for a listener that is not there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundListener {
    /// The published Unix socket path.
    Unix(PathBuf),
    /// The resolved loopback address, including a kernel-chosen port.
    Loopback(SocketAddr),
}

/// Called once after a successful bind and before the first accept.
pub type ReadyCallback<'a> = &'a mut dyn FnMut(&BoundListener) -> Result<(), FirestoneError>;

/// Runs the stateless REST and UI adapter on one listener.
///
/// `ready` runs after the listener is bound and before the accept loop starts,
/// so a printed URL is always reachable and is never printed for a failed bind.
pub fn run(
    paths: &Paths,
    listener: ServeListener,
    dispatcher: Arc<dyn Dispatcher>,
    config: &GlobalConfig,
    auth: UiAuth,
    ready: Option<ReadyCallback<'_>>,
) -> Result<(), FirestoneError> {
    let signals = ShutdownSignals::register()?;
    let shutdown = signals.flag();
    let activity = Arc::new(DispatchActivity::default());
    let tracked: Arc<dyn Dispatcher> = Arc::new(TrackedDispatcher {
        inner: dispatcher,
        activity: Arc::clone(&activity),
    });

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("firestone-serve")
        .build()
        .map_err(|source| {
            FirestoneError::new(
                ErrorKind::Dependency,
                "cannot create the Tokio serve runtime",
            )
            .with_hint("check the host thread and file-descriptor limits")
            .with_source(source)
        })?;

    let mut socket = None;
    let (accepted, bound) = match listener {
        ServeListener::Unix(path) => {
            if !auth.is_trusted() {
                return Err(FirestoneError::new(
                    ErrorKind::Usage,
                    "a session token cannot be used with a unix: listener",
                )
                .with_hint("a Unix socket is authenticated by its mode 0600; drop --token"));
            }
            let mut bound = BoundSocket::bind(paths, &path)?;
            let listener = bound.take_listener()?;
            let guard = runtime.enter();
            let listener = UnixListener::from_std(listener).map_err(|source| {
                listener_error(
                    ErrorKind::Generic,
                    "cannot attach the Unix listener to the Tokio runtime",
                    source,
                )
            })?;
            drop(guard);
            socket = Some(bound);
            (Listener::Unix(listener), BoundListener::Unix(path))
        }
        ServeListener::Loopback { addr } => {
            if auth.is_trusted() {
                return Err(FirestoneError::new(
                    ErrorKind::Usage,
                    "a loopback TCP listener always requires a session token",
                )
                .with_hint("pass --token FILE, or use `firestone ui`"));
            }
            let addr = require_loopback(addr)?;
            let listener = runtime
                .block_on(async move { TcpListener::bind(addr).await })
                .map_err(|source| {
                    let kind = if source.kind() == io::ErrorKind::AddrInUse {
                        ErrorKind::Conflict
                    } else {
                        ErrorKind::Dependency
                    };
                    FirestoneError::new(
                        kind,
                        format!("cannot bind the loopback listener at '{addr}'"),
                    )
                    .with_hint("choose a free loopback port, or pass port 0 for any free port")
                    .with_source(source)
                })?;
            let local = listener.local_addr().map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Generic,
                    "cannot read back the bound loopback address",
                )
                .with_hint("retry; if it fails again, report a Firestone bug")
                .with_source(source)
            })?;
            (Listener::Tcp(listener), BoundListener::Loopback(local))
        }
    };

    let gate = match &bound {
        BoundListener::Unix(_) => None,
        BoundListener::Loopback(address) => auth.gate(*address),
    };
    let app = auth::secured(
        merged_router(
            api::router(Arc::clone(&tracked), config, paths),
            crate::ui::router(tracked, config, paths),
        ),
        gate,
    );

    if let Some(ready) = ready {
        if let Err(error) = ready(&bound) {
            if let Some(socket) = socket.as_mut() {
                let _ = socket.cleanup();
            }
            return Err(error);
        }
    }

    let server_result = runtime.block_on(serve_until_shutdown(
        accepted,
        app,
        wait_for_shutdown(shutdown),
        GRACEFUL_DRAIN_TIMEOUT,
    ));

    let actions_drained = activity.wait_until_idle(ACTION_SAFE_POINT_TIMEOUT);
    runtime.shutdown_timeout(RUNTIME_TASK_DRAIN_TIMEOUT);
    let cleanup_result = match socket.as_mut() {
        Some(socket) => socket.cleanup(),
        None => Ok(()),
    };

    server_result?;
    if !actions_drained {
        return Err(FirestoneError::new(
            ErrorKind::Timeout,
            "serve shutdown timed out while an action was reaching its safe point",
        )
        .with_hint("inspect the machine and image state before restarting serve"));
    }
    cleanup_result
}

/// Rejects every bind address that is not a loopback literal.
///
/// This is a hard security boundary: no flag may bypass it, because a
/// routable bind would expose the token-gated UI and the whole `/v1` surface
/// to the local network.
fn require_loopback(addr: SocketAddr) -> Result<SocketAddr, FirestoneError> {
    let loopback = match addr.ip() {
        IpAddr::V4(address) => address.is_loopback(),
        IpAddr::V6(address) => address.is_loopback(),
    };
    if loopback {
        return Ok(addr);
    }
    Err(FirestoneError::new(
        ErrorKind::Usage,
        format!("serve listener address '{addr}' is not a loopback address"),
    )
    .with_hint(
        "use tcp:127.0.0.1:PORT or tcp:[::1]:PORT; Firestone never binds a routable address",
    ))
}

/// Joins the REST and UI routers while keeping both 404 contracts.
///
/// `Router::merge` cannot be used: both routers set their own fallback, and
/// merging two fallbacks panics. Instead every request enters a fallback
/// service that picks the router by path prefix, so `/v1` keeps the JSON
/// `ErrorEnvelope` 404 and every other path keeps the UI's own fallback.
fn merged_router(api: Router, ui: Router) -> Router {
    Router::new().fallback_service(tower::service_fn(move |request: Request<Body>| {
        let api = api.clone();
        let ui = ui.clone();
        async move {
            let path = request.uri().path();
            if path == "/v1" || path.starts_with("/v1/") {
                api.oneshot(request).await
            } else {
                ui.oneshot(request).await
            }
        }
    }))
}

struct ShutdownSignals {
    triggered: Arc<AtomicBool>,
    ids: Vec<signal_hook::SigId>,
}

impl ShutdownSignals {
    fn register() -> Result<Self, FirestoneError> {
        let triggered = Arc::new(AtomicBool::new(false));
        let mut ids = Vec::with_capacity(2);
        for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
            match signal_hook::flag::register(signal, Arc::clone(&triggered)) {
                Ok(id) => ids.push(id),
                Err(source) => {
                    for id in ids {
                        signal_hook::low_level::unregister(id);
                    }
                    return Err(FirestoneError::new(
                        ErrorKind::Dependency,
                        "cannot install the serve shutdown signal handlers",
                    )
                    .with_hint("check the process signal limits and retry")
                    .with_source(source));
                }
            }
        }
        Ok(Self { triggered, ids })
    }

    fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.triggered)
    }
}

impl Drop for ShutdownSignals {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

async fn wait_for_shutdown(triggered: Arc<AtomicBool>) -> Result<(), FirestoneError> {
    while !triggered.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

/// One bound listener the shared accept loop can drive.
enum Listener {
    Unix(UnixListener),
    Tcp(TcpListener),
}

impl From<UnixListener> for Listener {
    fn from(listener: UnixListener) -> Self {
        Self::Unix(listener)
    }
}

impl From<TcpListener> for Listener {
    fn from(listener: TcpListener) -> Self {
        Self::Tcp(listener)
    }
}

impl Listener {
    async fn accept(&self) -> io::Result<Connection> {
        match self {
            Self::Unix(listener) => listener
                .accept()
                .await
                .map(|(stream, _)| Connection::Unix(stream)),
            Self::Tcp(listener) => listener.accept().await.map(|(stream, _)| {
                // Nagle would stall the small NDJSON frames the UI polls for.
                let _ = stream.set_nodelay(true);
                Connection::Tcp(stream)
            }),
        }
    }

    const fn accept_failure(&self) -> &'static str {
        match self {
            Self::Unix(_) => "the Unix listener failed while accepting a request",
            Self::Tcp(_) => "the loopback listener failed while accepting a request",
        }
    }
}

/// One accepted connection, on either transport.
///
/// Both variants are `Unpin`, so the delegating projections below need no
/// pin projection helper and no `unsafe` block.
enum Connection {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl AsyncRead for Connection {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Unix(stream) => Pin::new(stream).poll_read(context, buffer),
            Self::Tcp(stream) => Pin::new(stream).poll_read(context, buffer),
        }
    }
}

impl AsyncWrite for Connection {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Unix(stream) => Pin::new(stream).poll_write(context, buffer),
            Self::Tcp(stream) => Pin::new(stream).poll_write(context, buffer),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Unix(stream) => Pin::new(stream).poll_flush(context),
            Self::Tcp(stream) => Pin::new(stream).poll_flush(context),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Unix(stream) => Pin::new(stream).poll_shutdown(context),
            Self::Tcp(stream) => Pin::new(stream).poll_shutdown(context),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Unix(stream) => Pin::new(stream).poll_write_vectored(context, buffers),
            Self::Tcp(stream) => Pin::new(stream).poll_write_vectored(context, buffers),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Self::Unix(stream) => stream.is_write_vectored(),
            Self::Tcp(stream) => stream.is_write_vectored(),
        }
    }
}

async fn serve_until_shutdown<L, F>(
    listener: L,
    app: Router,
    shutdown: F,
    drain_timeout: Duration,
) -> Result<(), FirestoneError>
where
    L: Into<Listener>,
    F: Future<Output = Result<(), FirestoneError>> + Send,
{
    let listener = listener.into();
    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let mut connections = JoinSet::new();
    tokio::pin!(shutdown);

    let outcome = loop {
        tokio::select! {
            biased;
            result = &mut shutdown => break result,
            accepted = listener.accept() => {
                match accepted {
                    Ok(stream) => {
                        connections.spawn(serve_connection(
                            stream,
                            app.clone(),
                            shutdown_receiver.clone(),
                        ));
                    }
                    Err(source)
                        if matches!(
                            source.kind(),
                            io::ErrorKind::Interrupted
                                | io::ErrorKind::ConnectionAborted
                                | io::ErrorKind::ConnectionReset
                        ) => {}
                    Err(source) => {
                        break Err(listener_error(
                            ErrorKind::Generic,
                            listener.accept_failure(),
                            source,
                        ));
                    }
                }
            }
            joined = connections.join_next(), if !connections.is_empty() => {
                // A broken client or panicking request owns only its connection task.
                let _ = joined;
            }
        }
    };

    drop(listener);
    let _ = shutdown_sender.send(true);
    let drained = tokio::time::timeout(drain_timeout, async {
        while connections.join_next().await.is_some() {}
    })
    .await
    .is_ok();
    if !drained {
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }

    outcome
}

async fn serve_connection(stream: Connection, app: Router, mut shutdown: watch::Receiver<bool>) {
    let service = service_fn(move |request: Request<Incoming>| {
        dispatch_request(app.clone(), request.map(Body::new))
    });
    let io = TokioIo::new(stream);
    let builder = http1::Builder::new();
    // `with_upgrades` is what lets a handler answer 101 and take the socket:
    // it is required by the WebSocket console and shell transports. The
    // returned `UpgradeableConnection` keeps `graceful_shutdown`, so the
    // shutdown select below is unchanged. An attached terminal has no idle
    // point, so it holds its connection until the drain timeout aborts it.
    let connection = builder.serve_connection(io, service).with_upgrades();
    tokio::pin!(connection);

    if *shutdown.borrow() {
        connection.as_mut().graceful_shutdown();
        let _ = connection.await;
        return;
    }

    tokio::select! {
        _ = &mut connection => {}
        _ = shutdown.changed() => {
            connection.as_mut().graceful_shutdown();
            let _ = connection.await;
        }
    }
}

async fn dispatch_request(
    app: Router,
    request: Request<Body>,
) -> Result<Response<Body>, Infallible> {
    match AssertUnwindSafe(app.oneshot(request)).catch_unwind().await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(error)) => match error {},
        Err(_) => Ok(internal_error_response()),
    }
}

fn internal_error_response() -> Response<Body> {
    let mut response = Response::new(Body::from(INTERNAL_ERROR_BODY));
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    response
}

#[derive(Default)]
struct DispatchActivity {
    count: Mutex<usize>,
    idle: Condvar,
}

impl DispatchActivity {
    fn start(self: &Arc<Self>) -> DispatchGuard {
        let mut count = lock_recover(&self.count);
        *count = count.saturating_add(1);
        drop(count);
        DispatchGuard {
            activity: Arc::clone(self),
        }
    }

    fn wait_until_idle(&self, timeout: Duration) -> bool {
        let count = lock_recover(&self.count);
        match self
            .idle
            .wait_timeout_while(count, timeout, |count| *count != 0)
        {
            Ok((count, _)) => *count == 0,
            Err(poisoned) => {
                let (count, _) = poisoned.into_inner();
                *count == 0
            }
        }
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

struct DispatchGuard {
    activity: Arc<DispatchActivity>,
}

impl Drop for DispatchGuard {
    fn drop(&mut self) {
        let mut count = lock_recover(&self.activity.count);
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.activity.idle.notify_all();
        }
    }
}

struct TrackedDispatcher {
    inner: Arc<dyn Dispatcher>,
    activity: Arc<DispatchActivity>,
}

impl Dispatcher for TrackedDispatcher {
    fn run<'a>(&'a self, action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a> {
        Box::pin(async move {
            let _guard = self.activity.start();
            self.inner.run(action, events).await
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    fn from_stat(stat: &FileStat) -> Self {
        #[cfg(target_os = "macos")]
        let device = stat.st_dev as u64;
        #[cfg(not(target_os = "macos"))]
        let device = stat.st_dev;
        Self {
            device,
            inode: stat.st_ino,
        }
    }
}

struct RuntimeDirectory {
    file: File,
    identity: Identity,
    path: PathBuf,
    uid: u32,
}

impl RuntimeDirectory {
    fn open(paths: &Paths) -> Result<Self, FirestoneError> {
        let path = paths.runtime_dir().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .open(&path)
            .map_err(|source| runtime_error("open", &path, source))?;
        let metadata = file
            .metadata()
            .map_err(|source| runtime_error("inspect opened", &path, source))?;
        let path_metadata = fs::symlink_metadata(&path)
            .map_err(|source| runtime_error("inspect", &path, source))?;
        let mode = metadata.mode() & 0o7777;
        if !metadata.is_dir()
            || metadata.uid() != paths.uid()
            || mode != 0o700
            || path_metadata.file_type().is_symlink()
            || path_metadata.dev() != metadata.dev()
            || path_metadata.ino() != metadata.ino()
        {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "runtime directory '{}' changed or is insecure while binding serve",
                    path.display()
                ),
            )
            .with_hint("use a current-user, non-symlink runtime directory with mode 0700"));
        }
        Ok(Self {
            identity: Identity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            file,
            path,
            uid: paths.uid(),
        })
    }

    fn stat(&self, name: &OsStr) -> Result<Option<FileStat>, FirestoneError> {
        match fstatat(&self.file, Path::new(name), AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(Some(stat)),
            Err(Errno::ENOENT) => Ok(None),
            Err(source) => Err(runtime_error(
                "inspect",
                &self.path.join(name),
                io::Error::from(source),
            )),
        }
    }

    fn unlink(&self, name: &OsStr) -> Result<(), FirestoneError> {
        match unlinkat(&self.file, Path::new(name), UnlinkatFlags::NoRemoveDir) {
            Ok(()) | Err(Errno::ENOENT) => Ok(()),
            Err(source) => Err(runtime_error(
                "remove",
                &self.path.join(name),
                io::Error::from(source),
            )),
        }
    }

    fn sync(&self) -> Result<(), FirestoneError> {
        self.file
            .sync_all()
            .map_err(|source| runtime_error("fsync", &self.path, source))
    }

    fn validate_path_identity(&self) -> Result<(), FirestoneError> {
        let metadata = fs::symlink_metadata(&self.path)
            .map_err(|source| runtime_error("inspect", &self.path, source))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != self.uid
            || metadata.mode() & 0o7777 != 0o700
            || metadata.dev() != self.identity.device
            || metadata.ino() != self.identity.inode
        {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "runtime directory '{}' changed while publishing serve",
                    self.path.display()
                ),
            )
            .with_hint("restore the original current-user mode-0700 runtime directory"));
        }
        Ok(())
    }
}

struct ServeLock {
    _file: Flock<File>,
}

impl ServeLock {
    fn acquire(
        paths: &Paths,
        runtime: &RuntimeDirectory,
        socket_path: &Path,
    ) -> Result<Self, FirestoneError> {
        let lock_path = paths.serve_lock();
        let name = lock_path.file_name().ok_or_else(|| {
            FirestoneError::new(ErrorKind::Generic, "serve lock path has no file name")
        })?;
        let create_flags = OFlag::O_RDWR
            | OFlag::O_CREAT
            | OFlag::O_EXCL
            | OFlag::O_NOFOLLOW
            | OFlag::O_NONBLOCK
            | OFlag::O_CLOEXEC;
        let (descriptor, created) = match openat(
            &runtime.file,
            Path::new(name),
            create_flags,
            Mode::from_bits_truncate(0o600),
        ) {
            Ok(descriptor) => (descriptor, true),
            Err(Errno::EEXIST) => {
                let descriptor = openat(
                    &runtime.file,
                    Path::new(name),
                    OFlag::O_RDWR | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|source| unsafe_lock_error(&lock_path, source))?;
                (descriptor, false)
            }
            Err(source) => return Err(unsafe_lock_error(&lock_path, source)),
        };
        let file = File::from(descriptor);
        let metadata = file
            .metadata()
            .map_err(|source| runtime_error("inspect opened", &lock_path, source))?;
        let mode = metadata.mode() & 0o7777;
        if !metadata.is_file() || metadata.uid() != runtime.uid || mode != 0o600 {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "serve lock '{}' is insecure: expected a current-user regular file with mode 0600",
                    lock_path.display()
                ),
            )
            .with_hint("remove the hostile serve lock and retry"));
        }
        if created {
            runtime.sync()?;
        }
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(file) => Ok(Self { _file: file }),
            Err((_, Errno::EWOULDBLOCK)) => Err(active_server_error(
                socket_path,
                "another firestone serve process owns the runtime lock",
            )),
            Err((_, source)) => Err(runtime_error("lock", &lock_path, io::Error::from(source))),
        }
    }
}

struct BoundSocket {
    listener: Option<StdUnixListener>,
    runtime: RuntimeDirectory,
    socket_path: PathBuf,
    socket_name: OsString,
    identity: Identity,
    _lock: ServeLock,
    cleaned: bool,
}

impl BoundSocket {
    fn bind(paths: &Paths, socket_path: &Path) -> Result<Self, FirestoneError> {
        let _ = umask(Mode::from_bits_truncate(0o077));
        paths.ensure_runtime_dir()?;
        let _ = umask(Mode::from_bits_truncate(0o177));
        let socket_name = validate_socket_path(paths, socket_path)?;
        UnixAddr::new(socket_path).map_err(|source| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "serve socket path '{}' is not bindable",
                    socket_path.display()
                ),
            )
            .with_hint("shorten FIRESTONE_RUNTIME_DIR or choose a shorter unix:PATH")
            .with_source(io::Error::from(source))
        })?;

        let runtime = RuntimeDirectory::open(paths)?;
        runtime.validate_path_identity()?;
        let lock = ServeLock::acquire(paths, &runtime, socket_path)?;
        if let Some(stat) = runtime.stat(&socket_name)? {
            validate_existing_socket(socket_path, &stat, runtime.uid)?;
            match StdUnixStream::connect(socket_path) {
                Ok(stream) => {
                    drop(stream);
                    return Err(active_server_error(
                        socket_path,
                        "a server is accepting connections",
                    ));
                }
                Err(source)
                    if matches!(
                        source.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) => {}
                Err(source) => {
                    return Err(active_server_error(
                        socket_path,
                        &format!("the existing socket could not be proven stale: {source}"),
                    ));
                }
            }
            let current = runtime.stat(&socket_name)?.ok_or_else(|| {
                active_server_error(socket_path, "the existing socket changed during takeover")
            })?;
            if Identity::from_stat(&current) != Identity::from_stat(&stat) {
                return Err(active_server_error(
                    socket_path,
                    "the existing socket changed during takeover",
                ));
            }
            runtime.unlink(&socket_name)?;
            runtime.sync()?;
        }
        runtime.validate_path_identity()?;

        let listener = StdUnixListener::bind(socket_path).map_err(|source| {
            let kind = if source.kind() == io::ErrorKind::AddrInUse {
                ErrorKind::Conflict
            } else {
                ErrorKind::Dependency
            };
            listener_error(
                kind,
                &format!("cannot bind serve socket '{}'", socket_path.display()),
                source,
            )
        })?;
        // Unix bind starts from mode 0777, so 0177 publishes the socket as 0600.
        // Restore 0077 before request handlers create private directories.
        let _ = umask(Mode::from_bits_truncate(0o077));
        let stat = runtime.stat(&socket_name)?.ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!(
                    "serve socket '{}' disappeared after bind",
                    socket_path.display()
                ),
            )
        })?;
        let identity = Identity::from_stat(&stat);
        let mut bound = Self {
            listener: Some(listener),
            runtime,
            socket_path: socket_path.to_path_buf(),
            socket_name,
            identity,
            _lock: lock,
            cleaned: false,
        };
        let nonblocking_result = match bound.listener.as_ref() {
            Some(listener) => listener.set_nonblocking(true),
            None => Err(io::Error::other("serve listener ownership was lost")),
        };
        if let Err(source) = nonblocking_result {
            let error = listener_error(
                ErrorKind::Generic,
                "cannot make the serve socket nonblocking",
                source,
            );
            let _ = bound.cleanup();
            return Err(error);
        }
        if let Err(error) = bound.runtime.validate_path_identity() {
            let _ = bound.cleanup();
            return Err(error);
        }
        if let Err(error) = validate_existing_socket(socket_path, &stat, paths.uid()) {
            let _ = bound.cleanup();
            return Err(error);
        }
        if let Err(error) = bound.runtime.sync() {
            let _ = bound.cleanup();
            return Err(error);
        }
        Ok(bound)
    }

    fn take_listener(&mut self) -> Result<StdUnixListener, FirestoneError> {
        self.listener.take().ok_or_else(|| {
            FirestoneError::new(ErrorKind::Generic, "serve listener ownership was lost")
        })
    }

    fn cleanup(&mut self) -> Result<(), FirestoneError> {
        if self.cleaned {
            return Ok(());
        }
        self.cleaned = true;
        drop(self.listener.take());
        let Some(stat) = self.runtime.stat(&self.socket_name)? else {
            return Ok(());
        };
        if Identity::from_stat(&stat) != self.identity
            || stat.st_uid != self.runtime.uid
            || !SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFSOCK)
        {
            return Ok(());
        }
        self.runtime.unlink(&self.socket_name)?;
        self.runtime.sync().map_err(|source| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!(
                    "serve stopped but could not persist removal of '{}'",
                    self.socket_path.display()
                ),
            )
            .with_hint("inspect the private runtime directory before restarting serve")
            .with_source(source)
        })
    }
}

impl Drop for BoundSocket {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn validate_socket_path(paths: &Paths, socket_path: &Path) -> Result<OsString, FirestoneError> {
    let parent = socket_path.parent();
    let name = socket_path.file_name();
    if !socket_path.is_absolute()
        || parent != Some(paths.runtime_dir())
        || name.is_none()
        || name == Some(OsStr::new(".serve.lock"))
    {
        return Err(FirestoneError::new(
            ErrorKind::Usage,
            format!(
                "serve listener '{}' must be a socket directly inside '{}'",
                socket_path.display(),
                paths.runtime_dir().display()
            ),
        )
        .with_hint("use unix:NAME or omit --listen for unix:serve.sock"));
    }
    Ok(name.map(OsStr::to_os_string).unwrap_or_default())
}

fn validate_existing_socket(
    path: &Path,
    stat: &FileStat,
    expected_uid: u32,
) -> Result<(), FirestoneError> {
    let kind = SFlag::from_bits_truncate(stat.st_mode);
    #[cfg(target_os = "macos")]
    let mode = u32::from(stat.st_mode) & 0o7777;
    #[cfg(not(target_os = "macos"))]
    let mode = stat.st_mode & 0o7777;
    if !kind.contains(SFlag::S_IFSOCK) || stat.st_uid != expected_uid || mode != 0o600 {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "serve socket '{}' is unsafe: expected a current-user Unix socket with mode 0600",
                path.display()
            ),
        )
        .with_hint("move the hostile runtime node aside and retry"));
    }
    Ok(())
}

fn active_server_error(path: &Path, detail: &str) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Conflict,
        format!("cannot start serve at '{}': {detail}", path.display()),
    )
    .with_hint("stop the active firestone serve process and retry")
}

fn unsafe_lock_error(path: &Path, source: Errno) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Dependency,
        format!("serve lock '{}' is not a safe regular file", path.display()),
    )
    .with_hint("remove the hostile serve lock and retry")
    .with_source(io::Error::from(source))
}

fn listener_error(kind: ErrorKind, message: &str, source: io::Error) -> FirestoneError {
    FirestoneError::new(kind, message)
        .with_hint("check the private runtime directory and retry")
        .with_source(source)
}

fn runtime_error(operation: &str, path: &Path, source: io::Error) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Dependency,
        format!("cannot {operation} serve runtime path '{}'", path.display()),
    )
    .with_hint("check current-user ownership and private runtime permissions")
    .with_source(source)
}
#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::{
        error::Error,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    use firestone_core::{
        Action, DispatchFuture, Dispatcher, Event, EventSink, FirestoneError, GlobalConfig, StepId,
    };
    use serde_json::json;
    use tokio::{
        io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
        net::{UnixListener, UnixStream},
        sync::oneshot,
        task::JoinHandle,
    };

    use super::{merged_router, require_loopback, serve_until_shutdown};
    use crate::{api, ui::auth::UiAuth};

    type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

    struct TestServer {
        _directory: tempfile::TempDir,
        socket: PathBuf,
        shutdown: Option<oneshot::Sender<()>>,
        task: JoinHandle<Result<(), FirestoneError>>,
    }

    impl TestServer {
        async fn start(
            dispatcher: Arc<dyn Dispatcher>,
            drain_timeout: Duration,
        ) -> TestResult<Self> {
            let directory = tempfile::tempdir()?;
            let socket = directory.path().join("serve.sock");
            let listener = UnixListener::bind(&socket)?;
            let paths = firestone_core::Paths::from_inputs(&firestone_core::PathInputs {
                firestone_home: Some(directory.path().to_path_buf()),
                ..firestone_core::PathInputs::capture()?
            })?;
            let app = api::router(dispatcher, &GlobalConfig::default(), &paths);
            let (shutdown, receiver) = oneshot::channel();
            let task = tokio::spawn(serve_until_shutdown(
                listener,
                app,
                async move {
                    receiver.await.map_err(|_| {
                        FirestoneError::new(
                            firestone_core::ErrorKind::Generic,
                            "test shutdown sender disappeared",
                        )
                    })?;
                    Ok(())
                },
                drain_timeout,
            ));
            Ok(Self {
                _directory: directory,
                socket,
                shutdown: Some(shutdown),
                task,
            })
        }

        async fn stop(mut self) -> TestResult {
            if let Some(shutdown) = self.shutdown.take() {
                shutdown.send(()).map_err(|_| "serve task stopped early")?;
            }
            self.task.await.map_err(|_| "serve task panicked")??;
            Ok(())
        }
    }

    async fn request(socket: &Path, request: &[u8]) -> TestResult<Vec<u8>> {
        let mut stream = UnixStream::connect(socket).await?;
        stream.write_all(request).await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        Ok(response)
    }

    async fn request_reader(socket: &Path, request: &[u8]) -> TestResult<BufReader<UnixStream>> {
        let mut stream = UnixStream::connect(socket).await?;
        stream.write_all(request).await?;
        Ok(BufReader::new(stream))
    }

    async fn read_headers(reader: &mut BufReader<UnixStream>) -> TestResult<Vec<u8>> {
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            if headers.len() >= 32 * 1024 {
                return Err("response headers exceeded test bound".into());
            }
            let read = reader.read_until(b'\n', &mut headers).await?;
            if read == 0 {
                return Err("response ended before headers completed".into());
            }
        }
        Ok(headers)
    }

    async fn read_chunk(reader: &mut BufReader<UnixStream>) -> TestResult<Vec<u8>> {
        let mut size_line = String::new();
        if reader.read_line(&mut size_line).await? == 0 {
            return Err("response ended before the next chunk".into());
        }
        let size = size_line
            .trim_end_matches(['\r', '\n'])
            .split(';')
            .next()
            .ok_or("missing chunk size")?;
        let size = usize::from_str_radix(size, 16)?;
        let mut data = vec![0_u8; size];
        reader.read_exact(&mut data).await?;
        let mut terminator = [0_u8; 2];
        reader.read_exact(&mut terminator).await?;
        if terminator != *b"\r\n" {
            return Err("invalid chunk terminator".into());
        }
        Ok(data)
    }

    fn body(response: &[u8]) -> TestResult<&[u8]> {
        response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| &response[index + 4..])
            .ok_or_else(|| "response has no header terminator".into())
    }

    fn step(id: &'static str) -> Event {
        Event::StepStart {
            id: StepId::from(id),
            label: id.to_owned(),
        }
    }

    fn encoded(event: &Event) -> TestResult<Vec<u8>> {
        let mut value = serde_json::to_vec(event)?;
        value.push(b'\n');
        Ok(value)
    }

    struct DelayedDispatcher;

    impl Dispatcher for DelayedDispatcher {
        fn run<'a>(&'a self, _action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a> {
            Box::pin(async move {
                events.emit(step("first"))?;
                std::thread::sleep(Duration::from_millis(80));
                events.emit(Event::StepDone {
                    id: StepId::from("second"),
                    detail: None,
                    elapsed_ms: 80,
                })?;
                events.emit(Event::Result {
                    action: "start".to_owned(),
                    payload: json!({"ok":true}),
                })
            })
        }
    }

    struct QuietDispatcher;

    impl Dispatcher for QuietDispatcher {
        fn run<'a>(&'a self, _action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a> {
            Box::pin(async move {
                events.emit(Event::Result {
                    action: "version".to_owned(),
                    payload: json!({"ok":true}),
                })
            })
        }
    }

    #[test]
    fn require_loopback_accepts_only_loopback_literals() -> TestResult {
        for accepted in ["127.0.0.1:0", "127.0.0.1:8080", "[::1]:8080", "127.9.9.9:1"] {
            let address: std::net::SocketAddr = accepted.parse()?;
            assert_eq!(require_loopback(address)?, address, "{accepted}");
        }
        Ok(())
    }

    #[test]
    fn require_loopback_rejects_wildcard_and_routable_addresses() -> TestResult {
        for rejected in [
            "0.0.0.0:8080",
            "[::]:8080",
            "192.168.1.10:8080",
            "8.8.8.8:80",
            "[::ffff:127.0.0.1]:8080",
        ] {
            let address: std::net::SocketAddr = rejected.parse()?;
            let error =
                require_loopback(address).expect_err("only loopback addresses may be bound");
            assert_eq!(error.kind(), firestone_core::ErrorKind::Usage);
            assert!(error.to_string().contains(rejected), "{rejected}");
            assert!(
                error
                    .info()
                    .hint
                    .is_some_and(|hint| hint.contains("127.0.0.1")),
                "{rejected}"
            );
        }
        Ok(())
    }

    fn stub_ui_router() -> axum::Router {
        axum::Router::new()
            .route("/", axum::routing::get(|| async { "firestone ui" }))
            .fallback(|| async { (axum::http::StatusCode::NOT_FOUND, "ui not found") })
    }

    async fn merged_probe(uri: &str) -> TestResult<(axum::http::StatusCode, Vec<u8>)> {
        use axum::body::Body;
        use tower::ServiceExt as _;

        let dispatcher: Arc<dyn Dispatcher> = Arc::new(QuietDispatcher);
        let directory = tempfile::tempdir()?;
        let paths = firestone_core::Paths::from_inputs(&firestone_core::PathInputs {
            firestone_home: Some(directory.path().to_path_buf()),
            ..firestone_core::PathInputs::capture()?
        })?;
        let app = merged_router(
            api::router(dispatcher, &GlobalConfig::default(), &paths),
            stub_ui_router(),
        );
        let request = axum::http::Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())?;
        let response = app.oneshot(request).await?;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024).await?;
        Ok((status, body.to_vec()))
    }

    #[tokio::test]
    async fn merged_router_keeps_the_rest_json_not_found_envelope() -> TestResult {
        let (status, body) = merged_probe("/v1/nope").await?;
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body)?,
            json!({
                "error": {
                    "kind": "not_found",
                    "message": "no REST route matches this request",
                    "hint": "check the HTTP method and the /v1 route path"
                }
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn merged_router_routes_the_ui_prefix_to_its_own_fallback() -> TestResult {
        let (status, body) = merged_probe("/ui/nope").await?;
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(body, b"ui not found");

        let (status, body) = merged_probe("/").await?;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(body, b"firestone ui");

        let (status, _) = merged_probe("/v1/version").await?;
        assert_eq!(status, axum::http::StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    async fn merged_router_sends_a_v1_prefixed_but_unrelated_path_to_the_ui() -> TestResult {
        // '/v1foo' is not inside the REST namespace, so it belongs to the UI.
        let (status, body) = merged_probe("/v1foo").await?;
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(body, b"ui not found");
        Ok(())
    }

    #[test]
    fn run_refuses_a_token_on_a_unix_listener_before_binding() -> TestResult {
        use firestone_core::{PathInputs, Paths};

        let directory = tempfile::tempdir()?;
        let inputs = PathInputs {
            firestone_home: Some(directory.path().to_path_buf()),
            ..PathInputs::capture()?
        };
        let paths = Paths::from_inputs(&inputs)?;
        let dispatcher: Arc<dyn Dispatcher> = Arc::new(QuietDispatcher);
        let token = crate::ui::auth::SessionToken::generate()?;
        let error = super::run(
            &paths,
            super::ServeListener::Unix(paths.serve_socket()),
            dispatcher,
            &GlobalConfig::default(),
            UiAuth::token(token),
            None,
        )
        .expect_err("a Unix socket never takes a session token");
        assert_eq!(error.kind(), firestone_core::ErrorKind::Usage);
        assert!(!paths.runtime_dir().exists());
        Ok(())
    }

    #[test]
    fn run_refuses_an_untokened_loopback_listener_before_binding() -> TestResult {
        use firestone_core::{PathInputs, Paths};

        let directory = tempfile::tempdir()?;
        let inputs = PathInputs {
            firestone_home: Some(directory.path().to_path_buf()),
            ..PathInputs::capture()?
        };
        let paths = Paths::from_inputs(&inputs)?;
        let dispatcher: Arc<dyn Dispatcher> = Arc::new(QuietDispatcher);
        let error = super::run(
            &paths,
            super::ServeListener::Loopback {
                addr: "127.0.0.1:0".parse()?,
            },
            dispatcher,
            &GlobalConfig::default(),
            UiAuth::trusted(),
            None,
        )
        .expect_err("a loopback listener is never unauthenticated");
        assert_eq!(error.kind(), firestone_core::ErrorKind::Usage);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unix_runtime_delayed_action_flushes_each_ndjson_frame() -> TestResult {
        let dispatcher: Arc<dyn Dispatcher> = Arc::new(DelayedDispatcher);
        let server = TestServer::start(dispatcher, Duration::from_secs(1)).await?;
        let mut reader = request_reader(
            &server.socket,
            b"POST /v1/machines/dev/start HTTP/1.1\r\nHost: firestone\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        )
        .await?;
        let headers = read_headers(&mut reader).await?;
        assert!(headers.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(
            headers
                .windows(b"application/x-ndjson".len())
                .any(|window| window == b"application/x-ndjson")
        );
        assert_eq!(read_chunk(&mut reader).await?, encoded(&step("first"))?);
        let started = Instant::now();
        assert_eq!(
            read_chunk(&mut reader).await?,
            encoded(&Event::StepDone {
                id: StepId::from("second"),
                detail: None,
                elapsed_ms: 80,
            })?
        );
        assert!(started.elapsed() >= Duration::from_millis(40));
        let terminal = read_chunk(&mut reader).await?;
        assert!(terminal.starts_with(br#"{"type":"Result""#));
        assert!(read_chunk(&mut reader).await?.is_empty());
        server.stop().await
    }

    struct FollowDispatcher {
        cancelled: AtomicBool,
    }

    impl Dispatcher for FollowDispatcher {
        fn run<'a>(&'a self, _action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a> {
            Box::pin(async move {
                events.emit(Event::Output {
                    data: "ready\n".to_owned(),
                })?;
                while !events.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                self.cancelled.store(true, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    async fn wait_for_flag(flag: &AtomicBool) -> TestResult {
        tokio::time::timeout(Duration::from_secs(1), async {
            while !flag.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unix_runtime_disconnected_follow_cancels_worker_without_leaking_connection()
    -> TestResult {
        let dispatcher = Arc::new(FollowDispatcher {
            cancelled: AtomicBool::new(false),
        });
        let erased: Arc<dyn Dispatcher> = dispatcher.clone();
        let server = TestServer::start(erased, Duration::from_millis(75)).await?;
        let mut reader = request_reader(
            &server.socket,
            b"GET /v1/machines/dev/logs?follow=true HTTP/1.1\r\nHost: firestone\r\nConnection: close\r\n\r\n",
        )
        .await?;
        let headers = read_headers(&mut reader).await?;
        assert!(headers.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert_eq!(read_chunk(&mut reader).await?, b"ready\n");
        drop(reader);
        wait_for_flag(&dispatcher.cancelled).await?;
        server.stop().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unix_runtime_shutdown_forces_open_follow_closed_after_bound() -> TestResult {
        let dispatcher = Arc::new(FollowDispatcher {
            cancelled: AtomicBool::new(false),
        });
        let erased: Arc<dyn Dispatcher> = dispatcher.clone();
        let server = TestServer::start(erased, Duration::from_millis(75)).await?;
        let mut reader = request_reader(
            &server.socket,
            b"GET /v1/machines/dev/logs?follow=true HTTP/1.1\r\nHost: firestone\r\n\r\n",
        )
        .await?;
        read_headers(&mut reader).await?;
        assert_eq!(read_chunk(&mut reader).await?, b"ready\n");
        let started = Instant::now();
        server.stop().await?;
        assert!(started.elapsed() < Duration::from_secs(1));
        wait_for_flag(&dispatcher.cancelled).await?;
        drop(reader);
        Ok(())
    }

    struct MutationDispatcher {
        completed: AtomicBool,
    }

    impl Dispatcher for MutationDispatcher {
        fn run<'a>(&'a self, _action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a> {
            Box::pin(async move {
                events.emit(step("prepare"))?;
                std::thread::sleep(Duration::from_millis(120));
                self.completed.store(true, Ordering::SeqCst);
                events.emit(Event::Result {
                    action: "start".to_owned(),
                    payload: json!({"safe":true}),
                })
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unix_runtime_graceful_shutdown_preserves_inflight_mutation_safe_point() -> TestResult {
        let dispatcher = Arc::new(MutationDispatcher {
            completed: AtomicBool::new(false),
        });
        let erased: Arc<dyn Dispatcher> = dispatcher.clone();
        let server = TestServer::start(erased, Duration::from_secs(1)).await?;
        let mut reader = request_reader(
            &server.socket,
            b"POST /v1/machines/dev/start HTTP/1.1\r\nHost: firestone\r\nContent-Length: 0\r\n\r\n",
        )
        .await?;
        read_headers(&mut reader).await?;
        assert_eq!(read_chunk(&mut reader).await?, encoded(&step("prepare"))?);
        server.stop().await?;
        assert!(dispatcher.completed.load(Ordering::SeqCst));
        drop(reader);
        Ok(())
    }

    struct PanicOnceDispatcher {
        panicked: AtomicBool,
    }

    impl Dispatcher for PanicOnceDispatcher {
        fn run<'a>(&'a self, action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a> {
            Box::pin(async move {
                if !self.panicked.swap(true, Ordering::SeqCst) {
                    panic!("request panic secret");
                }
                let result = if action == Action::Version {
                    "version"
                } else {
                    "start"
                };
                events.emit(Event::Result {
                    action: result.to_owned(),
                    payload: json!({"alive":true}),
                })
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unix_runtime_request_panic_and_broken_client_leave_listener_live_without_leak()
    -> TestResult {
        let dispatcher: Arc<dyn Dispatcher> = Arc::new(PanicOnceDispatcher {
            panicked: AtomicBool::new(false),
        });
        let server = TestServer::start(dispatcher, Duration::from_secs(1)).await?;
        let failed = request(
            &server.socket,
            b"POST /v1/machines/dev/start HTTP/1.1\r\nHost: firestone\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        )
        .await?;
        assert!(failed.starts_with(b"HTTP/1.1 500 Internal Server Error\r\n"));
        assert!(
            !failed
                .windows("request panic secret".len())
                .any(|window| { window == b"request panic secret" })
        );

        let mut broken = UnixStream::connect(&server.socket).await?;
        broken.write_all(b"GET /v1/ver").await?;
        drop(broken);

        let live = request(
            &server.socket,
            b"GET /v1/version HTTP/1.1\r\nHost: firestone\r\nConnection: close\r\n\r\n",
        )
        .await?;
        assert!(live.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(body(&live)?)?,
            json!({"alive":true})
        );
        server.stop().await
    }

    // --- WebSocket console and shell transports (SPEC 16.3) -----------------

    /// A loopback server whose connections accept protocol upgrades.
    ///
    /// This is the real `serve_until_shutdown` path, so it proves
    /// `serve_connection`'s `with_upgrades()` is what lets a handler answer
    /// `101` and keep the socket.
    struct UpgradeServer {
        directory: tempfile::TempDir,
        paths: firestone_core::Paths,
        address: std::net::SocketAddr,
        shutdown: Option<oneshot::Sender<()>>,
        task: JoinHandle<Result<(), FirestoneError>>,
    }

    impl UpgradeServer {
        async fn start(dispatcher: Arc<dyn Dispatcher>) -> TestResult<Self> {
            let directory = tempfile::tempdir()?;
            // The runtime-directory guard refuses a symlinked ancestor, and
            // macOS puts every temporary directory under a symlinked `/var`.
            let home = directory.path().canonicalize()?;
            let paths = firestone_core::Paths::from_inputs(&firestone_core::PathInputs {
                firestone_home: Some(home),
                ..firestone_core::PathInputs::capture()?
            })?;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let address = listener.local_addr()?;
            let app = crate::api::router(dispatcher, &GlobalConfig::default(), &paths);
            let (shutdown, receiver) = oneshot::channel();
            let task = tokio::spawn(serve_until_shutdown(
                listener,
                app,
                async move {
                    receiver.await.map_err(|_| {
                        FirestoneError::new(
                            firestone_core::ErrorKind::Generic,
                            "test shutdown sender disappeared",
                        )
                    })?;
                    Ok(())
                },
                Duration::from_millis(200),
            ));
            Ok(Self {
                directory,
                paths,
                address,
                shutdown: Some(shutdown),
                task,
            })
        }

        fn url(&self, path: &str) -> String {
            format!("ws://{}{path}", self.address)
        }

        async fn connect(
            &self,
            path: &str,
        ) -> Result<
            tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
            Box<tokio_tungstenite::tungstenite::Error>,
        > {
            use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

            let request = self.url(path).into_client_request().map_err(Box::new)?;
            let stream = tokio::net::TcpStream::connect(self.address)
                .await
                .map_err(|source| Box::new(tokio_tungstenite::tungstenite::Error::Io(source)))?;
            let (socket, _) = tokio_tungstenite::client_async(request, stream)
                .await
                .map_err(Box::new)?;
            Ok(socket)
        }

        async fn stop(mut self) -> TestResult {
            if let Some(shutdown) = self.shutdown.take() {
                shutdown.send(()).map_err(|_| "serve task stopped early")?;
            }
            self.task.await.map_err(|_| "serve task panicked")??;
            drop(self.directory);
            Ok(())
        }
    }

    /// Serves one console-broker acknowledgement, then echoes what it reads.
    ///
    /// This is the exact wire the real single-client broker speaks: a short
    /// `OK`/`BUSY` line followed by raw terminal bytes.
    fn spawn_fake_broker(
        paths: &firestone_core::Paths,
        name: &str,
        acknowledgement: &'static [u8],
        banner: &'static [u8],
        echo: bool,
    ) -> TestResult<JoinHandle<()>> {
        use std::os::unix::fs::PermissionsExt as _;

        paths.ensure_machine_runtime_dir(name)?;
        let socket = paths.machine_console_socket(name)?;
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        Ok(tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            if stream.write_all(acknowledgement).await.is_err() {
                return;
            }
            if !banner.is_empty() && stream.write_all(banner).await.is_err() {
                return;
            }
            if !echo {
                return;
            }
            let mut buffer = [0_u8; 1024];
            while let Ok(read) = stream.read(&mut buffer).await {
                if read == 0 || stream.write_all(&buffer[..read]).await.is_err() {
                    return;
                }
            }
        }))
    }

    /// The exact `show` payload the shared action emits, for one status.
    fn show_payload(status: firestone_core::MachineStatus) -> TestResult<serde_json::Value> {
        use firestone_core::{
            MachineSpec, MachineSpecPatch, MachineState, MachineView, StateImage, StateVersion,
        };

        let config = GlobalConfig::default();
        let spec = MachineSpec::from_layers(
            &config.defaults,
            &MachineSpecPatch::default(),
            &MachineSpecPatch::default(),
        )?;
        let view = MachineView {
            spec,
            state: MachineState {
                version: StateVersion,
                status,
                image: StateImage {
                    r#ref: "ubuntu-24.04".to_owned(),
                    id: None,
                    sha256: None,
                },
                mac: None,
                cid: 3,
                instance_id: None,
                shim_pid: None,
                vmm_pid: None,
                sidecar_pids: std::collections::BTreeMap::new(),
                runtime_dir: PathBuf::from("/nonexistent"),
                started_at: None,
                forwards: Vec::new(),
                degraded: Vec::new(),
                last_exit: None,
            },
            supervision: None,
        };
        Ok(serde_json::to_value(view)?)
    }

    struct ShowDispatcher {
        payload: serde_json::Value,
    }

    impl ShowDispatcher {
        fn arc(status: firestone_core::MachineStatus) -> TestResult<Arc<dyn Dispatcher>> {
            Ok(Arc::new(Self {
                payload: show_payload(status)?,
            }))
        }
    }

    impl Dispatcher for ShowDispatcher {
        fn run<'a>(&'a self, _action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a> {
            Box::pin(async move {
                events.emit(Event::Result {
                    action: "show".to_owned(),
                    payload: self.payload.clone(),
                })
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn console_websocket_upgrades_and_relays_binary_frames_both_ways() -> TestResult {
        use futures_util::{SinkExt as _, StreamExt as _};
        use tokio_tungstenite::tungstenite::Message;

        let dispatcher = ShowDispatcher::arc(firestone_core::MachineStatus::Running)?;
        let server = UpgradeServer::start(dispatcher).await?;
        let broker = spawn_fake_broker(&server.paths, "dev", b"OK\n", b"login: ", true)?;

        let mut socket = server.connect("/v1/machines/dev/console/ws").await?;
        assert_eq!(
            socket.next().await.ok_or("the banner never arrived")??,
            Message::Binary(b"login: ".as_slice().into())
        );

        // A text control frame is accepted and ignored on the console route.
        socket
            .send(Message::Text(r#"{"resize":{"rows":24,"cols":80}}"#.into()))
            .await?;
        socket
            .send(Message::Binary(b"root\n".as_slice().into()))
            .await?;
        assert_eq!(
            socket.next().await.ok_or("the echo never arrived")??,
            Message::Binary(b"root\n".as_slice().into())
        );

        socket.close(None).await?;
        broker.abort();
        server.stop().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn console_websocket_closes_with_machine_stopped_on_broker_eof() -> TestResult {
        use futures_util::StreamExt as _;
        use tokio_tungstenite::tungstenite::Message;

        let dispatcher = ShowDispatcher::arc(firestone_core::MachineStatus::Running)?;
        let server = UpgradeServer::start(dispatcher).await?;
        let broker = spawn_fake_broker(&server.paths, "dev", b"OK\n", b"bye", false)?;

        let mut socket = server.connect("/v1/machines/dev/console/ws").await?;
        assert_eq!(
            socket.next().await.ok_or("the banner never arrived")??,
            Message::Binary(b"bye".as_slice().into())
        );
        let close = socket
            .next()
            .await
            .ok_or("the close frame never arrived")??;
        match close {
            Message::Close(Some(frame)) => {
                assert_eq!(frame.reason.as_str(), "machine stopped");
                assert_eq!(u16::from(frame.code), 1000);
            }
            other => return Err(format!("expected a close frame, got {other:?}").into()),
        }

        broker.abort();
        server.stop().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn console_websocket_busy_broker_answers_409_without_upgrading() -> TestResult {
        let dispatcher = ShowDispatcher::arc(firestone_core::MachineStatus::Running)?;
        let server = UpgradeServer::start(dispatcher).await?;
        let broker = spawn_fake_broker(&server.paths, "dev", b"BUSY\n", b"", false)?;

        let error = server
            .connect("/v1/machines/dev/console/ws")
            .await
            .err()
            .ok_or("a busy broker must not upgrade")?;
        match *error {
            tokio_tungstenite::tungstenite::Error::Http(response) => {
                assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
                let body = response.body().as_ref().ok_or("the 409 carried no body")?;
                let envelope: serde_json::Value = serde_json::from_slice(body)?;
                assert_eq!(envelope["error"]["kind"], "busy");
                let hint = envelope["error"]["hint"]
                    .as_str()
                    .ok_or("the 409 carried no hint")?;
                assert!(hint.contains("firestone console dev"), "{hint}");
            }
            other => return Err(format!("expected an HTTP rejection, got {other:?}").into()),
        }

        broker.abort();
        server.stop().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn console_websocket_without_a_broker_reports_the_not_running_error() -> TestResult {
        let dispatcher = ShowDispatcher::arc(firestone_core::MachineStatus::Stopped)?;
        let server = UpgradeServer::start(dispatcher).await?;

        let error = server
            .connect("/v1/machines/dev/console/ws")
            .await
            .err()
            .ok_or("a stopped machine must not upgrade")?;
        match *error {
            tokio_tungstenite::tungstenite::Error::Http(response) => {
                let body = response
                    .body()
                    .as_ref()
                    .ok_or("the error carried no body")?;
                let envelope: serde_json::Value = serde_json::from_slice(body)?;
                assert_eq!(envelope["error"]["kind"], "not_running");
            }
            other => return Err(format!("expected an HTTP rejection, got {other:?}").into()),
        }

        server.stop().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shell_websocket_refuses_a_machine_that_is_not_running() -> TestResult {
        let dispatcher = ShowDispatcher::arc(firestone_core::MachineStatus::Stopped)?;
        let server = UpgradeServer::start(dispatcher).await?;

        let error = server
            .connect("/v1/machines/dev/shell/ws")
            .await
            .err()
            .ok_or("a stopped machine must not upgrade")?;
        match *error {
            tokio_tungstenite::tungstenite::Error::Http(response) => {
                let body = response
                    .body()
                    .as_ref()
                    .ok_or("the error carried no body")?;
                let envelope: serde_json::Value = serde_json::from_slice(body)?;
                assert_eq!(envelope["error"]["kind"], "not_running");
                let hint = envelope["error"]["hint"]
                    .as_str()
                    .ok_or("the error carried no hint")?;
                assert!(hint.contains("firestone start dev"), "{hint}");
            }
            other => return Err(format!("expected an HTTP rejection, got {other:?}").into()),
        }

        server.stop().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shell_websocket_upgrades_a_running_machine_and_ends_with_a_close_frame() -> TestResult
    {
        use futures_util::StreamExt as _;

        let dispatcher = ShowDispatcher::arc(firestone_core::MachineStatus::Running)?;
        let server = UpgradeServer::start(dispatcher).await?;
        // `shell_ssh_plan` writes the Firestone SSH identity and resolves the
        // machine's known_hosts file, so both directories must exist.
        std::fs::create_dir_all(server.paths.machine_dir("dev")?)?;

        // The handshake must reach 101: the PTY and the OpenSSH child are
        // allocated only after the upgrade. The session then ends on its own,
        // because this machine has no reachable vsock proxy.
        let mut socket = server.connect("/v1/machines/dev/shell/ws").await?;
        let ended = tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(message) = socket.next().await {
                match message {
                    Ok(tokio_tungstenite::tungstenite::Message::Close(_)) | Err(_) => return true,
                    Ok(_) => {}
                }
            }
            true
        })
        .await;
        assert!(ended.is_ok(), "the shell session never terminated");

        server.stop().await
    }
}
