//! Logging, metadata, progress formatting, and event collection helpers.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use crate::config::LogLevel;
use crate::decrypt::redact_secrets;
use crate::error::{Error, Result};
use crate::event::ProgressEvent;
use crate::manifest::{
    Choice, EncryptionInfo, EncryptionMethod, MediaPart, MediaSegment, MediaType, MssData,
    Playlist, RoleType, Stream,
};

/// Planned log file behavior.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogFilePlan {
    /// Whether file logging is enabled.
    pub enabled: bool,
    /// Resolved file path when enabled.
    pub path: Option<PathBuf>,
}

/// Logging configuration used by CLI and API adapters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogPlanConfig {
    /// Active log level.
    pub level: LogLevel,
    /// Disable file logging.
    pub no_log: bool,
    /// User-requested log file path.
    pub log_file_path: Option<PathBuf>,
    /// Directory used when no file path is supplied.
    pub default_log_dir: PathBuf,
    /// Deterministic suffix for tests or caller-provided timestamps.
    pub suffix: String,
}

/// Result of a non-blocking release update check.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UpdateCheckResult {
    /// Latest version tag when a release redirect exposed one.
    pub latest_version: Option<String>,
    /// Whether `latest_version` differs from the current package version.
    pub update_available: bool,
}

/// Injectable update-check transport used by CLI adapters and tests.
pub trait UpdateCheckClient {
    /// Returns the redirect location for a release endpoint, if one exists.
    fn redirect_location<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>>> + Send + 'a>>;
}

/// HTTP update-check transport used by the CLI compatibility path.
#[derive(Clone, Debug)]
pub struct UpdateCheckHttpClient {
    timeout: Duration,
}

/// Default endpoint used by the compatibility update check.
pub const DEFAULT_UPDATE_CHECK_URL: &str = "https://github.com/bakgio/haki-dl/releases/latest";

impl Default for UpdateCheckHttpClient {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
        }
    }
}

impl UpdateCheckHttpClient {
    /// Creates an update-check client.
    pub fn new() -> Self {
        Self::default()
    }

    /// Overrides the request timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl UpdateCheckClient for UpdateCheckHttpClient {
    fn redirect_location<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>>> + Send + 'a>> {
        Box::pin(async move {
            let client = reqwest::Client::builder()
                .timeout(self.timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|error| Error::http(error.to_string()))?;
            let response = client
                .get(url)
                .send()
                .await
                .map_err(|error| Error::http(error.to_string()))?;
            if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                return Ok(response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string));
            }
            Ok(None)
        })
    }
}

/// Terminal-independent progress summary.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProgressSummary {
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub bytes_per_second: u64,
    pub completed_segments: u64,
    pub total_segments: Option<u64>,
    pub refreshed_duration: Option<Duration>,
    pub recorded_duration: Option<Duration>,
    pub warnings: Vec<String>,
    pub outputs: Vec<PathBuf>,
    pub finished: Option<bool>,
}

/// In-memory event sink for API users and tests.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProgressEventCollector {
    events: Vec<ProgressEvent>,
}

impl ProgressEventCollector {
    /// Creates an empty collector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Emits an event after redacting secrets in diagnostic text.
    pub fn emit(&mut self, event: ProgressEvent) {
        self.events.push(redact_progress_event(event));
    }

    /// Returns collected events.
    pub fn events(&self) -> &[ProgressEvent] {
        &self.events
    }

    /// Builds a terminal-independent summary from collected events.
    pub fn summary(&self) -> ProgressSummary {
        summarize_events(&self.events)
    }
}

/// Returns whether a message at `message_level` should be emitted.
pub fn should_log(active: LogLevel, message_level: LogLevel) -> bool {
    if active == LogLevel::Off {
        return false;
    }
    rank(message_level) >= rank(active)
}

