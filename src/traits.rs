//! Async-ready component traits.

use std::future::Future;
use std::path::Path;

use crate::config::DownloadOptions;
use crate::error::Result;
use crate::event::ProgressEvent;
use crate::http::{HttpRequest, HttpResponse};
use crate::manifest::{Manifest, Stream};
use crate::mux::OutputArtifact;

/// HTTP transport abstraction.
pub trait HttpClient {
    /// Sends one HTTP request.
    fn send(&self, request: HttpRequest) -> impl Future<Output = Result<HttpResponse>> + Send + '_;
}

/// Storage abstraction.
pub trait Storage {
    /// Writes bytes to a path.
    fn write_all<'a>(
        &'a self,
        path: &'a Path,
        bytes: &'a [u8],
    ) -> impl Future<Output = Result<()>> + Send + 'a;
}

/// Manifest loading abstraction.
pub trait ManifestLoader {
    /// Loads and parses a manifest.
    fn load(&self, input: &str) -> impl Future<Output = Result<Manifest>> + Send + '_;
}

/// Key provider abstraction.
pub trait KeyProvider {
    /// Resolves key material by identifier.
    fn key_for(&self, key_id: &str) -> impl Future<Output = Result<Option<Vec<u8>>>> + Send + '_;
}

/// Decryptor abstraction.
pub trait Decryptor {
    /// Decrypts one input into one output path.
    fn decrypt<'a>(
        &'a self,
        input: &'a Path,
        output: &'a Path,
    ) -> impl Future<Output = Result<()>> + Send + 'a;
}

/// Muxer abstraction.
pub trait Muxer {
    /// Muxes selected stream artifacts into final artifacts.
    fn mux<'a>(
        &'a self,
        streams: &'a [Stream],
        options: &'a DownloadOptions,
    ) -> impl Future<Output = Result<Vec<OutputArtifact>>> + Send + 'a;
}

/// Progress sink abstraction.
pub trait ProgressSink {
    /// Emits one event.
    fn emit(&self, event: ProgressEvent) -> Result<()>;
}

/// Cancellation abstraction.
pub trait Cancellation {
    /// Returns whether cancellation has been requested.
    fn is_cancelled(&self) -> bool;
}
