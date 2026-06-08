//! Request options and compatibility configuration.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

use crate::error::{Error, Result};
use crate::manifest::RoleType;
use crate::mux::{MuxFormat, MuxImport};

/// Compatibility mode applied during request planning.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CompatibilityProfile {
    /// Preserve CLI-visible compatibility behavior.
    #[default]
    CliCompatible,
    /// Use safer API defaults when explicitly selected by callers.
    ApiSafe,
}

/// Console verbosity.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LogLevel {
    Debug,
    #[default]
    Info,
    Warn,
    Error,
    Off,
}

/// Decryption backend requested by the user.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DecryptionEngine {
    #[default]
    Mp4forge,
    Mp4decrypt,
    ShakaPackager,
    Ffmpeg,
}

/// Subtitle text output format.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SubtitleFormat {
    /// SubRip output.
    #[default]
    Srt,
    /// WebVTT output.
    Vtt,
}

/// Language for user-facing CLI text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiLanguage {
    /// English CLI output.
    EnUs,
}

/// HLS encryption method override.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HlsMethod {
    None,
    Aes128,
    Aes128Ecb,
    Cenc,
    SampleAes,
    SampleAesCtr,
    Chacha20,
    Unknown,
}

/// Final mux backend.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MuxerKind {
    /// External ffmpeg process.
    #[default]
    Ffmpeg,
    /// External mkvmerge process.
    Mkvmerge,
    /// Optional in-process MP4 backend.
    Mp4forge,
}

/// Segment or time clipping range supplied through `--custom-range`.
#[derive(Clone, Debug, PartialEq)]
pub enum CustomRange {
    Segment {
        input: String,
        start_index: i64,
        end_index: i64,
    },
    Time {
        input: String,
        start_seconds: f64,
        end_seconds: f64,
    },
}

/// A delayed start timestamp supplied as `yyyyMMddHHmmss`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskStartAt {
    raw: String,
}

impl TaskStartAt {
    /// Creates a delayed-start value from the raw `yyyyMMddHHmmss` form.
    pub fn new(raw: String) -> Self {
        Self { raw }
    }

    /// Returns the raw `yyyyMMddHHmmss` form.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Calculates how long remains until this timestamp using the current local clock.
    pub fn duration_until_now(&self) -> Result<Option<Duration>> {
        self.duration_until_datetime(current_local_datetime())
    }

    /// Calculates how long remains until this timestamp using another raw timestamp.
    pub fn duration_until_raw(&self, now_raw: &str) -> Result<Option<Duration>> {
        self.duration_until_datetime(parse_task_start_datetime(now_raw)?)
    }

    fn duration_until_datetime(&self, now: PrimitiveDateTime) -> Result<Option<Duration>> {
        let target = parse_task_start_datetime(&self.raw)?;
        if target <= now {
            return Ok(None);
        }
        Ok(Some((target - now).unsigned_abs()))
    }
}

fn current_local_datetime() -> PrimitiveDateTime {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    PrimitiveDateTime::new(now.date(), now.time())
}

fn parse_task_start_datetime(value: &str) -> Result<PrimitiveDateTime> {
    if value.len() != 14 || !value.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(Error::config("task start time must use yyyyMMddHHmmss"));
    }
    let year = parse_task_start_part(value, 0, 4)?;
    let month = parse_task_start_part(value, 4, 6)?;
    let day = parse_task_start_part(value, 6, 8)?;
    let hour = parse_task_start_part(value, 8, 10)?;
    let minute = parse_task_start_part(value, 10, 12)?;
    let second = parse_task_start_part(value, 12, 14)?;
    let month = Month::try_from(
        u8::try_from(month)
            .map_err(|_| Error::config("task start time contains an out-of-range month"))?,
    )
    .map_err(|_| Error::config("task start time contains an out-of-range month"))?;
    let date = Date::from_calendar_date(
        i32::try_from(year)
            .map_err(|_| Error::config("task start time contains an out-of-range year"))?,
        month,
        u8::try_from(day)
            .map_err(|_| Error::config("task start time contains an out-of-range day"))?,
    )
    .map_err(|_| Error::config("task start time contains an out-of-range date"))?;
    let time = Time::from_hms(
        u8::try_from(hour)
            .map_err(|_| Error::config("task start time contains an out-of-range hour"))?,
        u8::try_from(minute)
            .map_err(|_| Error::config("task start time contains an out-of-range minute"))?,
        u8::try_from(second)
            .map_err(|_| Error::config("task start time contains an out-of-range second"))?,
    )
    .map_err(|_| Error::config("task start time contains an out-of-range time"))?;
    Ok(PrimitiveDateTime::new(date, time))
}

