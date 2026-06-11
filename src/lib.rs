//! Library-first downloader API.

#![deny(unsafe_code)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]
#![deny(clippy::unwrap_used)]

pub mod api;
mod attribute;
pub mod backend;
mod base64;
pub mod cancellation;
pub mod cli;
pub mod config;
mod console;
pub mod dash;
mod datetime;
pub mod decrypt;
pub mod download;
pub mod error;
pub mod event;
pub mod hardening;
pub mod hls;
pub mod http;
#[cfg(feature = "cli")]
mod interactive_prompt;
pub mod live;
pub mod manifest;
mod media_info;
pub mod mss;
pub mod mux;
mod numeric;
pub mod observability;
pub mod processor;
pub mod progress;
#[cfg(feature = "rpc")]
pub mod rpc;
pub mod selection;
pub mod session;
pub mod source;
mod stream_label;
pub mod subtitle;
pub mod traits;

pub use api::{DownloadClient, DownloadRequest, LiveRecorder, ProgressCallback};
pub use backend::{BackendPolicy, BackendSelection, Mp4BackendPolicy};
pub use cancellation::CancellationToken;
pub use cli::{
    CLI_SCHEMA, CliApiBinding, CliOptionSchema, CliParseResult, CliSchemaValueKind, parse_args,
};
pub use config::{
    CompatibilityProfile, CustomKey, CustomRange, DecryptionEngine, DownloadOptions, HlsMethod,
    LogLevel, MuxAfterDoneOptions, MuxerKind, StreamFilter, SubtitleFormat, TaskStartAt,
    UiLanguage,
};
pub use dash::{DashManifest, DashParser};
pub use decrypt::{
    ExternalDecryptPlan, ExternalDecryptRequest, Mp4ProtectionInfo, PsshInfo, PsshSystem,
    SelectedKey, aes_128_cbc_decrypt, aes_128_ecb_decrypt, chacha20_decrypt_per_1024_bytes,
    custom_keys_to_pairs, decrypt_hls_segment_bytes, decrypt_hls_segment_file,
    plan_external_decrypt, read_mp4_protection_info, redact_secrets, search_key_text_file,
    select_key_pair,
};
pub use download::{
    DownloadMergePolicy, DownloadScheduler, SegmentDownloadResult, SegmentDownloader, SpeedState,
    StreamDownloadResult, cleanup_after_success, merge_policy_for_stream, planned_output_path,
    split_large_single_file_by_size, stream_output_extension, stream_temp_dir,
};
pub use error::{Error, Result};
pub use event::ProgressEvent;
pub use hardening::{
    BenchmarkMetric, BoundedEventQueue, ResourceLimits, contains_unredacted_secret,
    estimate_segment_count, redact_and_verify, scan_unchecked_exit_paths, validate_cleanup_paths,
    validate_manifest_size, validate_resource_limits,
};
pub use hls::{HlsManifest, HlsParser};
pub use http::{DefaultHttpClient, HttpRequest, HttpResponse, ProxyMode};
pub use live::{
    HttpLiveTsState, LiveIdentityMode, LiveOptionEffects, LiveRefreshResult, LiveStreamState,
    add_recorded_duration, compute_live_wait_seconds, filter_new_live_segments,
    live_option_effects, live_segment_name, plan_live_pipe_mux, sync_live_startup_windows,
    update_http_live_ts_state,
};
pub use manifest::{
    ByteRange, Choice, EncryptionInfo, EncryptionMethod, ExtractorType, KeySource, Manifest,
    MediaPart, MediaSegment, MediaType, MssData, Playlist, RoleType, Stream, StreamSelector,
    audio_channel_order, compare_streams_compatible, sort_streams_compatible,
};
pub use mss::{MssGeneratedInit, MssInitGenerator, MssManifest, MssParser};
pub use mux::{
    FfmpegMergeMetadata, FfmpegMergeRequest, MediaInfo, MergeOutputFormat, Mp4forgeSupport,
    Mp4forgeSupportMatrix, MuxCommandPlan, MuxFormat, MuxImport, MuxOptions, OutputArtifact,
    OutputFile, combine_files, merge_extension, mp4forge_support_for_stream, mux_extension,
    output_files_with_imports, partial_combine_files, plan_ffmpeg_merge, plan_ffmpeg_mux,
    plan_mkvmerge_mux, validate_mp4forge_merge_request, validate_mp4forge_mux_after_done,
};
pub use observability::{
    DEFAULT_UPDATE_CHECK_URL, LogFilePlan, LogPlanConfig, ProgressEventCollector, ProgressSummary,
    UpdateCheckClient, UpdateCheckHttpClient, UpdateCheckResult, append_log_file,
    check_update_with_client, format_duration, format_file_size, initialize_log_file,
    latest_version_from_release_redirect, plan_log_file, redact_progress_event, should_log,
    spawn_update_check_if_enabled, streams_metadata_json, summarize_events, write_metadata_jsons,
};
pub use processor::{
    ContentProcessor, DEFAULT_USER_AGENT, DefaultDashContentProcessor, DefaultHlsContentProcessor,
    DefaultHlsKeyProcessor, DefaultUrlProcessor, KeyProcessor, ParserConfig,
    SignedDashUrlProcessor, UrlProcessor, compatibility_headers, resolve_media_url,
    signed_dash_secure_value,
};
pub use progress::{AggregateProgress, SegmentProgress, StreamProgress};
pub use selection::{
    apply_custom_range, apply_stream_filters, auto_select_streams, clean_ad_segments, filter_drop,
    filter_keep, format_save_pattern, handle_file_collision, handle_file_collision_with_reserved,
    interactive_default_streams, order_streams, save_name_from_input,
    save_name_from_input_with_suffix, subtitle_only_streams, valid_file_name,
};
pub use session::{DownloadSession, SessionState};
pub use source::{LoadedSource, LoadedSourceKind, SourceLoader, write_raw_files};
pub use subtitle::{
    SubtitleCue, SubtitleImage, WebVttSubtitle, check_stpp_init, check_wvtt_init,
    extract_stpp_from_files, extract_stpp_from_segments, extract_ttml_documents,
    extract_ttml_from_files, extract_wvtt_from_files, extract_wvtt_from_segments, format_subtitle,
    parse_webvtt, parse_webvtt_bytes, write_image_pngs,
};
