//! Optional JSON-RPC server for remote haki-dl control.
//!
//! The native async Rust API remains the preferred in-process API. This module
//! is intended for external processes, GUI frontends, and remote controllers
//! that need JSON-RPC over HTTP or WebSocket.
//!
//! The server uses a haki-native method namespace. For example, callers submit
//! downloads with `haki.addUri`, receive a download GID, and can then poll
//! `haki.tellStatus` or subscribe to `haki.onProgress` WebSocket notifications.
//! RPC option objects are converted into [`DownloadOptions`] with forgiving key
//! spelling, so `saveDir`, `save-dir`, and `save_dir` all map to the same
//! setting.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, State};
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    AUTHORIZATION, CONTENT_TYPE,
};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tower_service::Service;

use crate::api::{DownloadClient, DownloadRequest, ProgressCallback};
use crate::base64::decode_base64;
use crate::cancellation::CancellationToken;
use crate::config::{
    CompatibilityProfile, CustomKey, CustomRange, DecryptionEngine, DownloadOptions, HlsMethod,
    LogLevel, MuxAfterDoneOptions, MuxerKind, StreamFilter, SubtitleFormat, TaskStartAt,
    UiLanguage,
};
use crate::error::{Error, Result};
use crate::event::ProgressEvent;
use crate::manifest::{RoleType, StreamSelector};
use crate::mux::{MuxFormat, MuxImport};
use crate::progress::{AggregateProgress, StreamProgress};

const JSONRPC_VERSION: &str = "2.0";
const NOTIFICATION_CAPACITY: usize = 1024;
const MAX_ACTIVE_DOWNLOADS: usize = 1024;
const MAX_RETAINED_DOWNLOADS: usize = 4096;
const DEFAULT_MAX_REQUEST_SIZE: usize = 2 * 1024 * 1024;
const DEFAULT_QUEUE_CONCURRENCY: usize = 4;

const METHOD_ADD: &str = "haki.add";
const METHOD_ADD_URI: &str = "haki.addUri";
const METHOD_PAUSE: &str = "haki.pause";
const METHOD_FORCE_PAUSE: &str = "haki.forcePause";
const METHOD_PAUSE_ALL: &str = "haki.pauseAll";
const METHOD_FORCE_PAUSE_ALL: &str = "haki.forcePauseAll";
const METHOD_UNPAUSE: &str = "haki.unpause";
const METHOD_UNPAUSE_ALL: &str = "haki.unpauseAll";
const METHOD_CHANGE_POSITION: &str = "haki.changePosition";
const METHOD_CHANGE_URI: &str = "haki.changeUri";
const METHOD_CHANGE_OPTION: &str = "haki.changeOption";
const METHOD_GET_GLOBAL_OPTION: &str = "haki.getGlobalOption";
const METHOD_CHANGE_GLOBAL_OPTION: &str = "haki.changeGlobalOption";
const METHOD_TELL_STATUS: &str = "haki.tellStatus";
const METHOD_TELL_ACTIVE: &str = "haki.tellActive";
const METHOD_TELL_WAITING: &str = "haki.tellWaiting";
const METHOD_TELL_STOPPED: &str = "haki.tellStopped";
const METHOD_GET_URIS: &str = "haki.getUris";
const METHOD_GET_FILES: &str = "haki.getFiles";
const METHOD_GET_SERVERS: &str = "haki.getServers";
const METHOD_GET_OPTION: &str = "haki.getOption";
const METHOD_REMOVE: &str = "haki.remove";
const METHOD_FORCE_REMOVE: &str = "haki.forceRemove";
const METHOD_REMOVE_RESULT: &str = "haki.removeDownloadResult";
const METHOD_PURGE_RESULT: &str = "haki.purgeDownloadResult";
const METHOD_GET_VERSION: &str = "haki.getVersion";
const METHOD_GET_SESSION_INFO: &str = "haki.getSessionInfo";
const METHOD_GET_GLOBAL_STAT: &str = "haki.getGlobalStat";
const METHOD_SHUTDOWN: &str = "haki.shutdown";
const METHOD_FORCE_SHUTDOWN: &str = "haki.forceShutdown";
const METHOD_SYSTEM_MULTICALL: &str = "system.multicall";
const METHOD_SYSTEM_LIST_METHODS: &str = "system.listMethods";
const METHOD_SYSTEM_LIST_NOTIFICATIONS: &str = "system.listNotifications";

const NOTIFY_START: &str = "haki.onDownloadStart";
const NOTIFY_PAUSE: &str = "haki.onDownloadPause";
const NOTIFY_STOP: &str = "haki.onDownloadStop";
const NOTIFY_COMPLETE: &str = "haki.onDownloadComplete";
const NOTIFY_ERROR: &str = "haki.onDownloadError";
const NOTIFY_PROGRESS: &str = "haki.onProgress";

/// JSON-RPC server configuration and runtime state.
#[derive(Clone)]
pub struct RpcServer {
    bind: SocketAddr,
    secret: Option<String>,
    manager: RpcSessionManager,
    max_request_size: usize,
    allow_origin_all: bool,
    basic_auth: Option<RpcBasicAuth>,
    tls: Option<RpcTlsConfig>,
}

impl RpcServer {
    /// Creates a builder with the default `127.0.0.1:6800` listen address.
    pub fn builder() -> RpcServerBuilder {
        RpcServerBuilder::default()
    }

    /// Returns the session manager used by this server.
    pub fn manager(&self) -> &RpcSessionManager {
        &self.manager
    }

    /// Serves JSON-RPC over HTTP POST and WebSocket at `/jsonrpc`.
    pub async fn serve(self) -> Result<()> {
        let listener = TcpListener::bind(self.bind).await?;
        self.serve_with_listener(listener).await
    }

    /// Serves JSON-RPC over HTTP POST and WebSocket using an existing listener.
    pub async fn serve_with_listener(self, listener: TcpListener) -> Result<()> {
        let (shutdown_tx, _) = broadcast::channel(1);
        let mut shutdown_rx = shutdown_tx.subscribe();
        let state = RpcAppState {
            manager: self.manager,
            secret: self.secret,
            max_request_size: self.max_request_size,
            allow_origin_all: self.allow_origin_all,
            basic_auth: self.basic_auth,
            shutdown: shutdown_tx.clone(),
        };
        let app = rpc_router(Arc::new(state));
        if let Some(tls) = self.tls {
            serve_tls_listener(listener, app, load_tls_config(&tls)?, shutdown_rx).await
        } else {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.recv().await;
                })
                .await
                .map_err(|error| Error::http(format!("rpc server failed: {error}")))
        }
    }
}

impl std::fmt::Debug for RpcServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RpcServer")
            .field("bind", &self.bind)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .field("max_request_size", &self.max_request_size)
            .field("allow_origin_all", &self.allow_origin_all)
            .field(
                "basic_auth",
                &self.basic_auth.as_ref().map(|_| "<redacted>"),
            )
            .field("tls", &self.tls)
            .finish_non_exhaustive()
    }
}

fn rpc_router(state: Arc<RpcAppState>) -> Router {
    Router::new()
        .route(
            "/jsonrpc",
            post(http_jsonrpc)
                .get(get_jsonrpc_or_ws)
                .options(http_jsonrpc_options),
        )
        .route("/", get(rpc_root))
        .with_state(state)
}

async fn serve_tls_listener(
    listener: TcpListener,
    app: Router,
    tls_config: ServerConfig,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> Result<()> {
    let acceptor = TlsAcceptor::from(Arc::new(tls_config));
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => return Ok(()),
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let acceptor = acceptor.clone();
                let app = app.clone();
                tokio::spawn(async move {
                    let Ok(stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |request: Request<Incoming>| {
                        let mut app = app.clone();
                        async move { app.call(request).await }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(io, service)
                        .with_upgrades()
                        .await;
                });
            }
        }
    }
}

fn load_tls_config(config: &RpcTlsConfig) -> Result<ServerConfig> {
    let certificates = load_pem_blocks(&config.certificate, &["CERTIFICATE"])?
        .into_iter()
        .map(CertificateDer::from)
        .collect::<Vec<_>>();
    if certificates.is_empty() {
        return Err(Error::config(format!(
            "rpc certificate file {} does not contain a PEM CERTIFICATE block",
            config.certificate.display()
        )));
    }
    let private_key = load_pem_blocks(
        &config.private_key,
        &["PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY"],
    )?
    .into_iter()
    .next()
    .ok_or_else(|| {
        Error::config(format!(
            "rpc private key file {} does not contain a supported PEM private key block",
            config.private_key.display()
        ))
    })?;
    let private_key = PrivateKeyDer::try_from(private_key).map_err(|error| {
        Error::config(format!(
            "rpc private key file {} is invalid: {error}",
            config.private_key.display()
        ))
    })?;
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)
        .map_err(|error| Error::config(format!("rpc TLS configuration is invalid: {error}")))
}

fn load_pem_blocks(path: &Path, labels: &[&str]) -> Result<Vec<Vec<u8>>> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| Error::config(format!("failed to read {}: {error}", path.display())))?;
    let mut blocks = Vec::new();
    for label in labels {
        blocks.extend(decode_pem_blocks(&text, label).map_err(|error| {
            Error::config(format!(
                "failed to decode PEM block in {}: {error}",
                path.display()
            ))
        })?);
    }
    Ok(blocks)
}

fn decode_pem_blocks(text: &str, label: &str) -> std::result::Result<Vec<Vec<u8>>, &'static str> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut blocks = Vec::new();
    let mut remaining = text;
    while let Some(begin_index) = remaining.find(&begin) {
        remaining = &remaining[begin_index + begin.len()..];
        let Some(end_index) = remaining.find(&end) else {
            return Err("unterminated PEM block");
        };
        let body = &remaining[..end_index];
        blocks.push(decode_base64(body)?);
        remaining = &remaining[end_index + end.len()..];
    }
    Ok(blocks)
}

/// Optional queue configuration for the JSON-RPC manager.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RpcQueueConfig {
    /// Enables queued RPC scheduling instead of starting every submitted job immediately.
    pub enabled: bool,
    /// Maximum number of queued downloads allowed to run at the same time.
    pub max_concurrent_downloads: usize,
    /// Keeps newly submitted queued downloads paused until they are explicitly unpaused.
    pub pause_new_downloads: bool,
}

impl Default for RpcQueueConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_concurrent_downloads: DEFAULT_QUEUE_CONCURRENCY,
            pause_new_downloads: false,
        }
    }
}

/// Deprecated Basic authorization compatibility for JSON-RPC clients.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RpcBasicAuth {
    user: String,
    password: String,
}

/// PEM certificate and private key files for encrypted JSON-RPC transport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RpcTlsConfig {
    certificate: PathBuf,
    private_key: PathBuf,
}

impl RpcTlsConfig {
    /// Creates a TLS configuration from PEM certificate and private key files.
    pub fn pem_files(certificate: impl Into<PathBuf>, private_key: impl Into<PathBuf>) -> Self {
        Self {
            certificate: certificate.into(),
            private_key: private_key.into(),
        }
    }
}

/// Builder for [`RpcServer`].
#[derive(Clone, Debug)]
pub struct RpcServerBuilder {
    bind: SocketAddr,
    secret: Option<String>,
    manager: RpcSessionManager,
    queue_config: RpcQueueConfig,
    max_request_size: usize,
    allow_origin_all: bool,
    basic_auth: Option<RpcBasicAuth>,
    tls: Option<RpcTlsConfig>,
}

impl Default for RpcServerBuilder {
    fn default() -> Self {
        Self {
            bind: default_bind_addr(),
            secret: None,
            manager: RpcSessionManager::new(),
            queue_config: RpcQueueConfig::default(),
            max_request_size: DEFAULT_MAX_REQUEST_SIZE,
            allow_origin_all: false,
            basic_auth: None,
            tls: None,
        }
    }
}

impl RpcServerBuilder {
    /// Sets the listen address.
    pub fn bind(mut self, bind: SocketAddr) -> Self {
        self.bind = bind;
        self
    }

    /// Sets whether the server listens on all IPv4 interfaces.
    ///
    /// The current listen port is preserved. When disabled, the server listens
    /// on `127.0.0.1`.
    pub fn rpc_listen_all(mut self, listen_all: bool) -> Self {
        let ip = if listen_all {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        } else {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        };
        self.bind = SocketAddr::new(ip, self.bind.port());
        self
    }

    /// Sets the JSON-RPC listen port while preserving the current listen host.
    pub fn rpc_listen_port(mut self, port: u16) -> Self {
        self.bind.set_port(port);
        self
    }

    /// Parses and sets the listen address.
    pub fn bind_str(mut self, bind: &str) -> Result<Self> {
        self.bind = bind
            .parse::<SocketAddr>()
            .map_err(|error| Error::config(format!("rpc listen address is invalid: {error}")))?;
        Ok(self)
    }

    /// Sets the optional JSON-RPC secret token.
    ///
    /// When set, mutating or status methods require a first parameter of
    /// `token:<secret>`. Method and notification listing remain public.
    pub fn secret(mut self, secret: impl Into<String>) -> Self {
        let secret = secret.into();
        self.secret = if secret.is_empty() {
            None
        } else {
            Some(secret)
        };
        self
    }

    /// Enables or disables the optional RPC queue scheduler.
    pub fn queue_enabled(mut self, enabled: bool) -> Self {
        self.queue_config.enabled = enabled;
        self
    }

    /// Sets the maximum number of queued downloads that may run at once.
    pub fn max_concurrent_downloads(mut self, max_concurrent_downloads: usize) -> Self {
        self.queue_config.max_concurrent_downloads = max_concurrent_downloads.max(1);
        self
    }

    /// Starts newly submitted queued downloads in `paused` state.
    ///
    /// This setting only affects the optional RPC queue. Direct RPC adds still
    /// plan and start immediately so existing non-queue behavior stays intact.
    pub fn pause_new_downloads(mut self, pause_new_downloads: bool) -> Self {
        if pause_new_downloads {
            self.queue_config.enabled = true;
        }
        self.queue_config.pause_new_downloads = pause_new_downloads;
        self
    }

    /// Sets the maximum accepted JSON-RPC request body size in bytes.
    pub fn max_request_size(mut self, max_request_size: usize) -> Self {
        self.max_request_size = max_request_size.max(1);
        self
    }

    /// Adds permissive CORS headers to HTTP JSON-RPC responses.
    pub fn allow_origin_all(mut self, allow_origin_all: bool) -> Self {
        self.allow_origin_all = allow_origin_all;
        self
    }

    /// Enables deprecated HTTP Basic authorization compatibility.
    ///
    /// Prefer [`Self::secret`] for new clients. Basic authorization is accepted
    /// for HTTP and WebSocket requests when configured.
    pub fn basic_auth(mut self, user: impl Into<String>, password: impl Into<String>) -> Self {
        let user = user.into();
        let password = password.into();
        self.basic_auth = if user.is_empty() && password.is_empty() {
            None
        } else {
            Some(RpcBasicAuth { user, password })
        };
        self
    }

    /// Enables encrypted JSON-RPC transport using PEM certificate and key files.
    ///
    /// PKCS12 and OS certificate store lookups are intentionally not accepted
    /// because they are platform-specific and would not be portable Rust
    /// behavior.
    pub fn secure_pem_files(
        mut self,
        certificate: impl Into<PathBuf>,
        private_key: impl Into<PathBuf>,
    ) -> Self {
        self.tls = Some(RpcTlsConfig::pem_files(certificate, private_key));
        self
    }

    /// Uses an externally owned session manager.
    pub fn manager(mut self, manager: RpcSessionManager) -> Self {
        self.manager = manager;
        self
    }