/// Resolves the log file path, including collision suffixing.
pub async fn plan_log_file(config: &LogPlanConfig) -> Result<LogFilePlan> {
    if config.no_log {
        return Ok(LogFilePlan {
            enabled: false,
            path: None,
        });
    }
    let mut path = match &config.log_file_path {
        Some(path) => path.clone(),
        None => config
            .default_log_dir
            .join(format!("{}.log", config.suffix)),
    };
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if tokio::fs::try_exists(&path).await? {
        let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("log")
            .to_string();
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("log")
            .to_string();
        let mut index = 1_u32;
        loop {
            let candidate = parent.join(format!("{stem}-{index}.{extension}"));
            if !tokio::fs::try_exists(&candidate).await? {
                path = candidate;
                break;
            }
            index += 1;
        }
    }
    Ok(LogFilePlan {
        enabled: true,
        path: Some(path),
    })
}

/// Creates the session log file and writes its startup header.
pub async fn initialize_log_file(
    config: &LogPlanConfig,
    started_at: &str,
    command_line: Option<&str>,
) -> Result<LogFilePlan> {
    let plan = plan_log_file(config).await?;
    let Some(path) = &plan.path else {
        return Ok(plan);
    };
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut text = String::new();
    text.push_str("LOG ");
    text.push_str(started_at.split_whitespace().next().unwrap_or(started_at));
    text.push('\n');
    text.push_str("Save Path: ");
    text.push_str(&parent.display().to_string());
    text.push('\n');
    text.push_str("Task Start: ");
    text.push_str(started_at);
    text.push('\n');
    if let Some(command_line) = command_line {
        text.push_str("Task CommandLine: ");
        text.push_str(command_line);
        text.push('\n');
    }
    text.push('\n');
    tokio::fs::write(path, text).await?;
    Ok(plan)
}

/// Appends one plain diagnostic line to a planned log file.
pub async fn append_log_file(plan: &LogFilePlan, line: &str) -> Result<()> {
    let Some(path) = &plan.path else {
        return Ok(());
    };
    use tokio::io::AsyncWriteExt;

    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .await?;
    file.write_all(line.as_bytes()).await?;
    file.write_all(b"\n").await?;
    Ok(())
}

/// Checks a release endpoint with an injected client.
pub async fn check_update_with_client(
    client: &impl UpdateCheckClient,
    endpoint: &str,
    current_version: &str,
) -> Result<UpdateCheckResult> {
    let Some(location) = client.redirect_location(endpoint).await? else {
        return Ok(UpdateCheckResult::default());
    };
    let latest_version = latest_version_from_release_redirect(&location);
    let update_available = latest_version
        .as_deref()
        .is_some_and(|latest| normalize_version(latest) != normalize_version(current_version));
    Ok(UpdateCheckResult {
        latest_version,
        update_available,
    })
}

/// Extracts a release tag from a redirect URL.
pub fn latest_version_from_release_redirect(location: &str) -> Option<String> {
    let marker = "/tag/";
    let index = location.find(marker)?;
    let tag = location.get(index + marker.len()..)?.trim();
    if tag.is_empty() || tag.starts_with("http") {
        return None;
    }
    Some(tag.to_string())
}

/// Starts the compatibility update check in the background.
pub fn spawn_update_check_if_enabled(disabled: bool) -> bool {
    if disabled {
        return false;
    }
    tokio::spawn(async {
        let client = UpdateCheckHttpClient::new();
        let _ =
            check_update_with_client(&client, DEFAULT_UPDATE_CHECK_URL, env!("CARGO_PKG_VERSION"))
                .await;
    });
    true
}

/// Formats a byte count for progress displays.
pub fn format_file_size(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = units[0];
    for candidate in units.iter().skip(1) {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = candidate;
    }
    if unit == "B" {
        format!("{bytes}B")
    } else {
        format!("{value:.2}{unit}")
    }
}

fn normalize_version(version: &str) -> &str {
    version.trim().trim_start_matches('v')
}

/// Formats a duration as `HH:MM:SS`.
pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds / 60) % 60;
    let seconds = seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

