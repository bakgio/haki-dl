//! Segment downloading, scheduling, progress, and cleanup helpers.

use std::collections::BTreeMap;
use std::fs::File;
use std::future::Future;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flate2::read::GzDecoder;
use reqwest::Client as ReqwestClient;
use reqwest::header::HeaderMap;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::api::ProgressCallback;
use crate::config::{DownloadOptions, LogLevel};
use crate::decrypt::{PsshSystem, read_mp4_protection_info, redact_secrets};
use crate::error::{Error, Result};
use crate::event::ProgressEvent;
use crate::http::{apply_request_headers, shared_http_client};
use crate::manifest::{EncryptionMethod, MediaSegment, MediaType, Stream};
use crate::media_info::{media_info_console_label, probe_ffmpeg_media_infos};
use crate::mss::MssInitGenerator;
use crate::mux::MediaInfo;
use crate::progress::{AggregateProgress, SegmentProgress, StreamProgress};
use crate::selection::{format_save_pattern, handle_file_collision, valid_file_name};
use crate::stream_label::{stream_download_label, stream_short_label};

const BUFFER_SIZE: usize = 16 * 1024;
const LARGE_SPLIT_SIZE: i64 = 10 * 1024 * 1024;
const SEGMENT_READ_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Default)]
pub(crate) struct DownloadEventEmitter {
    callback: Option<ProgressCallback>,
    shared_events: Option<Arc<Mutex<Vec<ProgressEvent>>>>,
}

impl DownloadEventEmitter {
    pub(crate) fn new(callback: Option<&ProgressCallback>) -> Self {
        Self {
            callback: callback.cloned(),
            shared_events: None,
        }
    }

    fn with_shared_events(&self, shared_events: Arc<Mutex<Vec<ProgressEvent>>>) -> Self {
        Self {
            callback: self.callback.clone(),
            shared_events: Some(shared_events),
        }
    }

    fn uses_shared_events(&self) -> bool {
        self.shared_events.is_some()
    }

    fn push(&self, events: &mut Vec<ProgressEvent>, event: ProgressEvent) -> Result<()> {
        if let Some(callback) = &self.callback {
            callback.emit(&event)?;
        }
        if let Some(shared_events) = &self.shared_events {
            let mut guard = shared_events
                .lock()
                .map_err(|_| Error::config("download event lock was poisoned"))?;
            guard.push(event);
            return Ok(());
        }
        events.push(event);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SegmentCompletedContext {
    pub(crate) stream: Stream,
    pub(crate) segment: MediaSegment,
    pub(crate) actual_file_path: PathBuf,
    pub(crate) init_file_path: Option<PathBuf>,
    pub(crate) stream_id: String,
    pub(crate) is_init: bool,
}

type SegmentCompletedFuture = Pin<Box<dyn Future<Output = Result<Vec<ProgressEvent>>> + Send>>;
type StreamCompletedFuture = Pin<Box<dyn Future<Output = Result<Vec<ProgressEvent>>> + Send>>;

pub(crate) type SegmentCompletedHook =
    Arc<dyn Fn(SegmentCompletedContext) -> SegmentCompletedFuture + Send + Sync>;
pub(crate) type StreamCompletedHook = Arc<dyn Fn(String) -> StreamCompletedFuture + Send + Sync>;

#[derive(Clone, Default)]
pub(crate) struct DownloadHooks {
    pub(crate) segment_completed: Option<SegmentCompletedHook>,
    pub(crate) stream_completed: Option<StreamCompletedHook>,
}

/// One downloaded segment file result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentDownloadResult {
    /// Final actual path. Existing `_dec` paths are preserved for later decrypt work.
    pub actual_file_path: PathBuf,
    /// Response content length when known.
    pub response_content_length: Option<u64>,
    /// Actual file length.
    pub actual_content_length: Option<u64>,
    /// Whether the first bytes matched an image disguise header.
    pub image_header: bool,
    /// Whether the first bytes matched a gzip header.
    pub gzip_header: bool,
    /// Whether the file was skipped because a compatible final file already existed.
    pub skipped_existing: bool,
}

impl SegmentDownloadResult {
    /// Returns true when the result passes content-length validation.
    pub fn success(&self) -> bool {
        match (self.actual_content_length, self.response_content_length) {
            (Some(actual), Some(response)) => actual == response,
            (Some(_), None) => true,
            _ => false,
        }
    }
}

/// Output of a stream download.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StreamDownloadResult {
    /// Stream identifier used for events.
    pub stream_id: String,
    /// Temporary directory used for the stream.
    pub temp_dir: PathBuf,
    /// Downloaded files in merge order.
    pub files: Vec<PathBuf>,
    /// Planned final output path for later merge/mux work.
    pub output_path: PathBuf,
    /// Whether binary merge was forced by stream metadata.
    pub binary_merge_required: bool,
    /// Whether a large single-file split disabled MP4 real-time decryption for this stream.
    pub disable_real_time_decryption: bool,
    /// Whether ffmpeg merge should use the AAC ADTS-to-ASC bitstream filter.
    pub use_aac_filter: bool,
    /// Probed media information used by later mux metadata planning.
    pub media_infos: Vec<MediaInfo>,
}

/// Merge policy detected before later merge/mux work.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DownloadMergePolicy {
    /// Binary merge is required or automatically selected.
    pub binary_merge_required: bool,
    /// Real-time MP4 decryption should be disabled.
    pub disable_real_time_decryption: bool,
    /// Final mux-after-done should be disabled by detected media information.
    pub disable_mux_after_done: bool,
    /// Unknown encryption was observed and must not use normal decrypt paths.
    pub unknown_encryption: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ThreadMode {
    Sequential,
    Concurrent(usize),
}

/// Compatibility speed and low-speed state.
#[derive(Debug)]
pub struct SpeedState {
    speed_limit: u64,
    downloaded_window: u64,
    total_downloaded: u64,
    bytes_per_second: u64,
    low_speed_count: u32,
    last_reset: Instant,
}

impl Default for SpeedState {
    fn default() -> Self {
        Self {
            speed_limit: u64::MAX,
            downloaded_window: 0,
            total_downloaded: 0,
            bytes_per_second: 0,
            low_speed_count: 0,
            last_reset: Instant::now(),
        }
    }
}

impl SpeedState {
    /// Creates a speed state with an optional bytes-per-second limit.
    pub fn new(speed_limit: Option<u64>) -> Self {
        Self {
            speed_limit: speed_limit.unwrap_or(u64::MAX),
            ..Self::default()
        }
    }

    /// Adds downloaded bytes and returns aggregate downloaded bytes.
    pub fn add(&mut self, size: u64) -> u64 {
        self.downloaded_window = self.downloaded_window.saturating_add(size);
        self.total_downloaded = self.total_downloaded.saturating_add(size);
        self.total_downloaded
    }

    /// Records one low-speed tick.
    pub fn add_low_speed_count(&mut self) -> u32 {
        self.low_speed_count = self.low_speed_count.saturating_add(1);
        self.low_speed_count
    }

    /// Resets low-speed detection.
    pub fn reset_low_speed_count(&mut self) {
        self.low_speed_count = 0;
    }

    /// Returns true once low-speed detection reaches the compatibility stop threshold.
    pub fn should_stop(&self) -> bool {
        self.low_speed_count >= 20
    }

    /// Total downloaded bytes.
    pub fn total_downloaded(&self) -> u64 {
        self.total_downloaded
    }

    /// Current compatibility speed estimate in bytes per second.
    pub fn bytes_per_second(&self) -> u64 {
        self.bytes_per_second
    }

    /// Speed value displayed in progress events.
    pub fn display_bytes_per_second(&self) -> u64 {
        if self.bytes_per_second == 0 {
            self.downloaded_window
        } else {
            self.bytes_per_second
        }
    }

    /// Consecutive low-speed ticks observed by compatibility speed detection.
    pub fn low_speed_count(&self) -> u32 {
        self.low_speed_count
    }

    /// Applies the one-second progress-column speed and low-speed tick.
    pub fn record_progress_tick(&mut self) {
        if self.last_reset.elapsed() >= Duration::from_secs(1) {
            self.finish_progress_tick();
        }
    }

    fn finish_progress_tick(&mut self) {
        self.bytes_per_second = self.downloaded_window;
        if self.downloaded_window == 0 {
            self.add_low_speed_count();
        } else {
            self.reset_low_speed_count();
        }
        self.downloaded_window = 0;
        self.last_reset = Instant::now();
    }
}

/// Segment downloader with compatibility retry and repair behavior.
#[derive(Clone, Debug, Default)]
pub struct SegmentDownloader;

impl SegmentDownloader {
    /// Creates a segment downloader.
    pub fn new() -> Self {
        Self
    }