    /// Builds the server.
    pub fn build(self) -> RpcServer {
        let manager = if self.queue_config == RpcQueueConfig::default() {
            self.manager
        } else {
            self.manager.with_queue_config(self.queue_config)
        };
        RpcServer {
            bind: self.bind,
            secret: self.secret,
            manager,
            max_request_size: self.max_request_size,
            allow_origin_all: self.allow_origin_all,
            basic_auth: self.basic_auth,
            tls: self.tls,
        }
    }
}

/// Shared manager for JSON-RPC downloads and retained results.
#[derive(Clone, Debug)]
pub struct RpcSessionManager {
    inner: Arc<RpcSessionInner>,
}

impl RpcSessionManager {
    /// Creates an empty manager.
    pub fn new() -> Self {
        let (notifications, _) = broadcast::channel(NOTIFICATION_CAPACITY);
        let seed = seed_gid_counter();
        Self {
            inner: Arc::new(RpcSessionInner {
                next_gid: AtomicU64::new(seed),
                next_queue_position: AtomicU64::new(0),
                session_id: format!("{seed:016x}"),
                states: Mutex::new(BTreeMap::new()),
                global_options: Mutex::new(RpcGlobalOptions::default()),
                notifications,
                queue_config: RpcQueueConfig::default(),
            }),
        }
    }

    /// Returns a manager view with the supplied queue configuration.
    pub fn with_queue_config(&self, queue_config: RpcQueueConfig) -> Self {
        Self {
            inner: Arc::new(RpcSessionInner {
                next_gid: AtomicU64::new(self.inner.next_gid.load(Ordering::Relaxed)),
                next_queue_position: AtomicU64::new(
                    self.inner.next_queue_position.load(Ordering::Relaxed),
                ),
                session_id: self.inner.session_id.clone(),
                states: Mutex::new(
                    self.lock_states()
                        .map(|states| states.clone())
                        .unwrap_or_default(),
                ),
                global_options: Mutex::new(
                    self.lock_global_options()
                        .map(|options| options.clone())
                        .unwrap_or_default(),
                ),
                notifications: self.inner.notifications.clone(),
                queue_config,
            }),
        }
    }

    /// Subscribes to lifecycle and progress notifications.
    pub fn subscribe(&self) -> broadcast::Receiver<RpcNotification> {
        self.inner.notifications.subscribe()
    }

    /// Adds a new download and returns its GID.
    pub fn add_download(&self, input: String, options: DownloadOptions) -> RpcCallResult<String> {
        self.add_download_with_options(input, options, Map::new())
    }

    /// Adds a new download with an RPC option snapshot retained for `getOption`.
    pub fn add_download_with_options(
        &self,
        input: String,
        options: DownloadOptions,
        options_snapshot: Map<String, Value>,
    ) -> RpcCallResult<String> {
        let stream_selector = stream_selector_from_options(&options);
        self.add_download_with_options_at(input, options, stream_selector, options_snapshot, None)
    }

    /// Adds a new download from an RPC option object and optional queue position.
    pub fn add_download_from_value(
        &self,
        input: String,
        options_value: Option<Value>,
        position: Option<i64>,
    ) -> RpcCallResult<String> {
        let parsed = self.merge_global_options(options_value)?;
        self.add_download_with_options_at(
            input,
            parsed.options,
            parsed.stream_selector,
            parsed.snapshot,
            position,
        )
    }

    fn add_download_with_options_at(
        &self,
        input: String,
        options: DownloadOptions,
        stream_selector: StreamSelector,
        options_snapshot: Map<String, Value>,
        position: Option<i64>,
    ) -> RpcCallResult<String> {
        if input.trim().is_empty() {
            return Err(RpcError::invalid_params("input must not be empty"));
        }

        let gid = self.next_gid();
        let cancellation_token = CancellationToken::new();
        if !self.inner.queue_config.enabled {
            let gid_for_callback = gid.clone();
            let callback_manager = self.clone();
            let progress_callback = ProgressCallback::new(move |event| {
                callback_manager
                    .record_event(&gid_for_callback, event)
                    .map_err(|error| Error::config(error.message))
            });
            let request = DownloadRequest::new(input.clone())
                .with_options(options.clone())
                .with_stream_selector(stream_selector.clone())
                .with_cancellation_token(cancellation_token.clone())
                .with_progress_callback(progress_callback);
            let session = DownloadClient::new()
                .prepare(request)
                .map_err(|error| RpcError::invalid_params(error.to_string()))?;

            let mut state = RpcDownloadState::new_with_options(RpcDownloadInit {
                gid: gid.clone(),
                input,
                options,
                stream_selector,
                options_snapshot,
                cancellation_token,
                status: RpcDownloadStatus::Active,
                queue_position: self.next_queue_position(),
            });
            state.run_generation = state.run_generation.saturating_add(1);
            let run_generation = state.run_generation;
            self.insert_state(state)?;
            self.send_lifecycle(NOTIFY_START, &gid);
            let manager = self.clone();
            let gid_for_task = gid.clone();
            tokio::spawn(async move {
                let result = Box::pin(session.start()).await;
                manager.finish_download(&gid_for_task, run_generation, result);
            });
            return Ok(gid);
        }

        let status = if self.inner.queue_config.pause_new_downloads {
            RpcDownloadStatus::Paused
        } else {
            RpcDownloadStatus::Waiting
        };
        self.insert_state(RpcDownloadState::new_with_options(RpcDownloadInit {
            gid: gid.clone(),
            input,
            options,
            stream_selector,
            options_snapshot,
            cancellation_token,
            status,
            queue_position: self.next_queue_position(),
        }))?;
        if let Some(position) = position {
            let _ = self.change_position(&gid, position, "POS_SET")?;
        }
        if status == RpcDownloadStatus::Waiting {
            self.start_waiting_downloads();
        }
        Ok(gid)
    }

    /// Returns one status object by GID.
    pub fn tell_status(&self, gid: &str, keys: &[String]) -> RpcCallResult<Value> {
        let state = self.find_state(gid)?;
        Ok(state.to_status_value(keys))
    }

    /// Returns active download status objects.
    pub fn tell_active(&self, keys: &[String]) -> RpcCallResult<Value> {
        let states = self.lock_states()?;
        let values = states
            .values()
            .filter(|state| state.status == RpcDownloadStatus::Active)
            .map(|state| state.to_status_value(keys))
            .collect::<Vec<_>>();
        Ok(Value::Array(values))
    }

    /// Returns completed, failed, or removed download status objects.
    pub fn tell_stopped(&self, offset: i64, count: usize, keys: &[String]) -> RpcCallResult<Value> {
        let states = self.lock_states()?;
        let values = paginate_states(
            ordered_states(
                states
                    .values()
                    .filter(|state| state.status.is_terminal())
                    .cloned()
                    .collect(),
            ),
            offset,
            count,
        )
        .into_iter()
        .map(|state| state.to_status_value(keys))
        .collect::<Vec<_>>();
        Ok(Value::Array(values))
    }

    /// Returns queued downloads.
    pub fn tell_waiting(&self, offset: i64, count: usize, keys: &[String]) -> RpcCallResult<Value> {
        let states = self.lock_states()?;
        let values = paginate_states(
            ordered_states(
                states
                    .values()
                    .filter(|state| state.status.is_queued())
                    .cloned()
                    .collect(),
            ),
            offset,
            count,
        )
        .into_iter()
        .map(|state| state.to_status_value(keys))
        .collect::<Vec<_>>();
        Ok(Value::Array(values))
    }

    /// Returns the global option template applied to future RPC-added jobs.
    pub fn get_global_option(&self) -> RpcCallResult<Value> {
        let options = self.lock_global_options()?;
        Ok(Value::Object(options.snapshot.clone()))
    }

    /// Changes the global option template applied to future RPC-added jobs.
    pub fn change_global_option(&self, options_value: &Value) -> RpcCallResult<String> {
        let object = options_value.as_object().ok_or_else(|| {
            RpcError::invalid_params("changeGlobalOption requires an options object")
        })?;
        let mut global = self.lock_global_options()?;
        let mut selector = global.stream_selector.clone();
        apply_options_object(&mut global.options, &mut selector, object)?;
        global.stream_selector = selector;
        global
            .snapshot
            .extend(sanitize_options_snapshot(object.clone()));
        Ok("OK".to_string())
    }

    fn merge_global_options(
        &self,
        options_value: Option<Value>,
    ) -> RpcCallResult<RpcPreparedOptions> {
        let global = self.lock_global_options()?.clone();
        let mut options = global.options;
        let mut selector = global.stream_selector;
        let mut snapshot = global.snapshot;
        if let Some(Value::Object(object)) = options_value {
            apply_options_object(&mut options, &mut selector, &object)?;
            snapshot.extend(sanitize_options_snapshot(object));
        } else if options_value.is_some() {
            return Err(RpcError::invalid_params("options must be an object"));
        }
        let stream_selector = selector.unwrap_or_else(|| stream_selector_from_options(&options));
        Ok(RpcPreparedOptions {
            options,
            stream_selector,
            snapshot,
        })
    }

    /// Returns the URI list for one download.
    pub fn get_uris(&self, gid: &str) -> RpcCallResult<Value> {
        let state = self.find_state(gid)?;
        if state.status.is_terminal() {
            return Err(RpcError::invalid_params("no uri data for gid"));
        }
        let uri_status = if state.status == RpcDownloadStatus::Active {
            "used"
        } else {
            "waiting"
        };
        Ok(Value::Array(vec![json!({
            "uri": state.input,
            "status": uri_status,
        })]))
    }

    /// Returns the file list for one download.
    pub fn get_files(&self, gid: &str) -> RpcCallResult<Value> {
        let state = self.find_state(gid)?;
        Ok(Value::Array(state.file_values()))
    }

    /// Returns server entries for one download.
    pub fn get_servers(&self, gid: &str) -> RpcCallResult<Value> {
        let state = self.find_state(gid)?;
        if state.status != RpcDownloadStatus::Active {
            return Err(RpcError::invalid_params("no active download for gid"));
        }
        if !state.input.starts_with("http://") && !state.input.starts_with("https://") {
            return Ok(Value::Array(vec![json!({
                "index": "1",
                "servers": [],
            })]));
        }
        Ok(Value::Array(vec![json!({
            "index": "1",
            "servers": [{
                "uri": state.input,
                "currentUri": state.input,
                "downloadSpeed": state.download_speed.to_string(),
            }],
        })]))
    }

    /// Returns the option snapshot supplied when the download was added.
    pub fn get_option(&self, gid: &str) -> RpcCallResult<Value> {
        let state = self.find_state(gid)?;
        Ok(Value::Object(state.options_snapshot))
    }

    /// Cancels or removes a download by GID.
    pub fn remove(&self, gid: &str) -> RpcCallResult<String> {
        let mut states = self.lock_states()?;
        let state = find_state_mut(&mut states, gid)?;
        if state.status.is_terminal() {
            return Err(RpcError::invalid_params(
                "download result cannot be removed",
            ));
        }
        let was_active = state.status == RpcDownloadStatus::Active;
        state.cancellation_token.cancel();
        state.status = RpcDownloadStatus::Removed;
        state.updated_at_ms = now_ms();
        let gid = state.gid.clone();
        drop(states);
        self.send_lifecycle(NOTIFY_STOP, &gid);
        if !was_active {
            self.start_waiting_downloads();
        }
        Ok(gid)
    }

    /// Pauses a queued or active download.
    pub fn pause(&self, gid: &str) -> RpcCallResult<String> {
        let mut states = self.lock_states()?;
        let actual_gid = find_state(&states, gid)?.gid.clone();
        let mut active_token = None;
        {
            let state = states
                .get_mut(&actual_gid)
                .ok_or_else(|| RpcError::invalid_params("gid not found"))?;
            match state.status {
                RpcDownloadStatus::Waiting | RpcDownloadStatus::Active => {
                    if state.status == RpcDownloadStatus::Active {
                        active_token = Some(state.cancellation_token.clone());
                        state.cancellation_token = CancellationToken::new();
                    }
                    state.status = RpcDownloadStatus::Paused;
                    state.updated_at_ms = now_ms();
                }
                RpcDownloadStatus::Paused => {
                    return Err(RpcError::invalid_params("download cannot be paused"));
                }
                _ => return Err(RpcError::invalid_params("download cannot be paused")),
            }
        }
        move_queued_state_to_front(&mut states, &actual_gid);
        drop(states);
        if let Some(token) = active_token {
            token.cancel();
        }
        self.send_lifecycle(NOTIFY_PAUSE, &actual_gid);
        Ok(actual_gid)
    }

    /// Resumes a paused queued download.
    pub fn unpause(&self, gid: &str) -> RpcCallResult<String> {
        let mut states = self.lock_states()?;
        let state = find_state_mut(&mut states, gid)?;
        match state.status {
            RpcDownloadStatus::Paused => {
                state.status = RpcDownloadStatus::Waiting;
                state.updated_at_ms = now_ms();
                let gid = state.gid.clone();
                drop(states);
                self.start_waiting_downloads();
                Ok(gid)
            }
            _ => Err(RpcError::invalid_params("download is not paused")),
        }
    }

    /// Pauses all queued or active downloads.
    pub fn pause_all(&self) -> RpcCallResult<String> {
        let mut paused = Vec::new();
        let mut active_tokens = Vec::new();
        let mut states = self.lock_states()?;
        for state in states.values_mut() {
            match state.status {
                RpcDownloadStatus::Waiting => {
                    state.status = RpcDownloadStatus::Paused;
                    state.updated_at_ms = now_ms();
                    paused.push(state.gid.clone());
                }
                RpcDownloadStatus::Active => {
                    active_tokens.push(state.cancellation_token.clone());
                    state.cancellation_token = CancellationToken::new();
                    state.status = RpcDownloadStatus::Paused;
                    state.updated_at_ms = now_ms();
                    paused.push(state.gid.clone());
                }
                _ => {}
            }
        }
        drop(states);
        for token in active_tokens {
            token.cancel();
        }
        for gid in paused {
            self.send_lifecycle(NOTIFY_PAUSE, &gid);
        }
        Ok("OK".to_string())
    }

    /// Resumes all paused queued downloads.
    pub fn unpause_all(&self) -> RpcCallResult<String> {
        let mut states = self.lock_states()?;
        for state in states.values_mut() {
            if state.status == RpcDownloadStatus::Paused {
                state.status = RpcDownloadStatus::Waiting;
                state.updated_at_ms = now_ms();
            }
        }
        drop(states);
        self.start_waiting_downloads();
        Ok("OK".to_string())
    }

    /// Reorders a queued download and returns its new zero-based position.
    pub fn change_position(&self, gid: &str, pos: i64, how: &str) -> RpcCallResult<i64> {
        let mut states = self.lock_states()?;
        let actual_gid = find_state(&states, gid)?.gid.clone();
        let mut queue = ordered_states(
            states
                .values()
                .filter(|state| state.status.is_queued())
                .cloned()
                .collect(),
        );
        let current = queue
            .iter()
            .position(|state| state.gid == actual_gid)
            .ok_or_else(|| RpcError::invalid_params("download is not queued"))?;
        let item = queue.remove(current);
        let len = queue.len();
        let target = match how {
            "POS_SET" => pos,
            "POS_CUR" => i64::try_from(current)
                .unwrap_or(i64::MAX)
                .saturating_add(pos),
            "POS_END" => i64::try_from(len).unwrap_or(i64::MAX).saturating_add(pos),
            _ => return Err(RpcError::invalid_params("position mode is invalid")),
        }
        .clamp(0, i64::try_from(len).unwrap_or(i64::MAX));
        let target_usize =
            usize::try_from(target).map_err(|_| RpcError::invalid_params("position is invalid"))?;
        queue.insert(target_usize, item);
        for (index, state) in queue.iter().enumerate() {
            if let Some(original) = states.get_mut(&state.gid) {
                original.queue_position = u64::try_from(index).unwrap_or(u64::MAX);
                original.updated_at_ms = now_ms();
            }
        }
        Ok(target)
    }