/// Redacts secrets from warning and progress diagnostic text.
pub fn redact_progress_event(event: ProgressEvent) -> ProgressEvent {
    match event {
        ProgressEvent::Log { level, message } => ProgressEvent::Log {
            level,
            message: redact_secrets(&message),
        },
        ProgressEvent::ExtraLog { message } => ProgressEvent::ExtraLog {
            message: redact_secrets(&message),
        },
        ProgressEvent::MediaInfo { stream_id, lines } => ProgressEvent::MediaInfo {
            stream_id,
            lines: lines
                .into_iter()
                .map(|line| redact_secrets(&line))
                .collect(),
        },
        ProgressEvent::Warning { message } => ProgressEvent::Warning {
            message: redact_secrets(&message),
        },
        ProgressEvent::DecryptProgress { stream_id, message } => ProgressEvent::DecryptProgress {
            stream_id,
            message: redact_secrets(&message),
        },
        ProgressEvent::MergeProgress { stream_id, message } => ProgressEvent::MergeProgress {
            stream_id,
            message: redact_secrets(&message),
        },
        ProgressEvent::SubtitleProgress { stream_id, message } => ProgressEvent::SubtitleProgress {
            stream_id,
            message: redact_secrets(&message),
        },
        ProgressEvent::MuxProgress { message } => ProgressEvent::MuxProgress {
            message: redact_secrets(&message),
        },
        ProgressEvent::ExternalToolOutput { message } => ProgressEvent::ExternalToolOutput {
            message: redact_secrets(&message),
        },
        ProgressEvent::ConsoleLine { message } => ProgressEvent::ConsoleLine {
            message: redact_secrets(&message),
        },
        other => other,
    }
}

/// Summarizes events for callers that do not render terminal progress columns.
pub fn summarize_events(events: &[ProgressEvent]) -> ProgressSummary {
    let mut summary = ProgressSummary::default();
    for event in events {
        match event {
            ProgressEvent::AggregateProgress(progress) => {
                summary.downloaded_bytes = progress.downloaded_bytes;
                summary.total_bytes = progress.total_bytes;
                summary.bytes_per_second = progress.bytes_per_second;
            }
            ProgressEvent::StreamProgress(progress) => {
                summary.downloaded_bytes = summary.downloaded_bytes.max(progress.downloaded_bytes);
                summary.completed_segments =
                    summary.completed_segments.max(progress.completed_segments);
                summary.total_segments = progress.total_segments;
                summary.bytes_per_second = summary.bytes_per_second.max(progress.bytes_per_second);
            }
            ProgressEvent::LiveRefresh {
                refreshed_duration,
                recorded_duration,
                ..
            } => {
                summary.refreshed_duration = Some(*refreshed_duration);
                summary.recorded_duration = Some(*recorded_duration);
            }
            ProgressEvent::Warning { message } => summary.warnings.push(message.clone()),
            ProgressEvent::ExternalToolOutput { message } => summary.warnings.push(message.clone()),
            ProgressEvent::ConsoleLine { message } => summary.warnings.push(message.clone()),
            ProgressEvent::OutputArtifact(artifact) => summary.outputs.push(artifact.path.clone()),
            ProgressEvent::Finished { success } => summary.finished = Some(*success),
            _ => {}
        }
    }
    summary
}

/// Serializes stream metadata into a stable JSON document.
pub fn streams_metadata_json(streams: &[Stream]) -> String {
    if streams.is_empty() {
        return "[]".to_string();
    }
    let mut output = String::from("[\n");
    for (index, stream) in streams.iter().enumerate() {
        if index > 0 {
            output.push_str(",\n");
        }
        output.push_str("  ");
        output.push_str(&stream_metadata_json(stream, 2));
    }
    output.push_str("\n]");
    n_json_line_endings(output)
}

/// Writes raw and selected stream metadata JSON files.
pub async fn write_metadata_jsons(
    directory: &Path,
    raw_streams: &[Stream],
    selected_streams: &[Stream],
) -> Result<Vec<PathBuf>> {
    tokio::fs::create_dir_all(directory).await?;
    let raw = directory.join("meta.json");
    let selected = directory.join("meta_selected.json");
    write_n_utf8_text_file(&raw, &streams_metadata_json(raw_streams)).await?;
    write_n_utf8_text_file(&selected, &streams_metadata_json(selected_streams)).await?;
    Ok(vec![raw, selected])
}