fn parse_task_start_part(value: &str, start: usize, end: usize) -> Result<u32> {
    value
        .get(start..end)
        .ok_or_else(|| Error::config("task start time field is invalid"))?
        .parse::<u32>()
        .map_err(|_| Error::config("task start time field is invalid"))
}

/// Canonicalized custom content key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CustomKey {
    Track { track_id: u32, key_hex: String },
    Kid { kid_hex: String, key_hex: String },
    Key { key_hex: String },
}

/// Stream selection/drop filter. Pattern fields intentionally stay as strings
/// so API callers can build values before a regex engine is chosen.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamFilter {
    pub for_choice: String,
    pub id: Option<String>,
    pub language: Option<String>,
    pub name: Option<String>,
    pub codecs: Option<String>,
    pub resolution: Option<String>,
    pub frame_rate: Option<String>,
    pub channels: Option<String>,
    pub range: Option<String>,
    pub url: Option<String>,
    pub segment_count_min: Option<i64>,
    pub segment_count_max: Option<i64>,
    pub playlist_duration_min: Option<f64>,
    pub playlist_duration_max: Option<f64>,
    pub bandwidth_min: Option<i64>,
    pub bandwidth_max: Option<i64>,
    pub role: Option<RoleType>,
}

/// Options for final mux-after-done behavior.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MuxAfterDoneOptions {
    /// Requested output format.
    pub format: MuxFormat,
    /// Requested mux backend.
    pub muxer: MuxerKind,
    /// Optional process-backed fallback used only when an explicit mp4forge mux fails.
    ///
    /// Currently only [`MuxerKind::Ffmpeg`] is accepted because mp4forge
    /// mux-after-done is restricted to MP4 output.
    pub fallback_muxer: Option<MuxerKind>,
    /// Optional backend binary path for process-backed muxers.
    pub bin_path: Option<PathBuf>,
    /// Keep intermediate files after mux succeeds.
    pub keep: bool,
    /// Exclude subtitle artifacts from the final mux.
    pub skip_sub: bool,
}

impl Default for MuxAfterDoneOptions {
    fn default() -> Self {
        Self {
            format: MuxFormat::Mp4,
            muxer: MuxerKind::Ffmpeg,
            fallback_muxer: None,
            bin_path: None,
            keep: false,
            skip_sub: false,
        }
    }
}