    /// Replaces the single source URI for a queued or paused download.
    pub fn change_uri(
        &self,
        gid: &str,
        file_index: usize,
        del_uris: &[String],
        add_uris: &[String],
        position: Option<i64>,
    ) -> RpcCallResult<Value> {
        if file_index != 1 {
            return Err(RpcError::invalid_params("fileIndex is out of range"));
        }
        if position.is_some_and(|position| position != 0) {
            return Err(RpcError::invalid_params(
                "haki.changeUri supports only the single source URI position",
            ));
        }
        if add_uris.len() > 1 {
            return Err(RpcError::invalid_params(
                "haki.changeUri supports exactly one added URI",
            ));
        }
        if add_uris.first().is_some_and(|uri| uri.trim().is_empty()) {
            return Err(RpcError::invalid_params("URI must not be empty"));
        }

        let mut states = self.lock_states()?;
        let state = find_state_mut(&mut states, gid)?;
        if !state.status.is_queued() {
            return Err(RpcError::invalid_params(
                "haki.changeUri supports queued or paused downloads only",
            ));
        }

        let deleted = usize::from(del_uris.iter().any(|uri| uri == &state.input));
        if add_uris.is_empty() {
            return if deleted == 0 {
                Ok(json!([0, 0]))
            } else {
                Err(RpcError::invalid_params(
                    "haki.changeUri cannot remove the only source URI",
                ))
            };
        }
        if deleted == 0 {
            return Err(RpcError::invalid_params(
                "haki.changeUri requires deleting the current URI before replacement",
            ));
        }

        state.input = add_uris[0].clone();
        state.updated_at_ms = now_ms();
        Ok(json!([deleted, 1]))
    }

    /// Changes options for a queued download or restarts an active download
    /// with the updated options.
    pub fn change_option(&self, gid: &str, options_value: &Value) -> RpcCallResult<String> {
        let object = options_value
            .as_object()
            .ok_or_else(|| RpcError::invalid_params("changeOption requires an options object"))?;
        let mut active_restart = None;
        let mut active_token = None;
        let mut states = self.lock_states()?;
        let state = find_state_mut(&mut states, gid)?;
        let is_active = state.status == RpcDownloadStatus::Active;
        if !is_active && !state.status.is_queued() {
            return Err(RpcError::invalid_params("download cannot change options"));
        }
        let mut selector = Some(state.stream_selector.clone());
        apply_options_object(&mut state.options, &mut selector, object)?;
        state.stream_selector =
            selector.unwrap_or_else(|| stream_selector_from_options(&state.options));
        state
            .options_snapshot
            .extend(sanitize_options_snapshot(object.clone()));
        state.updated_at_ms = now_ms();
        if is_active {
            active_token = Some(state.cancellation_token.clone());
            state.cancellation_token = CancellationToken::new();
            state.run_generation = state.run_generation.saturating_add(1);
            active_restart = Some(RpcQueuedStart::from_state(state));
        }
        drop(states);
        if let Some(token) = active_token {
            token.cancel();
        }
        if let Some(start) = active_restart {
            self.spawn_download(start);
        }
        Ok("OK".to_string())
    }

    /// Removes one retained completed, failed, or cancelled result.
    pub fn remove_download_result(&self, gid: &str) -> RpcCallResult<String> {
        let mut states = self.lock_states()?;
        let actual_gid = find_state(&states, gid)?.gid.clone();
        let active = states
            .get(&actual_gid)
            .map(|state| !state.status.is_terminal())
            .unwrap_or(false);
        if active {
            return Err(RpcError::invalid_params(
                "cannot remove result for an unfinished download",
            ));
        }
        states.remove(&actual_gid);
        Ok("OK".to_string())
    }

    /// Removes all retained non-active results.
    pub fn purge_download_results(&self) -> RpcCallResult<String> {
        let mut states = self.lock_states()?;
        states.retain(|_, state| !state.status.is_terminal());
        Ok("OK".to_string())
    }

    /// Returns aggregate active-download counters.
    pub fn global_stat(&self) -> RpcCallResult<Value> {
        let states = self.lock_states()?;
        let mut download_speed = 0_u64;
        let mut completed_length = 0_u64;
        let mut active_count = 0_u64;
        let mut waiting_count = 0_u64;
        let mut stopped_count = 0_u64;
        for state in states.values() {
            if state.status == RpcDownloadStatus::Active {
                active_count += 1;
                download_speed = download_speed.saturating_add(state.download_speed);
                completed_length = completed_length.saturating_add(state.completed_length);
            } else if state.status.is_queued() {
                waiting_count += 1;
            } else {
                stopped_count += 1;
            }
        }
        Ok(json!({
            "downloadSpeed": download_speed.to_string(),
            "uploadSpeed": "0",
            "numActive": active_count.to_string(),
            "numWaiting": waiting_count.to_string(),
            "numStopped": stopped_count.to_string(),
            "numStoppedTotal": stopped_count.to_string(),
            "completedLength": completed_length.to_string(),
        }))
    }

    /// Returns server session identity.
    pub fn session_info(&self) -> Value {
        json!({
            "sessionId": self.inner.session_id.clone(),
        })
    }

    fn next_gid(&self) -> String {
        let value = self.inner.next_gid.fetch_add(1, Ordering::Relaxed);
        format!("{value:016x}")
    }

    fn next_queue_position(&self) -> u64 {
        self.inner
            .next_queue_position
            .fetch_add(1, Ordering::Relaxed)
    }

    fn insert_state(&self, state: RpcDownloadState) -> RpcCallResult<()> {
        let mut states = self.lock_states()?;
        prune_retained_states(&mut states, MAX_RETAINED_DOWNLOADS.saturating_sub(1));
        if states
            .values()
            .filter(|state| state.status == RpcDownloadStatus::Active)
            .count()
            >= MAX_ACTIVE_DOWNLOADS
            && state.status == RpcDownloadStatus::Active
        {
            return Err(RpcError::invalid_params("too many active downloads"));
        }
        if states.len() >= MAX_RETAINED_DOWNLOADS {
            return Err(RpcError::invalid_params("too many retained downloads"));
        }
        states.insert(state.gid.clone(), state);
        Ok(())
    }

    fn start_waiting_downloads(&self) {
        let Ok(starts) = self.dequeue_waiting_downloads() else {
            return;
        };
        for start in starts {
            self.spawn_download(start);
        }
    }

    fn dequeue_waiting_downloads(&self) -> RpcCallResult<Vec<RpcQueuedStart>> {
        let mut states = self.lock_states()?;
        let active_count = states
            .values()
            .filter(|state| state.status == RpcDownloadStatus::Active)
            .count();
        let max_active = if self.inner.queue_config.enabled {
            self.inner.queue_config.max_concurrent_downloads.max(1)
        } else {
            MAX_ACTIVE_DOWNLOADS
        };
        let slots = max_active.saturating_sub(active_count);
        if slots == 0 {
            return Ok(Vec::new());
        }
        let gids = ordered_states(
            states
                .values()
                .filter(|state| state.status == RpcDownloadStatus::Waiting)
                .cloned()
                .collect(),
        )
        .into_iter()
        .take(slots)
        .map(|state| state.gid)
        .collect::<Vec<_>>();
        let mut starts = Vec::with_capacity(gids.len());
        for gid in gids {
            if let Some(state) = states.get_mut(&gid) {
                state.status = RpcDownloadStatus::Active;
                state.run_generation = state.run_generation.saturating_add(1);
                state.updated_at_ms = now_ms();
                starts.push(RpcQueuedStart::from_state(state));
            }
        }
        Ok(starts)
    }

    fn spawn_download(&self, start: RpcQueuedStart) {
        self.send_lifecycle(NOTIFY_START, &start.gid);
        let manager = self.clone();
        tokio::spawn(async move {
            let gid_for_callback = start.gid.clone();
            let callback_manager = manager.clone();
            let progress_callback = ProgressCallback::new(move |event| {
                callback_manager
                    .record_event(&gid_for_callback, event)
                    .map_err(|error| Error::config(error.message))
            });
            let request = DownloadRequest::new(start.input)
                .with_options(start.options)
                .with_stream_selector(start.stream_selector)
                .with_cancellation_token(start.cancellation_token)
                .with_progress_callback(progress_callback);
            let result = match DownloadClient::new().prepare(request) {
                Ok(session) => Box::pin(session.start()).await,
                Err(error) => Err(error),
            };
            manager.finish_download(&start.gid, start.run_generation, result);
        });
    }

    fn find_state(&self, gid: &str) -> RpcCallResult<RpcDownloadState> {
        let states = self.lock_states()?;
        Ok(find_state(&states, gid)?.clone())
    }

    fn lock_states(
        &self,
    ) -> RpcCallResult<std::sync::MutexGuard<'_, BTreeMap<String, RpcDownloadState>>> {
        self.inner
            .states
            .lock()
            .map_err(|_| RpcError::internal("rpc session state is unavailable"))
    }

    fn lock_global_options(&self) -> RpcCallResult<std::sync::MutexGuard<'_, RpcGlobalOptions>> {
        self.inner
            .global_options
            .lock()
            .map_err(|_| RpcError::internal("rpc global options are unavailable"))
    }

    fn record_event(&self, gid: &str, event: &ProgressEvent) -> RpcCallResult<()> {
        let mut states = self.lock_states()?;
        let state = match states.get_mut(gid) {
            Some(state) => state,
            None => return Ok(()),
        };
        state.apply_event(event);
        let notification = if should_push_progress(event) {
            Some(RpcNotification::progress(
                state.gid.clone(),
                state.to_status_value(&[]),
            ))
        } else {
            None
        };
        drop(states);
        if let Some(notification) = notification {
            self.send_notification(notification);
        }
        Ok(())
    }

    fn finish_download(&self, gid: &str, run_generation: u64, result: Result<Vec<ProgressEvent>>) {
        let mut lifecycle = Some(NOTIFY_COMPLETE);
        let mut stale_completion = false;
        if let Ok(mut states) = self.lock_states()
            && let Some(state) = states.get_mut(gid)
        {
            if state.run_generation != run_generation {
                stale_completion = true;
            } else {
                state.updated_at_ms = now_ms();
                match result {
                    Ok(events) => {
                        for event in events {
                            state.apply_event(&event);
                        }
                        if state.status != RpcDownloadStatus::Removed {
                            state.status = RpcDownloadStatus::Complete;
                        } else {
                            lifecycle = Some(NOTIFY_STOP);
                        }
                    }
                    Err(Error::UserCancelled) => {
                        if state.status == RpcDownloadStatus::Paused {
                            lifecycle = None;
                        } else {
                            state.status = RpcDownloadStatus::Removed;
                            state.error_message = Some("operation cancelled".to_string());
                            lifecycle = Some(NOTIFY_STOP);
                        }
                    }
                    Err(error) => {
                        state.status = RpcDownloadStatus::Error;
                        state.error_message = Some(error.to_string());
                        lifecycle = Some(NOTIFY_ERROR);
                    }
                }
                prune_retained_states(&mut states, MAX_RETAINED_DOWNLOADS);
            }
        }
        if stale_completion {
            return;
        }
        if let Some(lifecycle) = lifecycle {
            self.send_lifecycle(lifecycle, gid);
        }
        self.start_waiting_downloads();
    }

    fn send_lifecycle(&self, method: &str, gid: &str) {
        self.send_notification(RpcNotification::lifecycle(method, gid));
    }

    fn send_notification(&self, notification: RpcNotification) {
        let _ = self.inner.notifications.send(notification);
    }
}

impl Default for RpcSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct RpcSessionInner {
    next_gid: AtomicU64,
    next_queue_position: AtomicU64,
    session_id: String,
    states: Mutex<BTreeMap<String, RpcDownloadState>>,
    global_options: Mutex<RpcGlobalOptions>,
    notifications: broadcast::Sender<RpcNotification>,
    queue_config: RpcQueueConfig,
}

#[derive(Clone, Debug, Default)]
struct RpcGlobalOptions {
    options: DownloadOptions,
    stream_selector: Option<StreamSelector>,
    snapshot: Map<String, Value>,
}

#[derive(Clone, Debug)]
struct RpcPreparedOptions {
    options: DownloadOptions,
    stream_selector: StreamSelector,
    snapshot: Map<String, Value>,
}

#[derive(Clone, Debug)]
struct RpcQueuedStart {
    gid: String,
    input: String,
    options: DownloadOptions,
    stream_selector: StreamSelector,
    cancellation_token: CancellationToken,
    run_generation: u64,
}

impl RpcQueuedStart {
    fn from_state(state: &RpcDownloadState) -> Self {
        Self {
            gid: state.gid.clone(),
            input: state.input.clone(),
            options: state.options.clone(),
            stream_selector: state.stream_selector.clone(),
            cancellation_token: state.cancellation_token.clone(),
            run_generation: state.run_generation,
        }
    }
}

#[derive(Clone, Debug)]
struct RpcDownloadState {
    gid: String,
    input: String,
    options: DownloadOptions,
    stream_selector: StreamSelector,
    options_snapshot: Map<String, Value>,
    status: RpcDownloadStatus,
    cancellation_token: CancellationToken,
    run_generation: u64,
    queue_position: u64,
    total_length: Option<u64>,
    completed_length: u64,
    download_speed: u64,
    streams: BTreeMap<String, RpcStreamState>,
    artifacts: Vec<String>,
    error_message: Option<String>,
    created_at_ms: u128,
    updated_at_ms: u128,
}

struct RpcDownloadInit {
    gid: String,
    input: String,
    options: DownloadOptions,
    stream_selector: StreamSelector,
    options_snapshot: Map<String, Value>,
    cancellation_token: CancellationToken,
    status: RpcDownloadStatus,
    queue_position: u64,
}

impl RpcDownloadState {
    fn new_with_options(init: RpcDownloadInit) -> Self {
        let now = now_ms();
        Self {
            gid: init.gid,
            input: init.input,
            options: init.options,
            stream_selector: init.stream_selector,
            options_snapshot: init.options_snapshot,
            status: init.status,
            cancellation_token: init.cancellation_token,
            run_generation: 0,
            queue_position: init.queue_position,
            total_length: None,
            completed_length: 0,
            download_speed: 0,
            streams: BTreeMap::new(),
            artifacts: Vec::new(),
            error_message: None,
            created_at_ms: now,
            updated_at_ms: now,
        }
    }

    fn apply_event(&mut self, event: &ProgressEvent) {
        self.updated_at_ms = now_ms();
        match event {
            ProgressEvent::AggregateProgress(progress) => self.apply_aggregate(*progress),
            ProgressEvent::StreamProgress(progress) => self.apply_stream(progress),
            ProgressEvent::LiveRefresh {
                stream_id,
                label,
                recorded_segments,
                total_segments,
                recorded_size,
                ..
            } => {
                let stream_id = stream_id
                    .clone()
                    .or_else(|| label.clone())
                    .unwrap_or_else(|| "live".to_string());
                let stream = self.streams.entry(stream_id.clone()).or_insert_with(|| {
                    RpcStreamState::new(stream_id.clone(), label.clone().unwrap_or(stream_id))
                });
                stream.completed_segments = *recorded_segments;
                stream.total_segments = Some(*total_segments);
                if let Some(size) = recorded_size {
                    stream.completed_length = *size;
                }
            }
            ProgressEvent::SegmentFinished {
                stream_id,
                segment_index,
            } => {
                let stream = self
                    .streams
                    .entry(stream_id.clone())
                    .or_insert_with(|| RpcStreamState::new(stream_id.clone(), stream_id.clone()));
                stream.finished_segments.insert(*segment_index);
                stream.completed_segments = stream
                    .completed_segments
                    .max(segment_index.saturating_add(1));
            }
            ProgressEvent::OutputArtifact(artifact) => {
                self.artifacts.push(artifact.path.display().to_string());
            }
            ProgressEvent::Cancelled => {
                self.status = RpcDownloadStatus::Removed;
            }
            ProgressEvent::Finished { success } => {
                if *success && self.status == RpcDownloadStatus::Active {
                    self.status = RpcDownloadStatus::Complete;
                } else if !success {
                    self.status = RpcDownloadStatus::Error;
                }
            }
            _ => {}
        }
    }