fn stream_metadata_json(stream: &Stream, indent: usize) -> String {
    let mut fields = Vec::new();
    push_option_enum(
        &mut fields,
        "MediaType",
        stream.media_type.map(media_type_text),
    );
    push_option_string(&mut fields, "GroupId", stream.group_id.as_deref());
    push_option_string(&mut fields, "Language", stream.language.as_deref());
    push_option_string(&mut fields, "Name", stream.name.as_deref());
    push_option_enum(&mut fields, "Default", stream.default.map(choice_text));
    push_option_number(&mut fields, "SkippedDuration", stream.skipped_duration);
    if let Some(data) = &stream.mss_data {
        fields.push(("MSSData".to_string(), mss_data_json(data, indent + 2)));
    }
    push_option_number(&mut fields, "Bandwidth", stream.bandwidth);
    push_option_string(&mut fields, "Codecs", stream.codecs.as_deref());
    push_option_string(&mut fields, "Resolution", stream.resolution.as_deref());
    push_option_number(&mut fields, "FrameRate", stream.frame_rate);
    push_option_string(&mut fields, "Channels", stream.channels.as_deref());
    push_option_string(&mut fields, "Extension", stream.extension.as_deref());
    if let Some(role) = stream.role {
        fields.push(("Role".to_string(), json_string(&role_text(role))));
    }
    push_option_string(&mut fields, "VideoRange", stream.video_range.as_deref());
    push_option_string(
        &mut fields,
        "Characteristics",
        stream.characteristics.as_deref(),
    );
    push_option_string(&mut fields, "PublishTime", stream.publish_time.as_deref());
    push_option_string(&mut fields, "AudioId", stream.audio_id.as_deref());
    push_option_string(&mut fields, "VideoId", stream.video_id.as_deref());
    push_option_string(&mut fields, "SubtitleId", stream.subtitle_id.as_deref());
    push_option_string(&mut fields, "PeriodId", stream.period_id.as_deref());
    fields.push((
        "Url".to_string(),
        json_string(&metadata_stream_url(&stream.url)),
    ));
    fields.push((
        "OriginalUrl".to_string(),
        json_string(&metadata_stream_url(&stream.original_url)),
    ));
    if let Some(playlist) = &stream.playlist {
        fields.push(("Playlist".to_string(), playlist_json(playlist, indent + 2)));
    }
    fields.push((
        "SegmentsCount".to_string(),
        stream.segments_count().to_string(),
    ));
    object_json(&fields, indent)
}

fn playlist_json(playlist: &Playlist, indent: usize) -> String {
    let mut fields = vec![
        ("Url".to_string(), json_string("")),
        ("IsLive".to_string(), playlist.is_live.to_string()),
        (
            "RefreshIntervalMs".to_string(),
            number_json(playlist.refresh_interval_ms),
        ),
        (
            "TotalDuration".to_string(),
            number_json(playlist.total_duration()),
        ),
    ];
    push_option_number(&mut fields, "TargetDuration", playlist.target_duration);
    if let Some(init) = &playlist.media_init {
        fields.push(("MediaInit".to_string(), segment_json(init, indent + 2)));
    }
    fields.push((
        "MediaParts".to_string(),
        array_json(
            &playlist
                .media_parts
                .iter()
                .map(|part| media_part_json(part, indent + 4))
                .collect::<Vec<_>>(),
            indent + 2,
        ),
    ));
    object_json(&fields, indent)
}

fn media_part_json(part: &MediaPart, indent: usize) -> String {
    object_json(
        &[(
            "MediaSegments".to_string(),
            array_json(
                &part
                    .media_segments
                    .iter()
                    .map(|segment| segment_json(segment, indent + 4))
                    .collect::<Vec<_>>(),
                indent + 2,
            ),
        )],
        indent,
    )
}

