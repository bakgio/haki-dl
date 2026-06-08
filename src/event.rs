//! Progress and lifecycle event schema.

use std::path::PathBuf;
use std::time::Duration;

use crate::config::LogLevel;
use crate::mux::OutputArtifact;
use crate::progress::{AggregateProgress, SegmentProgress, StreamProgress};

/// Events emitted by download sessions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProgressEvent {
    /// Session planning started.
    PlanningStarted,
    /// Structured log line emitted by the session.
    Log { level: LogLevel, message: String },
    /// Log-file-only diagnostic text.
    ExtraLog { message: String },
    /// Media information read from a downloaded init or first media segment.
    MediaInfo {
        stream_id: Option<String>,
        lines: Vec<String>,
    },
    /// A log file was created for this session.
    LogFileCreated { path: PathBuf },
    /// A compatibility update check was scheduled.
    UpdateCheckStarted,
    /// The session is waiting for a delayed start timestamp.
    TaskStartDelay { until: String, remaining: Duration },
    /// Manifest loading started.
    ManifestLoading,
    /// Manifest was parsed.
    ManifestParsed { stream_count: usize },
    /// A stream was selected.
    StreamSelected { stream_id: String },
    /// A download progress task was created for a selected stream.
    StreamTaskCreated { stream_id: String, label: String },
    /// A stream-level progress update.
    StreamProgress(StreamProgress),
    /// A segment was queued.
    SegmentQueued {
        stream_id: String,
        segment_index: u64,
    },
    /// A segment started.
    SegmentStarted {
        stream_id: String,
        segment_index: u64,
    },
    /// A segment progress update.
    SegmentProgress(SegmentProgress),
    /// A segment retry was scheduled.
    SegmentRetry {
        stream_id: String,
        segment_index: u64,
        retry_attempt: u32,
    },
    /// A segment finished.
    SegmentFinished {
        stream_id: String,
        segment_index: u64,
    },
    /// Aggregate progress update.
    AggregateProgress(AggregateProgress),
    /// Decryption progress message.
    DecryptProgress {
        stream_id: Option<String>,
        message: String,
    },
    /// Merge progress message.
    MergeProgress {
        stream_id: Option<String>,
        message: String,
    },
    /// Subtitle repair/extraction progress message.
    SubtitleProgress {
        stream_id: Option<String>,
        message: String,
    },
    /// Final mux progress message.
    MuxProgress { message: String },
    /// External media tool stderr or command text.
    ExternalToolOutput { message: String },
    /// Raw terminal line emitted without a log prefix.
    ConsoleLine { message: String },
    /// Live playlist refresh state.
    LiveRefresh {
        /// Stream identifier for the live progress task when known.
        stream_id: Option<String>,
        /// Human-readable stream label for terminal progress when known.
        label: Option<String>,
        refreshed_duration: Duration,
        recorded_duration: Duration,
        /// Number of segments already recorded for this live task.
        recorded_segments: u64,
        /// Number of segments discovered for this live task.
        total_segments: u64,
        /// Whether the task is waiting for the next live refresh.
        is_waiting: bool,
        /// Direct HTTP live recordings have a recording-size column.
        recorded_size: Option<u64>,
    },
    /// Service metadata discovered in a direct HTTP MPEG-TS recording.
    LiveServiceInfo {
        program_id: String,
        service_provider: Option<String>,
        service_name: Option<String>,
    },
    /// A warning that should be visible to CLI and API consumers.
    Warning { message: String },
    /// An output artifact was produced.
    OutputArtifact(OutputArtifact),
    /// Temporary cleanup touched a path.
    Cleanup { path: PathBuf },
    /// Cancellation was observed.
    Cancelled,
    /// Session finished.
    Finished { success: bool },
}