    fn apply_aggregate(&mut self, progress: AggregateProgress) {
        self.completed_length = progress.downloaded_bytes;
        self.total_length = progress.total_bytes;
        self.download_speed = progress.bytes_per_second;
    }

    fn apply_stream(&mut self, progress: &StreamProgress) {
        let stream = self
            .streams
            .entry(progress.stream_id.clone())
            .or_insert_with(|| {
                RpcStreamState::new(progress.stream_id.clone(), progress.stream_id.clone())
            });
        stream.completed_length = progress.downloaded_bytes;
        stream.total_length = progress.total_bytes;
        stream.download_speed = progress.bytes_per_second;
        stream.completed_segments = progress.completed_segments;
        stream.total_segments = progress.total_segments;
        self.completed_length = self
            .streams
            .values()
            .map(|stream| stream.completed_length)
            .fold(0_u64, u64::saturating_add);
        let known_total = self
            .streams
            .values()
            .map(|stream| stream.total_length)
            .try_fold(0_u64, |acc, total| {
                total.map(|value| acc.saturating_add(value))
            });
        self.total_length = known_total;
        self.download_speed = self
            .streams
            .values()
            .map(|stream| stream.download_speed)
            .fold(0_u64, u64::saturating_add);
    }

    fn to_status_value(&self, keys: &[String]) -> Value {
        let mut object = Map::new();
        self.insert_status_key(&mut object, keys, "gid", Value::String(self.gid.clone()));
        self.insert_status_key(
            &mut object,
            keys,
            "status",
            Value::String(self.status.as_str().to_string()),
        );
        self.insert_status_key(
            &mut object,
            keys,
            "totalLength",
            Value::String(self.total_length.unwrap_or(0).to_string()),
        );
        self.insert_status_key(
            &mut object,
            keys,
            "completedLength",
            Value::String(self.completed_length.to_string()),
        );
        self.insert_status_key(
            &mut object,
            keys,
            "downloadSpeed",
            Value::String(self.download_speed.to_string()),
        );
        self.insert_status_key(
            &mut object,
            keys,
            "uploadSpeed",
            Value::String("0".to_string()),
        );
        self.insert_status_key(
            &mut object,
            keys,
            "uploadLength",
            Value::String("0".to_string()),
        );
        if self.status.is_terminal() {
            self.insert_status_key(
                &mut object,
                keys,
                "errorCode",
                Value::String(
                    if self.status == RpcDownloadStatus::Error {
                        "1"
                    } else {
                        "0"
                    }
                    .to_string(),
                ),
            );
        }
        self.insert_status_key(
            &mut object,
            keys,
            "connections",
            Value::String(if self.status == RpcDownloadStatus::Active {
                "1".to_string()
            } else {
                "0".to_string()
            }),
        );
        self.insert_status_key(
            &mut object,
            keys,
            "bitfield",
            Value::String(self.segment_bitfield()),
        );
        self.insert_status_key(
            &mut object,
            keys,
            "pieceLength",
            Value::String(self.segment_piece_length().to_string()),
        );
        self.insert_status_key(
            &mut object,
            keys,
            "numPieces",
            Value::String(self.segment_piece_count().to_string()),
        );
        self.insert_status_key(&mut object, keys, "dir", Value::String(self.status_dir()));
        self.insert_status_key(&mut object, keys, "files", Value::Array(self.file_values()));
        self.insert_status_key(
            &mut object,
            keys,
            "input",
            Value::String(self.input.clone()),
        );
        self.insert_status_key(
            &mut object,
            keys,
            "streamProgress",
            Value::Array(
                self.streams
                    .values()
                    .map(RpcStreamState::to_value)
                    .collect::<Vec<_>>(),
            ),
        );
        self.insert_status_key(
            &mut object,
            keys,
            "artifacts",
            Value::Array(
                self.artifacts
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect::<Vec<_>>(),
            ),
        );
        self.insert_status_key(
            &mut object,
            keys,
            "errorMessage",
            self.error_message
                .as_ref()
                .map(|message| Value::String(message.clone()))
                .unwrap_or(Value::Null),
        );
        self.insert_status_key(
            &mut object,
            keys,
            "createdAt",
            Value::String(self.created_at_ms.to_string()),
        );
        self.insert_status_key(
            &mut object,
            keys,
            "updatedAt",
            Value::String(self.updated_at_ms.to_string()),
        );
        self.insert_status_key(
            &mut object,
            keys,
            "queuePosition",
            Value::String(self.queue_position.to_string()),
        );
        Value::Object(object)
    }

    fn file_values(&self) -> Vec<Value> {
        if self.artifacts.is_empty() {
            return vec![json!({
                "index": "1",
                "path": "",
                "length": self.total_length.unwrap_or(0).to_string(),
                "completedLength": self.completed_length.to_string(),
                "selected": "true",
                "uris": [{ "uri": self.input, "status": "used" }],
            })];
        }
        self.artifacts
            .iter()
            .enumerate()
            .map(|(index, path)| {
                json!({
                    "index": (index + 1).to_string(),
                    "path": path,
                    "length": "0",
                    "completedLength": "0",
                    "selected": "true",
                    "uris": [],
                })
            })
            .collect()
    }

    fn status_dir(&self) -> String {
        if let Some(path) = &self.options.save_dir {
            return path.display().to_string();
        }
        std::env::current_dir()
            .ok()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| ".".to_string())
    }

    fn segment_piece_count(&self) -> u64 {
        self.streams
            .values()
            .map(RpcStreamState::piece_count)
            .fold(0_u64, u64::saturating_add)
    }

    fn segment_piece_length(&self) -> u64 {
        let pieces = self.segment_piece_count();
        if pieces == 0 {
            return 0;
        }
        self.total_length
            .map(|total| total.div_ceil(pieces))
            .unwrap_or(0)
    }

    fn segment_bitfield(&self) -> String {
        let total = self.segment_piece_count();
        if total == 0 {
            return String::new();
        }
        let Ok(byte_len) = usize::try_from(total.div_ceil(8)) else {
            return String::new();
        };
        let mut bytes = vec![0_u8; byte_len];
        let mut offset = 0_u64;
        for stream in self.streams.values() {
            stream.write_bitfield(&mut bytes, offset);
            offset = offset.saturating_add(stream.piece_count());
        }
        bytes
            .into_iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join("")
    }

    fn insert_status_key(
        &self,
        object: &mut Map<String, Value>,
        keys: &[String],
        key: &str,
        value: Value,
    ) {
        if keys.is_empty() || keys.iter().any(|candidate| candidate == key) {
            object.insert(key.to_string(), value);
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RpcStreamState {
    stream_id: String,
    label: String,
    total_length: Option<u64>,
    completed_length: u64,
    download_speed: u64,
    completed_segments: u64,
    total_segments: Option<u64>,
    finished_segments: BTreeSet<u64>,
}

impl RpcStreamState {
    fn new(stream_id: String, label: String) -> Self {
        Self {
            stream_id,
            label,
            total_length: None,
            completed_length: 0,
            download_speed: 0,
            completed_segments: 0,
            total_segments: None,
            finished_segments: BTreeSet::new(),
        }
    }

    fn to_value(&self) -> Value {
        json!({
            "streamId": self.stream_id,
            "label": self.label,
            "totalLength": self.total_length.unwrap_or(0).to_string(),
            "completedLength": self.completed_length.to_string(),
            "downloadSpeed": self.download_speed.to_string(),
            "completedSegments": self.completed_segments.to_string(),
            "totalSegments": self.total_segments.unwrap_or(0).to_string(),
        })
    }

    fn piece_count(&self) -> u64 {
        self.total_segments
            .unwrap_or(self.completed_segments)
            .max(self.completed_segments)
            .max(u64::try_from(self.finished_segments.len()).unwrap_or(u64::MAX))
    }

    fn write_bitfield(&self, bytes: &mut [u8], offset: u64) {
        let piece_count = self.piece_count();
        if self.finished_segments.is_empty() {
            for index in 0..self.completed_segments.min(piece_count) {
                set_bit(bytes, offset.saturating_add(index));
            }
            return;
        }
        for index in &self.finished_segments {
            if *index < piece_count {
                set_bit(bytes, offset.saturating_add(*index));
            }
        }
    }
}

fn set_bit(bytes: &mut [u8], index: u64) {
    let Ok(byte_index) = usize::try_from(index / 8) else {
        return;
    };
    if let Some(byte) = bytes.get_mut(byte_index) {
        let bit = 7_u32.saturating_sub(u32::try_from(index % 8).unwrap_or(0));
        *byte |= 1_u8 << bit;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RpcDownloadStatus {
    Waiting,
    Active,
    Paused,
    Complete,
    Error,
    Removed,
}

impl RpcDownloadStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Complete => "complete",
            Self::Error => "error",
            Self::Removed => "removed",
        }
    }

    fn is_queued(self) -> bool {
        matches!(self, Self::Waiting | Self::Paused)
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Error | Self::Removed)
    }
}

/// Server-sent JSON-RPC notification.
#[derive(Clone, Debug, Serialize)]
pub struct RpcNotification {
    jsonrpc: &'static str,
    method: String,
    params: Vec<Value>,
}

impl RpcNotification {
    fn lifecycle(method: &str, gid: &str) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            method: method.to_string(),
            params: vec![json!({ "gid": gid })],
        }
    }

    fn progress(gid: String, status: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            method: NOTIFY_PROGRESS.to_string(),
            params: vec![json!({
                "gid": gid,
                "status": status,
            })],
        }
    }
}

#[derive(Clone)]
struct RpcAppState {
    manager: RpcSessionManager,
    secret: Option<String>,
    max_request_size: usize,
    allow_origin_all: bool,
    basic_auth: Option<RpcBasicAuth>,
    shutdown: broadcast::Sender<()>,
}

#[derive(Clone, Copy, Debug, Default)]
struct RpcRequestAuth {
    basic_authorized: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct JsonRpcRequest {
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

#[derive(Clone, Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcErrorObject>,
}

impl JsonRpcResponse {
    fn result(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id: id.unwrap_or(Value::Null),
            result: Some(result),
            error: None,
        }
    }

    fn error(id: Option<Value>, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id: id.unwrap_or(Value::Null),
            result: None,
            error: Some(JsonRpcErrorObject {
                code: error.code,
                message: error.message,
            }),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct JsonRpcErrorObject {
    code: i64,
    message: String,
}

/// Result alias used by the JSON-RPC manager API.
pub type RpcCallResult<T> = std::result::Result<T, RpcError>;

/// JSON-RPC method error with a protocol code and message.
#[derive(Clone, Debug)]
pub struct RpcError {
    /// JSON-RPC error code.
    pub code: i64,
    /// JSON-RPC error message.
    pub message: String,
}

impl RpcError {
    fn parse_error(message: impl Into<String>) -> Self {
        Self {
            code: -32700,
            message: message.into(),
        }
    }

    fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            code: -32600,
            message: message.into(),
        }
    }

    fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("method not found: {method}"),
        }
    }

    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: message.into(),
        }
    }

    fn unauthorized() -> Self {
        Self {
            code: -32001,
            message: "unauthorized".to_string(),
        }
    }
}

async fn rpc_root() -> &'static str {
    "haki-dl JSON-RPC is available at /jsonrpc"
}

async fn http_jsonrpc(
    State(state): State<Arc<RpcAppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let auth = RpcRequestAuth {
        basic_authorized: basic_auth_authorized(&headers, state.basic_auth.as_ref()),
    };
    let response = if body.len() > state.max_request_size {
        Value::Object(response_value(JsonRpcResponse::error(
            None,
            RpcError::invalid_request("JSON-RPC request body is too large"),
        )))
    } else {
        match parse_jsonrpc_body(&body) {
            Ok(value) => handle_jsonrpc_value(&state, value, auth).await,
            Err(error) => Value::Object(response_value(JsonRpcResponse::error(None, error))),
        }
    };
    with_optional_cors(
        (StatusCode::OK, axum::Json(response)).into_response(),
        state.allow_origin_all,
    )
}

async fn http_jsonrpc_options(State(state): State<Arc<RpcAppState>>) -> Response {
    with_optional_cors(
        StatusCode::NO_CONTENT.into_response(),
        state.allow_origin_all,
    )
}

async fn get_jsonrpc_or_ws(
    State(state): State<Arc<RpcAppState>>,
    request: Request<Body>,
) -> impl IntoResponse {
    let (mut parts, _) = request.into_parts();
    let headers = parts.headers.clone();
    let auth = RpcRequestAuth {
        basic_authorized: basic_auth_authorized(&headers, state.basic_auth.as_ref()),
    };
    if is_websocket_upgrade(&headers) {
        return match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
            Ok(ws) => ws
                .on_upgrade(move |socket| handle_websocket(state, socket, auth))
                .into_response(),
            Err(rejection) => rejection.into_response(),
        };
    }
    let query = parse_get_query_string(parts.uri.query().unwrap_or_default());
    let (response, callback) = match parse_jsonrpc_get_query(&query) {
        Ok((value, callback)) => (handle_jsonrpc_value(&state, value, auth).await, callback),
        Err(error) => (
            Value::Object(response_value(JsonRpcResponse::error(None, error))),
            query.get("jsoncallback").cloned(),
        ),
    };
    let text = json_or_jsonp_text(&response, callback.as_deref());
    let mut response = Response::new(Body::from(text));
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/javascript; charset=utf-8"),
    );
    with_optional_cors(response, state.allow_origin_all)
}