fn segment_json(segment: &MediaSegment, indent: usize) -> String {
    let mut fields = vec![
        ("Index".to_string(), segment.index.to_string()),
        ("Duration".to_string(), number_json(segment.duration)),
    ];
    push_option_string(&mut fields, "Title", segment.title.as_deref());
    push_option_string(
        &mut fields,
        "DateTime",
        segment.program_date_time.as_deref(),
    );
    push_option_number(&mut fields, "StartRange", segment.start_range);
    push_option_number(&mut fields, "StopRange", segment.stop_range());
    push_option_number(&mut fields, "ExpectLength", segment.expected_length);
    fields.push((
        "EncryptInfo".to_string(),
        encryption_json(&segment.encryption, indent + 2),
    ));
    fields.push((
        "IsEncrypted".to_string(),
        segment.is_encrypted().to_string(),
    ));
    fields.push((
        "Url".to_string(),
        json_string(&metadata_segment_url(&segment.url)),
    ));
    push_option_string(&mut fields, "NameFromVar", segment.name_from_var.as_deref());
    object_json(&fields, indent)
}

fn encryption_json(encryption: &EncryptionInfo, indent: usize) -> String {
    let mut fields = vec![(
        "Method".to_string(),
        json_string(encryption_method_text(encryption.method)),
    )];
    push_option_bytes(&mut fields, "Key", encryption.key.as_deref());
    push_option_bytes(&mut fields, "IV", encryption.iv.as_deref());
    object_json(&fields, indent)
}

fn mss_data_json(data: &MssData, indent: usize) -> String {
    object_json(
        &[
            ("FourCC".to_string(), json_string(&data.four_cc)),
            (
                "CodecPrivateData".to_string(),
                json_string(&data.codec_private_data),
            ),
            ("Type".to_string(), json_string(&data.stream_type)),
            ("Timesacle".to_string(), data.timescale.to_string()),
            ("SamplingRate".to_string(), data.sampling_rate.to_string()),
            ("Channels".to_string(), data.channels.to_string()),
            (
                "BitsPerSample".to_string(),
                data.bits_per_sample.to_string(),
            ),
            (
                "NalUnitLengthField".to_string(),
                data.nal_unit_length_field.to_string(),
            ),
            ("Duration".to_string(), data.duration.to_string()),
            ("IsProtection".to_string(), data.is_protection.to_string()),
            (
                "ProtectionSystemID".to_string(),
                json_string(&data.protection_system_id),
            ),
            (
                "ProtectionData".to_string(),
                json_string(&data.protection_data),
            ),
        ],
        indent,
    )
}

fn metadata_stream_url(value: &str) -> String {
    value.replace(' ', "%20")
}

fn metadata_segment_url(value: &str) -> String {
    value.replace("%20", " ")
}

fn n_json_line_endings(value: String) -> String {
    if cfg!(windows) {
        value.replace('\n', "\r\n")
    } else {
        value
    }
}

async fn write_n_utf8_text_file(path: &Path, text: &str) -> std::io::Result<()> {
    let mut bytes = Vec::with_capacity(3 + text.len());
    bytes.extend_from_slice(&[0xef, 0xbb, 0xbf]);
    bytes.extend_from_slice(text.as_bytes());
    tokio::fs::write(path, bytes).await
}

fn object_json(fields: &[(String, String)], indent: usize) -> String {
    if fields.is_empty() {
        return "{}".to_string();
    }
    let current = " ".repeat(indent);
    let child = " ".repeat(indent + 2);
    let mut output = String::from("{\n");
    for (index, (name, value)) in fields.iter().enumerate() {
        if index > 0 {
            output.push_str(",\n");
        }
        output.push_str(&child);
        output.push_str(&json_string(name));
        output.push_str(": ");
        output.push_str(value);
    }
    output.push('\n');
    output.push_str(&current);
    output.push('}');
    output
}