    /// Downloads one segment to a temporary path.
    #[allow(clippy::too_many_arguments)]
    pub async fn download_segment(
        &self,
        segment: &MediaSegment,
        save_path: &Path,
        speed: &Arc<Mutex<SpeedState>>,
        headers: &BTreeMap<String, String>,
        options: &DownloadOptions,
        events: &mut Vec<ProgressEvent>,
        stream_id: &str,
    ) -> Result<SegmentDownloadResult> {
        let client = http_client(options)?;
        let emitter = DownloadEventEmitter::default();
        self.download_segment_with_http_client(
            segment, save_path, speed, headers, options, events, stream_id, &client, &emitter,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn download_segment_with_http_client(
        &self,
        segment: &MediaSegment,
        save_path: &Path,
        speed: &Arc<Mutex<SpeedState>>,
        headers: &BTreeMap<String, String>,
        options: &DownloadOptions,
        events: &mut Vec<ProgressEvent>,
        stream_id: &str,
        http_client: &ReqwestClient,
        emitter: &DownloadEventEmitter,
    ) -> Result<SegmentDownloadResult> {
        let segment_index = u64_from_i64(segment.index);
        emitter.push(
            events,
            ProgressEvent::SegmentStarted {
                stream_id: stream_id.to_string(),
                segment_index,
            },
        )?;
        let final_path = remove_tmp_extension(save_path);
        if let Ok(metadata) = tokio::fs::metadata(&final_path).await
            && metadata.is_file()
        {
            let size = metadata.len();
            add_speed(speed, size)?;
            let result = SegmentDownloadResult {
                actual_file_path: final_path,
                response_content_length: None,
                actual_content_length: Some(0),
                image_header: false,
                gzip_header: false,
                skipped_existing: true,
            };
            emitter.push(
                events,
                ProgressEvent::SegmentFinished {
                    stream_id: stream_id.to_string(),
                    segment_index,
                },
            )?;
            return Ok(result);
        }
        let dec_path = decrypted_path(&final_path);
        if let Ok(metadata) = tokio::fs::metadata(&dec_path).await
            && metadata.is_file()
        {
            let size = metadata.len();
            add_speed(speed, size)?;
            let result = SegmentDownloadResult {
                actual_file_path: dec_path,
                response_content_length: None,
                actual_content_length: Some(0),
                image_header: false,
                gzip_header: false,
                skipped_existing: true,
            };
            emitter.push(
                events,
                ProgressEvent::SegmentFinished {
                    stream_id: stream_id.to_string(),
                    segment_index,
                },
            )?;
            return Ok(result);
        }

        let mut retry_attempt = 0_u32;
        let mut remaining_retries = options.download_retry_count;
        loop {
            match self
                .download_once(
                    segment,
                    save_path,
                    speed,
                    headers,
                    stream_id,
                    events,
                    http_client,
                    emitter,
                )
                .await
            {
                Ok(mut result) => {
                    if result.actual_file_path != final_path {
                        finalize_segment_file(&mut result, &final_path).await?;
                    }
                    emitter.push(
                        events,
                        ProgressEvent::SegmentFinished {
                            stream_id: stream_id.to_string(),
                            segment_index,
                        },
                    )?;
                    return Ok(result);
                }
                Err(error) if remaining_retries > 0 => {
                    push_retry_extra_logs(
                        emitter,
                        events,
                        remaining_retries,
                        &error,
                        &segment.url,
                        false,
                    )?;
                    emitter.push(
                        events,
                        ProgressEvent::Log {
                            level: LogLevel::Debug,
                            message: format!(
                                "{} retryCount: {remaining_retries}",
                                download_error_message(&error)
                            ),
                        },
                    )?;
                    emitter.push(
                        events,
                        ProgressEvent::Log {
                            level: LogLevel::Debug,
                            message: format!("{} {}", segment.url, error),
                        },
                    )?;
                    remaining_retries -= 1;
                    retry_attempt = retry_attempt.saturating_add(1);
                    emitter.push(
                        events,
                        ProgressEvent::SegmentRetry {
                            stream_id: stream_id.to_string(),
                            segment_index,
                            retry_attempt,
                        },
                    )?;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    if matches!(error, Error::UserCancelled) {
                        return Err(error);
                    }
                }
                Err(error) => {
                    push_retry_extra_logs(emitter, events, 0, &error, &segment.url, true)?;
                    return Err(error);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    async fn download_once(
        &self,
        segment: &MediaSegment,
        save_path: &Path,
        speed: &Arc<Mutex<SpeedState>>,
        headers: &BTreeMap<String, String>,
        stream_id: &str,
        events: &mut Vec<ProgressEvent>,
        http_client: &ReqwestClient,
        emitter: &DownloadEventEmitter,
    ) -> Result<SegmentDownloadResult> {
        if let Some(parent) = save_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        emitter.push(
            events,
            ProgressEvent::Log {
                level: LogLevel::Debug,
                message: format_segment_fetch_debug_message(segment, headers)?,
            },
        )?;
        if segment.url.starts_with("file:") {
            return copy_file_uri(segment, save_path, speed, stream_id, events, emitter).await;
        }
        if let Some(value) = segment.url.strip_prefix("base64://") {
            return write_special_bytes(
                base64_decode(value)?,
                save_path,
                speed,
                stream_id,
                segment.index,
                events,
            )
            .await;
        }
        if let Some(value) = segment.url.strip_prefix("hex://") {
            return write_special_bytes(
                hex_to_bytes(value)?,
                save_path,
                speed,
                stream_id,
                segment.index,
                events,
            )
            .await;
        }
        if tokio::fs::metadata(Path::new(&segment.url))
            .await
            .is_ok_and(|metadata| metadata.is_file())
        {
            return copy_local_file(
                segment,
                Path::new(&segment.url),
                save_path,
                speed,
                stream_id,
                events,
                emitter,
            )
            .await;
        }
        download_http(
            segment,
            save_path,
            speed,
            headers,
            stream_id,
            events,
            http_client,
            emitter,
        )
        .await
    }
}

/// Coordinates stream-level downloads.
#[derive(Clone, Debug)]
pub struct DownloadScheduler {
    downloader: SegmentDownloader,
}

impl Default for DownloadScheduler {
    fn default() -> Self {
        Self {
            downloader: SegmentDownloader::new(),
        }
    }
}

impl DownloadScheduler {
    /// Creates a scheduler.
    pub fn new() -> Self {
        Self::default()
    }

    /// Downloads selected streams.
    pub async fn download_streams(
        &self,
        streams: &[Stream],
        dir_prefix: &Path,
        save_dir: &Path,
        options: &DownloadOptions,
        headers: &BTreeMap<String, String>,
    ) -> Result<(Vec<StreamDownloadResult>, Vec<ProgressEvent>)> {
        self.download_streams_with_first_media_probe(
            streams, dir_prefix, save_dir, options, headers, false,
        )
        .await
    }

    /// Downloads selected streams, optionally forcing the first media segment
    /// through the probe path even when an init segment exists.
    pub async fn download_streams_with_first_media_probe(
        &self,
        streams: &[Stream],
        dir_prefix: &Path,
        save_dir: &Path,
        options: &DownloadOptions,
        headers: &BTreeMap<String, String>,
        force_first_media_probe: bool,
    ) -> Result<(Vec<StreamDownloadResult>, Vec<ProgressEvent>)> {
        let client = http_client(options)?;
        let (results, events) = self
            .download_streams_with_first_media_probe_and_http_client_capture(
                streams,
                dir_prefix,
                save_dir,
                options,
                headers,
                force_first_media_probe,
                &client,
                None,
            )
            .await;
        results.map(|results| (results, events))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn download_streams_with_first_media_probe_and_http_client_capture(
        &self,
        streams: &[Stream],
        dir_prefix: &Path,
        save_dir: &Path,
        options: &DownloadOptions,
        headers: &BTreeMap<String, String>,
        force_first_media_probe: bool,
        client: &ReqwestClient,
        progress_callback: Option<&ProgressCallback>,
    ) -> (Result<Vec<StreamDownloadResult>>, Vec<ProgressEvent>) {
        let hooks = DownloadHooks::default();
        self.download_streams_with_first_media_probe_and_http_client_capture_with_hooks(
            streams,
            dir_prefix,
            save_dir,
            options,
            headers,
            force_first_media_probe,
            client,
            progress_callback,
            &hooks,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn download_streams_with_first_media_probe_and_http_client_capture_with_hooks(
        &self,
        streams: &[Stream],
        dir_prefix: &Path,
        save_dir: &Path,
        options: &DownloadOptions,
        headers: &BTreeMap<String, String>,
        force_first_media_probe: bool,
        client: &ReqwestClient,
        progress_callback: Option<&ProgressCallback>,
        hooks: &DownloadHooks,
    ) -> (Result<Vec<StreamDownloadResult>>, Vec<ProgressEvent>) {
        let emitter = DownloadEventEmitter::new(progress_callback);
        if options.concurrent_download {
            return self
                .download_streams_concurrent_capture_with_hooks(
                    streams,
                    dir_prefix,
                    save_dir,
                    options,
                    headers,
                    force_first_media_probe,
                    client,
                    &emitter,
                    hooks,
                )
                .await;
        }

        let mut results = Vec::new();
        let mut events = Vec::new();
        if let Err(error) = emit_stream_tasks(streams, &mut events, &emitter) {
            return (Err(error), events);
        }
        for (task_id, stream) in streams.iter().enumerate() {
            let result = match self
                .download_stream_with_http_client_and_hooks(
                    stream,
                    task_id,
                    dir_prefix,
                    save_dir,
                    options,
                    headers,
                    force_first_media_probe,
                    &mut events,
                    client,
                    &emitter,
                    hooks,
                )
                .await
            {
                Ok(result) => result,
                Err(error) => return (Err(error), events),
            };
            results.push(result);
        }
        (Ok(results), events)
    }

    /// Downloads one stream.
    #[allow(clippy::too_many_arguments)]
    pub async fn download_stream(
        &self,
        stream: &Stream,
        task_id: usize,
        dir_prefix: &Path,
        save_dir: &Path,
        options: &DownloadOptions,
        headers: &BTreeMap<String, String>,
        force_first_media_probe: bool,
        events: &mut Vec<ProgressEvent>,
    ) -> Result<StreamDownloadResult> {
        let http_client = http_client(options)?;
        let emitter = DownloadEventEmitter::default();
        emit_stream_tasks(std::slice::from_ref(stream), events, &emitter)?;
        self.download_stream_with_http_client(
            stream,
            task_id,
            dir_prefix,
            save_dir,
            options,
            headers,
            force_first_media_probe,
            events,
            &http_client,
            &emitter,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn download_stream_with_http_client(
        &self,
        stream: &Stream,
        task_id: usize,
        dir_prefix: &Path,
        save_dir: &Path,
        options: &DownloadOptions,
        headers: &BTreeMap<String, String>,
        force_first_media_probe: bool,
        events: &mut Vec<ProgressEvent>,
        http_client: &ReqwestClient,
        emitter: &DownloadEventEmitter,
    ) -> Result<StreamDownloadResult> {
        let hooks = DownloadHooks::default();
        self.download_stream_with_http_client_and_hooks(
            stream,
            task_id,
            dir_prefix,
            save_dir,
            options,
            headers,
            force_first_media_probe,
            events,
            http_client,
            emitter,
            &hooks,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn download_stream_with_http_client_and_hooks(
        &self,
        stream: &Stream,
        task_id: usize,
        dir_prefix: &Path,
        save_dir: &Path,
        options: &DownloadOptions,
        headers: &BTreeMap<String, String>,
        force_first_media_probe: bool,
        events: &mut Vec<ProgressEvent>,
        http_client: &ReqwestClient,
        emitter: &DownloadEventEmitter,
        hooks: &DownloadHooks,
    ) -> Result<StreamDownloadResult> {
        let stream_id = stream_identifier(stream, task_id);
        let speed = Arc::new(Mutex::new(SpeedState::new(options.max_speed)));
        let mut files = Vec::new();
        let mut segment_results = Vec::new();
        let mut media_infos = Vec::new();
        let mut media_info_read = false;
        let mut completed = 0_u64;
        let mut segments = media_segments(stream);
        let mut large_single_file_split = false;
        if segments.len() == 1
            && !stream
                .playlist
                .as_ref()
                .is_some_and(|playlist| playlist.is_live)
        {
            let segment = segments
                .first()
                .ok_or_else(|| Error::protocol("stream segment list is empty"))?;
            if segment.url.starts_with("file:") {
                emitter.push(
                    events,
                    ProgressEvent::Log {
                        level: LogLevel::Debug,
                        message: "The 'file' scheme is not supported.".to_string(),
                    },
                )?;
            }
            if let Some(split) =
                split_large_single_file_with_http_client(segment, headers, http_client).await?
            {
                segments = split;
                large_single_file_split = true;
                emitter.push(
                    events,
                    ProgressEvent::Warning {
                        message: "The entire file has been cut into small segments to accelerate"
                            .to_string(),
                    },
                )?;
                if options.mp4_real_time_decryption {
                    emitter.push(
                        events,
                        ProgressEvent::Warning {
                            message: "Real-time decryption has been disabled".to_string(),
                        },
                    )?;
                }
            }
        }
        let temp_dir = stream_temp_dir(dir_prefix, task_id, stream);
        let dir_name = temp_dir
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        emitter.push(
            events,
            ProgressEvent::Log {
                level: LogLevel::Debug,
                message: format!(
                    "dirName: {dir_name}; tmpDir: {}; saveDir: {}; saveName: {}",
                    temp_dir.display(),
                    save_dir.display(),
                    options.save_name.as_deref().unwrap_or_default()
                ),
            },
        )?;
        tokio::fs::create_dir_all(&temp_dir).await?;
        tokio::fs::create_dir_all(save_dir).await?;
        let total_segments = segments.len().saturating_add(usize::from(
            stream
                .playlist
                .as_ref()
                .and_then(|p| p.media_init.as_ref())
                .is_some(),
        ));
        let initial_total_segments = if is_live_record_stream(stream, options) {
            segments.len()
        } else {
            total_segments
        };
        let policy = merge_policy_for_stream(stream, large_single_file_split, false);
        let hooks = if policy.disable_real_time_decryption {
            DownloadHooks::default()
        } else {
            hooks.clone()
        };
        let mut missing_later_segments = 0_usize;
        if !is_live_record_stream(stream, options) {
            emitter.push(
                events,
                ProgressEvent::Log {
                    level: LogLevel::Info,
                    message: format!("Start downloading...{}", stream_download_label(stream)),
                },
            )?;
        }
        if let Some(message) = automatic_binary_merge_warning(stream, options) {
            emitter.push(
                events,
                ProgressEvent::Warning {
                    message: message.to_string(),
                },
            )?;
        }
        push_stream_progress(
            events,
            &stream_id,
            &speed,
            completed,
            initial_total_segments,
            emitter,
        )?;
        if let Some(init) = stream
            .playlist
            .as_ref()
            .and_then(|playlist| playlist.media_init.as_ref())
        {
            emitter.push(
                events,
                ProgressEvent::SegmentQueued {
                    stream_id: stream_id.clone(),
                    segment_index: 0,
                },
            )?;
            let path = temp_dir.join("_init.mp4.tmp");
            let result = self
                .downloader
                .download_segment_with_http_client(
                    init,
                    &path,
                    &speed,
                    headers,
                    options,
                    events,
                    &stream_id,
                    http_client,
                    emitter,
                )
                .await?;
            if !result.success() {
                return Err(content_length_mismatch_error());
            }
            push_download_protection_logs(events, &stream_id, &result.actual_file_path, emitter)
                .await?;
            emit_hook_events(
                events,
                handle_segment_completed(
                    &hooks,
                    SegmentCompletedContext {
                        stream: stream.clone(),
                        segment: init.clone(),
                        actual_file_path: result.actual_file_path.clone(),
                        init_file_path: None,
                        stream_id: stream_id.clone(),
                        is_init: true,
                    },
                )
                .await?,
                emitter,
            )?;
            probe_and_log_media_info_once(
                &mut media_infos,
                &mut media_info_read,
                &stream_id,
                options,
                &result.actual_file_path,
                events,
                emitter,
            )
            .await?;
            files.push(result.actual_file_path.clone());
            segment_results.push(result);
            completed += 1;
            push_stream_progress(
                events,
                &stream_id,
                &speed,
                completed,
                total_segments,
                emitter,
            )?;
        }
        let pad_width = segments.len().to_string().len();
        let has_init = stream
            .playlist
            .as_ref()
            .and_then(|playlist| playlist.media_init.as_ref())
            .is_some();
        let init_file_path = files.first().cloned().filter(|_| has_init);
        let probe_first_media = !segments.is_empty() && (!has_init || force_first_media_probe);
        if probe_first_media {
            let first = segments.remove(0);
            let result = self
                .download_media_segment(
                    &first,
                    &temp_dir,
                    pad_width,
                    stream,
                    &speed,
                    headers,
                    options,
                    events,
                    &stream_id,
                    http_client,
                    emitter,
                )
                .await?;
            if !result.success() {
                return Err(content_length_mismatch_error());
            }
            push_download_protection_logs(events, &stream_id, &result.actual_file_path, emitter)
                .await?;
            rewrite_mss_init_from_first_segment(stream, &files, &result.actual_file_path).await?;
            emit_hook_events(
                events,
                handle_segment_completed(
                    &hooks,
                    SegmentCompletedContext {
                        stream: stream.clone(),
                        segment: first.clone(),
                        actual_file_path: result.actual_file_path.clone(),
                        init_file_path: init_file_path.clone(),
                        stream_id: stream_id.clone(),
                        is_init: false,
                    },
                )
                .await?,
                emitter,
            )?;
            probe_and_log_media_info_once(
                &mut media_infos,
                &mut media_info_read,
                &stream_id,
                options,
                &result.actual_file_path,
                events,
                emitter,
            )
            .await?;
            files.push(result.actual_file_path.clone());
            segment_results.push(result);
            completed += 1;
            push_stream_progress(
                events,
                &stream_id,
                &speed,
                completed,
                total_segments,
                emitter,
            )?;
        }
        match thread_mode(options.thread_count, segments.len())? {
            ThreadMode::Sequential => {
                for segment in &segments {
                    let result = match self
                        .download_media_segment(
                            segment,
                            &temp_dir,
                            pad_width,
                            stream,
                            &speed,
                            headers,
                            options,
                            events,
                            &stream_id,
                            http_client,
                            emitter,
                        )
                        .await
                    {
                        Ok(result) => result,
                        Err(error) if should_collect_later_segment_error(&error) => {
                            missing_later_segments = missing_later_segments.saturating_add(1);
                            emitter.push(
                                events,
                                ProgressEvent::Warning {
                                    message: download_error_message(&error),
                                },
                            )?;
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    if result.success() {
                        emit_hook_events(
                            events,
                            handle_segment_completed(
                                &hooks,
                                SegmentCompletedContext {
                                    stream: stream.clone(),
                                    segment: segment.clone(),
                                    actual_file_path: result.actual_file_path.clone(),
                                    init_file_path: init_file_path.clone(),
                                    stream_id: stream_id.clone(),
                                    is_init: false,
                                },
                            )
                            .await?,
                            emitter,
                        )?;
                        files.push(result.actual_file_path.clone());
                        completed += 1;
                        push_stream_progress(
                            events,
                            &stream_id,
                            &speed,
                            completed,
                            total_segments,
                            emitter,
                        )?;
                    }
                    segment_results.push(result);
                }
            }
            ThreadMode::Concurrent(chunk_size) => {
                let (results, missing) = self
                    .download_segments_concurrent(
                        &segments,
                        &temp_dir,
                        pad_width,
                        stream,
                        &speed,
                        headers,
                        options,
                        events,
                        &stream_id,
                        http_client,
                        chunk_size,
                        emitter,
                        &hooks,
                        init_file_path.as_deref(),
                    )
                    .await?;
                missing_later_segments = missing_later_segments.saturating_add(missing);
                for result in results {
                    if result.success() {
                        files.push(result.actual_file_path.clone());
                        completed += 1;
                    }
                    segment_results.push(result);
                }
                push_stream_progress(
                    events,
                    &stream_id,
                    &speed,
                    completed,
                    total_segments,
                    emitter,
                )?;
            }
        }
        if options.check_segments_count && missing_later_segments > 0 {
            return Err(segment_count_mismatch_error(
                total_segments,
                segment_results.len(),
            ));
        }
        ensure_content_lengths(&segment_results)?;
        emit_hook_events(
            events,
            handle_stream_completed(&hooks, stream_id.clone()).await?,
            emitter,
        )?;
        let output_path = planned_output_path(save_dir, stream, task_id, options);
        Ok(StreamDownloadResult {
            stream_id,
            temp_dir,
            files,
            output_path,
            binary_merge_required: policy.binary_merge_required,
            disable_real_time_decryption: policy.disable_real_time_decryption,
            use_aac_filter: false,
            media_infos,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn download_media_segment(
        &self,
        segment: &MediaSegment,
        temp_dir: &Path,
        pad_width: usize,
        stream: &Stream,
        speed: &Arc<Mutex<SpeedState>>,
        headers: &BTreeMap<String, String>,
        options: &DownloadOptions,
        events: &mut Vec<ProgressEvent>,
        stream_id: &str,
        http_client: &ReqwestClient,
        emitter: &DownloadEventEmitter,
    ) -> Result<SegmentDownloadResult> {
        emitter.push(
            events,
            ProgressEvent::SegmentQueued {
                stream_id: stream_id.to_string(),
                segment_index: u64_from_i64(segment.index),
            },
        )?;
        let extension = stream.extension.as_deref().unwrap_or("clip");
        let path = temp_dir.join(format!(
            "{:0width$}.{extension}.tmp",
            segment.index,
            width = pad_width
        ));
        self.downloader
            .download_segment_with_http_client(
                segment,
                &path,
                speed,
                headers,
                options,
                events,
                stream_id,
                http_client,
                emitter,
            )
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn download_segments_concurrent(
        &self,
        segments: &[MediaSegment],
        temp_dir: &Path,
        pad_width: usize,
        stream: &Stream,
        speed: &Arc<Mutex<SpeedState>>,
        headers: &BTreeMap<String, String>,
        options: &DownloadOptions,
        events: &mut Vec<ProgressEvent>,
        stream_id: &str,
        http_client: &ReqwestClient,
        chunk_size: usize,
        emitter: &DownloadEventEmitter,
        hooks: &DownloadHooks,
        init_file_path: Option<&Path>,
    ) -> Result<(Vec<SegmentDownloadResult>, usize)> {
        let shared_events =
            (!emitter.uses_shared_events()).then(|| Arc::new(Mutex::new(Vec::new())));
        let concurrent_emitter = shared_events
            .as_ref()
            .map(|events| emitter.with_shared_events(Arc::clone(events)))
            .unwrap_or_else(|| emitter.clone());
        let mut downloaded = Vec::new();
        let mut missing = 0_usize;
        for (chunk_index, chunk) in segments.chunks(chunk_size).enumerate() {
            let mut handles = tokio::task::JoinSet::new();
            for (offset, segment) in chunk.iter().enumerate() {
                let order = chunk_index
                    .saturating_mul(chunk_size)
                    .saturating_add(offset);
                let downloader = self.downloader.clone();
                let segment = segment.clone();
                let temp_dir = temp_dir.to_path_buf();
                let stream = stream.clone();
                let speed = Arc::clone(speed);
                let headers = headers.clone();
                let options = options.clone();
                let http_client = http_client.clone();
                let emitter = concurrent_emitter.clone();
                let hooks = hooks.clone();
                let init_file_path = init_file_path.map(Path::to_path_buf);
                let stream_id = stream_id.to_string();
                handles.spawn(async move {
                    let mut local_events = Vec::new();
                    let extension = stream.extension.as_deref().unwrap_or("clip");
                    emitter.push(
                        &mut local_events,
                        ProgressEvent::SegmentQueued {
                            stream_id: stream_id.clone(),
                            segment_index: u64_from_i64(segment.index),
                        },
                    )?;
                    let path = temp_dir.join(format!(
                        "{:0width$}.{extension}.tmp",
                        segment.index,
                        width = pad_width
                    ));
                    let result = downloader
                        .download_segment_with_http_client(
                            &segment,
                            &path,
                            &speed,
                            &headers,
                            &options,
                            &mut local_events,
                            &stream_id,
                            &http_client,
                            &emitter,
                        )
                        .await?;
                    if result.success() {
                        emit_hook_events(
                            &mut local_events,
                            handle_segment_completed(
                                &hooks,
                                SegmentCompletedContext {
                                    stream,
                                    segment,
                                    actual_file_path: result.actual_file_path.clone(),
                                    init_file_path,
                                    stream_id,
                                    is_init: false,
                                },
                            )
                            .await?,
                            &emitter,
                        )?;
                    }
                    Ok::<_, Error>((order, result))
                });
            }
            let mut chunk_results = Vec::new();
            while let Some(joined) = handles.join_next().await {
                match joined {
                    Ok(Ok(result)) => chunk_results.push(result),
                    Ok(Err(error)) if should_collect_later_segment_error(&error) => {
                        missing = missing.saturating_add(1);
                        concurrent_emitter.push(
                            events,
                            ProgressEvent::Warning {
                                message: download_error_message(&error),
                            },
                        )?;
                    }
                    Ok(Err(error)) => return Err(error),
                    Err(_) => return Err(Error::http("download worker failed")),
                }
            }
            chunk_results.sort_by_key(|(order, _)| *order);
            downloaded.extend(chunk_results.into_iter().map(|(_, result)| result));
        }
        if let Some(shared_events) = shared_events
            && let Ok(mut guard) = shared_events.lock()
        {
            events.append(&mut guard);
        }
        Ok((downloaded, missing))
    }

    #[allow(clippy::too_many_arguments)]
    async fn download_streams_concurrent_capture_with_hooks(
        &self,
        streams: &[Stream],
        dir_prefix: &Path,
        save_dir: &Path,
        options: &DownloadOptions,
        headers: &BTreeMap<String, String>,
        force_first_media_probe: bool,
        http_client: &ReqwestClient,
        emitter: &DownloadEventEmitter,
        hooks: &DownloadHooks,
    ) -> (Result<Vec<StreamDownloadResult>>, Vec<ProgressEvent>) {
        let shared_events = Arc::new(Mutex::new(Vec::new()));
        let concurrent_emitter = emitter.with_shared_events(Arc::clone(&shared_events));
        if let Ok(mut guard) = shared_events.lock()
            && let Err(error) = emit_stream_tasks(streams, &mut guard, emitter)
        {
            return (Err(error), Vec::new());
        }
        let mut handles = tokio::task::JoinSet::new();
        for (task_id, stream) in streams.iter().enumerate() {
            let scheduler = self.clone();
            let stream = stream.clone();
            let dir_prefix = dir_prefix.to_path_buf();
            let save_dir = save_dir.to_path_buf();
            let options = options.clone();
            let headers = headers.clone();
            let http_client = http_client.clone();
            let emitter = concurrent_emitter.clone();
            let hooks = hooks.clone();
            handles.spawn(async move {
                let mut local_events = Vec::new();
                let result = scheduler
                    .download_stream_with_http_client_and_hooks(
                        &stream,
                        task_id,
                        &dir_prefix,
                        &save_dir,
                        &options,
                        &headers,
                        force_first_media_probe,
                        &mut local_events,
                        &http_client,
                        &emitter,
                        &hooks,
                    )
                    .await?;
                Ok::<_, Error>((task_id, result))
            });
        }
        let mut results = Vec::new();
        let mut scope_result = Ok(());
        while let Some(joined) = handles.join_next().await {
            match joined {
                Ok(Ok(result)) => results.push(result),
                Ok(Err(error)) => {
                    scope_result = Err(error);
                    break;
                }
                Err(_) => {
                    scope_result = Err(Error::http("download worker failed"));
                    break;
                }
            }
        }
        handles.abort_all();
        results.sort_by_key(|(task_id, _)| *task_id);
        let mut events = Vec::new();
        if let Ok(mut guard) = shared_events.lock() {
            events.append(&mut guard);
        }
        match scope_result {
            Ok(()) => (
                Ok(results.into_iter().map(|(_, result)| result).collect()),
                events,
            ),
            Err(error) => (Err(error), events),
        }
    }
}

async fn handle_segment_completed(
    hooks: &DownloadHooks,
    context: SegmentCompletedContext,
) -> Result<Vec<ProgressEvent>> {
    match &hooks.segment_completed {
        Some(hook) => hook(context).await,
        None => Ok(Vec::new()),
    }
}

async fn handle_stream_completed(
    hooks: &DownloadHooks,
    stream_id: String,
) -> Result<Vec<ProgressEvent>> {
    match &hooks.stream_completed {
        Some(hook) => hook(stream_id).await,
        None => Ok(Vec::new()),
    }
}

fn emit_hook_events(
    events: &mut Vec<ProgressEvent>,
    hook_events: Vec<ProgressEvent>,
    emitter: &DownloadEventEmitter,
) -> Result<()> {
    for event in hook_events {
        emitter.push(events, event)?;
    }
    Ok(())
}

async fn rewrite_mss_init_from_first_segment(
    stream: &Stream,
    files: &[PathBuf],
    first_media_path: &Path,
) -> Result<()> {
    if stream.mss_data.is_none() {
        return Ok(());
    }
    let Some(init_path) = files.first() else {
        return Ok(());
    };
    let first_segment = tokio::fs::read(first_media_path).await?;
    let generated = MssInitGenerator::generate_with_first_segment(stream, &first_segment)?;
    tokio::fs::write(init_path, generated.bytes).await?;
    Ok(())
}

fn thread_mode(thread_count: i32, segment_count: usize) -> Result<ThreadMode> {
    if thread_count == 1 {
        return Ok(ThreadMode::Sequential);
    }
    Ok(ThreadMode::Concurrent(thread_chunk_size(
        thread_count,
        segment_count,
    )?))
}

fn thread_chunk_size(thread_count: i32, segment_count: usize) -> Result<usize> {
    match thread_count {
        -1 => Ok(segment_count.max(1)),
        0 | i32::MIN..=-2 => Err(Error::config("--thread-count is invalid")),
        value => usize::try_from(value).map_err(|_| Error::config("--thread-count is invalid")),
    }
}

fn format_fetch_debug_message(url: &str, headers: &BTreeMap<String, String>) -> String {
    let mut message = format!("Fetch: {}", redact_fetch_value(url));
    if is_http_url(url) {
        for (key, value) in headers {
            message.push('\n');
            message.push_str(key);
            message.push_str(": ");
            message.push_str(&redact_fetch_value(value));
        }
    }
    message
}

fn format_segment_fetch_debug_message(
    segment: &MediaSegment,
    headers: &BTreeMap<String, String>,
) -> Result<String> {
    format_segment_fetch_debug_message_for_url(segment, &segment.url, headers)
}

fn format_segment_fetch_debug_message_for_url(
    segment: &MediaSegment,
    url: &str,
    headers: &BTreeMap<String, String>,
) -> Result<String> {
    let mut message = format_fetch_debug_message(url, headers);
    if is_http_url(url)
        && let Some(range) = range_header(segment)?
    {
        message.push('\n');
        message.push_str("Range: ");
        message.push_str(&range);
    }
    Ok(message)
}

fn format_header_map(headers: &HeaderMap) -> String {
    let mut text = String::new();
    for (name, value) in headers {
        if let Ok(value) = value.to_str() {
            text.push_str(name.as_str());
            text.push_str(": ");
            text.push_str(value);
            text.push('\n');
        }
    }
    text.trim_end().to_string()
}

fn redact_fetch_value(value: &str) -> String {
    let redacted = redact_secrets(value);
    const MAX_DEBUG_VALUE_LEN: usize = 512;
    if redacted.len() <= MAX_DEBUG_VALUE_LEN {
        redacted
    } else {
        let truncated = redacted
            .chars()
            .take(MAX_DEBUG_VALUE_LEN)
            .collect::<String>();
        format!("{truncated}...")
    }
}

fn is_http_url(url: &str) -> bool {
    let lower = url
        .get(..url.len().min(8))
        .unwrap_or(url)
        .to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Builds the compatibility temporary directory name for a stream.
pub fn stream_temp_dir(dir_prefix: &Path, task_id: usize, stream: &Stream) -> PathBuf {
    let dir_name = format!(
        "{task_id}_{}_{}_{}_{}",
        valid_file_name(stream.group_id.as_deref().unwrap_or_default(), "-", false),
        stream.codecs.as_deref().unwrap_or_default(),
        stream
            .bandwidth
            .map(|value| value.to_string())
            .unwrap_or_default(),
        stream.language.as_deref().unwrap_or_default()
    );
    dir_prefix.join(dir_name)
}

/// Returns the planned output extension for a stream.
pub fn stream_output_extension(stream: &Stream, options: &DownloadOptions) -> String {
    let mut output_ext = stream
        .extension
        .as_ref()
        .map(|extension| format!(".{extension}"))
        .unwrap_or_else(|| ".ts".to_string());
    if stream.media_type == Some(MediaType::Audio)
        && matches!(stream.extension.as_deref(), Some("m4s" | "mp4"))
    {
        output_ext = ".m4a".to_string();
    } else if stream.media_type != Some(MediaType::Subtitles)
        && matches!(stream.extension.as_deref(), Some("m4s" | "mp4"))
    {
        output_ext = ".mp4".to_string();
    }
    if options.auto_subtitle_fix && stream.media_type == Some(MediaType::Subtitles) {
        output_ext = match options.sub_format {
            crate::config::SubtitleFormat::Srt => ".srt".to_string(),
            crate::config::SubtitleFormat::Vtt => ".vtt".to_string(),
        };
    }
    output_ext
}

/// Plans the output path before merge/subtitle work.
pub fn planned_output_path(
    save_dir: &Path,
    stream: &Stream,
    task_id: usize,
    options: &DownloadOptions,
) -> PathBuf {
    let dir_name = format!(
        "{task_id}_{}_{}_{}_{}",
        valid_file_name(stream.group_id.as_deref().unwrap_or_default(), "-", false),
        stream.codecs.as_deref().unwrap_or_default(),
        stream
            .bandwidth
            .map(|value| value.to_string())
            .unwrap_or_default(),
        stream.language.as_deref().unwrap_or_default()
    );
    let save_name = if let Some(pattern) = &options.save_pattern
        && !pattern.trim().is_empty()
    {
        format_save_pattern(pattern, stream, options.save_name.as_deref(), task_id)
    } else if let Some(save_name) = &options.save_name {
        format!(
            "{}.{}",
            save_name,
            stream.language.as_deref().unwrap_or_default()
        )
        .trim_end_matches('.')
        .to_string()
    } else {
        dir_name
    };
    let output = save_dir.join(format!(
        "{save_name}{}",
        stream_output_extension(stream, options)
    ));
    handle_file_collision(&output, stream)
}

/// Computes automatic binary merge and related flags from stream metadata.
pub fn merge_policy_for_stream(
    stream: &Stream,
    large_single_file_split: bool,
    dolby_vision_detected: bool,
) -> DownloadMergePolicy {
    let mut policy = DownloadMergePolicy::default();
    if large_single_file_split {
        policy.disable_real_time_decryption = true;
    }
    if dolby_vision_detected {
        policy.binary_merge_required = true;
        policy.disable_mux_after_done = true;
    }
    if let Some(playlist) = &stream.playlist {
        if playlist.media_parts.len() > 1 {
            policy.binary_merge_required = true;
        }
        if playlist.media_init.is_some() && stream.media_type != Some(MediaType::Subtitles) {
            policy.binary_merge_required = true;
        }
        if stream_has_encryption_method(stream, EncryptionMethod::Cenc) {
            policy.binary_merge_required = true;
        }
        if stream_has_encryption_method(stream, EncryptionMethod::Unknown) {
            policy.binary_merge_required = true;
            policy.unknown_encryption = true;
        }
    }
    policy
}

fn stream_has_encryption_method(stream: &Stream, method: EncryptionMethod) -> bool {
    stream.playlist.as_ref().is_some_and(|playlist| {
        playlist
            .media_init
            .iter()
            .chain(
                playlist
                    .media_parts
                    .iter()
                    .flat_map(|part| part.media_segments.iter()),
            )
            .any(|segment| segment.encryption.method == method)
    })
}

fn automatic_binary_merge_warning<'a>(
    stream: &Stream,
    options: &DownloadOptions,
) -> Option<&'a str> {
    if options.binary_merge {
        return None;
    }
    let playlist = stream.playlist.as_ref()?;
    if stream_has_encryption_method(stream, EncryptionMethod::Cenc) {
        return Some("When CENC encryption is detected, binary merging is automatically enabled");
    }
    if stream_has_encryption_method(stream, EncryptionMethod::Unknown) {
        return Some(
            "An unrecognized encryption method is detected, binary merging is automatically enabled",
        );
    }
    if playlist.media_init.is_some() && stream.media_type != Some(MediaType::Subtitles) {
        return Some("fMP4 is detected, binary merging is automatically enabled");
    }
    None
}

fn is_live_record_stream(stream: &Stream, options: &DownloadOptions) -> bool {
    !options.live_perform_as_vod
        && stream
            .playlist
            .as_ref()
            .is_some_and(|playlist| playlist.is_live)
}

/// Splits a single large ranged segment into compatibility-sized chunks.
pub fn split_large_single_file_by_size(
    segment: &MediaSegment,
    file_size: i64,
) -> Option<Vec<MediaSegment>> {
    if file_size <= 0 || segment.start_range.is_some() {
        return None;
    }
    let mut clips = Vec::new();
    let original = file_size;
    let mut remaining = file_size;
    let mut index = 0_i64;
    let mut counter = 0_i64;
    while remaining > 0 {
        let mut to = counter + LARGE_SPLIT_SIZE;
        if remaining - LARGE_SPLIT_SIZE > 0 {
            remaining -= LARGE_SPLIT_SIZE;
        } else {
            to = original;
            remaining = 0;
        }
        clips.push(MediaSegment {
            index,
            url: segment.url.clone(),
            start_range: Some(counter),
            expected_length: if to == -1 {
                None
            } else {
                Some(to - counter + 1)
            },
            encryption: segment.encryption.clone(),
            ..MediaSegment::default()
        });
        counter = to.saturating_add(1);
        index += 1;
    }
    Some(clips)
}

/// Splits a single ranged-capable HTTP file using known length or a bounded HEAD probe.
pub async fn split_large_single_file(
    segment: &MediaSegment,
    headers: &BTreeMap<String, String>,
    options: &DownloadOptions,
) -> Result<Option<Vec<MediaSegment>>> {
    let http_client = http_client(options)?;
    split_large_single_file_with_http_client(segment, headers, &http_client).await
}

async fn split_large_single_file_with_http_client(
    segment: &MediaSegment,
    headers: &BTreeMap<String, String>,
    http_client: &ReqwestClient,
) -> Result<Option<Vec<MediaSegment>>> {
    if segment.start_range.is_some() {
        return Ok(None);
    }
    if !(segment.url.starts_with("http://") || segment.url.starts_with("https://")) {
        return Ok(None);
    }
    let Some(size) = probe_split_file_size(segment, headers, http_client).await? else {
        return Ok(None);
    };
    Ok(split_large_single_file_by_size(segment, size))
}

/// Deletes raw files and empty directories after successful workflows.
pub async fn cleanup_after_success(
    dir_prefix: &Path,
    raw_files: &[String],
    options: &DownloadOptions,
) -> Result<Vec<PathBuf>> {
    let mut touched = Vec::new();
    if !options.del_after_done || options.skip_merge {
        return Ok(touched);
    }
    for raw_file in raw_files {
        let path = dir_prefix.join(raw_file);
        if tokio::fs::try_exists(&path).await? {
            tokio::fs::remove_file(&path).await?;
            touched.push(path);
        }
    }
    safe_delete_empty_dirs(dir_prefix, &mut touched).await?;
    Ok(touched)
}

async fn copy_file_uri(
    segment: &MediaSegment,
    save_path: &Path,
    speed: &Arc<Mutex<SpeedState>>,
    stream_id: &str,
    events: &mut Vec<ProgressEvent>,
    emitter: &DownloadEventEmitter,
) -> Result<SegmentDownloadResult> {
    let path = file_uri_to_path(&segment.url);
    copy_local_file(segment, &path, save_path, speed, stream_id, events, emitter).await
}

async fn copy_local_file(
    segment: &MediaSegment,
    source_path: &Path,
    save_path: &Path,
    speed: &Arc<Mutex<SpeedState>>,
    stream_id: &str,
    events: &mut Vec<ProgressEvent>,
    emitter: &DownloadEventEmitter,
) -> Result<SegmentDownloadResult> {
    let mut input = tokio::fs::File::open(source_path).await?;
    let mut output = tokio::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(save_path)
        .await?;
    let source_len = input.metadata().await?.len();
    let start = segment.start_range.unwrap_or(0);
    let start = u64::try_from(start).map_err(|_| Error::protocol("byte range start is invalid"))?;
    if start > 0 {
        input.seek(SeekFrom::Start(start)).await?;
    }
    let current_position = input.stream_position().await?;
    let source_len_i64 =
        i64::try_from(source_len).map_err(|_| Error::protocol("source length is too large"))?;
    let current_position_i64 = i64::try_from(current_position)
        .map_err(|_| Error::protocol("byte range position is too large"))?;
    let stop = segment.stop_range().unwrap_or(source_len_i64);
    let expected = stop
        .checked_sub(current_position_i64)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| Error::protocol("byte range length is invalid"))?;
    if expected < 0 {
        return Err(Error::protocol("byte range length is invalid"));
    }
    let full_copy_expected = source_len_i64
        .checked_add(1)
        .ok_or_else(|| Error::protocol("source length is too large"))?;
    if expected == full_copy_expected {
        let _ = tokio::io::copy(&mut input, &mut output).await?;
        add_speed(speed, source_len)?;
        push_segment_progress(
            events,
            stream_id,
            segment.index,
            speed,
            Some(source_len),
            emitter,
        )?;
    } else {
        let expected =
            u64::try_from(expected).map_err(|_| Error::protocol("byte range length is invalid"))?;
        copy_local_partial(
            &mut input,
            &mut output,
            expected,
            speed,
            stream_id,
            segment.index,
            events,
            emitter,
        )
        .await?;
    }
    let result = SegmentDownloadResult {
        actual_file_path: save_path.to_path_buf(),
        response_content_length: None,
        actual_content_length: {
            output.flush().await?;
            Some(output.metadata().await?.len())
        },
        image_header: false,
        gzip_header: false,
        skipped_existing: false,
    };
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
async fn copy_local_partial(
    input: &mut tokio::fs::File,
    output: &mut tokio::fs::File,
    expected: u64,
    speed: &Arc<Mutex<SpeedState>>,
    stream_id: &str,
    segment_index: i64,
    events: &mut Vec<ProgressEvent>,
    emitter: &DownloadEventEmitter,
) -> Result<()> {
    let mut remaining = expected;
    let mut buffer = [0_u8; BUFFER_SIZE];
    while remaining > 0 {
        let len = std::cmp::min(buffer.len() as u64, remaining) as usize;
        let size = input.read(&mut buffer[..len]).await?;
        if size > 0 {
            output.write_all(&buffer[..size]).await?;
            add_speed(speed, size as u64)?;
            remaining = remaining.saturating_sub(size as u64);
            push_segment_progress(
                events,
                stream_id,
                segment_index,
                speed,
                Some(expected),
                emitter,
            )?;
            continue;
        }

        buffer[..len].fill(0);
        output.write_all(&buffer[..len]).await?;
        add_speed(speed, len as u64)?;
        remaining = remaining.saturating_sub(len as u64);
        push_segment_progress(
            events,
            stream_id,
            segment_index,
            speed,
            Some(expected),
            emitter,
        )?;
    }
    Ok(())
}

async fn write_special_bytes(
    bytes: Vec<u8>,
    save_path: &Path,
    _speed: &Arc<Mutex<SpeedState>>,
    _stream_id: &str,
    _segment_index: i64,
    _events: &mut Vec<ProgressEvent>,
) -> Result<SegmentDownloadResult> {
    tokio::fs::write(save_path, &bytes).await?;
    let result = SegmentDownloadResult {
        actual_file_path: save_path.to_path_buf(),
        response_content_length: None,
        actual_content_length: Some(bytes.len() as u64),
        image_header: false,
        gzip_header: false,
        skipped_existing: false,
    };
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
async fn download_http(
    segment: &MediaSegment,
    save_path: &Path,
    speed: &Arc<Mutex<SpeedState>>,
    headers: &BTreeMap<String, String>,
    stream_id: &str,
    events: &mut Vec<ProgressEvent>,
    client: &ReqwestClient,
    emitter: &DownloadEventEmitter,
) -> Result<SegmentDownloadResult> {
    let mut current_url = segment.url.clone();
    loop {
        let mut request = client.get(&current_url);
        request = apply_request_headers(request, headers);
        if let Some(range) = range_header(segment)? {
            request = request.header("Range", &range);
        }
        let mut response = request
            .send()
            .await
            .map_err(|error| Error::http(error.to_string()))?;
        let status = response.status().as_u16();
        if (300..=399).contains(&status)
            && let Some(location) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
        {
            let redirected = resolve_redirect(&current_url, location);
            if redirected != current_url {
                emitter.push(
                    events,
                    ProgressEvent::Log {
                        level: LogLevel::Debug,
                        message: format_header_map(response.headers()),
                    },
                )?;
                current_url = redirected;
                emitter.push(
                    events,
                    ProgressEvent::Log {
                        level: LogLevel::Debug,
                        message: format_segment_fetch_debug_message_for_url(
                            segment,
                            &current_url,
                            headers,
                        )?,
                    },
                )?;
                let _ = response.bytes().await;
                continue;
            }
        }
        if !(200..=299).contains(&status) {
            let _ = response.bytes().await;
            return Err(Error::http(format!(
                "HTTP status {status} for {current_url}"
            )));
        }
        let response_content_length = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let mut output = tokio::fs::File::create(save_path).await?;
        let mut first_probe = [0_u8; BUFFER_SIZE];
        let mut first_probe_set = false;
        loop {
            let chunk = match tokio::time::timeout(SEGMENT_READ_TIMEOUT, response.chunk()).await {
                Ok(Ok(Some(chunk))) => chunk,
                Ok(Ok(None)) => break,
                Ok(Err(error)) => return Err(Error::http(error.to_string())),
                Err(_) => {
                    if should_cancel_for_low_speed(speed)? {
                        push_low_speed_cancel_debug(events, emitter)?;
                        return Err(Error::http("download speed too slow"));
                    }
                    continue;
                }
            };
            let size = chunk.len();
            if size == 0 {
                break;
            }
            if !first_probe_set {
                let len = size.min(BUFFER_SIZE);
                first_probe[..len].copy_from_slice(&chunk[..len]);
                first_probe_set = true;
            }
            output.write_all(&chunk).await?;
            add_speed(speed, size as u64)?;
            enforce_speed_limit(speed).await?;
            if should_cancel_for_low_speed(speed)? {
                push_low_speed_cancel_debug(events, emitter)?;
                return Err(Error::http("download speed too slow"));
            }
            push_segment_progress(
                events,
                stream_id,
                segment.index,
                speed,
                response_content_length,
                emitter,
            )?;
        }
        output.flush().await?;
        drop(output);
        let actual = tokio::fs::metadata(save_path).await?.len();
        let mut result = SegmentDownloadResult {
            actual_file_path: save_path.to_path_buf(),
            response_content_length,
            actual_content_length: Some(actual),
            image_header: is_image_header(&first_probe),
            gzip_header: is_gzip_header(&first_probe),
            skipped_existing: false,
        };
        repair_downloaded_file(&mut result).await?;
        return Ok(result);
    }
}

fn push_low_speed_cancel_debug(
    events: &mut Vec<ProgressEvent>,
    emitter: &DownloadEventEmitter,
) -> Result<()> {
    emitter.push(
        events,
        ProgressEvent::Log {
            level: LogLevel::Debug,
            message: "Cancel...".to_string(),
        },
    )
}

fn content_length_mismatch_error() -> Error {
    Error::http("downloaded segment length did not match response")
}

fn segment_count_mismatch_error(expected: usize, actual: usize) -> Error {
    Error::http(format!(
        "Segment count check not pass, total: {expected}, downloaded: {actual}."
    ))
}

fn download_error_message(error: &Error) -> String {
    match error {
        Error::Io(error) => error.to_string(),
        Error::Http { message }
        | Error::Protocol { message }
        | Error::Decrypt { message }
        | Error::Mux { message }
        | Error::Subtitle { message }
        | Error::Live { message }
        | Error::Config { message }
        | Error::Compatibility { message } => message.clone(),
        Error::UserCancelled => "operation cancelled".to_string(),
    }
}

fn push_retry_extra_logs(
    emitter: &DownloadEventEmitter,
    events: &mut Vec<ProgressEvent>,
    retry_count: i32,
    error: &Error,
    url: &str,
    exhausted: bool,
) -> Result<()> {
    let error_message = redact_secrets(&download_error_message(error));
    let url = redact_secrets(url);
    emitter.push(
        events,
        ProgressEvent::ExtraLog {
            message: format!(
                "Ah oh!\nRetryCount => {retry_count}\nException  => {error_message}\nUrl        => {url}",
            ),
        },
    )?;
    if exhausted {
        emitter.push(
            events,
            ProgressEvent::ExtraLog {
                message: format!(
                    "The retry attempts have been exhausted and the download of this segment has failed.\nException  => {error_message}\nUrl        => {url}",
                ),
            },
        )?;
    }
    Ok(())
}

async fn probe_and_log_media_info_once(
    media_infos: &mut Vec<MediaInfo>,
    media_info_read: &mut bool,
    stream_id: &str,
    options: &DownloadOptions,
    path: &Path,
    events: &mut Vec<ProgressEvent>,
    emitter: &DownloadEventEmitter,
) -> Result<()> {
    if *media_info_read {
        return Ok(());
    }
    let Some(ffmpeg) = options.ffmpeg_binary_path.as_deref() else {
        return Ok(());
    };
    let infos = probe_ffmpeg_media_infos(ffmpeg, path).await?;
    if infos.is_empty() {
        return Ok(());
    }
    if options.log_level != LogLevel::Off {
        emitter.push(
            events,
            ProgressEvent::MediaInfo {
                stream_id: Some(stream_id.to_string()),
                lines: infos.iter().map(media_info_console_label).collect(),
            },
        )?;
    }
    *media_infos = infos;
    *media_info_read = true;
    Ok(())
}

async fn push_download_protection_logs(
    events: &mut Vec<ProgressEvent>,
    stream_id: &str,
    path: &Path,
    emitter: &DownloadEventEmitter,
) -> Result<()> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut bytes = vec![0_u8; 1024 * 1024];
    let size = file.read(&mut bytes).await?;
    bytes.truncate(size);
    let info = read_mp4_protection_info(&bytes);
    if let Some(scheme) = info.scheme {
        emitter.push(
            events,
            ProgressEvent::DecryptProgress {
                stream_id: Some(stream_id.to_string()),
                message: format!("Type: {scheme}"),
            },
        )?;
    }
    for pssh in info.psshs {
        emitter.push(
            events,
            ProgressEvent::DecryptProgress {
                stream_id: Some(stream_id.to_string()),
                message: format!("PSSH({}): {}", pssh_system_label(pssh.system), pssh.data),
            },
        )?;
    }
    if let Some(kid) = info.kid {
        emitter.push(
            events,
            ProgressEvent::DecryptProgress {
                stream_id: Some(stream_id.to_string()),
                message: format!("KID: {kid}"),
            },
        )?;
    }
    Ok(())
}

fn pssh_system_label(system: PsshSystem) -> &'static str {
    match system {
        PsshSystem::Widevine => "WV",
        PsshSystem::PlayReady => "PR",
        PsshSystem::FairPlay => "FP",
    }
}

fn ensure_content_lengths(results: &[SegmentDownloadResult]) -> Result<()> {
    if results.iter().any(|result| !result.success()) {
        return Err(content_length_mismatch_error());
    }
    Ok(())
}

fn should_collect_later_segment_error(error: &Error) -> bool {
    !matches!(error, Error::UserCancelled)
}

pub(crate) fn http_client(options: &DownloadOptions) -> Result<ReqwestClient> {
    shared_http_client(
        options.http_request_timeout,
        options.custom_proxy.as_deref(),
        options.use_system_proxy,
        options.allow_insecure_tls,
    )
}

async fn probe_split_file_size(
    segment: &MediaSegment,
    headers: &BTreeMap<String, String>,
    client: &ReqwestClient,
) -> Result<Option<i64>> {
    let response = match client.head(&segment.url).send().await {
        Ok(response) => response,
        Err(_) => return Ok(None),
    };
    if !(200..=299).contains(&response.status().as_u16()) {
        let _ = response.bytes().await;
        return Ok(None);
    }
    if response.headers().get("Accept-Ranges").is_none() {
        let _ = response.bytes().await;
        return Ok(None);
    }
    let length = response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value > 0);
    let _ = response.bytes().await;
    if let Some(length) = length {
        return Ok(Some(length));
    }
    let mut request = client.head(&segment.url);
    request = apply_request_headers(request, headers);
    let response = match request.send().await {
        Ok(response) => response,
        Err(_) => return Ok(None),
    };
    if !(200..=299).contains(&response.status().as_u16()) {
        let _ = response.bytes().await;
        return Ok(None);
    };
    let length = response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value > 0);
    let _ = response.bytes().await;
    Ok(length)
}

async fn repair_downloaded_file(result: &mut SegmentDownloadResult) -> Result<()> {
    if !result.image_header && !result.gzip_header {
        return Ok(());
    }
    let path = result.actual_file_path.clone();
    let image_header = result.image_header;
    let gzip_header = result.gzip_header;
    tokio::task::spawn_blocking(move || {
        if image_header {
            process_image_header(&path)?;
        }
        if gzip_header {
            decompress_gzip_file(&path)?;
        }
        Ok::<_, Error>(())
    })
    .await
    .map_err(|_| Error::http("segment repair worker failed"))??;
    Ok(())
}

async fn finalize_segment_file(
    result: &mut SegmentDownloadResult,
    final_path: &Path,
) -> Result<()> {
    if result.actual_file_path != final_path {
        if tokio::fs::try_exists(final_path).await? {
            tokio::fs::remove_file(final_path).await?;
        }
        tokio::fs::rename(&result.actual_file_path, final_path).await?;
        result.actual_file_path = final_path.to_path_buf();
    }
    Ok(())
}

fn push_stream_progress(
    events: &mut Vec<ProgressEvent>,
    stream_id: &str,
    speed: &Arc<Mutex<SpeedState>>,
    completed: u64,
    total: usize,
    emitter: &DownloadEventEmitter,
) -> Result<()> {
    let guard = speed
        .lock()
        .map_err(|_| Error::http("speed state lock failed"))?;
    let downloaded = guard.total_downloaded();
    let bytes_per_second = guard.display_bytes_per_second();
    let low_speed_count = guard.low_speed_count();
    drop(guard);
    emitter.push(
        events,
        ProgressEvent::StreamProgress(StreamProgress {
            stream_id: stream_id.to_string(),
            downloaded_bytes: downloaded,
            total_bytes: None,
            bytes_per_second,
            low_speed_count,
            completed_segments: completed,
            total_segments: Some(u64_from_usize(total)),
        }),
    )?;
    emitter.push(
        events,
        ProgressEvent::AggregateProgress(AggregateProgress {
            downloaded_bytes: downloaded,
            total_bytes: None,
            bytes_per_second,
        }),
    )?;
    Ok(())
}

fn push_segment_progress(
    events: &mut Vec<ProgressEvent>,
    stream_id: &str,
    segment_index: i64,
    speed: &Arc<Mutex<SpeedState>>,
    total_bytes: Option<u64>,
    emitter: &DownloadEventEmitter,
) -> Result<()> {
    let mut guard = speed
        .lock()
        .map_err(|_| Error::http("speed state lock failed"))?;
    guard.record_progress_tick();
    let downloaded = guard.total_downloaded();
    let bytes_per_second = guard.display_bytes_per_second();
    let low_speed_count = guard.low_speed_count();
    emitter.push(
        events,
        ProgressEvent::SegmentProgress(SegmentProgress {
            stream_id: stream_id.to_string(),
            segment_index: u64_from_i64(segment_index),
            downloaded_bytes: downloaded,
            total_bytes,
            bytes_per_second,
            low_speed_count,
            retry_attempt: 0,
        }),
    )?;
    Ok(())
}

fn add_speed(speed: &Arc<Mutex<SpeedState>>, size: u64) -> Result<()> {
    let mut guard = speed
        .lock()
        .map_err(|_| Error::http("speed state lock failed"))?;
    guard.add(size);
    Ok(())
}

fn should_cancel_for_low_speed(speed: &Arc<Mutex<SpeedState>>) -> Result<bool> {
    let mut guard = speed
        .lock()
        .map_err(|_| Error::http("speed state lock failed"))?;
    guard.record_progress_tick();
    if guard.should_stop() {
        guard.reset_low_speed_count();
        return Ok(true);
    }
    Ok(false)
}

async fn enforce_speed_limit(speed: &Arc<Mutex<SpeedState>>) -> Result<()> {
    loop {
        let should_wait = {
            let mut guard = speed
                .lock()
                .map_err(|_| Error::http("speed state lock failed"))?;
            guard.record_progress_tick();
            guard.speed_limit != u64::MAX && guard.downloaded_window > guard.speed_limit
        };
        if !should_wait {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

fn emit_stream_tasks(
    streams: &[Stream],
    events: &mut Vec<ProgressEvent>,
    emitter: &DownloadEventEmitter,
) -> Result<()> {
    for (task_id, stream) in streams.iter().enumerate() {
        emitter.push(
            events,
            ProgressEvent::StreamTaskCreated {
                stream_id: stream_identifier(stream, task_id),
                label: stream_short_label(stream),
            },
        )?;
    }
    Ok(())
}

fn media_segments(stream: &Stream) -> Vec<MediaSegment> {
    stream
        .playlist
        .as_ref()
        .map(|playlist| {
            playlist
                .media_parts
                .iter()
                .flat_map(|part| part.media_segments.iter().cloned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn stream_identifier(stream: &Stream, task_id: usize) -> String {
    if !stream.id.is_empty() {
        return stream.id.clone();
    }
    stream
        .group_id
        .clone()
        .unwrap_or_else(|| format!("stream-{task_id}"))
}

fn remove_tmp_extension(path: &Path) -> PathBuf {
    if path.extension().and_then(|value| value.to_str()) == Some("tmp") {
        return path.with_extension("");
    }
    path.to_path_buf()
}

fn decrypted_path(path: &Path) -> PathBuf {
    let dir = path.parent().unwrap_or_else(|| Path::new(""));
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{value}"))
        .unwrap_or_default();
    dir.join(format!("{stem}_dec{ext}"))
}

fn range_header(segment: &MediaSegment) -> Result<Option<String>> {
    match (segment.start_range, segment.stop_range()) {
        (Some(start), Some(stop)) => {
            validate_http_range_bound(start, "start")?;
            validate_http_range_bound(stop, "end")?;
            if stop < start {
                return Err(Error::http("byte range end is before start"));
            }
            Ok(Some(format!("bytes={start}-{stop}")))
        }
        (Some(start), None) => {
            validate_http_range_bound(start, "start")?;
            Ok(Some(format!("bytes={start}-")))
        }
        (None, Some(stop)) => {
            validate_http_range_bound(stop, "end")?;
            Ok(Some(format!("bytes=0-{stop}")))
        }
        (None, None) => Ok(None),
    }
}

fn validate_http_range_bound(value: i64, label: &str) -> Result<()> {
    if value < 0 {
        return Err(Error::http(format!("byte range {label} is invalid")));
    }
    Ok(())
}

fn is_gzip_header(bytes: &[u8]) -> bool {
    bytes.len() > 2 && bytes[0] == 0x1f && bytes[1] == 0x8b
}

fn is_image_header(bytes: &[u8]) -> bool {
    (bytes.len() > 3 && bytes[0..4] == [137, 80, 78, 71])
        || (bytes.len() > 3 && bytes[0..4] == [0x47, 0x49, 0x46, 0x38])
        || (bytes.len() > 10
            && bytes[0] == 0x42
            && bytes[1] == 0x4d
            && bytes[5] == 0
            && bytes[6] == 0
            && bytes[7] == 0
            && bytes[8] == 0)
        || (bytes.len() > 3 && bytes[0] == 0xff && bytes[1] == 0xd8 && bytes[2] == 0xff)
}

fn process_image_header(path: &Path) -> Result<()> {
    let mut data = std::fs::read(path)?;
    if data.len() >= 4 && data[0..4] == [137, 80, 78, 71] {
        data = strip_png_disguise(data);
    } else if data.len() >= 4 && data[0..4] == [0x47, 0x49, 0x46, 0x38] {
        if data.len() < 42 {
            return Err(Error::http("image header repair failed"));
        }
        data = data.split_off(42);
    } else if data.len() >= 2 && data[0] == 0x42 && data[1] == 0x4d {
        if data.len() < 9 {
            return Err(Error::http("image header repair failed"));
        }
        if data[5] == 0 && data[6] == 0 && data[7] == 0 && data[8] == 0 {
            if data.len() < 0x3e {
                return Err(Error::http("image header repair failed"));
            }
            data = data.split_off(0x3e);
        }
    } else if data.len() >= 3 && data[0] == 0xff && data[1] == 0xd8 && data[2] == 0xff {
        let skip = find_ts_sync_offset(&data).unwrap_or(0);
        data = data.split_off(skip);
    }
    std::fs::write(path, data)?;
    Ok(())
}

fn strip_png_disguise(mut data: Vec<u8>) -> Vec<u8> {
    for (length, a, b) in [
        (120, 118, 119),
        (6102, 6100, 6101),
        (69, 67, 68),
        (771, 769, 770),
    ] {
        if data.len() > length && data.get(a) == Some(&96) && data.get(b) == Some(&130) {
            return data.split_off(length);
        }
    }
    let skip = find_ts_sync_offset(&data).unwrap_or(0);
    data.split_off(skip)
}

fn find_ts_sync_offset(data: &[u8]) -> Option<usize> {
    let limit = data.len().saturating_sub(188 * 2 + 4);
    (4..limit).find(|index| {
        data.get(*index) == Some(&0x47)
            && data.get(index + 188) == Some(&0x47)
            && data.get(index + 188 + 188) == Some(&0x47)
    })
}

fn decompress_gzip_file(path: &Path) -> Result<()> {
    let temporary = path.with_extension("dezip_tmp");
    let decoded = (|| -> Result<()> {
        let input = File::open(path)?;
        let mut decoder = GzDecoder::new(input);
        let mut output = File::create(&temporary)?;
        std::io::copy(&mut decoder, &mut output)?;
        Ok(())
    })();

    if decoded.is_err() {
        if temporary.exists() {
            std::fs::remove_file(&temporary)?;
        }
        return Ok(());
    }

    std::fs::remove_file(path)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn file_uri_to_path(value: &str) -> PathBuf {
    if let Ok(url) = reqwest::Url::parse(value)
        && url.scheme() == "file"
        && let Ok(path) = url.to_file_path()
    {
        return path;
    }
    let stripped = value
        .trim_start_matches("file://")
        .trim_start_matches("file:");
    #[cfg(windows)]
    {
        let mut normalized = stripped.trim_start_matches('/');
        if let Some(rest) = normalized.strip_prefix("?/") {
            normalized = rest;
        }
        PathBuf::from(normalized)
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(stripped)
    }
}

async fn safe_delete_empty_dirs(path: &Path, touched: &mut Vec<PathBuf>) -> Result<()> {
    let mut current = Some(path.to_path_buf());
    while let Some(path) = current {
        if path.as_os_str().is_empty() || !tokio::fs::try_exists(&path).await? {
            break;
        }
        if !tokio::fs::metadata(&path).await?.is_dir() {
            break;
        }
        let mut entries = tokio::fs::read_dir(&path).await?;
        if entries.next_entry().await?.is_some() {
            break;
        }
        tokio::fs::remove_dir(&path).await?;
        touched.push(path.clone());
        current = path.parent().map(Path::to_path_buf);
    }
    Ok(())
}

fn resolve_redirect(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }
    let Some((origin, base_path)) = split_url_origin_and_path(base) else {
        return location.to_string();
    };
    let (location_path, suffix) = split_path_suffix(location);
    let joined = if location_path.starts_with('/') {
        normalize_url_path(location_path)
    } else {
        let base_dir = base_path
            .rsplit_once('/')
            .map(|(prefix, _)| format!("{prefix}/"))
            .unwrap_or_else(|| "/".to_string());
        normalize_url_path(&format!("{base_dir}{location_path}"))
    };
    format!("{origin}{joined}{suffix}")
}

fn split_url_origin_and_path(url: &str) -> Option<(&str, &str)> {
    let scheme_end = url.find("://")?;
    let after_scheme = scheme_end + 3;
    let rest = url.get(after_scheme..)?;
    let path_start = rest.find('/').map(|index| after_scheme + index);
    let index = path_start.unwrap_or(url.len());
    let origin = url.get(..index)?;
    let path = url.get(index..).unwrap_or("/");
    Some((origin, path))
}

fn split_path_suffix(value: &str) -> (&str, &str) {
    let query = value.find('?');
    let fragment = value.find('#');
    let split = match (query, fragment) {
        (Some(left), Some(right)) => left.min(right),
        (Some(index), None) | (None, Some(index)) => index,
        (None, None) => value.len(),
    };
    (&value[..split], &value[split..])
}

fn normalize_url_path(path: &str) -> String {
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                let _ = parts.pop();
            }
            value => parts.push(value),
        }
    }
    format!("/{}", parts.join("/"))
}

fn hex_to_bytes(value: &str) -> Result<Vec<u8>> {
    let value = value.trim();
    let value = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    if value.len() & 1 != 0 {
        return Err(Error::config("hex length must be even"));
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    let mut chars = value.chars();
    while let Some(high) = chars.next() {
        let low = chars
            .next()
            .ok_or_else(|| Error::config("hex length must be even"))?;
        let high = hex_value(high).ok_or_else(|| Error::config("hex is invalid"))?;
        let low = hex_value(low).ok_or_else(|| Error::config("hex is invalid"))?;
        bytes.push((high << 4) | low);
    }
    Ok(bytes)
}

fn hex_value(ch: char) -> Option<u8> {
    match ch {
        '0'..='9' => Some(ch as u8 - b'0'),
        'a'..='f' => Some(ch as u8 - b'a' + 10),
        'A'..='F' => Some(ch as u8 - b'A' + 10),
        _ => None,
    }
}

fn base64_decode(value: &str) -> Result<Vec<u8>> {
    crate::base64::decode_base64(value).map_err(Error::config)
}

fn u64_from_i64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

fn u64_from_usize(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