async fn handle_websocket(state: Arc<RpcAppState>, mut socket: WebSocket, auth: RpcRequestAuth) {
    let mut notifications = state.manager.subscribe();
    loop {
        tokio::select! {
            message = socket.recv() => {
                let Some(message) = message else {
                    break;
                };
                let Ok(message) = message else {
                    break;
                };
                match message {
                    Message::Text(text) => {
                        let value = if text.len() > state.max_request_size {
                            Value::Object(response_value(JsonRpcResponse::error(
                                None,
                                RpcError::invalid_request("JSON-RPC request body is too large"),
                            )))
                        } else {
                            match serde_json::from_str::<Value>(&text) {
                            Ok(value) => handle_jsonrpc_value(&state, value, auth).await,
                            Err(error) => Value::Object(response_value(JsonRpcResponse::error(
                                None,
                                RpcError::parse_error(format!("invalid JSON: {error}")),
                            ))),
                            }
                        };
                        if send_ws_json(&mut socket, &value).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            notification = notifications.recv() => {
                let Ok(notification) = notification else {
                    continue;
                };
                let Ok(value) = serde_json::to_value(notification) else {
                    continue;
                };
                if send_ws_json(&mut socket, &value).await.is_err() {
                    break;
                }
            }
        }
    }
}

fn with_optional_cors(mut response: Response, allow_origin_all: bool) -> Response {
    if allow_origin_all {
        let headers = response.headers_mut();
        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
        headers.insert(
            ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("POST, GET, OPTIONS"),
        );
        headers.insert(
            ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("content-type, authorization"),
        );
    }
    response
}

fn basic_auth_authorized(headers: &HeaderMap, basic_auth: Option<&RpcBasicAuth>) -> bool {
    let Some(basic_auth) = basic_auth else {
        return false;
    };
    let Some(header) = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Some(encoded) = header.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(decoded) = decode_base64(encoded) else {
        return false;
    };
    let Ok(decoded) = String::from_utf8(decoded) else {
        return false;
    };
    let Some((user, password)) = decoded.split_once(':') else {
        return false;
    };
    user == basic_auth.user && password == basic_auth.password
}

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

fn parse_get_query_string(query: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        values.insert(percent_decode_query(key), percent_decode_query(value));
    }
    values
}

fn percent_decode_query(value: &str) -> String {
    let mut output = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hi = hex_value(bytes[index + 1]);
                let lo = hex_value(bytes[index + 2]);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    output.push((hi << 4) | lo);
                    index += 3;
                } else {
                    output.push(bytes[index]);
                    index += 1;
                }
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_jsonrpc_get_query(
    query: &BTreeMap<String, String>,
) -> RpcCallResult<(Value, Option<String>)> {
    let callback = query
        .get("jsoncallback")
        .filter(|value| !value.is_empty())
        .cloned();
    let method = query.get("method").filter(|value| !value.is_empty());
    let id = query.get("id").filter(|value| !value.is_empty());
    let params_json = match query.get("params").filter(|value| !value.is_empty()) {
        Some(params) => {
            let decoded = decode_base64(params)
                .map_err(|error| RpcError::parse_error(format!("invalid GET params: {error}")))?;
            Some(serde_json::from_slice::<Value>(&decoded).map_err(|error| {
                RpcError::parse_error(format!("invalid GET params JSON: {error}"))
            })?)
        }
        None => None,
    };
    if method.is_none() && id.is_none() {
        let Some(value) = params_json else {
            return Err(RpcError::invalid_request(
                "GET JSON-RPC request requires method/id or params",
            ));
        };
        return Ok((value, callback));
    }
    let mut object = Map::new();
    if let Some(method) = method {
        object.insert("method".to_string(), Value::String(method.to_string()));
    }
    if let Some(id) = id {
        object.insert("id".to_string(), Value::String(id.to_string()));
    }
    if let Some(params) = params_json {
        object.insert("params".to_string(), params);
    }
    Ok((Value::Object(object), callback))
}

fn json_or_jsonp_text(value: &Value, callback: Option<&str>) -> String {
    let text = serde_json::to_string(value)
        .unwrap_or_else(|_| "{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32603,\"message\":\"internal serialization error\"}}".to_string());
    match callback.filter(|value| !value.is_empty()) {
        Some(callback) => format!("{callback}({text})"),
        None => text,
    }
}

async fn send_ws_json(socket: &mut WebSocket, value: &Value) -> std::result::Result<(), ()> {
    let text = serde_json::to_string(value).map_err(|_| ())?;
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}

fn parse_jsonrpc_body(body: &[u8]) -> RpcCallResult<Value> {
    serde_json::from_slice::<Value>(body)
        .map_err(|error| RpcError::parse_error(format!("invalid JSON: {error}")))
}

async fn handle_jsonrpc_value(state: &RpcAppState, value: Value, auth: RpcRequestAuth) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            handle_jsonrpc_batch(state, items, auth)
                .await
                .into_iter()
                .collect::<Vec<_>>(),
        ),
        Value::Object(_) => Value::Object(handle_jsonrpc_object(state, value, auth).await),
        _ => Value::Object(response_value(JsonRpcResponse::error(
            None,
            RpcError::invalid_request("request must be an object or array"),
        ))),
    }
}

async fn handle_jsonrpc_batch(
    state: &RpcAppState,
    items: Vec<Value>,
    auth: RpcRequestAuth,
) -> Vec<Value> {
    let mut responses = Vec::with_capacity(items.len());
    for item in items {
        if item.is_object() {
            responses.push(Value::Object(
                handle_jsonrpc_object(state, item, auth).await,
            ));
        }
    }
    responses
}

async fn handle_jsonrpc_object(
    state: &RpcAppState,
    value: Value,
    auth: RpcRequestAuth,
) -> Map<String, Value> {
    let id = value.get("id").cloned();
    let request = match serde_json::from_value::<JsonRpcRequest>(value) {
        Ok(request) => request,
        Err(error) => {
            return response_value(JsonRpcResponse::error(
                id,
                RpcError::invalid_request(format!("invalid request: {error}")),
            ));
        }
    };
    let response = match process_request(state, request.clone(), auth).await {
        Ok(result) => JsonRpcResponse::result(request.id, result),
        Err(error) => JsonRpcResponse::error(request.id, error),
    };
    response_value(response)
}

async fn process_request(
    state: &RpcAppState,
    request: JsonRpcRequest,
    auth: RpcRequestAuth,
) -> RpcCallResult<Value> {
    let params = strip_auth(
        &request.method,
        params_to_vec(request.params)?,
        state.secret.as_deref(),
        state.basic_auth.is_some(),
        auth,
    )?;
    match request.method.as_str() {
        METHOD_ADD => add_haki(&state.manager, &params),
        METHOD_ADD_URI => add_uri(&state.manager, &params),
        METHOD_PAUSE | METHOD_FORCE_PAUSE => pause_download(&state.manager, &params),
        METHOD_PAUSE_ALL | METHOD_FORCE_PAUSE_ALL => Ok(Value::String(state.manager.pause_all()?)),
        METHOD_UNPAUSE => unpause_download(&state.manager, &params),
        METHOD_UNPAUSE_ALL => Ok(Value::String(state.manager.unpause_all()?)),
        METHOD_CHANGE_POSITION => change_position(&state.manager, &params),
        METHOD_CHANGE_URI => change_uri(&state.manager, &params),
        METHOD_CHANGE_OPTION => change_option(&state.manager, &params),
        METHOD_GET_GLOBAL_OPTION => Ok(state.manager.get_global_option()?),
        METHOD_CHANGE_GLOBAL_OPTION => change_global_option(&state.manager, &params),
        METHOD_TELL_STATUS => tell_status(&state.manager, &params),
        METHOD_TELL_ACTIVE => tell_active(&state.manager, &params),
        METHOD_TELL_WAITING => tell_waiting(&state.manager, &params),
        METHOD_TELL_STOPPED => tell_stopped(&state.manager, &params),
        METHOD_GET_URIS => get_uris(&state.manager, &params),
        METHOD_GET_FILES => get_files(&state.manager, &params),
        METHOD_GET_SERVERS => get_servers(&state.manager, &params),
        METHOD_GET_OPTION => get_option(&state.manager, &params),
        METHOD_REMOVE | METHOD_FORCE_REMOVE => remove_download(&state.manager, &params),
        METHOD_REMOVE_RESULT => remove_download_result(&state.manager, &params),
        METHOD_PURGE_RESULT => Ok(Value::String(state.manager.purge_download_results()?)),
        METHOD_GET_VERSION => Ok(version_value()),
        METHOD_GET_SESSION_INFO => Ok(state.manager.session_info()),
        METHOD_GET_GLOBAL_STAT => state.manager.global_stat(),
        METHOD_SHUTDOWN | METHOD_FORCE_SHUTDOWN => {
            let _ = state.shutdown.send(());
            Ok(Value::String("OK".to_string()))
        }
        METHOD_SYSTEM_LIST_METHODS => Ok(Value::Array(
            supported_methods()
                .iter()
                .map(|method| Value::String((*method).to_string()))
                .collect(),
        )),
        METHOD_SYSTEM_LIST_NOTIFICATIONS => Ok(Value::Array(
            supported_notifications()
                .iter()
                .map(|method| Value::String((*method).to_string()))
                .collect(),
        )),
        METHOD_SYSTEM_MULTICALL => system_multicall(state, &params, auth).await,
        method => Err(RpcError::method_not_found(method)),
    }
}

fn response_value(response: JsonRpcResponse) -> Map<String, Value> {
    match serde_json::to_value(response) {
        Ok(Value::Object(object)) => object,
        Ok(_) | Err(_) => {
            let mut object = Map::new();
            object.insert(
                "jsonrpc".to_string(),
                Value::String(JSONRPC_VERSION.to_string()),
            );
            object.insert("id".to_string(), Value::Null);
            object.insert(
                "error".to_string(),
                json!({ "code": -32603, "message": "internal serialization error" }),
            );
            object
        }
    }
}

fn params_to_vec(params: Option<Value>) -> RpcCallResult<Vec<Value>> {
    match params {
        None => Ok(Vec::new()),
        Some(Value::Array(values)) => Ok(values),
        Some(value) => Ok(vec![value]),
    }
}

fn strip_auth(
    method: &str,
    mut params: Vec<Value>,
    secret: Option<&str>,
    basic_auth_configured: bool,
    auth: RpcRequestAuth,
) -> RpcCallResult<Vec<Value>> {
    let token = params
        .first()
        .and_then(Value::as_str)
        .and_then(|value| value.strip_prefix("token:"))
        .map(str::to_string);
    if token.is_some() {
        params.remove(0);
    }
    if is_public_method(method) {
        return Ok(params);
    }
    let token_authorized = secret.is_some_and(|secret| token.as_deref() == Some(secret));
    let auth_required = secret.is_some() || basic_auth_configured;
    if auth_required && !token_authorized && !auth.basic_authorized {
        return Err(RpcError::unauthorized());
    }
    Ok(params)
}

fn is_public_method(method: &str) -> bool {
    matches!(
        method,
        METHOD_SYSTEM_LIST_METHODS | METHOD_SYSTEM_LIST_NOTIFICATIONS | METHOD_SYSTEM_MULTICALL
    )
}

fn add_haki(manager: &RpcSessionManager, params: &[Value]) -> RpcCallResult<Value> {
    let first = params
        .first()
        .ok_or_else(|| RpcError::invalid_params("haki.add requires a request object"))?;
    let object = first
        .as_object()
        .ok_or_else(|| RpcError::invalid_params("haki.add requires a request object"))?;
    let input = object
        .get("input")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("haki.add input is required"))?
        .to_string();
    let options_value = object.get("options").cloned();
    let position = object
        .get("position")
        .map(|value| option_i64("position", value))
        .transpose()?;
    Ok(Value::String(manager.add_download_from_value(
        input,
        options_value,
        position,
    )?))
}

fn add_uri(manager: &RpcSessionManager, params: &[Value]) -> RpcCallResult<Value> {
    let uri_list = params
        .first()
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params("haki.addUri requires a URI array"))?;
    if uri_list.len() != 1 {
        return Err(RpcError::invalid_params(
            "haki.addUri requires exactly one URI",
        ));
    }
    let input = uri_list
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("haki.addUri requires at least one URI"))?
        .to_string();
    let options_value = params.get(1).cloned();
    let position = params
        .get(2)
        .map(|value| option_i64("position", value))
        .transpose()?;
    Ok(Value::String(manager.add_download_from_value(
        input,
        options_value,
        position,
    )?))
}

fn tell_status(manager: &RpcSessionManager, params: &[Value]) -> RpcCallResult<Value> {
    let gid = required_string(params, 0, "tellStatus requires a gid")?;
    let keys = params
        .get(1)
        .map(parse_key_filter)
        .transpose()?
        .unwrap_or_default();
    manager.tell_status(gid, &keys)
}

fn tell_active(manager: &RpcSessionManager, params: &[Value]) -> RpcCallResult<Value> {
    let keys = params
        .first()
        .map(parse_key_filter)
        .transpose()?
        .unwrap_or_default();
    manager.tell_active(&keys)
}

fn tell_waiting(manager: &RpcSessionManager, params: &[Value]) -> RpcCallResult<Value> {
    let offset = params
        .first()
        .map(|value| option_i64("offset", value))
        .transpose()?
        .unwrap_or(0);
    let count = params
        .get(1)
        .map(|value| option_usize("count", value))
        .transpose()?
        .unwrap_or(1000);
    let keys = params
        .get(2)
        .map(parse_key_filter)
        .transpose()?
        .unwrap_or_default();
    manager.tell_waiting(offset, count, &keys)
}

fn tell_stopped(manager: &RpcSessionManager, params: &[Value]) -> RpcCallResult<Value> {
    let offset = params
        .first()
        .map(|value| option_i64("offset", value))
        .transpose()?
        .unwrap_or(0);
    let count = params
        .get(1)
        .map(|value| option_usize("count", value))
        .transpose()?
        .unwrap_or(1000);
    let keys = params
        .get(2)
        .map(parse_key_filter)
        .transpose()?
        .unwrap_or_default();
    manager.tell_stopped(offset, count, &keys)
}

fn pause_download(manager: &RpcSessionManager, params: &[Value]) -> RpcCallResult<Value> {
    let gid = required_string(params, 0, "pause requires a gid")?;
    Ok(Value::String(manager.pause(gid)?))
}

fn unpause_download(manager: &RpcSessionManager, params: &[Value]) -> RpcCallResult<Value> {
    let gid = required_string(params, 0, "unpause requires a gid")?;
    Ok(Value::String(manager.unpause(gid)?))
}

fn change_position(manager: &RpcSessionManager, params: &[Value]) -> RpcCallResult<Value> {
    let gid = required_string(params, 0, "changePosition requires a gid")?;
    let pos = params
        .get(1)
        .ok_or_else(|| RpcError::invalid_params("changePosition requires a position"))
        .and_then(|value| option_i64("position", value))?;
    let how = required_string(params, 2, "changePosition requires a position mode")?;
    Ok(json!(manager.change_position(gid, pos, how)?))
}

fn change_uri(manager: &RpcSessionManager, params: &[Value]) -> RpcCallResult<Value> {
    let gid = required_string(params, 0, "changeUri requires a gid")?;
    let file_index = params
        .get(1)
        .ok_or_else(|| RpcError::invalid_params("changeUri requires a file index"))
        .and_then(|value| option_usize("fileIndex", value))?;
    if file_index == 0 {
        return Err(RpcError::invalid_params("fileIndex is out of range"));
    }
    let del_uris = parse_uri_list_param(params.get(2), "delUris")?;
    let add_uris = parse_uri_list_param(params.get(3), "addUris")?;
    let position = params
        .get(4)
        .map(|value| option_i64("position", value))
        .transpose()?;
    manager.change_uri(gid, file_index, &del_uris, &add_uris, position)
}

fn change_option(manager: &RpcSessionManager, params: &[Value]) -> RpcCallResult<Value> {
    let gid = required_string(params, 0, "changeOption requires a gid")?;
    let options = params
        .get(1)
        .ok_or_else(|| RpcError::invalid_params("changeOption requires an options object"))?;
    Ok(Value::String(manager.change_option(gid, options)?))
}

fn change_global_option(manager: &RpcSessionManager, params: &[Value]) -> RpcCallResult<Value> {
    let options = params
        .first()
        .ok_or_else(|| RpcError::invalid_params("changeGlobalOption requires an options object"))?;
    Ok(Value::String(manager.change_global_option(options)?))
}

fn get_uris(manager: &RpcSessionManager, params: &[Value]) -> RpcCallResult<Value> {
    let gid = required_string(params, 0, "getUris requires a gid")?;
    manager.get_uris(gid)
}

fn get_files(manager: &RpcSessionManager, params: &[Value]) -> RpcCallResult<Value> {
    let gid = required_string(params, 0, "getFiles requires a gid")?;
    manager.get_files(gid)
}

fn get_servers(manager: &RpcSessionManager, params: &[Value]) -> RpcCallResult<Value> {
    let gid = required_string(params, 0, "getServers requires a gid")?;
    manager.get_servers(gid)
}