fn array_json(values: &[String], indent: usize) -> String {
    if values.is_empty() {
        return "[]".to_string();
    }
    let current = " ".repeat(indent);
    let child = " ".repeat(indent + 2);
    let mut output = String::from("[\n");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            output.push_str(",\n");
        }
        output.push_str(&child);
        output.push_str(value);
    }
    output.push('\n');
    output.push_str(&current);
    output.push(']');
    output
}

fn push_option_string(fields: &mut Vec<(String, String)>, name: &str, value: Option<&str>) {
    if let Some(value) = value {
        fields.push((name.to_string(), json_string(value)));
    }
}

fn push_option_enum(fields: &mut Vec<(String, String)>, name: &str, value: Option<&str>) {
    if let Some(value) = value {
        fields.push((name.to_string(), json_string(value)));
    }
}

fn push_option_number<T: ToString>(
    fields: &mut Vec<(String, String)>,
    name: &str,
    value: Option<T>,
) {
    if let Some(value) = value {
        fields.push((name.to_string(), value.to_string()));
    }
}

fn push_option_bytes(fields: &mut Vec<(String, String)>, name: &str, value: Option<&[u8]>) {
    if let Some(value) = value {
        fields.push((name.to_string(), json_string(&base64_encode(value))));
    }
}

fn media_type_text(value: MediaType) -> &'static str {
    match value {
        MediaType::Audio => "AUDIO",
        MediaType::Video => "VIDEO",
        MediaType::Subtitles => "SUBTITLES",
        MediaType::ClosedCaptions => "CLOSED_CAPTIONS",
    }
}

fn role_text(value: RoleType) -> String {
    match value {
        RoleType::Subtitle => "Subtitle".to_string(),
        RoleType::Main => "Main".to_string(),
        RoleType::Alternate => "Alternate".to_string(),
        RoleType::Supplementary => "Supplementary".to_string(),
        RoleType::Commentary => "Commentary".to_string(),
        RoleType::Dub => "Dub".to_string(),
        RoleType::Description => "Description".to_string(),
        RoleType::Sign => "Sign".to_string(),
        RoleType::Metadata => "Metadata".to_string(),
        RoleType::ForcedSubtitle => "ForcedSubtitle".to_string(),
        RoleType::Numeric(value) => value.to_string(),
    }
}

fn choice_text(value: Choice) -> &'static str {
    match value {
        Choice::No => "NO",
        Choice::Yes => "YES",
    }
}

fn encryption_method_text(value: EncryptionMethod) -> &'static str {
    match value {
        EncryptionMethod::None => "NONE",
        EncryptionMethod::Aes128 => "AES_128",
        EncryptionMethod::Aes128Ecb => "AES_128_ECB",
        EncryptionMethod::SampleAes => "SAMPLE_AES",
        EncryptionMethod::SampleAesCtr => "SAMPLE_AES_CTR",
        EncryptionMethod::Cenc => "CENC",
        EncryptionMethod::Chacha20 => "CHACHA20",
        EncryptionMethod::Unknown => "UNKNOWN",
    }
}

fn number_json(value: f64) -> String {
    value.to_string()
}

fn json_string(value: &str) -> String {
    format!("\"{}\"", escape_json(value))
}

fn escape_json(value: &str) -> String {
    let mut output = String::new();
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            _ => output.push(ch),
        }
    }
    output
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let triple = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        output.push(char::from(TABLE[((triple >> 18) & 0x3f) as usize]));
        output.push(char::from(TABLE[((triple >> 12) & 0x3f) as usize]));
        if chunk.len() > 1 {
            output.push(char::from(TABLE[((triple >> 6) & 0x3f) as usize]));
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(char::from(TABLE[(triple & 0x3f) as usize]));
        } else {
            output.push('=');
        }
    }
    output
}

fn rank(level: LogLevel) -> u8 {
    match level {
        LogLevel::Debug => 0,
        LogLevel::Info => 1,
        LogLevel::Warn => 2,
        LogLevel::Error => 3,
        LogLevel::Off => 4,
    }
}