/// Typed download options using canonical snake_case option names.
#[derive(Clone, Debug)]
pub struct DownloadOptions {
    /// Active compatibility profile.
    pub compatibility_profile: CompatibilityProfile,
    pub log_level: LogLevel,
    /// Temporary directory root.
    pub tmp_dir: Option<PathBuf>,
    /// Save directory.
    pub save_dir: Option<PathBuf>,
    /// Save name without extension.
    pub save_name: Option<String>,
    /// Save pattern.
    pub save_pattern: Option<String>,
    pub log_file_path: Option<PathBuf>,
    pub ui_language: Option<UiLanguage>,
    pub urlprocessor_args: Option<String>,
    /// Base URL override.
    pub base_url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub custom_proxy: Option<String>,
    pub use_system_proxy: bool,
    /// Allow invalid TLS certificates for compatibility with problematic sources.
    pub allow_insecure_tls: bool,
    pub append_url_params: bool,
    pub custom_range: Option<CustomRange>,
    pub keys: Vec<CustomKey>,
    pub key_text_file: Option<PathBuf>,
    pub decryption_engine: DecryptionEngine,
    pub decryption_binary_path: Option<PathBuf>,
    pub mp4_real_time_decryption: bool,
    pub use_shaka_packager: bool,
    pub custom_hls_method: Option<HlsMethod>,
    pub custom_hls_key: Option<Vec<u8>>,
    pub custom_hls_iv: Option<Vec<u8>>,
    pub allow_hls_multi_ext_map: bool,
    /// Maximum concurrent segment workers per stream.
    pub thread_count: i32,
    /// Segment retry count.
    pub download_retry_count: i32,
    /// HTTP request timeout.
    pub http_request_timeout: Duration,
    pub max_speed: Option<u64>,
    pub auto_select: bool,
    /// Download selected streams concurrently.
    pub concurrent_download: bool,
    /// Select only subtitles.
    pub sub_only: bool,
    pub select_video: Vec<StreamFilter>,
    pub select_audio: Vec<StreamFilter>,
    pub select_subtitle: Vec<StreamFilter>,
    pub drop_video: Vec<StreamFilter>,
    pub drop_audio: Vec<StreamFilter>,
    pub drop_subtitle: Vec<StreamFilter>,
    pub ad_keywords: Vec<String>,
    /// Skip post-download merge.
    pub skip_merge: bool,
    /// Skip download after metadata and selection planning.
    pub skip_download: bool,
    /// Check segment count.
    pub check_segments_count: bool,
    /// Use binary concatenation for per-stream merge.
    pub binary_merge: bool,
    /// Use ffmpeg concat demuxer for ffmpeg merge.
    pub use_ffmpeg_concat_demuxer: bool,
    /// Delete temporary files after success.
    pub del_after_done: bool,
    /// Omit date metadata where supported.
    pub no_date_info: bool,
    /// Disable log file output.
    pub no_log: bool,
    /// Write metadata JSON sidecars.
    pub write_meta_json: bool,
    /// Repair or extract subtitles when possible.
    pub auto_subtitle_fix: bool,
    /// Subtitle output format.
    pub sub_format: SubtitleFormat,
    /// Optional ffmpeg binary path.
    pub ffmpeg_binary_path: Option<PathBuf>,
    /// Optional mkvmerge binary path.
    pub mkvmerge_binary_path: Option<PathBuf>,
    /// Optional final mux-after-done options.
    pub mux_after_done: Option<MuxAfterDoneOptions>,
    pub mux_imports: Vec<MuxImport>,
    /// Live streams should be handled as VOD when possible.
    pub live_perform_as_vod: bool,
    /// Merge live output as segments arrive.
    pub live_real_time_merge: bool,
    /// Keep live segments.
    pub live_keep_segments: bool,
    /// Pipe live media into a mux process where supported.
    pub live_pipe_mux: bool,
    pub live_record_limit: Option<Duration>,
    pub live_wait_time: Option<i32>,
    /// Number of live segments to take on startup.
    pub live_take_count: i32,
    pub live_fix_vtt_by_audio: bool,
    pub task_start_at: Option<TaskStartAt>,
    pub force_ansi_console: bool,
    pub no_ansi_color: bool,
    pub disable_update_check: bool,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            compatibility_profile: CompatibilityProfile::CliCompatible,
            log_level: LogLevel::Info,
            tmp_dir: None,
            save_dir: None,
            save_name: None,
            save_pattern: None,
            log_file_path: None,
            ui_language: None,
            urlprocessor_args: None,
            base_url: None,
            headers: BTreeMap::new(),
            custom_proxy: None,
            use_system_proxy: true,
            allow_insecure_tls: false,
            append_url_params: false,
            custom_range: None,
            keys: Vec::new(),
            key_text_file: None,
            decryption_engine: DecryptionEngine::Mp4forge,
            decryption_binary_path: None,
            mp4_real_time_decryption: false,
            use_shaka_packager: false,
            custom_hls_method: None,
            custom_hls_key: None,
            custom_hls_iv: None,
            allow_hls_multi_ext_map: false,
            thread_count: default_thread_count(),
            download_retry_count: 3,
            http_request_timeout: Duration::from_secs(100),
            max_speed: None,
            auto_select: false,
            concurrent_download: false,
            sub_only: false,
            select_video: Vec::new(),
            select_audio: Vec::new(),
            select_subtitle: Vec::new(),
            drop_video: Vec::new(),
            drop_audio: Vec::new(),
            drop_subtitle: Vec::new(),
            ad_keywords: Vec::new(),
            skip_merge: false,
            skip_download: false,
            check_segments_count: true,
            binary_merge: false,
            use_ffmpeg_concat_demuxer: false,
            del_after_done: true,
            no_date_info: false,
            no_log: false,
            write_meta_json: true,
            auto_subtitle_fix: true,
            sub_format: SubtitleFormat::Srt,
            ffmpeg_binary_path: None,
            mkvmerge_binary_path: None,
            mux_after_done: None,
            mux_imports: Vec::new(),
            live_perform_as_vod: false,
            live_real_time_merge: false,
            live_keep_segments: true,
            live_pipe_mux: false,
            live_record_limit: None,
            live_wait_time: None,
            live_take_count: 16,
            live_fix_vtt_by_audio: false,
            task_start_at: None,
            force_ansi_console: false,
            no_ansi_color: false,
            disable_update_check: false,
        }
    }
}

const fn fallback_thread_count() -> i32 {
    1
}

fn default_thread_count() -> i32 {
    match std::thread::available_parallelism() {
        Ok(count) => i32::try_from(count.get()).unwrap_or(i32::MAX),
        Err(_) => fallback_thread_count(),
    }
}