fn get_option(manager: &RpcSessionManager, params: &[Value]) -> RpcCallResult<Value> {
    let gid = required_string(params, 0, "getOption requires a gid")?;
    manager.get_option(gid)
}

fn remove_download(manager: &RpcSessionManager, params: &[Value]) -> RpcCallResult<Value> {
    let gid = required_string(params, 0, "remove requires a gid")?;
    Ok(Value::String(manager.remove(gid)?))
}

fn remove_download_result(manager: &RpcSessionManager, params: &[Value]) -> RpcCallResult<Value> {
    let gid = required_string(params, 0, "removeDownloadResult requires a gid")?;
    Ok(Value::String(manager.remove_download_result(gid)?))
}

async fn system_multicall(
    state: &RpcAppState,
    params: &[Value],
    _auth: RpcRequestAuth,
) -> RpcCallResult<Value> {
    let calls = params
        .first()
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params("system.multicall requires a call array"))?;
    let mut results = Vec::with_capacity(calls.len());
    for call in calls {
        let Some(call) = call.as_object() else {
            results.push(multicall_fault(RpcError::invalid_params(
                "system.multicall entries must be objects",
            )));
            continue;
        };
        let Some(method) = call
            .get("methodName")
            .or_else(|| call.get("method"))
            .and_then(Value::as_str)
        else {
            results.push(multicall_fault(RpcError::invalid_params(
                "system.multicall entry method is required",
            )));
            continue;
        };
        if method == METHOD_SYSTEM_MULTICALL {
            results.push(multicall_fault(RpcError::invalid_params(
                "Recursive system.multicall forbidden.",
            )));
            continue;
        }
        let params = call.get("params").cloned();
        let request = JsonRpcRequest {
            id: None,
            method: method.to_string(),
            params,
        };
        match Box::pin(process_request(state, request, RpcRequestAuth::default())).await {
            Ok(value) => results.push(Value::Array(vec![value])),
            Err(error) => results.push(multicall_fault(error)),
        }
    }
    Ok(Value::Array(results))
}

fn multicall_fault(error: RpcError) -> Value {
    json!({ "faultCode": error.code, "faultString": error.message })
}

fn parse_key_filter(value: &Value) -> RpcCallResult<Vec<String>> {
    value
        .as_array()
        .ok_or_else(|| RpcError::invalid_params("keys must be an array"))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| RpcError::invalid_params("keys must contain only strings"))
        })
        .collect()
}

fn parse_uri_list_param(value: Option<&Value>, name: &str) -> RpcCallResult<Vec<String>> {
    let values = value
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params(format!("changeUri requires {name}")))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| RpcError::invalid_params(format!("{name} must contain strings")))
        })
        .collect()
}

fn apply_options_object(
    options: &mut DownloadOptions,
    stream_selector: &mut Option<StreamSelector>,
    object: &Map<String, Value>,
) -> RpcCallResult<()> {
    for (key, value) in object {
        match normalize_option_key(key).as_str() {
            "dir" | "savedir" => {
                options.save_dir = Some(PathBuf::from(required_option_string(key, value)?))
            }
            "out" | "savename" => {
                options.save_name = Some(required_option_string(key, value)?.to_string())
            }
            "savepattern" => options.save_pattern = option_string_or_empty(value),
            "logfilepath" => {
                options.log_file_path = Some(PathBuf::from(required_option_string(key, value)?))
            }
            "urlprocessorargs" => {
                options.urlprocessor_args = option_string_or_empty(value);
            }
            "baseurl" => options.base_url = option_string_or_empty(value),
            "header" | "headers" => apply_headers_option(&mut options.headers, value)?,
            "tmpdir" => options.tmp_dir = Some(PathBuf::from(required_option_string(key, value)?)),
            "nolog" => options.no_log = option_bool(key, value)?,
            "loglevel" => options.log_level = parse_log_level(required_option_string(key, value)?)?,
            "compatibilityprofile" => {
                options.compatibility_profile =
                    parse_compatibility_profile(required_option_string(key, value)?)?;
            }
            "uilanguage" => {
                options.ui_language = Some(parse_ui_language(required_option_string(key, value)?)?);
            }
            "autoselect" => options.auto_select = option_bool(key, value)?,
            "subonly" => options.sub_only = option_bool(key, value)?,
            "streamselector" | "selector" => {
                *stream_selector = Some(parse_stream_selector_option(value)?);
            }
            "streamids" | "explicitids" | "selectids" => {
                *stream_selector = Some(StreamSelector::ExplicitIds(parse_string_list_option(
                    key, value,
                )?));
            }
            "skipdownload" => options.skip_download = option_bool(key, value)?,
            "skipmerge" => options.skip_merge = option_bool(key, value)?,
            "checksegmentscount" => options.check_segments_count = option_bool(key, value)?,
            "binarymerge" => options.binary_merge = option_bool(key, value)?,
            "useffmpegconcatdemuxer" => {
                options.use_ffmpeg_concat_demuxer = option_bool(key, value)?
            }
            "delafterdone" => options.del_after_done = option_bool(key, value)?,
            "nodateinfo" => options.no_date_info = option_bool(key, value)?,
            "writemetajson" => options.write_meta_json = option_bool(key, value)?,
            "autosubtitlefix" => options.auto_subtitle_fix = option_bool(key, value)?,
            "subformat" => {
                options.sub_format = parse_subtitle_format(required_option_string(key, value)?)?
            }
            "appendurlparams" => options.append_url_params = option_bool(key, value)?,
            "usesystemproxy" => options.use_system_proxy = option_bool(key, value)?,
            "allowinsecuretls" => options.allow_insecure_tls = option_bool(key, value)?,
            "disableupdatecheck" => options.disable_update_check = option_bool(key, value)?,
            "forceansiconsole" => options.force_ansi_console = option_bool(key, value)?,
            "noansicolor" => options.no_ansi_color = option_bool(key, value)?,
            "customproxy" => options.custom_proxy = option_string_or_empty(value),
            "threadcount" | "split" => options.thread_count = option_i32(key, value)?,
            "downloadretrycount" => options.download_retry_count = option_i32(key, value)?,
            "maxspeed" | "maxdownloadlimit" | "maxoveralldownloadlimit" => {
                options.max_speed = Some(option_speed(key, value)?);
            }
            "httprequesttimeout" | "timeout" => {
                options.http_request_timeout = Duration::from_secs(option_u64(key, value)?);
            }
            "ffmpegbinarypath" => {
                options.ffmpeg_binary_path =
                    Some(PathBuf::from(required_option_string(key, value)?))
            }
            "mkvmergebinarypath" => {
                options.mkvmerge_binary_path =
                    Some(PathBuf::from(required_option_string(key, value)?))
            }
            "decryptionbinarypath" => {
                options.decryption_binary_path =
                    Some(PathBuf::from(required_option_string(key, value)?));
            }
            "decryptionengine" => {
                options.decryption_engine =
                    parse_decryption_engine(required_option_string(key, value)?)?;
            }
            "useshakapackager" => {
                options.use_shaka_packager = option_bool(key, value)?;
                if options.use_shaka_packager {
                    options.decryption_engine = DecryptionEngine::ShakaPackager;
                }
            }
            "mp4realtimedecryption" => options.mp4_real_time_decryption = option_bool(key, value)?,
            "key" | "keys" => {
                options.keys.extend(parse_option_keys(value)?);
            }
            "keytextfile" => {
                options.key_text_file = Some(PathBuf::from(required_option_string(key, value)?))
            }
            "customrange" => options.custom_range = parse_custom_range_option(key, value)?,
            "customhlsmethod" => {
                options.custom_hls_method =
                    Some(parse_hls_method(required_option_string(key, value)?)?);
            }
            "customhlskey" => {
                options.custom_hls_key = parse_hls_bytes_option(key, value)?;
            }
            "customhlsiv" => {
                options.custom_hls_iv = parse_hls_bytes_option(key, value)?;
            }
            "allowhlsmultiextmap" => options.allow_hls_multi_ext_map = option_bool(key, value)?,
            "concurrentdownload" => options.concurrent_download = option_bool(key, value)?,
            "selectvideo" => push_stream_filters(&mut options.select_video, key, value)?,
            "selectaudio" => push_stream_filters(&mut options.select_audio, key, value)?,
            "selectsubtitle" => push_stream_filters(&mut options.select_subtitle, key, value)?,
            "dropvideo" => push_stream_filters(&mut options.drop_video, key, value)?,
            "dropaudio" => push_stream_filters(&mut options.drop_audio, key, value)?,
            "dropsubtitle" => push_stream_filters(&mut options.drop_subtitle, key, value)?,
            "adkeyword" | "adkeywords" => {
                options
                    .ad_keywords
                    .extend(parse_string_list_option(key, value)?);
            }
            "muxafterdone" => {
                options.mux_after_done = Some(parse_mux_after_done_option(
                    required_option_string(key, value)?,
                )?);
                options.binary_merge = true;
            }
            "muximport" | "muximports" => {
                options
                    .mux_imports
                    .extend(parse_mux_imports_option(key, value)?);
            }
            "liverealtimemerge" => options.live_real_time_merge = option_bool(key, value)?,
            "livepipemux" => {
                options.live_pipe_mux = option_bool(key, value)?;
                if options.live_pipe_mux {
                    options.live_real_time_merge = true;
                }
            }
            "livekeepsegments" => options.live_keep_segments = option_bool(key, value)?,
            "liveperformasvod" => options.live_perform_as_vod = option_bool(key, value)?,
            "liverecordlimit" => options.live_record_limit = Some(parse_duration_text(key, value)?),
            "livetakecount" => options.live_take_count = option_i32(key, value)?,
            "livewaittime" => options.live_wait_time = Some(option_i32(key, value)?),
            "livefixvttbyaudio" => options.live_fix_vtt_by_audio = option_bool(key, value)?,
            "taskstartat" => {
                options.task_start_at = Some(TaskStartAt::new(
                    required_option_string(key, value)?.to_string(),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn stream_selector_from_options(options: &DownloadOptions) -> StreamSelector {
    if options.sub_only {
        StreamSelector::SubtitlesOnly
    } else if options.auto_select {
        StreamSelector::Auto
    } else {
        StreamSelector::default()
    }
}

fn apply_headers_option(
    headers: &mut BTreeMap<String, String>,
    value: &Value,
) -> RpcCallResult<()> {
    match value {
        Value::Object(object) => {
            for (name, header_value) in object {
                headers.insert(
                    name.trim().to_ascii_lowercase(),
                    required_option_string(name, header_value)?.to_string(),
                );
            }
            Ok(())
        }
        Value::Array(values) => {
            for value in values {
                apply_headers_option(headers, value)?;
            }
            Ok(())
        }
        Value::String(value) => {
            let Some((name, header_value)) = parse_header_value(value) else {
                return Err(RpcError::invalid_params(
                    "header must use name: value syntax",
                ));
            };
            headers.insert(name, header_value);
            Ok(())
        }
        _ => Err(RpcError::invalid_params(
            "headers must be an object, string, or string array",
        )),
    }
}

fn parse_header_value(value: &str) -> Option<(String, String)> {
    let index = value.find(':')?;
    let name = value.get(..index)?.trim().to_ascii_lowercase();
    let header_value = value.get(index + 1..)?.trim().to_string();
    Some((name, header_value))
}

fn parse_stream_selector_option(value: &Value) -> RpcCallResult<StreamSelector> {
    match value {
        Value::String(value) => match normalize_option_key(value).as_str() {
            "interactive" => Ok(StreamSelector::Interactive),
            "auto" | "autoselect" => Ok(StreamSelector::Auto),
            "subtitlesonly" | "subonly" => Ok(StreamSelector::SubtitlesOnly),
            _ => Ok(StreamSelector::ExplicitIds(vec![value.to_string()])),
        },
        Value::Array(_) => Ok(StreamSelector::ExplicitIds(parse_string_list_option(
            "streamSelector",
            value,
        )?)),
        Value::Object(object) => {
            let Some(ids) = object.get("ids").or_else(|| object.get("explicitIds")) else {
                return Err(RpcError::invalid_params(
                    "streamSelector object requires ids or explicitIds",
                ));
            };
            Ok(StreamSelector::ExplicitIds(parse_string_list_option(
                "streamSelector.ids",
                ids,
            )?))
        }
        _ => Err(RpcError::invalid_params(
            "streamSelector must be a string, string array, or object",
        )),
    }
}

fn parse_string_list_option(key: &str, value: &Value) -> RpcCallResult<Vec<String>> {
    match value {
        Value::String(value) => Ok(vec![value.to_string()]),
        Value::Array(values) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| RpcError::invalid_params(format!("{key} must contain strings")))
            })
            .collect(),
        _ => Err(RpcError::invalid_params(format!(
            "{key} must be a string or string array"
        ))),
    }
}

fn push_stream_filters(
    target: &mut Vec<StreamFilter>,
    key: &str,
    value: &Value,
) -> RpcCallResult<()> {
    match value {
        Value::Array(values) => {
            for value in values {
                push_stream_filters(target, key, value)?;
            }
            Ok(())
        }
        Value::Object(object) => {
            target.push(parse_stream_filter_object(object)?);
            Ok(())
        }
        Value::String(value) => {
            target.push(parse_stream_filter_text(value)?);
            Ok(())
        }
        _ => Err(RpcError::invalid_params(format!(
            "{key} must be a string, object, or array"
        ))),
    }
}

fn parse_stream_filter_text(value: &str) -> RpcCallResult<StreamFilter> {
    let params = ComplexParams::parse(value);
    let for_choice = if is_direct_for_choice(value) {
        value.to_string()
    } else {
        params.get("for")?.unwrap_or_else(|| "best".to_string())
    };
    parse_stream_filter_parts(
        for_choice,
        params.get("id")?,
        params.get("lang")?,
        params.get("name")?,
        params.get("codecs")?,
        params.get("res")?,
        params.get("frame")?,
        params.get("channel")?,
        params.get("range")?,
        params.get("url")?,
        params.get("segsMin")?,
        params.get("segsMax")?,
        params.get("plistDurMin")?,
        params.get("plistDurMax")?,
        params.get("bwMin")?,
        params.get("bwMax")?,
        params.get("role")?,
    )
}

fn parse_stream_filter_object(object: &Map<String, Value>) -> RpcCallResult<StreamFilter> {
    let get = |name: &str| -> RpcCallResult<Option<String>> {
        object
            .get(name)
            .map(|value| required_option_string(name, value).map(str::to_string))
            .transpose()
    };
    parse_stream_filter_parts(
        get("for")?.unwrap_or_else(|| "best".to_string()),
        get("id")?,
        get("lang")?.or(get("language")?),
        get("name")?,
        get("codecs")?,
        get("res")?.or(get("resolution")?),
        get("frame")?.or(get("frameRate")?),
        get("channel")?.or(get("channels")?),
        get("range")?,
        get("url")?,
        get("segsMin")?.or(get("segmentCountMin")?),
        get("segsMax")?.or(get("segmentCountMax")?),
        get("plistDurMin")?.or(get("playlistDurationMin")?),
        get("plistDurMax")?.or(get("playlistDurationMax")?),
        get("bwMin")?.or(get("bandwidthMin")?),
        get("bwMax")?.or(get("bandwidthMax")?),
        get("role")?,
    )
}

#[allow(clippy::too_many_arguments)]
fn parse_stream_filter_parts(
    for_choice: String,
    id: Option<String>,
    language: Option<String>,
    name: Option<String>,
    codecs: Option<String>,
    resolution: Option<String>,
    frame_rate: Option<String>,
    channels: Option<String>,
    range: Option<String>,
    url: Option<String>,
    segment_count_min: Option<String>,
    segment_count_max: Option<String>,
    playlist_duration_min: Option<String>,
    playlist_duration_max: Option<String>,
    bandwidth_min: Option<String>,
    bandwidth_max: Option<String>,
    role: Option<String>,
) -> RpcCallResult<StreamFilter> {
    if !for_choice.is_empty() && !is_direct_for_choice(&for_choice) {
        return Err(RpcError::invalid_params(format!(
            "for={for_choice} is invalid"
        )));
    }
    Ok(StreamFilter {
        for_choice,
        id: validate_filter_regex(id, "id")?,
        language: validate_filter_regex(language, "lang")?,
        name: validate_filter_regex(name, "name")?,
        codecs: validate_filter_regex(codecs, "codecs")?,
        resolution: validate_filter_regex(resolution, "res")?,
        frame_rate: validate_filter_regex(frame_rate, "frame")?,
        channels: validate_filter_regex(channels, "channel")?,
        range: validate_filter_regex(range, "range")?,
        url: validate_filter_regex(url, "url")?,
        segment_count_min: parse_optional_i64(segment_count_min, "segsMin")?,
        segment_count_max: parse_optional_i64(segment_count_max, "segsMax")?,
        playlist_duration_min: parse_optional_filter_duration(
            playlist_duration_min,
            "plistDurMin",
        )?,
        playlist_duration_max: parse_optional_filter_duration(
            playlist_duration_max,
            "plistDurMax",
        )?,
        bandwidth_min: parse_optional_bandwidth(bandwidth_min, "bwMin")?,
        bandwidth_max: parse_optional_bandwidth(bandwidth_max, "bwMax")?,
        role: role.and_then(|value| RoleType::parse_enum_token(&value)),
    })
}

fn validate_filter_regex(value: Option<String>, name: &str) -> RpcCallResult<Option<String>> {
    if let Some(value) = value {
        if value.is_empty() {
            return Ok(None);
        }
        let _ = regex::Regex::new(&value).map_err(|error| {
            RpcError::invalid_params(format!("{name} regex is invalid: {error}"))
        })?;
        Ok(Some(value))
    } else {
        Ok(None)
    }
}

fn is_direct_for_choice(value: &str) -> bool {
    if value == "all" {
        return true;
    }
    ["best", "worst"].iter().any(|prefix| {
        value
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.chars().all(|ch| ch.is_ascii_digit()))
    })
}

fn parse_optional_i64(value: Option<String>, option: &str) -> RpcCallResult<Option<i64>> {
    match value {
        Some(value) if value.is_empty() => Ok(None),
        Some(value) => value
            .parse::<i64>()
            .map(Some)
            .map_err(|_| RpcError::invalid_params(format!("{option} must be an integer"))),
        None => Ok(None),
    }
}

fn parse_optional_bandwidth(value: Option<String>, option: &str) -> RpcCallResult<Option<i64>> {
    match value {
        Some(value) if value.is_empty() => Ok(None),
        Some(value) => value
            .parse::<i64>()
            .map(|value| value.saturating_mul(1000))
            .map(Some)
            .map_err(|_| RpcError::invalid_params(format!("{option} must be an integer"))),
        None => Ok(None),
    }
}

fn parse_optional_filter_duration(
    value: Option<String>,
    option: &str,
) -> RpcCallResult<Option<f64>> {
    match value {
        Some(value) if value.is_empty() => Ok(None),
        Some(value) => parse_filter_duration_seconds(&value, option).map(Some),
        None => Ok(None),
    }
}

fn parse_filter_duration_seconds(value: &str, option: &str) -> RpcCallResult<f64> {
    if value.chars().any(|ch| matches!(ch, 'h' | 'm' | 's')) {
        let mut index = 0_usize;
        let bytes = value.as_bytes();
        let mut total = 0_i64;
        while index < bytes.len() {
            let start = index;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            if start == index || index >= bytes.len() {
                return Err(RpcError::invalid_params(format!(
                    "{option} duration must use h/m/s units"
                )));
            }
            let number = value[start..index]
                .parse::<i64>()
                .map_err(|_| RpcError::invalid_params(format!("{option} duration is invalid")))?;
            let multiplier = match bytes[index] as char {
                'h' => 3600,
                'm' => 60,
                's' => 1,
                _ => {
                    return Err(RpcError::invalid_params(format!(
                        "{option} duration must use h/m/s units"
                    )));
                }
            };
            total = total.saturating_add(number.saturating_mul(multiplier));
            index += 1;
        }
        return Ok(total as f64);
    }
    parse_colon_duration_seconds(value, option)
}

fn parse_custom_range_option(key: &str, value: &Value) -> RpcCallResult<Option<CustomRange>> {
    let value = required_option_string(key, value)?;
    if value.is_empty() {
        return Ok(None);
    }
    let parts = value.split('-').collect::<Vec<_>>();
    if parts.len() != 2 {
        return Err(RpcError::invalid_params(
            "customRange must use start-end syntax",
        ));
    }
    let start = parts[0].trim();
    let end = parts[1].trim();
    if value.contains(':') {
        let start_seconds = if start.is_empty() {
            0.0
        } else {
            parse_colon_duration_seconds(start, key)?
        };
        let end_seconds = if end.is_empty() {
            f64::MAX
        } else {
            parse_colon_duration_seconds(end, key)?
        };
        Ok(Some(CustomRange::Time {
            input: value.to_string(),
            start_seconds,
            end_seconds,
        }))
    } else {
        let start_index = if start.is_empty() {
            0
        } else {
            start
                .parse::<i64>()
                .map_err(|_| RpcError::invalid_params("customRange start is invalid"))?
        };
        let end_index = if end.is_empty() {
            i64::MAX
        } else {
            end.parse::<i64>()
                .map_err(|_| RpcError::invalid_params("customRange end is invalid"))?
        };
        Ok(Some(CustomRange::Segment {
            input: value.to_string(),
            start_index,
            end_index,
        }))
    }
}

fn parse_colon_duration_seconds(value: &str, option: &str) -> RpcCallResult<f64> {
    if value.trim().is_empty() {
        return Err(RpcError::invalid_params(format!(
            "{option} duration must not be empty"
        )));
    }
    let mut total = 0_f64;
    for (index, part) in value
        .replace('\u{ff1a}', ":")
        .split(':')
        .rev()
        .take(4)
        .enumerate()
    {
        let parsed = part
            .trim()
            .parse::<i64>()
            .map_err(|_| RpcError::invalid_params(format!("{option} duration is invalid")))?;
        let multiplier = match index {
            0 => 1_i64,
            1 => 60_i64,
            2 => 3600_i64,
            3 => 86400_i64,
            _ => 1_i64,
        };
        total += parsed.saturating_mul(multiplier) as f64;
    }
    Ok(total)
}

fn parse_subtitle_format(value: &str) -> RpcCallResult<SubtitleFormat> {
    match value.to_ascii_lowercase().as_str() {
        "srt" => Ok(SubtitleFormat::Srt),
        "vtt" => Ok(SubtitleFormat::Vtt),
        _ => Err(RpcError::invalid_params(format!(
            "invalid subtitle format {value}"
        ))),
    }
}

fn parse_hls_method(value: &str) -> RpcCallResult<HlsMethod> {
    match value.to_ascii_uppercase().as_str() {
        "NONE" => Ok(HlsMethod::None),
        "AES_128" | "AES128" => Ok(HlsMethod::Aes128),
        "AES_128_ECB" | "AES128ECB" => Ok(HlsMethod::Aes128Ecb),
        "CENC" => Ok(HlsMethod::Cenc),
        "SAMPLE_AES" | "SAMPLEAES" => Ok(HlsMethod::SampleAes),
        "SAMPLE_AES_CTR" | "SAMPLEAESCTR" => Ok(HlsMethod::SampleAesCtr),
        "CHACHA20" => Ok(HlsMethod::Chacha20),
        "UNKNOWN" => Ok(HlsMethod::Unknown),
        _ => Err(RpcError::invalid_params(format!(
            "invalid HLS method {value}"
        ))),
    }
}

fn parse_hls_bytes_option(key: &str, value: &Value) -> RpcCallResult<Option<Vec<u8>>> {
    match value {
        Value::Null => Ok(None),
        Value::String(value) if value.is_empty() => Ok(None),
        Value::String(value) => parse_hls_bytes_text(key, value).map(Some),
        Value::Array(values) => values
            .iter()
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|value| u8::try_from(value).ok())
                    .ok_or_else(|| {
                        RpcError::invalid_params(format!("{key} byte arrays must contain 0..255"))
                    })
            })
            .collect::<RpcCallResult<Vec<_>>>()
            .map(Some),
        _ => Err(RpcError::invalid_params(format!(
            "{key} must be a string, byte array, or null"
        ))),
    }
}

