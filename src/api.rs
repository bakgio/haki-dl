//! High-level async API entry points.

use std::fmt;
use std::sync::Arc;

use crate::cancellation::CancellationToken;
use crate::config::DownloadOptions;
use crate::error::{Error, Result};
use crate::event::ProgressEvent;
use crate::manifest::StreamSelector;
use crate::session::DownloadSession;

/// Main async library client.
#[derive(Clone, Debug, Default)]
pub struct DownloadClient;

impl DownloadClient {
    /// Creates a new client using default components.
    pub fn new() -> Self {
        Self
    }

    /// Validates and plans a new session without starting network or download work.
    pub fn prepare(&self, request: DownloadRequest) -> Result<DownloadSession> {
        if request.input.trim().is_empty() {
            return Err(Error::config("input must not be empty"));
        }
        Ok(DownloadSession::new(request))
    }
}

/// Cloneable callback for structured live progress events.
///
/// The callback receives typed [`ProgressEvent`] values from API sessions. It is
/// separate from CLI console rendering and does not require parsing terminal
/// progress text.
type ProgressCallbackFn = dyn Fn(&ProgressEvent) -> Result<()> + Send + Sync + 'static;

#[derive(Clone)]
pub struct ProgressCallback {
    callback: Arc<ProgressCallbackFn>,
}

impl ProgressCallback {
    /// Creates a callback from a closure.
    pub fn new<F>(callback: F) -> Self
    where
        F: Fn(&ProgressEvent) -> Result<()> + Send + Sync + 'static,
    {
        Self {
            callback: Arc::new(callback),
        }
    }

    /// Emits one event to the callback.
    pub fn emit(&self, event: &ProgressEvent) -> Result<()> {
        (self.callback)(event)
    }
}

impl fmt::Debug for ProgressCallback {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProgressCallback")
    }
}

/// API request for a download or recording session.
#[derive(Clone, Debug)]
pub struct DownloadRequest {
    /// Manifest URL, local path, or direct media URL.
    pub input: String,
    /// Typed options.
    pub options: DownloadOptions,
    /// Stream selection strategy.
    pub stream_selector: StreamSelector,
    /// Cancellation token observed by the session.
    pub cancellation_token: CancellationToken,
    /// Optional live progress callback invoked as events are emitted.
    pub progress_callback: Option<ProgressCallback>,
}

impl DownloadRequest {
    /// Creates a request with default options.
    pub fn new(input: impl Into<String>) -> Self {
        Self {
            input: input.into(),
            options: DownloadOptions::default(),
            stream_selector: StreamSelector::default(),
            cancellation_token: CancellationToken::new(),
            progress_callback: None,
        }
    }

    /// Replaces the request options.
    pub fn with_options(mut self, options: DownloadOptions) -> Self {
        self.options = options;
        self
    }

    /// Replaces the stream selector.
    pub fn with_stream_selector(mut self, stream_selector: StreamSelector) -> Self {
        self.stream_selector = stream_selector;
        self
    }

    /// Replaces the cancellation token.
    pub fn with_cancellation_token(mut self, cancellation_token: CancellationToken) -> Self {
        self.cancellation_token = cancellation_token;
        self
    }

    /// Replaces the structured live progress callback.
    pub fn with_progress_callback(mut self, progress_callback: ProgressCallback) -> Self {
        self.progress_callback = Some(progress_callback);
        self
    }
}

/// Live-recording API entry point.
#[derive(Clone, Debug)]
pub struct LiveRecorder {
    request: DownloadRequest,
}

impl LiveRecorder {
    /// Creates a live recorder from a request.
    pub fn new(request: DownloadRequest) -> Self {
        Self { request }
    }

    /// Returns the underlying request.
    pub fn request(&self) -> &DownloadRequest {
        &self.request
    }

    /// Starts the live recording session through the same async API pipeline as normal downloads.
    pub async fn start(self) -> Result<Vec<ProgressEvent>> {
        Box::pin(DownloadClient::new().prepare(self.request)?.start()).await
    }
}