fn parse_hls_bytes_text(key: &str, value: &str) -> RpcCallResult<Vec<u8>> {
    let hex = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    if !hex.is_empty()
        && hex.chars().all(|ch| ch.is_ascii_hexdigit())
        && hex.len().is_multiple_of(2)
    {
        return hex_to_bytes(hex, key);
    }
    decode_base64(value).map_err(|message| RpcError::invalid_params(format!("{key}: {message}")))
}

fn hex_to_bytes(value: &str, key: &str) -> RpcCallResult<Vec<u8>> {
    value
        .as_bytes()
        .chunks(2)
        .map(|chunk| {
            let text = std::str::from_utf8(chunk)
                .map_err(|_| RpcError::invalid_params(format!("{key} hex is invalid")))?;
            u8::from_str_radix(text, 16)
                .map_err(|_| RpcError::invalid_params(format!("{key} hex is invalid")))
        })
        .collect()
}

fn parse_mux_after_done_option(value: &str) -> RpcCallResult<MuxAfterDoneOptions> {
    let params = ComplexParams::parse(value);
    let format_value = params
        .get("format")?
        .unwrap_or_else(|| first_colon_token(value));
    let muxer_value = params.get("muxer")?.unwrap_or_else(|| "ffmpeg".to_string());
    let format = parse_mux_format(&format_value)?;
    let muxer = parse_muxer(&muxer_value)?;
    if muxer == MuxerKind::Mkvmerge && format_value == "mp4" {
        return Err(RpcError::invalid_params(
            "mkvmerge cannot be used for mp4 mux-after-done",
        ));
    }
    if muxer == MuxerKind::Mp4forge && format != MuxFormat::Mp4 {
        return Err(RpcError::invalid_params(
            "mp4forge mux-after-done is only valid for mp4 output",
        ));
    }
    let fallback_muxer = parse_optional_fallback_muxer(params.get("fallback_muxer")?, muxer)?;
    let bin_path = match params.get("bin_path")? {
        Some(value) if value.is_empty() => {
            return Err(RpcError::invalid_params("bin_path must not be empty"));
        }
        Some(value) if value == "auto" => None,
        Some(value) => Some(PathBuf::from(value)),
        None => None,
    };
    Ok(MuxAfterDoneOptions {
        format,
        muxer,
        fallback_muxer,
        bin_path,
        keep: parse_complex_bool(params.get("keep")?, false, "keep")?,
        skip_sub: parse_complex_bool(params.get("skip_sub")?, false, "skip_sub")?,
    })
}

fn parse_optional_fallback_muxer(
    value: Option<String>,
    primary_muxer: MuxerKind,
) -> RpcCallResult<Option<MuxerKind>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Err(RpcError::invalid_params("fallback_muxer must not be empty"));
    }
    if value == "none" {
        return Ok(None);
    }
    if primary_muxer != MuxerKind::Mp4forge {
        return Err(RpcError::invalid_params(
            "fallback_muxer is only valid with muxer=mp4forge",
        ));
    }
    let muxer = parse_muxer(&value)?;
    if muxer != MuxerKind::Ffmpeg {
        return Err(RpcError::invalid_params("fallback_muxer must be ffmpeg"));
    }
    Ok(Some(muxer))
}

fn parse_mux_format(value: &str) -> RpcCallResult<MuxFormat> {
    match value.to_ascii_lowercase().as_str() {
        "mp4" => Ok(MuxFormat::Mp4),
        "mkv" => Ok(MuxFormat::Mkv),
        "ts" => Ok(MuxFormat::Ts),
        _ => Err(RpcError::invalid_params(format!(
            "invalid mux format {value}"
        ))),
    }
}

fn parse_muxer(value: &str) -> RpcCallResult<MuxerKind> {
    match normalize_option_key(value).as_str() {
        "ffmpeg" => Ok(MuxerKind::Ffmpeg),
        "mkvmerge" => Ok(MuxerKind::Mkvmerge),
        "mp4forge" => Ok(MuxerKind::Mp4forge),
        _ => Err(RpcError::invalid_params(format!("invalid muxer {value}"))),
    }
}

fn parse_complex_bool(value: Option<String>, default: bool, name: &str) -> RpcCallResult<bool> {
    match value {
        Some(value) => match value.as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(RpcError::invalid_params(format!(
                "{name} must be true or false"
            ))),
        },
        None => Ok(default),
    }
}

fn parse_mux_imports_option(key: &str, value: &Value) -> RpcCallResult<Vec<MuxImport>> {
    match value {
        Value::Array(values) => values
            .iter()
            .map(|value| parse_mux_import_option(key, value))
            .collect(),
        _ => Ok(vec![parse_mux_import_option(key, value)?]),
    }
}

fn parse_mux_import_option(key: &str, value: &Value) -> RpcCallResult<MuxImport> {
    match value {
        Value::String(value) => parse_mux_import_text(value),
        Value::Object(object) => {
            let path = object
                .get("path")
                .ok_or_else(|| RpcError::invalid_params(format!("{key} object requires path")))
                .and_then(|value| required_option_string("path", value))?;
            let mut import = MuxImport::new(PathBuf::from(path));
            import.language = optional_object_string(object, "lang")?
                .or(optional_object_string(object, "language")?);
            import.name = optional_object_string(object, "name")?;
            Ok(import)
        }
        _ => Err(RpcError::invalid_params(format!(
            "{key} must be a string, object, or array"
        ))),
    }
}

fn parse_mux_import_text(value: &str) -> RpcCallResult<MuxImport> {
    let params = ComplexParams::parse(value);
    let path = params.get("path")?.unwrap_or_else(|| value.to_string());
    let mut import = MuxImport::new(PathBuf::from(path));
    import.language = params.get("lang")?;
    import.name = params.get("name")?;
    Ok(import)
}

fn optional_object_string(object: &Map<String, Value>, key: &str) -> RpcCallResult<Option<String>> {
    object
        .get(key)
        .map(|value| required_option_string(key, value).map(str::to_string))
        .transpose()
}

fn first_colon_token(value: &str) -> String {
    value
        .find(':')
        .and_then(|index| value.get(..index))
        .unwrap_or(value)
        .to_string()
}

struct ComplexParams {
    source: String,
}

impl ComplexParams {
    fn parse(value: &str) -> Self {
        Self {
            source: value.to_string(),
        }
    }

    fn get(&self, key: &str) -> RpcCallResult<Option<String>> {
        if key.is_empty() || self.source.is_empty() {
            return Ok(None);
        }
        let needle = format!("{key}=");
        let Some(index) = self.source.find(&needle) else {
            if self.source.contains(key) && self.source.ends_with(key) {
                return Ok(Some("true".to_string()));
            }
            return Ok(None);
        };
        let start = index + needle.len();
        let Some(rest) = self.source.get(start..) else {
            return Err(RpcError::invalid_params(format!(
                "complex option {key} is invalid"
            )));
        };
        let mut result = String::new();
        let mut last = '\0';
        for ch in rest.chars() {
            if ch == ':' {
                if last == '\\' {
                    result = result.replace('\\', "");
                    last = ch;
                    result.push(ch);
                } else {
                    break;
                }
            } else {
                last = ch;
                result.push(ch);
            }
        }

        let cleaned = result
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
        if cleaned.contains('"') || cleaned.contains('\'') {
            return Err(RpcError::invalid_params(format!(
                "complex option {key} is invalid"
            )));
        }
        Ok(Some(cleaned))
    }
}

fn sanitize_options_snapshot(mut object: Map<String, Value>) -> Map<String, Value> {
    for (key, value) in &mut object {
        match normalize_option_key(key).as_str() {
            "key" | "keys" | "customhlskey" | "customhlsiv" => {
                *value = Value::String("<redacted>".to_string());
            }
            "customproxy" => {
                if let Some(proxy) = value.as_str() {
                    *value = Value::String(redact_proxy_credentials(proxy));
                }
            }
            _ => {}
        }
    }
    object
}

fn redact_proxy_credentials(proxy: &str) -> String {
    let Some(scheme_end) = proxy.find("://") else {
        return proxy.to_string();
    };
    let authority_start = scheme_end + 3;
    let Some(at_relative) = proxy[authority_start..].find('@') else {
        return proxy.to_string();
    };
    let host_start = authority_start + at_relative + 1;
    format!(
        "{}<redacted>@{}",
        &proxy[..authority_start],
        &proxy[host_start..]
    )
}

fn required_string<'a>(params: &'a [Value], index: usize, message: &str) -> RpcCallResult<&'a str> {
    params
        .get(index)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params(message))
}

fn required_option_string<'a>(key: &str, value: &'a Value) -> RpcCallResult<&'a str> {
    value
        .as_str()
        .ok_or_else(|| RpcError::invalid_params(format!("{key} must be a string")))
}

fn option_string_or_empty(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn option_bool(key: &str, value: &Value) -> RpcCallResult<bool> {
    match value {
        Value::Bool(value) => Ok(*value),
        Value::String(value) if value.eq_ignore_ascii_case("true") => Ok(true),
        Value::String(value) if value.eq_ignore_ascii_case("false") => Ok(false),
        _ => Err(RpcError::invalid_params(format!("{key} must be a boolean"))),
    }
}

fn option_i32(key: &str, value: &Value) -> RpcCallResult<i32> {
    match value {
        Value::Number(value) => value
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| RpcError::invalid_params(format!("{key} must fit in i32"))),
        Value::String(value) => value
            .parse::<i32>()
            .map_err(|_| RpcError::invalid_params(format!("{key} must be an integer"))),
        _ => Err(RpcError::invalid_params(format!(
            "{key} must be an integer"
        ))),
    }
}

fn option_i64(key: &str, value: &Value) -> RpcCallResult<i64> {
    match value {
        Value::Number(value) => value
            .as_i64()
            .ok_or_else(|| RpcError::invalid_params(format!("{key} must fit in i64"))),
        Value::String(value) => value
            .parse::<i64>()
            .map_err(|_| RpcError::invalid_params(format!("{key} must be an integer"))),
        _ => Err(RpcError::invalid_params(format!(
            "{key} must be an integer"
        ))),
    }
}

fn option_u64(key: &str, value: &Value) -> RpcCallResult<u64> {
    match value {
        Value::Number(value) => value
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params(format!("{key} must be an unsigned integer"))),
        Value::String(value) => value
            .parse::<u64>()
            .map_err(|_| RpcError::invalid_params(format!("{key} must be an unsigned integer"))),
        _ => Err(RpcError::invalid_params(format!(
            "{key} must be an unsigned integer"
        ))),
    }
}

fn option_usize(key: &str, value: &Value) -> RpcCallResult<usize> {
    let parsed = option_u64(key, value)?;
    usize::try_from(parsed).map_err(|_| RpcError::invalid_params(format!("{key} is out of range")))
}

fn option_speed(key: &str, value: &Value) -> RpcCallResult<u64> {
    match value {
        Value::Number(_) => option_u64(key, value),
        Value::String(text) => parse_speed_text(text),
        _ => Err(RpcError::invalid_params(format!(
            "{key} must be a speed value"
        ))),
    }
}

fn parse_speed_text(value: &str) -> RpcCallResult<u64> {
    let trimmed = value.trim();
    if trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        return trimmed
            .parse::<u64>()
            .map_err(|_| RpcError::invalid_params("speed value is out of range"));
    }
    let unit = trimmed
        .chars()
        .last()
        .ok_or_else(|| RpcError::invalid_params("speed value must not be empty"))?;
    let number = trimmed
        .get(..trimmed.len().saturating_sub(unit.len_utf8()))
        .ok_or_else(|| RpcError::invalid_params("speed value is invalid"))?;
    let multiplier = match unit.to_ascii_uppercase() {
        'K' => 1024.0,
        'M' => 1024.0 * 1024.0,
        'G' => 1024.0 * 1024.0 * 1024.0,
        _ => return Err(RpcError::invalid_params("speed unit must be K, M, or G")),
    };
    let parsed = number
        .parse::<f64>()
        .map_err(|_| RpcError::invalid_params("speed value must be numeric"))?;
    if parsed < 0.0 {
        return Err(RpcError::invalid_params("speed value must not be negative"));
    }
    Ok((parsed * multiplier) as u64)
}

fn parse_duration_text(key: &str, value: &Value) -> RpcCallResult<Duration> {
    if let Some(seconds) = value.as_u64() {
        return Ok(Duration::from_secs(seconds));
    }
    let text = required_option_string(key, value)?;
    let parts = text.split(':').collect::<Vec<_>>();
    match parts.as_slice() {
        [seconds] => seconds
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|_| RpcError::invalid_params(format!("{key} duration is invalid"))),
        [minutes, seconds] => {
            let minutes = minutes
                .parse::<u64>()
                .map_err(|_| RpcError::invalid_params(format!("{key} duration is invalid")))?;
            let seconds = seconds
                .parse::<u64>()
                .map_err(|_| RpcError::invalid_params(format!("{key} duration is invalid")))?;
            Ok(Duration::from_secs(
                minutes.saturating_mul(60).saturating_add(seconds),
            ))
        }
        [hours, minutes, seconds] => {
            let hours = hours
                .parse::<u64>()
                .map_err(|_| RpcError::invalid_params(format!("{key} duration is invalid")))?;
            let minutes = minutes
                .parse::<u64>()
                .map_err(|_| RpcError::invalid_params(format!("{key} duration is invalid")))?;
            let seconds = seconds
                .parse::<u64>()
                .map_err(|_| RpcError::invalid_params(format!("{key} duration is invalid")))?;
            Ok(Duration::from_secs(
                hours
                    .saturating_mul(3600)
                    .saturating_add(minutes.saturating_mul(60))
                    .saturating_add(seconds),
            ))
        }
        _ => Err(RpcError::invalid_params(format!(
            "{key} duration is invalid"
        ))),
    }
}

fn parse_log_level(value: &str) -> RpcCallResult<LogLevel> {
    match value.to_ascii_lowercase().as_str() {
        "debug" => Ok(LogLevel::Debug),
        "info" => Ok(LogLevel::Info),
        "warn" => Ok(LogLevel::Warn),
        "error" => Ok(LogLevel::Error),
        "off" => Ok(LogLevel::Off),
        _ => Err(RpcError::invalid_params(format!(
            "invalid log level {value}"
        ))),
    }
}

fn parse_compatibility_profile(value: &str) -> RpcCallResult<CompatibilityProfile> {
    match normalize_option_key(value).as_str() {
        "clicompatible" | "cli" => Ok(CompatibilityProfile::CliCompatible),
        "apisafe" | "api" => Ok(CompatibilityProfile::ApiSafe),
        _ => Err(RpcError::invalid_params(format!(
            "invalid compatibility profile {value}"
        ))),
    }
}

fn parse_ui_language(value: &str) -> RpcCallResult<UiLanguage> {
    match value {
        "auto" | "en-US" => Ok(UiLanguage::EnUs),
        _ => Err(RpcError::invalid_params(format!(
            "invalid UI language {value}"
        ))),
    }
}

fn parse_decryption_engine(value: &str) -> RpcCallResult<DecryptionEngine> {
    match normalize_option_key(value).as_str() {
        "mp4forge" => Ok(DecryptionEngine::Mp4forge),
        "mp4decrypt" => Ok(DecryptionEngine::Mp4decrypt),
        "shakapackager" => Ok(DecryptionEngine::ShakaPackager),
        "ffmpeg" => Ok(DecryptionEngine::Ffmpeg),
        _ => Err(RpcError::invalid_params(format!(
            "invalid decryption engine {value}"
        ))),
    }
}

fn parse_option_keys(value: &Value) -> RpcCallResult<Vec<CustomKey>> {
    match value {
        Value::String(value) => Ok(vec![parse_custom_key(value)?]),
        Value::Array(values) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| RpcError::invalid_params("keys must contain only strings"))
                    .and_then(parse_custom_key)
            })
            .collect(),
        _ => Err(RpcError::invalid_params(
            "keys must be a string or string array",
        )),
    }
}

fn parse_custom_key(value: &str) -> RpcCallResult<CustomKey> {
    let pieces = value.split(':').collect::<Vec<_>>();
    match pieces.as_slice() {
        [key] => {
            validate_hex(key, 32, "key")?;
            Ok(CustomKey::Key {
                key_hex: key.to_ascii_lowercase(),
            })
        }
        [first, key] => {
            validate_hex(key, 32, "key")?;
            if first.chars().all(|ch| ch.is_ascii_digit()) {
                let track_id = first
                    .parse::<u32>()
                    .map_err(|_| RpcError::invalid_params("track id is invalid"))?;
                Ok(CustomKey::Track {
                    track_id,
                    key_hex: key.to_ascii_lowercase(),
                })
            } else {
                validate_hex(first, 32, "kid")?;
                Ok(CustomKey::Kid {
                    kid_hex: first.to_ascii_lowercase(),
                    key_hex: key.to_ascii_lowercase(),
                })
            }
        }
        _ => Err(RpcError::invalid_params(
            "key must be KEY, KID:KEY, or TRACK:KEY",
        )),
    }
}

fn validate_hex(value: &str, len: usize, name: &str) -> RpcCallResult<()> {
    if value.len() == len && value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(RpcError::invalid_params(format!(
            "{name} must be {len} hex characters"
        )))
    }
}

fn normalize_option_key(value: &str) -> String {
    value
        .chars()
        .filter(|ch| *ch != '-' && *ch != '_' && *ch != ' ')
        .flat_map(char::to_lowercase)
        .collect()
}

fn version_value() -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "enabledFeatures": ["rpc"],
    })
}

fn supported_methods() -> &'static [&'static str] {
    &[
        METHOD_ADD,
        METHOD_ADD_URI,
        METHOD_PAUSE,
        METHOD_FORCE_PAUSE,
        METHOD_PAUSE_ALL,
        METHOD_FORCE_PAUSE_ALL,
        METHOD_UNPAUSE,
        METHOD_UNPAUSE_ALL,
        METHOD_CHANGE_POSITION,
        METHOD_CHANGE_URI,
        METHOD_CHANGE_OPTION,
        METHOD_GET_GLOBAL_OPTION,
        METHOD_CHANGE_GLOBAL_OPTION,
        METHOD_TELL_STATUS,
        METHOD_TELL_ACTIVE,
        METHOD_TELL_WAITING,
        METHOD_TELL_STOPPED,
        METHOD_GET_URIS,
        METHOD_GET_FILES,
        METHOD_GET_SERVERS,
        METHOD_GET_OPTION,
        METHOD_REMOVE,
        METHOD_FORCE_REMOVE,
        METHOD_REMOVE_RESULT,
        METHOD_PURGE_RESULT,
        METHOD_GET_VERSION,
        METHOD_GET_SESSION_INFO,
        METHOD_GET_GLOBAL_STAT,
        METHOD_SHUTDOWN,
        METHOD_FORCE_SHUTDOWN,
        METHOD_SYSTEM_MULTICALL,
        METHOD_SYSTEM_LIST_METHODS,
        METHOD_SYSTEM_LIST_NOTIFICATIONS,
    ]
}

fn supported_notifications() -> &'static [&'static str] {
    &[
        NOTIFY_START,
        NOTIFY_PAUSE,
        NOTIFY_STOP,
        NOTIFY_COMPLETE,
        NOTIFY_ERROR,
        NOTIFY_PROGRESS,
    ]
}

fn find_state<'a>(
    states: &'a BTreeMap<String, RpcDownloadState>,
    gid: &str,
) -> RpcCallResult<&'a RpcDownloadState> {
    if let Some(state) = states.get(gid) {
        return Ok(state);
    }
    let mut matches = states.values().filter(|state| state.gid.starts_with(gid));
    let first = matches.next();
    if first.is_some() && matches.next().is_none() {
        return first.ok_or_else(|| RpcError::invalid_params("gid not found"));
    }
    Err(RpcError::invalid_params("gid not found"))
}

fn find_state_mut<'a>(
    states: &'a mut BTreeMap<String, RpcDownloadState>,
    gid: &str,
) -> RpcCallResult<&'a mut RpcDownloadState> {
    if states.contains_key(gid) {
        return states
            .get_mut(gid)
            .ok_or_else(|| RpcError::invalid_params("gid not found"));
    }
    let mut matches = states
        .keys()
        .filter(|candidate| candidate.starts_with(gid))
        .cloned()
        .collect::<Vec<_>>();
    if matches.len() == 1 {
        let key = matches.remove(0);
        return states
            .get_mut(&key)
            .ok_or_else(|| RpcError::invalid_params("gid not found"));
    }
    Err(RpcError::invalid_params("gid not found"))
}

fn prune_retained_states(states: &mut BTreeMap<String, RpcDownloadState>, max_len: usize) {
    while states.len() > max_len {
        let Some(oldest_stopped) = states
            .values()
            .filter(|state| state.status.is_terminal())
            .min_by_key(|state| state.updated_at_ms)
            .map(|state| state.gid.clone())
        else {
            break;
        };
        states.remove(&oldest_stopped);
    }
}

fn ordered_states(mut states: Vec<RpcDownloadState>) -> Vec<RpcDownloadState> {
    states.sort_by_key(|state| (state.queue_position, state.created_at_ms));
    states
}

fn move_queued_state_to_front(states: &mut BTreeMap<String, RpcDownloadState>, gid: &str) {
    for (candidate_gid, state) in states.iter_mut() {
        if candidate_gid != gid && state.status.is_queued() {
            state.queue_position = state.queue_position.saturating_add(1);
        }
    }
    if let Some(state) = states.get_mut(gid) {
        state.queue_position = 0;
    }
}

fn paginate_states(
    states: Vec<RpcDownloadState>,
    offset: i64,
    count: usize,
) -> Vec<RpcDownloadState> {
    if count == 0 || states.is_empty() {
        return Vec::new();
    }
    if offset >= 0 {
        return states
            .into_iter()
            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
            .take(count)
            .collect();
    }
    let len = i64::try_from(states.len()).unwrap_or(i64::MAX);
    let start = len.saturating_add(offset).clamp(0, len.saturating_sub(1));
    states
        .into_iter()
        .take(usize::try_from(start.saturating_add(1)).unwrap_or(usize::MAX))
        .rev()
        .take(count)
        .collect()
}

fn should_push_progress(event: &ProgressEvent) -> bool {
    matches!(
        event,
        ProgressEvent::AggregateProgress(_)
            | ProgressEvent::StreamProgress(_)
            | ProgressEvent::LiveRefresh { .. }
            | ProgressEvent::OutputArtifact(_)
    )
}

fn default_bind_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 6800))
}

fn seed_gid_counter() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(1);
    let bytes = nanos.to_le_bytes();
    let mut value = 0_u64;
    for (index, byte) in bytes.iter().take(8).enumerate() {
        value |= u64::from(*byte) << (index * 8);
    }
    if value == 0 { 1 } else { value }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}
