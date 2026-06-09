//! Session state models.

use std::collections::{BTreeMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::fs::File;
#[cfg(not(windows))]
use std::fs::OpenOptions;
use std::io::IsTerminal;
#[cfg(not(windows))]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use tokio::io::AsyncReadExt;
use tokio::process::{Child as TokioChild, Command};

use crate::api::{DownloadRequest, ProgressCallback};
use crate::cancellation::CancellationToken;
use crate::config::{
    CustomRange, DecryptionEngine, DownloadOptions, LogLevel, MuxAfterDoneOptions, MuxerKind,
    SubtitleFormat,
};
use crate::dash::DashParser;
use crate::datetime::{current_local_iso_timestamp, parse_manifest_timestamp_millis};
use crate::decrypt::{
    SelectedKey, custom_keys_to_pairs, decrypt_hls_segment_file, read_mp4_protection_info,
    redact_secrets, search_key_text_file, select_key_pair,
};
use crate::download::{
    DownloadEventEmitter, DownloadHooks, DownloadScheduler, SegmentCompletedHook,
    StreamCompletedHook, StreamDownloadResult, http_client, planned_output_path,
};
use crate::error::{Error, Result};
use crate::event::ProgressEvent;
use crate::hls::HlsParser;
use crate::http::{
    DefaultHttpClient, LIVE_REFRESH_RETRY_ATTEMPTS, LIVE_REFRESH_RETRY_DELAY,
    SOURCE_RETRY_ATTEMPTS, SOURCE_RETRY_DELAY, SOURCE_RETRY_DELAY_INCREMENT, apply_request_headers,
    sleep_for_retry,
};
use crate::live::{
    HttpLiveTsState, LiveStreamState, add_recorded_duration, compute_live_wait_seconds,
    filter_new_live_segments, live_option_effects, plan_live_pipe_mux, sync_live_startup_windows,
    update_http_live_ts_state,
};
use crate::manifest::{
    EncryptionMethod, ExtractorType, Manifest, MediaSegment, MediaType, Stream, StreamSelector,
};
use crate::media_info::{media_info_console_label, probe_ffmpeg_media_infos};
use crate::mss::MssParser;
use crate::mux::{
    FfmpegMergeMetadata, FfmpegMergeRequest, MediaInfo, MergeOutputFormat, Mp4forgeSupportMatrix,
    MuxCommandPlan, MuxFormat, OutputArtifact, OutputFile, combine_files,
    mp4forge_support_for_stream, mux_extension, output_files_with_imports, partial_combine_files,
    plan_ffmpeg_merge, plan_ffmpeg_mux, plan_mkvmerge_mux, validate_mp4forge_mux_after_done,
};
use crate::observability::{
    DEFAULT_UPDATE_CHECK_URL, LogFilePlan, LogPlanConfig, UpdateCheckHttpClient, append_log_file,
    check_update_with_client, initialize_log_file, should_log, streams_metadata_json,
};
use crate::processor::ParserConfig;
use crate::progress::AggregateProgress;
use crate::selection::{
    apply_custom_range, auto_select_streams, clean_ad_segments, filter_drop, filter_keep,
    format_save_pattern, handle_file_collision, handle_file_collision_with_reserved,
    interactive_default_streams, order_streams, save_name_from_input, subtitle_only_streams,
    valid_file_name,
};
use crate::source::{
    HTTP_LIVE_TS_MARKER, LoadedSource, LoadedSourceKind, SourceLoader, write_raw_files,
};
use crate::stream_label::{stream_full_label, stream_short_label};
use crate::subtitle::{
    WebVttSubtitle, check_stpp_init, check_wvtt_init, extract_stpp_from_files,
    extract_ttml_from_files, extract_wvtt_from_files_with_console_lines, format_subtitle,
    parse_webvtt_bytes, write_image_pngs,
};

/// High-level session state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SessionState {
    /// Session was created but has not started.
    #[default]
    Planned,
    /// Session is running.
    Running,
    /// Session completed successfully.
    Finished,
    /// Session failed.
    Failed,
    /// Session was cancelled.
    Cancelled,
}

/// A planned download session.
#[derive(Clone, Debug)]
pub struct DownloadSession {
    request: DownloadRequest,
    state: SessionState,
}

#[derive(Clone)]
struct WorkDirectories {
    temp_root: PathBuf,
    save_dir: PathBuf,
}

struct ExternalDecryptCommand {
    arguments: Vec<String>,
    working_directory: Option<PathBuf>,
}

const KEEP_IMAGE_SEGMENTS_ENV: &str = "HAKI_DL_KEEP_IMAGE_SEGMENTS";
const LIVE_PIPE_OPTIONS_ENV: &str = "HAKI_DL_LIVE_PIPE_OPTIONS";
const LIVE_PIPE_TMP_DIR_ENV: &str = "HAKI_DL_LIVE_PIPE_TMP_DIR";

#[derive(Clone, Debug, Eq, PartialEq)]
struct LivePipeEnvironment {
    custom_destination: Option<String>,
    pipe_dir: PathBuf,
}

fn live_pipe_environment() -> LivePipeEnvironment {
    live_pipe_environment_from(|name| env::var_os(name), env::temp_dir())
}

fn live_pipe_environment_from(
    mut lookup: impl FnMut(&str) -> Option<OsString>,
    default_pipe_dir: PathBuf,
) -> LivePipeEnvironment {
    let custom_destination = lookup(LIVE_PIPE_OPTIONS_ENV)
        .and_then(|value| value.into_string().ok())
        .filter(|value| !value.is_empty());
    let pipe_dir = lookup(LIVE_PIPE_TMP_DIR_ENV)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(default_pipe_dir);

    LivePipeEnvironment {
        custom_destination,
        pipe_dir,
    }
}

impl DownloadSession {
    /// Creates a new planned session.
    pub fn new(request: DownloadRequest) -> Self {
        Self {
            request,
            state: SessionState::Planned,
        }
    }

    /// Returns the original request.
    pub fn request(&self) -> &DownloadRequest {
        &self.request
    }

    /// Returns current session state.
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// Starts the session.
    pub async fn start(self) -> Result<Vec<ProgressEvent>> {
        Box::pin(execute_request(self.request)).await
    }
}

async fn execute_request(mut request: DownloadRequest) -> Result<Vec<ProgressEvent>> {
    request.cancellation_token.check()?;
    let progress_callback = request.progress_callback.clone();
    let mut emitted_event_count = 0_usize;
    let mut events = vec![ProgressEvent::PlanningStarted];
    emit_new_progress_events(
        &events,
        progress_callback.as_ref(),
        &mut emitted_event_count,
    )?;
    let log_plan = initialize_session_log(&request.options, &mut events).await;
    spawn_update_check_logger(
        request.options.disable_update_check,
        progress_callback.clone(),
        log_plan.clone(),
    );
    let live_pipe_mux_requested = request.options.live_pipe_mux;
    let mux_after_done_forced_binary =
        request.options.mux_after_done.is_some() && !request.options.binary_merge;
    normalize_session_options(&mut request.options);
    if live_pipe_mux_requested {
        push_session_log(
            &mut events,
            &log_plan,
            request.options.log_level,
            LogLevel::Warn,
            "LivePipeMux detected, forced enable LiveRealTimeMerge",
        )
        .await;
    }
    if mux_after_done_forced_binary {
        push_session_log(
            &mut events,
            &log_plan,
            request.options.log_level,
            LogLevel::Warn,
            "MuxAfterDone is detected, binary merging is automatically enabled",
        )
        .await;
    }
    validate_source_option_shape(&request.options).await?;
    validate_runtime_tools(&mut request.options).await?;
    push_startup_extra_logs(&mut events, &log_plan, &request.options).await;
    emit_new_progress_events(
        &events,
        progress_callback.as_ref(),
        &mut emitted_event_count,
    )?;
    wait_for_task_start(
        &request.options,
        &request.cancellation_token,
        &mut events,
        &log_plan,
    )
    .await?;
    emit_new_progress_events(
        &events,
        progress_callback.as_ref(),
        &mut emitted_event_count,
    )?;
    ensure_session_save_name(&mut request.options, &request.input);
    push_session_log(
        &mut events,
        &log_plan,
        request.options.log_level,
        LogLevel::Info,
        format!("Loading URL: {}", request.input),
    )
    .await;
    events.push(ProgressEvent::ManifestLoading);
    emit_new_progress_events(
        &events,
        progress_callback.as_ref(),
        &mut emitted_event_count,
    )?;
    let mut config = ParserConfig::from_options(&request.options);
    if is_http_url(&request.input) {
        push_session_log(
            &mut events,
            &log_plan,
            request.options.log_level,
            LogLevel::Debug,
            format_fetch_debug_message(&request.input, &config.headers),
        )
        .await;
    }
    let transport = DefaultHttpClient::from_options(&request.options);
    let loader = source_loader_for_transport(&transport);
    let session_http_client = http_client(&request.options)?;
    let loaded =
        load_source_with_retry(&loader, &request, &mut config, &mut events, &log_plan).await?;
    request.cancellation_token.check()?;
    for debug_log in &loaded.debug_logs {
        push_session_log(
            &mut events,
            &log_plan,
            request.options.log_level,
            LogLevel::Debug,
            debug_log,
        )
        .await;
    }
    push_session_log(
        &mut events,
        &log_plan,
        request.options.log_level,
        LogLevel::Info,
        format!("Content Matched: {}", source_content_label(loaded.kind)),
    )
    .await;
    push_session_log(
        &mut events,
        &log_plan,
        request.options.log_level,
        LogLevel::Info,
        "Parsing streams...",
    )
    .await;
    push_parser_diagnostics(&mut events, &log_plan, request.options.log_level, &config).await;
    emit_new_progress_events(
        &events,
        progress_callback.as_ref(),
        &mut emitted_event_count,
    )?;
    let dirs = prepare_work_directories(&request.options)?;
    if loaded.kind == LoadedSourceKind::HttpLiveTs {
        if request.options.write_meta_json {
            let stream = direct_http_live_stream(&loaded.final_url, &loaded.original_url);
            push_session_log(
                &mut events,
                &log_plan,
                request.options.log_level,
                LogLevel::Warn,
                "Writing meta json",
            )
            .await;
            let _ = write_source_sidecars(&dirs.temp_root, &loaded.raw_files, &[stream]).await?;
        }
        let events = execute_http_live_ts(
            &request,
            &loaded.final_url,
            &config.headers,
            &dirs,
            events,
            &log_plan,
        )
        .await?;
        emit_new_progress_events(
            &events,
            progress_callback.as_ref(),
            &mut emitted_event_count,
        )?;
        return Ok(events);
    }

    let mut manifest =
        parse_loaded_manifest(&loaded.text, loaded.kind, &loaded.final_url, &config).await?;
    push_parser_diagnostics(&mut events, &log_plan, request.options.log_level, &config).await;
    for warning in &manifest.warnings {
        events.push(ProgressEvent::Warning {
            message: warning.clone(),
        });
    }
    events.push(ProgressEvent::ManifestParsed {
        stream_count: manifest.streams.len(),
    });
    let raw_streams = manifest.streams.clone();
    if request.options.write_meta_json {
        push_session_log(
            &mut events,
            &log_plan,
            request.options.log_level,
            LogLevel::Warn,
            "Writing meta json",
        )
        .await;
        let _ = write_source_sidecars(&dirs.temp_root, &loaded.raw_files, &raw_streams).await?;
    }
    push_extracted_stream_logs(
        &mut events,
        &log_plan,
        request.options.log_level,
        &manifest.streams,
    )
    .await;
    emit_new_progress_events(
        &events,
        progress_callback.as_ref(),
        &mut emitted_event_count,
    )?;
    let mut selected = select_streams(&manifest.streams, &request)?;
    if selected_requires_playlist_fetch(loaded.kind, &selected) {
        push_session_log(
            &mut events,
            &log_plan,
            request.options.log_level,
            LogLevel::Info,
            "Parsing streams...",
        )
        .await;
        match loaded.kind {
            LoadedSourceKind::Hls => {
                hls_parser_for_transport(&transport)
                    .fetch_playlists(&mut selected, &mut config)
                    .await?;
            }
            LoadedSourceKind::Dash => {
                DashParser::new().process_stream_urls(&mut selected, &config)?;
            }
            LoadedSourceKind::Mss => {
                MssParser::new().process_stream_urls(&mut selected, &config)?;
            }
            LoadedSourceKind::BinaryData => {}
            _ => {}
        }
        push_parser_diagnostics(&mut events, &log_plan, request.options.log_level, &config).await;
    }
    let selected_is_live = selected.iter().any(stream_is_live);
    let mut live_states = Vec::new();
    let mut live_record_limit_reached_logged = false;
    if selected_is_live && !request.options.live_perform_as_vod {
        apply_live_runtime_option_effects(&mut request.options, &selected);
        push_session_log(
            &mut events,
            &log_plan,
            request.options.log_level,
            LogLevel::Warn,
            "Live stream found",
        )
        .await;
        emit_new_progress_events(
            &events,
            progress_callback.as_ref(),
            &mut emitted_event_count,
        )?;
    }
    if !selected_is_live || request.options.live_perform_as_vod {
        if let Some(custom_range) = &request.options.custom_range {
            push_session_log(
                &mut events,
                &log_plan,
                request.options.log_level,
                LogLevel::Info,
                format!("User customed range: {}", custom_range_input(custom_range)),
            )
            .await;
            push_session_log(
                &mut events,
                &log_plan,
                request.options.log_level,
                LogLevel::Warn,
                "Please note that custom range may sometimes result in audio and video being out of sync",
            )
            .await;
        }
        apply_custom_range(&mut selected, request.options.custom_range.as_ref());
    }
    clean_ad_segments_with_logs(
        &mut selected,
        &request.options.ad_keywords,
        &mut events,
        &log_plan,
        request.options.log_level,
    )
    .await?;
    if selected.is_empty() {
        return Err(Error::compatibility("no streams were selected"));
    }
    validate_mp4forge_decrypt_before_download(&selected, &request.options)?;
    validate_explicit_mp4forge_before_download(&selected, &request.options)?;
    for stream in &selected {
        events.push(ProgressEvent::StreamSelected {
            stream_id: stream_identity(stream),
        });
    }
    push_selected_stream_logs(&mut events, &log_plan, request.options.log_level, &selected).await;
    if request.options.write_meta_json {
        push_session_log(
            &mut events,
            &log_plan,
            request.options.log_level,
            LogLevel::Warn,
            "Writing meta json",
        )
        .await;
        let mut selected_file = BTreeMap::new();
        selected_file.insert(
            "meta_selected.json".to_string(),
            streams_metadata_json(&selected),
        );
        for path in write_raw_files(&selected_file, &dirs.temp_root).await? {
            events.push(ProgressEvent::OutputArtifact(OutputArtifact::new(path)));
        }
    }
    emit_new_progress_events(
        &events,
        progress_callback.as_ref(),
        &mut emitted_event_count,
    )?;
    request.cancellation_token.check()?;
    manifest.streams = selected.clone();
    manifest.is_live = selected.iter().any(|stream| {
        stream
            .playlist
            .as_ref()
            .is_some_and(|playlist| playlist.is_live)
    });
    if request.options.skip_download {
        events.push(ProgressEvent::Finished { success: true });
        emit_new_progress_events(
            &events,
            progress_callback.as_ref(),
            &mut emitted_event_count,
        )?;
        return Ok(events);
    }

    push_save_name_log(
        &mut events,
        &log_plan,
        request.options.log_level,
        request.options.save_name.as_deref(),
    )
    .await;
    tokio::fs::create_dir_all(&dirs.temp_root).await?;
    tokio::fs::create_dir_all(&dirs.save_dir).await?;
    if selected_is_live && !request.options.live_perform_as_vod {
        sync_live_startup_windows(&mut selected, request.options.live_take_count);
        let live_wait_seconds =
            compute_live_wait_seconds(&selected, request.options.live_wait_time);
        push_session_log(
            &mut events,
            &log_plan,
            request.options.log_level,
            LogLevel::Warn,
            format!("set refresh interval to {live_wait_seconds} seconds"),
        )
        .await;
        if let Some(limit) = request.options.live_record_limit {
            push_session_log(
                &mut events,
                &log_plan,
                request.options.log_level,
                LogLevel::Warn,
                format!(
                    "Live recording duration limit: {}",
                    format_duration_short_for_log(limit)
                ),
            )
            .await;
        }
        if matches!(
            loaded.kind,
            LoadedSourceKind::Hls | LoadedSourceKind::Dash | LoadedSourceKind::Mss
        ) {
            validate_live_refresh_request(&selected, &request.options)?;
            live_states = seed_live_states(
                &mut selected,
                loaded.kind == LoadedSourceKind::Hls,
                request.options.live_record_limit,
                &mut events,
            )?;
            log_live_record_limit_reached_if_needed(
                &live_states,
                &request.options,
                &mut live_record_limit_reached_logged,
                &mut events,
                &log_plan,
            )
            .await;
        }
        if !request.options.binary_merge && live_streams_need_fmp4_binary_merge(&selected) {
            request.options.binary_merge = true;
            push_session_log(
                &mut events,
                &log_plan,
                request.options.log_level,
                LogLevel::Warn,
                "fMP4 is detected, binary merging is automatically enabled",
            )
            .await;
        }
    }
    emit_new_progress_events(
        &events,
        progress_callback.as_ref(),
        &mut emitted_event_count,
    )?;
    let realtime_hooks = realtime_decrypt_hooks(&request.options, &request.cancellation_token);
    let (download_result, download_events) = DownloadScheduler::new()
        .download_streams_with_first_media_probe_and_http_client_capture_with_hooks(
            &selected,
            &dirs.temp_root,
            &dirs.save_dir,
            &request.options,
            &config.headers,
            loaded.kind == LoadedSourceKind::Mss,
            &session_http_client,
            progress_callback.as_ref(),
            &realtime_hooks,
        )
        .await;
    append_extra_log_events(&log_plan, &download_events).await;
    events.extend(download_events);
    emitted_event_count = events.len();
    let mut download_results = download_result?;
    apply_media_info_decisions(
        &mut selected,
        &mut download_results,
        &mut request.options,
        loaded.kind,
        &dirs.save_dir,
        &mut events,
        &log_plan,
    )
    .await?;
    emit_new_progress_events(
        &events,
        progress_callback.as_ref(),
        &mut emitted_event_count,
    )?;
    request.cancellation_token.check()?;
    let is_live_session = selected_is_live && !request.options.live_perform_as_vod;
    let live_audio_start_ms = if is_live_session {
        probe_live_audio_start_ms(&selected, &download_results, &request.options)?
    } else {
        None
    };
    if is_live_session && request.options.live_pipe_mux {
        let mut live_pipe_session = LivePipeMuxSession::start(
            &selected,
            &download_results,
            &request.options,
            &dirs,
            &mut events,
        )
        .await?;
        emit_new_progress_events(
            &events,
            progress_callback.as_ref(),
            &mut emitted_event_count,
        )?;
        stream_downloads_to_live_pipe(
            &selected,
            &download_results,
            &request.options,
            true,
            live_audio_start_ms,
            &mut events,
            &mut live_pipe_session,
            &request.cancellation_token,
        )
        .await?;
        emit_new_progress_events(
            &events,
            progress_callback.as_ref(),
            &mut emitted_event_count,
        )?;
        if selected_is_live
            && !request.options.live_perform_as_vod
            && matches!(
                loaded.kind,
                LoadedSourceKind::Hls | LoadedSourceKind::Dash | LoadedSourceKind::Mss
            )
        {
            record_live_downloaded_durations(&selected, &mut live_states, &mut events);
            download_live_refreshes(
                &selected,
                &mut live_states,
                &download_results,
                &request,
                loaded.kind,
                &mut config,
                &dirs,
                &transport,
                Some(&mut live_pipe_session),
                live_audio_start_ms,
                &mut events,
            )
            .await?;
            log_live_record_limit_reached_if_needed(
                &live_states,
                &request.options,
                &mut live_record_limit_reached_logged,
                &mut events,
                &log_plan,
            )
            .await;
            emit_new_progress_events(
                &events,
                progress_callback.as_ref(),
                &mut emitted_event_count,
            )?;
        }
        live_pipe_session
            .finish(&mut events, &request.cancellation_token)
            .await?;
        emit_new_progress_events(
            &events,
            progress_callback.as_ref(),
            &mut emitted_event_count,
        )?;
    } else {
        let write_live_outputs = !is_live_session || request.options.live_real_time_merge;
        let merged_outputs = if write_live_outputs {
            let outputs = finalize_downloads(
                &selected,
                &download_results,
                &request.options,
                is_live_session,
                live_audio_start_ms,
                &mut events,
                &request.cancellation_token,
            )
            .await?;
            emit_new_progress_events(
                &events,
                progress_callback.as_ref(),
                &mut emitted_event_count,
            )?;
            outputs
        } else {
            Vec::new()
        };
        if selected_is_live
            && !request.options.live_perform_as_vod
            && matches!(
                loaded.kind,
                LoadedSourceKind::Hls | LoadedSourceKind::Dash | LoadedSourceKind::Mss
            )
        {
            record_live_downloaded_durations(&selected, &mut live_states, &mut events);
            download_live_refreshes(
                &selected,
                &mut live_states,
                &download_results,
                &request,
                loaded.kind,
                &mut config,
                &dirs,
                &transport,
                None,
                live_audio_start_ms,
                &mut events,
            )
            .await?;
            log_live_record_limit_reached_if_needed(
                &live_states,
                &request.options,
                &mut live_record_limit_reached_logged,
                &mut events,
                &log_plan,
            )
            .await;
            emit_new_progress_events(
                &events,
                progress_callback.as_ref(),
                &mut emitted_event_count,
            )?;
        }
        if !merged_outputs.is_empty() {
            run_mux_after_done(
                &merged_outputs,
                &request.options,
                &mut events,
                &request.cancellation_token,
            )
            .await?;
        }
        emit_new_progress_events(
            &events,
            progress_callback.as_ref(),
            &mut emitted_event_count,
        )?;
    }
    if should_cleanup_task_temp_root(&request.options, is_live_session) {
        cleanup_task_temp_root(&dirs.temp_root, &mut events).await?;
        emit_new_progress_events(
            &events,
            progress_callback.as_ref(),
            &mut emitted_event_count,
        )?;
    }
    events.push(ProgressEvent::Finished { success: true });
    emit_new_progress_events(
        &events,
        progress_callback.as_ref(),
        &mut emitted_event_count,
    )?;
    if should_log(request.options.log_level, LogLevel::Info) {
        append_session_log_file(&log_plan, LogLevel::Info, "session finished").await;
    }
    Ok(events)
}

fn spawn_update_check_logger(
    disabled: bool,
    progress_callback: Option<ProgressCallback>,
    log_plan: LogFilePlan,
) {
    if disabled {
        return;
    }
    tokio::spawn(async move {
        let client = UpdateCheckHttpClient::new();
        let Ok(result) =
            check_update_with_client(&client, DEFAULT_UPDATE_CHECK_URL, env!("CARGO_PKG_VERSION"))
                .await
        else {
            return;
        };
        let Some(latest_version) = result.latest_version.filter(|_| result.update_available) else {
            return;
        };
        let message = format!("New version detected! {latest_version}");
        append_session_log_file(&log_plan, LogLevel::Info, &message).await;
        let Some(progress_callback) = progress_callback else {
            return;
        };
        let _ = progress_callback.emit(&ProgressEvent::Log {
            level: LogLevel::Info,
            message,
        });
    });
}

async fn validate_source_option_shape(options: &DownloadOptions) -> Result<()> {
    if !options.mux_imports.is_empty() && options.mux_after_done.is_none() {
        return Err(Error::config("--mux-import requires --mux-after-done"));
    }
    validate_mux_after_done_fallback_shape(options)?;
    for import in &options.mux_imports {
        if !tokio::fs::metadata(&import.path)
            .await
            .is_ok_and(|metadata| metadata.is_file())
        {
            return Err(Error::config("--mux-import path must be an existing file"));
        }
    }
    for (name, filters) in [
        ("--select-video", options.select_video.as_slice()),
        ("--select-audio", options.select_audio.as_slice()),
        ("--select-subtitle", options.select_subtitle.as_slice()),
        ("--drop-video", options.drop_video.as_slice()),
        ("--drop-audio", options.drop_audio.as_slice()),
        ("--drop-subtitle", options.drop_subtitle.as_slice()),
    ] {
        if filters.len() > 1 {
            return Err(Error::config(format!("{name} expects a single value")));
        }
    }
    Ok(())
}

fn validate_mux_after_done_fallback_shape(options: &DownloadOptions) -> Result<()> {
    let Some(mux_options) = &options.mux_after_done else {
        return Ok(());
    };
    let Some(fallback_muxer) = mux_options.fallback_muxer else {
        return Ok(());
    };
    if mux_options.muxer != MuxerKind::Mp4forge {
        return Err(Error::config(
            "fallback_muxer is only valid with muxer=mp4forge",
        ));
    }
    match fallback_muxer {
        MuxerKind::Ffmpeg => Ok(()),
        MuxerKind::Mkvmerge => Err(Error::config(
            "mkvmerge cannot be used as an mp4forge fallback for mp4 mux-after-done",
        )),
        MuxerKind::Mp4forge => Err(Error::config("fallback_muxer must be ffmpeg")),
    }
}

fn emit_new_progress_events(
    events: &[ProgressEvent],
    progress_callback: Option<&ProgressCallback>,
    emitted_event_count: &mut usize,
) -> Result<()> {
    let Some(progress_callback) = progress_callback else {
        *emitted_event_count = events.len();
        return Ok(());
    };
    while *emitted_event_count < events.len() {
        progress_callback.emit(&events[*emitted_event_count])?;
        *emitted_event_count += 1;
    }
    Ok(())
}

fn normalize_session_options(options: &mut DownloadOptions) {
    if options
        .custom_proxy
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        options.custom_proxy = None;
    }
    if options
        .custom_hls_key
        .as_ref()
        .is_some_and(|value| value.is_empty())
    {
        options.custom_hls_key = None;
    }
    if options
        .custom_hls_iv
        .as_ref()
        .is_some_and(|value| value.is_empty())
    {
        options.custom_hls_iv = None;
    }
    if options.use_shaka_packager {
        options.decryption_engine = DecryptionEngine::ShakaPackager;
    }
    if options.live_pipe_mux {
        options.live_real_time_merge = true;
    }
    if let Some(mux_options) = &options.mux_after_done {
        options.binary_merge = true;
        if mux_options.muxer == MuxerKind::Ffmpeg && options.ffmpeg_binary_path.is_none() {
            options.ffmpeg_binary_path = mux_options.bin_path.clone();
        }
    }
}

fn ensure_session_save_name(options: &mut DownloadOptions, input: &str) {
    if options
        .save_name
        .as_deref()
        .is_none_or(|value| value.is_empty())
    {
        options.save_name = Some(save_name_from_input(input, true));
    }
}

fn source_loader_for_transport(transport: &DefaultHttpClient) -> SourceLoader {
    SourceLoader::new().with_http(transport.clone())
}

fn hls_parser_for_transport(transport: &DefaultHttpClient) -> HlsParser {
    HlsParser::new().with_http(transport.clone())
}

async fn load_source_with_retry(
    loader: &SourceLoader,
    request: &DownloadRequest,
    config: &mut ParserConfig,
    events: &mut Vec<ProgressEvent>,
    log_plan: &LogFilePlan,
) -> Result<LoadedSource> {
    if SOURCE_RETRY_ATTEMPTS == 0 {
        return Err(Error::http("Failed to execute action after 0 retries."));
    }
    let mut retry_count = 0_usize;
    let mut next_delay = SOURCE_RETRY_DELAY;
    loop {
        match loader.load_source(&request.input, config).await {
            Ok(loaded) => return Ok(loaded),
            Err(error) if source_retryable_error(&error) => {
                retry_count += 1;
                let message = format!(
                    "{} ({retry_count}/{SOURCE_RETRY_ATTEMPTS})",
                    error.compatibility_message()
                );
                if should_log(request.options.log_level, LogLevel::Warn) {
                    append_session_log_file(log_plan, LogLevel::Warn, &message).await;
                }
                events.push(ProgressEvent::Warning { message });
                sleep_for_retry(next_delay, Some(&request.cancellation_token)).await?;
                if retry_count >= SOURCE_RETRY_ATTEMPTS {
                    return Err(Error::http(format!(
                        "Failed to execute action after {SOURCE_RETRY_ATTEMPTS} retries."
                    )));
                }
                next_delay = next_delay.saturating_add(SOURCE_RETRY_DELAY_INCREMENT);
            }
            Err(error) => return Err(error),
        }
    }
}

fn source_retryable_error(error: &Error) -> bool {
    matches!(error, Error::Http { .. } | Error::Io(_))
}

async fn validate_runtime_tools(options: &mut DownloadOptions) -> Result<()> {
    validate_ffmpeg_runtime_tool(options).await?;
    validate_mkvmerge_runtime_tool(options).await?;
    validate_decrypt_runtime_tool(options).await
}

async fn validate_ffmpeg_runtime_tool(options: &mut DownloadOptions) -> Result<()> {
    if let Some(path) = &options.ffmpeg_binary_path {
        validate_runtime_tool_path(path, "ffmpeg").await?;
    } else {
        let path = find_runtime_tool(&["ffmpeg"])
            .await
            .ok_or_else(|| Error::config("ffmpeg runtime tool was not found"))?;
        options.ffmpeg_binary_path = Some(path);
    }

    Ok(())
}

async fn validate_mkvmerge_runtime_tool(options: &mut DownloadOptions) -> Result<()> {
    let Some(mux_options) = &options.mux_after_done else {
        return Ok(());
    };
    let needs_mkvmerge = mux_options.muxer == MuxerKind::Mkvmerge
        || mux_options.fallback_muxer == Some(MuxerKind::Mkvmerge);
    if !needs_mkvmerge {
        return Ok(());
    }
    if mux_options.muxer == MuxerKind::Mkvmerge
        && let Some(path) = &mux_options.bin_path
    {
        validate_runtime_tool_path(path, "mkvmerge").await?;
        return Ok(());
    }
    if let Some(path) = &options.mkvmerge_binary_path {
        validate_runtime_tool_path(path, "mkvmerge").await?;
        return Ok(());
    }
    let path = find_runtime_tool(&["mkvmerge"])
        .await
        .ok_or_else(|| Error::config("mkvmerge runtime tool was not found"))?;
    options.mkvmerge_binary_path = Some(path);
    Ok(())
}

async fn validate_decrypt_runtime_tool(options: &mut DownloadOptions) -> Result<()> {
    validate_decrypt_runtime_tool_with_dirs(options, &runtime_tool_search_dirs()).await
}

async fn validate_decrypt_runtime_tool_with_dirs(
    options: &mut DownloadOptions,
    search_dirs: &[PathBuf],
) -> Result<()> {
    if !decrypt_runtime_tool_required(options) {
        return Ok(());
    }
    if options.decryption_engine != DecryptionEngine::Mp4forge
        && let Some(path) = &options.decryption_binary_path
    {
        validate_runtime_tool_path(path, "decryption").await?;
        return Ok(());
    }
    let path = match options.decryption_engine {
        DecryptionEngine::Mp4forge => return validate_mp4forge_decrypt_feature(),
        DecryptionEngine::Ffmpeg => {
            // Any explicit decrypt path was validated above.
            options
                .ffmpeg_binary_path
                .clone()
                .ok_or_else(|| Error::config("ffmpeg runtime tool was not found"))?
        }
        DecryptionEngine::Mp4decrypt => find_runtime_tool_in_dirs(&["mp4decrypt"], search_dirs)
            .await
            .ok_or_else(|| Error::config("mp4decrypt runtime tool was not found"))?,
        DecryptionEngine::ShakaPackager => find_runtime_tool_in_dirs(
            &[
                "shaka-packager",
                "shaka_packager",
                "packager-linux-x64",
                "packager-osx-x64",
                "packager-win-x64",
            ],
            search_dirs,
        )
        .await
        .ok_or_else(|| Error::config("shaka-packager runtime tool was not found"))?,
    };
    options.decryption_binary_path = Some(path);
    Ok(())
}

async fn initialize_session_log(
    options: &DownloadOptions,
    events: &mut Vec<ProgressEvent>,
) -> LogFilePlan {
    let started_at = log_timestamp();
    let config = LogPlanConfig {
        level: options.log_level,
        no_log: options.no_log,
        log_file_path: options.log_file_path.clone(),
        default_log_dir: default_log_dir(),
        suffix: log_file_suffix(&started_at),
    };
    match initialize_log_file(&config, &started_at, Some(&session_command_line())).await {
        Ok(plan) => {
            if let Some(path) = &plan.path {
                events.push(ProgressEvent::LogFileCreated { path: path.clone() });
            }
            push_session_log(
                events,
                &plan,
                options.log_level,
                LogLevel::Info,
                format!("haki-dl {}", env!("CARGO_PKG_VERSION")),
            )
            .await;
            plan
        }
        Err(error) => {
            events.push(ProgressEvent::Warning {
                message: format!("log init failed: {error}"),
            });
            LogFilePlan {
                enabled: false,
                path: None,
            }
        }
    }
}

async fn push_session_log(
    events: &mut Vec<ProgressEvent>,
    log_plan: &LogFilePlan,
    active_level: LogLevel,
    level: LogLevel,
    message: impl Into<String>,
) {
    if !should_log(active_level, level) {
        return;
    }
    let message = message.into();
    append_session_log_file(log_plan, level, &message).await;
    events.push(ProgressEvent::Log { level, message });
}

async fn push_media_info_log(
    events: &mut Vec<ProgressEvent>,
    log_plan: &LogFilePlan,
    active_level: LogLevel,
    stream_id: Option<String>,
    media_infos: &[MediaInfo],
) {
    let show_warn = should_log(active_level, LogLevel::Warn);
    let show_info = should_log(active_level, LogLevel::Info);
    if !show_warn && !show_info {
        return;
    }
    if show_warn {
        append_session_log_file(log_plan, LogLevel::Warn, "Reading media info...").await;
    }
    let lines = media_infos
        .iter()
        .map(media_info_console_label)
        .collect::<Vec<_>>();
    if show_info {
        for line in &lines {
            append_session_log_file(log_plan, LogLevel::Info, line).await;
        }
    }
    events.push(ProgressEvent::MediaInfo { stream_id, lines });
}

fn push_raw_console_lines<I>(events: &mut Vec<ProgressEvent>, lines: I)
where
    I: IntoIterator<Item = String>,
{
    for message in lines {
        events.push(ProgressEvent::ConsoleLine { message });
    }
}

async fn log_live_record_limit_reached_if_needed(
    live_states: &[LiveStreamState],
    options: &DownloadOptions,
    already_logged: &mut bool,
    events: &mut Vec<ProgressEvent>,
    log_plan: &LogFilePlan,
) {
    if *already_logged
        || options.live_record_limit.is_none()
        || live_states.is_empty()
        || live_states.iter().any(|state| !state.record_limit_reached)
    {
        return;
    }
    push_session_log(
        events,
        log_plan,
        options.log_level,
        LogLevel::Warn,
        "Live recording limit reached, will stop recording soon",
    )
    .await;
    *already_logged = true;
}

async fn push_parser_diagnostics(
    events: &mut Vec<ProgressEvent>,
    log_plan: &LogFilePlan,
    active_level: LogLevel,
    config: &ParserConfig,
) {
    for diagnostic in config.drain_diagnostics() {
        push_session_log(
            events,
            log_plan,
            active_level,
            diagnostic.level,
            diagnostic.message,
        )
        .await;
    }
}

fn parser_diagnostic_events(config: &ParserConfig) -> Vec<ProgressEvent> {
    config
        .drain_diagnostics()
        .into_iter()
        .map(|diagnostic| ProgressEvent::Log {
            level: diagnostic.level,
            message: diagnostic.message,
        })
        .collect()
}

fn log_file_level_prefix(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Debug => "DEBUG:",
        LogLevel::Info => "INFO :",
        LogLevel::Warn => "WARN :",
        LogLevel::Error => "ERROR:",
        LogLevel::Off => "OFF:",
    }
}

async fn append_session_log_file(log_plan: &LogFilePlan, level: LogLevel, message: &str) {
    let prefix = log_file_level_prefix(level);
    if message.is_empty() {
        let _ = append_log_file(log_plan, &format!("{} {prefix}", log_file_time())).await;
        return;
    }
    for line in message.lines() {
        let _ = append_log_file(log_plan, &format!("{} {prefix} {line}", log_file_time())).await;
    }
}

async fn append_extra_log_file(log_plan: &LogFilePlan, message: impl AsRef<str>) {
    let message = message.as_ref();
    if message.is_empty() {
        let _ = append_log_file(log_plan, &format!("{} EXTRA:", log_file_time())).await;
        return;
    }
    for line in message.lines() {
        let _ = append_log_file(log_plan, &format!("{} EXTRA: {line}", log_file_time())).await;
    }
}

async fn append_extra_log_events(log_plan: &LogFilePlan, events: &[ProgressEvent]) {
    for event in events {
        if let ProgressEvent::ExtraLog { message } = event {
            append_extra_log_file(log_plan, message).await;
        }
    }
}

fn log_file_time() -> String {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        now.hour(),
        now.minute(),
        now.second(),
        now.nanosecond() / 1_000_000
    )
}

fn session_command_line() -> String {
    env::args().collect::<Vec<_>>().join(" ")
}

fn format_fetch_debug_message(url: &str, headers: &BTreeMap<String, String>) -> String {
    if !is_http_url(url) {
        return format!("Fetch: {}", redact_secrets(url));
    }
    let mut message = format!("Fetch: {}", redact_secrets(url));
    message.push_str("\nAccept-Encoding: gzip, deflate");
    message.push_str("\nCache-Control: no-cache");
    for (key, value) in headers {
        message.push('\n');
        message.push_str(key);
        message.push_str(": ");
        message.push_str(&redact_secrets(value));
    }
    message
}

fn format_request_headers_debug_message(headers: &BTreeMap<String, String>) -> String {
    headers
        .iter()
        .map(|(key, value)| format!("{key}: {}", redact_secrets(value)))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn push_startup_extra_logs(
    events: &mut Vec<ProgressEvent>,
    log_plan: &LogFilePlan,
    options: &DownloadOptions,
) {
    if let Some(path) = &options.ffmpeg_binary_path {
        append_extra_log_file(log_plan, format!("ffmpeg => {}", path.display())).await;
    }
    if let Some(path) = selected_mkvmerge_path(options) {
        append_extra_log_file(log_plan, format!("mkvmerge => {}", path.display())).await;
    }
    if decrypt_runtime_tool_required(options) {
        match options.decryption_engine {
            DecryptionEngine::Mp4forge => {
                append_extra_log_file(log_plan, "mp4forge => in-process").await;
            }
            DecryptionEngine::Mp4decrypt => {
                if let Some(path) = &options.decryption_binary_path {
                    append_extra_log_file(log_plan, format!("mp4decrypt => {}", path.display()))
                        .await;
                }
            }
            DecryptionEngine::ShakaPackager => {
                if let Some(path) = &options.decryption_binary_path {
                    append_extra_log_file(
                        log_plan,
                        format!("shaka-packager => {}", path.display()),
                    )
                    .await;
                }
            }
            DecryptionEngine::Ffmpeg => {}
        }
    }
    for (key, value) in &options.headers {
        push_startup_extra_log(
            events,
            log_plan,
            format!("User-Defined Header => {key}: {}", redact_secrets(value)),
        )
        .await;
    }
    push_filter_extra_logs(events, log_plan, options).await;
}

async fn push_startup_extra_log(
    events: &mut Vec<ProgressEvent>,
    log_plan: &LogFilePlan,
    message: impl AsRef<str>,
) {
    let message = message.as_ref().to_string();
    append_extra_log_file(log_plan, &message).await;
    events.push(ProgressEvent::ExtraLog { message });
}

fn selected_mkvmerge_path(options: &DownloadOptions) -> Option<&Path> {
    let mux_options = options.mux_after_done.as_ref()?;
    if mux_options.muxer != MuxerKind::Mkvmerge
        && mux_options.fallback_muxer != Some(MuxerKind::Mkvmerge)
    {
        return None;
    }
    mux_options
        .bin_path
        .as_deref()
        .or(options.mkvmerge_binary_path.as_deref())
}

async fn push_filter_extra_logs(
    events: &mut Vec<ProgressEvent>,
    log_plan: &LogFilePlan,
    options: &DownloadOptions,
) {
    push_filter_extra_log(
        events,
        log_plan,
        options,
        "DropVideoFilter",
        &options.drop_video,
    )
    .await;
    push_filter_extra_log(
        events,
        log_plan,
        options,
        "DropAudioFilter",
        &options.drop_audio,
    )
    .await;
    push_filter_extra_log(
        events,
        log_plan,
        options,
        "DropSubtitleFilter",
        &options.drop_subtitle,
    )
    .await;
    push_filter_extra_log(
        events,
        log_plan,
        options,
        "VideoFilter",
        &options.select_video,
    )
    .await;
    push_filter_extra_log(
        events,
        log_plan,
        options,
        "AudioFilter",
        &options.select_audio,
    )
    .await;
    push_filter_extra_log(
        events,
        log_plan,
        options,
        "SubtitleFilter",
        &options.select_subtitle,
    )
    .await;
}

async fn push_filter_extra_log(
    events: &mut Vec<ProgressEvent>,
    log_plan: &LogFilePlan,
    _options: &DownloadOptions,
    name: &str,
    filters: &[crate::config::StreamFilter],
) {
    if filters.is_empty() {
        return;
    }
    let value = filters
        .iter()
        .map(format_stream_filter_for_log)
        .collect::<Vec<_>>()
        .join(";");
    let message = format!("{name} => {value}");
    append_extra_log_file(log_plan, &message).await;
    events.push(ProgressEvent::ExtraLog { message });
}

fn format_stream_filter_for_log(filter: &crate::config::StreamFilter) -> String {
    let mut parts = Vec::new();
    if !filter.for_choice.is_empty() {
        parts.push(format!("for={}", filter.for_choice));
    }
    push_filter_part(&mut parts, "id", filter.id.as_deref());
    push_filter_part(&mut parts, "lang", filter.language.as_deref());
    push_filter_part(&mut parts, "name", filter.name.as_deref());
    push_filter_part(&mut parts, "codecs", filter.codecs.as_deref());
    push_filter_part(&mut parts, "res", filter.resolution.as_deref());
    push_filter_part(&mut parts, "frame", filter.frame_rate.as_deref());
    push_filter_part(&mut parts, "channel", filter.channels.as_deref());
    push_filter_part(&mut parts, "range", filter.range.as_deref());
    push_filter_part(&mut parts, "url", filter.url.as_deref());
    push_filter_part_i64(&mut parts, "segsMin", filter.segment_count_min);
    push_filter_part_i64(&mut parts, "segsMax", filter.segment_count_max);
    push_filter_part_f64(&mut parts, "plistDurMin", filter.playlist_duration_min);
    push_filter_part_f64(&mut parts, "plistDurMax", filter.playlist_duration_max);
    push_filter_part_i64(&mut parts, "bwMin", filter.bandwidth_min);
    push_filter_part_i64(&mut parts, "bwMax", filter.bandwidth_max);
    if let Some(role) = filter.role {
        parts.push(format!("role={role:?}"));
    }
    if parts.is_empty() {
        "all".to_string()
    } else {
        parts.join(":")
    }
}

fn push_filter_part(parts: &mut Vec<String>, name: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        parts.push(format!("{name}={value}"));
    }
}

fn push_filter_part_i64(parts: &mut Vec<String>, name: &str, value: Option<i64>) {
    if let Some(value) = value {
        parts.push(format!("{name}={value}"));
    }
}

fn push_filter_part_f64(parts: &mut Vec<String>, name: &str, value: Option<f64>) {
    if let Some(value) = value {
        parts.push(format!("{name}={value}"));
    }
}

fn is_http_url(url: &str) -> bool {
    let lower = url
        .get(..url.len().min(8))
        .unwrap_or(url)
        .to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

fn push_debug_event(events: &mut Vec<ProgressEvent>, message: impl Into<String>) {
    events.push(ProgressEvent::Log {
        level: LogLevel::Debug,
        message: message.into(),
    });
}

async fn push_save_name_log(
    events: &mut Vec<ProgressEvent>,
    log_plan: &LogFilePlan,
    active_level: LogLevel,
    save_name: Option<&str>,
) {
    if let Some(save_name) = save_name.filter(|value| !value.is_empty()) {
        push_session_log(
            events,
            log_plan,
            active_level,
            LogLevel::Info,
            format!("Save Name: {save_name}"),
        )
        .await;
    }
}

fn push_decrypt_protection_logs(
    events: &mut Vec<ProgressEvent>,
    stream_id: Option<String>,
    init_info: &crate::decrypt::Mp4ProtectionInfo,
    file_info: &crate::decrypt::Mp4ProtectionInfo,
    kid: Option<&str>,
) {
    if let Some(scheme) = file_info.scheme.as_ref().or(init_info.scheme.as_ref()) {
        events.push(ProgressEvent::DecryptProgress {
            stream_id: stream_id.clone(),
            message: format!("Type: {scheme}"),
        });
    }
    for (label, pssh) in pssh_console_fields(file_info, init_info) {
        events.push(ProgressEvent::DecryptProgress {
            stream_id: stream_id.clone(),
            message: format!("PSSH({label}): {pssh}"),
        });
    }
    if let Some(kid) = kid.filter(|value| !value.is_empty()) {
        events.push(ProgressEvent::DecryptProgress {
            stream_id,
            message: format!("KID: {kid}"),
        });
    }
}

fn pssh_console_fields<'a>(
    file_info: &'a crate::decrypt::Mp4ProtectionInfo,
    init_info: &'a crate::decrypt::Mp4ProtectionInfo,
) -> Vec<(&'static str, &'a str)> {
    file_info
        .psshs
        .iter()
        .chain(init_info.psshs.iter())
        .map(|info| (pssh_system_label(info.system), info.data.as_str()))
        .collect()
}

fn pssh_system_label(system: crate::decrypt::PsshSystem) -> &'static str {
    match system {
        crate::decrypt::PsshSystem::Widevine => "WV",
        crate::decrypt::PsshSystem::PlayReady => "PR",
        crate::decrypt::PsshSystem::FairPlay => "FP",
    }
}

async fn push_extracted_stream_logs(
    events: &mut Vec<ProgressEvent>,
    log_plan: &LogFilePlan,
    active_level: LogLevel,
    streams: &[Stream],
) {
    let (basic_streams, audio_streams, subtitle_streams) = split_stream_groups(streams);
    push_session_log(
        events,
        log_plan,
        active_level,
        LogLevel::Info,
        format!(
            "Extracted, there are {} streams, with {} basic streams, {} audio streams, {} subtitle streams",
            streams.len(),
            basic_streams.len(),
            audio_streams.len(),
            subtitle_streams.len()
        ),
    )
    .await;
    for stream in display_stream_order(streams) {
        push_session_log(
            events,
            log_plan,
            active_level,
            LogLevel::Info,
            stream_full_label(&stream),
        )
        .await;
    }
}

async fn push_selected_stream_logs(
    events: &mut Vec<ProgressEvent>,
    log_plan: &LogFilePlan,
    active_level: LogLevel,
    streams: &[Stream],
) {
    push_session_log(
        events,
        log_plan,
        active_level,
        LogLevel::Info,
        "Selected streams:",
    )
    .await;
    for stream in streams {
        push_session_log(
            events,
            log_plan,
            active_level,
            LogLevel::Info,
            stream_full_label(stream),
        )
        .await;
    }
}

fn display_stream_order(streams: &[Stream]) -> Vec<Stream> {
    let mut streams = streams.to_vec();
    order_streams(&mut streams);
    let (basic_streams, audios, subtitles) = split_stream_groups(&streams);
    basic_streams
        .into_iter()
        .chain(audios)
        .chain(subtitles)
        .collect()
}

fn source_content_label(kind: LoadedSourceKind) -> &'static str {
    match kind {
        LoadedSourceKind::Hls => "HTTP Live Streaming",
        LoadedSourceKind::Dash => "Dynamic Adaptive Streaming over HTTP",
        LoadedSourceKind::Mss => "Microsoft Smooth Streaming",
        LoadedSourceKind::HttpLiveTs => "HTTP Live MPEG2-TS",
        LoadedSourceKind::BinaryData => "Binary Data",
    }
}

async fn clean_ad_segments_with_logs(
    streams: &mut [Stream],
    keywords: &[String],
    events: &mut Vec<ProgressEvent>,
    log_plan: &LogFilePlan,
    active_level: LogLevel,
) -> Result<()> {
    if keywords.is_empty() {
        return Ok(());
    }
    for keyword in keywords {
        push_session_log(
            events,
            log_plan,
            active_level,
            LogLevel::Info,
            format!("User customed Ad keyword: {keyword}"),
        )
        .await;
    }
    let before = streams
        .iter()
        .map(Stream::segments_count)
        .collect::<Vec<_>>();
    clean_ad_segments(streams, keywords)?;
    for (stream, count_before) in streams.iter().zip(before) {
        let count_after = stream.segments_count();
        if count_before != count_after {
            push_session_log(
                events,
                log_plan,
                active_level,
                LogLevel::Warn,
                format!("{count_before} segments => {count_after} segments"),
            )
            .await;
        }
    }
    Ok(())
}

fn custom_range_input(range: &CustomRange) -> &str {
    match range {
        CustomRange::Segment { input, .. } | CustomRange::Time { input, .. } => input,
    }
}

async fn wait_for_task_start(
    options: &DownloadOptions,
    cancellation_token: &crate::cancellation::CancellationToken,
    events: &mut Vec<ProgressEvent>,
    log_plan: &LogFilePlan,
) -> Result<()> {
    let Some(task_start_at) = &options.task_start_at else {
        return Ok(());
    };
    let Some(remaining) = task_start_at.duration_until_now()? else {
        return Ok(());
    };
    events.push(ProgressEvent::TaskStartDelay {
        until: task_start_at.as_str().to_string(),
        remaining,
    });
    if should_log(options.log_level, LogLevel::Info) {
        append_session_log_file(
            log_plan,
            LogLevel::Info,
            &format!("The program will wait until: {}", task_start_at.as_str()),
        )
        .await;
    }
    let deadline = Instant::now()
        .checked_add(remaining)
        .ok_or_else(|| Error::config("task start delay is too large"))?;
    loop {
        cancellation_token.check()?;
        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }
        let sleep_for = (deadline - now).min(Duration::from_secs(1));
        tokio::time::sleep(sleep_for).await;
    }
}

fn default_log_dir() -> PathBuf {
    if let Ok(current_exe) = env::current_exe()
        && let Some(parent) = current_exe.parent()
        && !parent.as_os_str().is_empty()
    {
        return parent.join("Logs");
    }
    env::current_dir()
        .map(|path| path.join("Logs"))
        .unwrap_or_else(|_| PathBuf::from("Logs"))
}

fn log_timestamp() -> String {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    format!(
        "{:04}/{:02}/{:02} {:02}:{:02}:{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

fn log_file_suffix(started_at: &str) -> String {
    started_at
        .chars()
        .map(|ch| if ch.is_ascii_digit() { ch } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn decrypt_runtime_tool_required(options: &DownloadOptions) -> bool {
    !options.keys.is_empty() || options.key_text_file.is_some()
}

#[cfg(feature = "mp4forge")]
fn validate_mp4forge_decrypt_feature() -> Result<()> {
    Ok(())
}

#[cfg(not(feature = "mp4forge"))]
fn validate_mp4forge_decrypt_feature() -> Result<()> {
    Err(Error::config(
        "mp4forge decryption requires the mp4forge feature",
    ))
}

async fn validate_runtime_tool_path(path: &Path, name: &str) -> Result<()> {
    if tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_file())
    {
        return Ok(());
    }
    Err(Error::config(format!(
        "{name} runtime tool path does not exist: {}",
        path.display()
    )))
}

async fn find_runtime_tool(names: &[&str]) -> Option<PathBuf> {
    find_runtime_tool_in_dirs(names, &runtime_tool_search_dirs()).await
}

async fn find_runtime_tool_in_dirs(names: &[&str], directories: &[PathBuf]) -> Option<PathBuf> {
    for directory in directories {
        for name in names {
            for file_name in runtime_tool_file_names(name) {
                let path = directory.join(file_name);
                if tokio::fs::metadata(&path)
                    .await
                    .is_ok_and(|metadata| metadata.is_file())
                {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn runtime_tool_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(current) = env::current_dir() {
        dirs.push(current);
    }
    if let Ok(current_exe) = env::current_exe()
        && let Some(parent) = current_exe.parent()
        && !parent.as_os_str().is_empty()
    {
        dirs.push(parent.to_path_buf());
    }
    if let Some(path) = env::var_os("PATH") {
        dirs.extend(env::split_paths(&path));
    }
    dirs
}

fn runtime_tool_file_names(name: &str) -> Vec<String> {
    if cfg!(windows) && !name.to_ascii_lowercase().ends_with(".exe") {
        vec![format!("{name}.exe")]
    } else {
        vec![name.to_string()]
    }
}

fn validate_live_refresh_request(_streams: &[Stream], options: &DownloadOptions) -> Result<()> {
    if options.live_record_limit.is_none() {
        return Ok(());
    }
    Ok(())
}

fn apply_live_runtime_option_effects(options: &mut DownloadOptions, streams: &[Stream]) {
    let effects = live_option_effects(options, streams);
    if effects.concurrent_download {
        options.concurrent_download = true;
    }
    if effects.mp4_real_time_decryption {
        options.mp4_real_time_decryption = true;
    }
    options.live_fix_vtt_by_audio = effects.live_fix_vtt_by_audio;
}

fn live_streams_need_fmp4_binary_merge(streams: &[Stream]) -> bool {
    streams.iter().any(|stream| {
        stream.media_type != Some(MediaType::Subtitles)
            && stream
                .playlist
                .as_ref()
                .is_some_and(|playlist| playlist.media_init.is_some())
    })
}

fn seed_live_states(
    streams: &mut [Stream],
    extractor_is_hls: bool,
    record_limit: Option<Duration>,
    events: &mut Vec<ProgressEvent>,
) -> Result<Vec<LiveStreamState>> {
    let mut states = Vec::with_capacity(streams.len());
    for (stream_index, stream) in streams.iter_mut().enumerate() {
        let mut state = LiveStreamState::default();
        let mut refresh =
            filter_new_live_segments(stream, &mut state, extractor_is_hls, record_limit)?;
        annotate_live_refresh_events(&mut refresh.events, stream, stream_index, false, None);
        events.extend(refresh.events);
        states.push(state);
    }
    Ok(states)
}

fn record_live_downloaded_durations(
    streams: &[Stream],
    states: &mut [LiveStreamState],
    events: &mut Vec<ProgressEvent>,
) {
    for (stream_index, (stream, state)) in streams.iter().zip(states.iter_mut()).enumerate() {
        let segments = stream_media_segments(stream);
        add_recorded_duration(state, &segments);
        let (recorded_segments, total_segments) = live_display_segment_counts(stream, state);
        events.push(ProgressEvent::LiveRefresh {
            stream_id: Some(live_stream_task_id(stream, stream_index)),
            label: Some(stream_short_label(stream)),
            refreshed_duration: Duration::from_secs(state.refreshed_duration_secs),
            recorded_duration: Duration::from_secs(state.recorded_duration_secs),
            recorded_segments,
            total_segments,
            is_waiting: recorded_segments >= total_segments,
            recorded_size: None,
        });
    }
}

fn live_display_segment_counts(stream: &Stream, state: &LiveStreamState) -> (u64, u64) {
    let init_count = u64::from(
        stream
            .playlist
            .as_ref()
            .is_some_and(|playlist| playlist.media_init.is_some()),
    );
    (
        state.recorded_segments.saturating_add(init_count),
        state.refreshed_segments.saturating_add(init_count),
    )
}

fn probe_live_audio_start_ms(
    streams: &[Stream],
    results: &[StreamDownloadResult],
    options: &DownloadOptions,
) -> Result<Option<i64>> {
    if !options.live_fix_vtt_by_audio {
        return Ok(None);
    }
    let Some((stream, result)) = streams
        .iter()
        .zip(results.iter())
        .find(|(stream, _)| stream.media_type == Some(MediaType::Audio))
    else {
        return Ok(None);
    };
    let Some(path) = first_media_file_for_probe(stream, result) else {
        return Ok(None);
    };
    let binary = options
        .ffmpeg_binary_path
        .clone()
        .unwrap_or_else(|| PathBuf::from("ffmpeg"));
    probe_ffmpeg_start_ms(&binary, path).map(Some)
}

fn first_media_file_for_probe<'a>(
    stream: &Stream,
    result: &'a StreamDownloadResult,
) -> Option<&'a Path> {
    let offset = usize::from(
        stream
            .playlist
            .as_ref()
            .is_some_and(|playlist| playlist.media_init.is_some()),
    );
    result
        .files
        .get(offset)
        .or_else(|| result.files.first())
        .map(PathBuf::as_path)
}

fn probe_ffmpeg_start_ms(binary: &Path, file: &Path) -> Result<i64> {
    let output = StdCommand::new(binary)
        .arg("-hide_banner")
        .arg("-i")
        .arg(file)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| Error::live(error.to_string()))?;
    Ok(parse_ffmpeg_start_ms(&String::from_utf8_lossy(
        &output.stderr,
    )))
}

fn parse_ffmpeg_start_ms(output: &str) -> i64 {
    output
        .lines()
        .find_map(|line| {
            let start = line.find("start: ")? + "start: ".len();
            let value = line.get(start..)?.split(',').next()?.trim();
            parse_seconds_to_ms(value)
        })
        .unwrap_or_default()
}

fn parse_seconds_to_ms(value: &str) -> Option<i64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let whole_ms = whole.trim().parse::<i64>().ok()?.checked_mul(1000)?;
    let mut frac = 0_i64;
    let mut scale = 100_i64;
    for ch in fraction.chars().take(3) {
        if !ch.is_ascii_digit() {
            break;
        }
        frac += i64::from(ch as u8 - b'0') * scale;
        scale /= 10;
    }
    whole_ms.checked_add(frac)
}

async fn apply_media_info_decisions(
    streams: &mut [Stream],
    results: &mut [StreamDownloadResult],
    options: &mut DownloadOptions,
    source_kind: LoadedSourceKind,
    save_dir: &Path,
    events: &mut Vec<ProgressEvent>,
    log_plan: &LogFilePlan,
) -> Result<()> {
    let ffmpeg = options
        .ffmpeg_binary_path
        .clone()
        .ok_or_else(|| Error::config("ffmpeg runtime tool was not found"))?;
    for (index, (stream, result)) in streams.iter_mut().zip(results.iter_mut()).enumerate() {
        let media_infos = if result.media_infos.is_empty() {
            probe_download_media_info(stream, result, source_kind, &ffmpeg).await?
        } else {
            result.media_infos.clone()
        };
        if !media_infos.is_empty() && result.media_infos.is_empty() {
            push_media_info_log(
                events,
                log_plan,
                options.log_level,
                Some(result.stream_id.clone()),
                &media_infos,
            )
            .await;
        }
        apply_change_spec_info(stream, result, options, &media_infos, events);
        result.media_infos = media_infos;
        result.output_path = planned_output_path(save_dir, stream, index, options);
    }
    reserve_unique_output_paths(streams, results);
    Ok(())
}

fn reserve_unique_output_paths(streams: &[Stream], results: &mut [StreamDownloadResult]) {
    let mut reserved = HashSet::new();
    for (stream, result) in streams.iter().zip(results.iter_mut()) {
        result.output_path =
            handle_file_collision_with_reserved(&result.output_path, stream, &reserved);
        reserved.insert(result.output_path.clone());
    }
}

async fn probe_download_media_info(
    stream: &Stream,
    result: &StreamDownloadResult,
    source_kind: LoadedSourceKind,
    ffmpeg: &Path,
) -> Result<Vec<MediaInfo>> {
    let Some(path) = media_info_probe_file(stream, result, source_kind) else {
        return Ok(Vec::new());
    };
    probe_ffmpeg_media_infos(ffmpeg, path).await
}

fn media_info_probe_file<'a>(
    stream: &Stream,
    result: &'a StreamDownloadResult,
    source_kind: LoadedSourceKind,
) -> Option<&'a Path> {
    if source_kind != LoadedSourceKind::Mss
        && stream
            .playlist
            .as_ref()
            .is_some_and(|playlist| playlist.media_init.is_some())
    {
        return result.files.first().map(PathBuf::as_path);
    }
    first_media_file_for_probe(stream, result)
}

fn apply_change_spec_info(
    stream: &mut Stream,
    result: &mut StreamDownloadResult,
    options: &mut DownloadOptions,
    media_infos: &[MediaInfo],
    events: &mut Vec<ProgressEvent>,
) {
    if media_infos.is_empty() {
        return;
    }
    if !options.binary_merge && media_infos.iter().any(|info| info.dolby_vision) {
        options.binary_merge = true;
        result.binary_merge_required = true;
        events.push(ProgressEvent::Warning {
            message: "Dolby Vision content is detected, binary merging is automatically enabled"
                .to_string(),
        });
    }
    if options.mux_after_done.is_some() && media_infos.iter().any(|info| info.dolby_vision) {
        options.mux_after_done = None;
        events.push(ProgressEvent::Warning {
            message: "Dolby Vision content is detected, mux after done is automatically disabled"
                .to_string(),
        });
    }
    if media_infos
        .iter()
        .filter(|info| info.media_type.as_deref() == Some("Audio"))
        .all(|info| {
            info.base_info
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains("aac")
        })
    {
        result.use_aac_filter = true;
    }
    if media_infos
        .iter()
        .all(|info| info.media_type.as_deref() == Some("Audio"))
    {
        stream.media_type = Some(MediaType::Audio);
    } else if media_infos
        .iter()
        .all(|info| info.media_type.as_deref() == Some("Subtitle"))
    {
        stream.media_type = Some(MediaType::Subtitles);
        if stream
            .extension
            .as_deref()
            .is_none_or(|extension| extension.eq_ignore_ascii_case("ts"))
        {
            stream.extension = Some("vtt".to_string());
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn download_live_refreshes(
    base_streams: &[Stream],
    states: &mut [LiveStreamState],
    initial_results: &[StreamDownloadResult],
    request: &DownloadRequest,
    source_kind: LoadedSourceKind,
    config: &mut ParserConfig,
    dirs: &WorkDirectories,
    transport: &DefaultHttpClient,
    live_pipe_session: Option<&mut LivePipeMuxSession>,
    live_audio_start_ms: Option<i64>,
    events: &mut Vec<ProgressEvent>,
) -> Result<()> {
    if states.is_empty() || states.iter().all(|state| state.record_limit_reached) {
        return Ok(());
    }
    let output_paths = initial_results
        .iter()
        .map(|result| result.output_path.clone())
        .collect::<Vec<_>>();
    let pipe_indexes = live_pipe_indexes(base_streams);
    let http_client = http_client(&request.options)?;
    let extractor_is_hls = source_kind == LoadedSourceKind::Hls;
    let mut live_pipe_session = live_pipe_session;
    wait_for_live_refresh(
        compute_live_wait_seconds(base_streams, request.options.live_wait_time),
        &request.cancellation_token,
    )
    .await?;
    while states.iter().any(|state| !state.record_limit_reached) {
        request.cancellation_token.check()?;
        let mut refreshed_streams = base_streams.to_vec();
        let mut retry_events = Vec::new();
        let mut attempt = 0_usize;
        loop {
            attempt = attempt.saturating_add(1);
            match refresh_live_streams(
                &mut refreshed_streams,
                source_kind,
                request,
                config,
                transport,
            )
            .await
            {
                Ok(()) => break,
                Err(error) if attempt < LIVE_REFRESH_RETRY_ATTEMPTS => {
                    retry_events.push(ProgressEvent::Warning {
                        message: format!(
                            "{} ({attempt}/{LIVE_REFRESH_RETRY_ATTEMPTS})",
                            error.compatibility_message()
                        ),
                    });
                    sleep_for_retry(LIVE_REFRESH_RETRY_DELAY, Some(&request.cancellation_token))
                        .await?;
                }
                Err(error) => return Err(error),
            }
        }
        retry_events.extend(parser_diagnostic_events(config));
        let refresh_wait_seconds =
            compute_live_wait_seconds(&refreshed_streams, request.options.live_wait_time);
        let mut batches = Vec::new();
        for (stream_index, (stream, state)) in refreshed_streams
            .iter_mut()
            .zip(states.iter_mut())
            .enumerate()
        {
            if state.record_limit_reached {
                clear_stream_media_segments(stream);
                continue;
            }
            let refresh = filter_new_live_segments(
                stream,
                state,
                extractor_is_hls,
                request.options.live_record_limit,
            )?;
            if refresh.new_segments.is_empty() {
                continue;
            }
            let mut queued_events = retry_events.clone();
            let mut refresh_events = refresh.events;
            annotate_live_refresh_events(&mut refresh_events, stream, stream_index, false, None);
            queued_events.extend(refresh_events);
            batches.push(LiveQueuedSegments {
                stream_index,
                stream: stream.clone(),
                state: state.clone(),
                events: queued_events,
            });
        }
        if batches.is_empty() {
            if states.iter().all(|state| state.record_limit_reached) {
                break;
            }
            wait_for_live_refresh(refresh_wait_seconds, &request.cancellation_token).await?;
            continue;
        }
        for batch in &batches {
            events.extend(batch.events.clone());
        }
        let downloaded = download_live_batches(
            &mut batches,
            request,
            source_kind,
            dirs,
            &config.headers,
            &http_client,
            &output_paths,
            events,
        )
        .await?;
        if let Some(session) = live_pipe_session.as_deref_mut() {
            stream_indexed_downloads_to_live_pipe(
                &downloaded,
                &request.options,
                true,
                live_audio_start_ms,
                events,
                session,
                &pipe_indexes,
                &request.cancellation_token,
            )
            .await?;
        } else {
            let streams = downloaded
                .iter()
                .map(|item| item.stream.clone())
                .collect::<Vec<_>>();
            let results = downloaded
                .iter()
                .map(|item| item.result.clone())
                .collect::<Vec<_>>();
            append_live_refresh_downloads(
                &streams,
                &results,
                &request.options,
                true,
                live_audio_start_ms,
                events,
                &request.cancellation_token,
            )
            .await?;
        }
        record_live_downloaded_indexed(&downloaded, states, events);
        if states.iter().all(|state| state.record_limit_reached) {
            break;
        }
        wait_for_live_refresh(refresh_wait_seconds, &request.cancellation_token).await?;
    }
    Ok(())
}

struct LiveQueuedSegments {
    stream_index: usize,
    stream: Stream,
    state: LiveStreamState,
    events: Vec<ProgressEvent>,
}

struct LiveDownloadedBatch {
    stream_index: usize,
    stream: Stream,
    state: LiveStreamState,
    result: StreamDownloadResult,
}

#[allow(clippy::too_many_arguments)]
async fn download_live_batches(
    batches: &mut [LiveQueuedSegments],
    request: &DownloadRequest,
    source_kind: LoadedSourceKind,
    dirs: &WorkDirectories,
    headers: &BTreeMap<String, String>,
    http_client: &reqwest::Client,
    output_paths: &[PathBuf],
    events: &mut Vec<ProgressEvent>,
) -> Result<Vec<LiveDownloadedBatch>> {
    clean_ad_segments_in_batches(batches, &request.options.ad_keywords)?;
    let shared_events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let emitter = DownloadEventEmitter::default();
    let realtime_hooks = realtime_decrypt_hooks(&request.options, &request.cancellation_token);
    let mut handles = tokio::task::JoinSet::new();
    for batch in batches.iter() {
        let scheduler = DownloadScheduler::new();
        let stream = batch.stream.clone();
        let state = batch.state.clone();
        let stream_index = batch.stream_index;
        let dirs = dirs.clone();
        let options = request.options.clone();
        let headers = headers.clone();
        let http_client = http_client.clone();
        let events = Arc::clone(&shared_events);
        let emitter = emitter.clone();
        let hooks = realtime_hooks.clone();
        handles.spawn(async move {
            let mut local_events = Vec::new();
            let result = scheduler
                .download_stream_with_http_client_and_hooks(
                    &stream,
                    stream_index,
                    &dirs.temp_root,
                    &dirs.save_dir,
                    &options,
                    &headers,
                    source_kind == LoadedSourceKind::Mss,
                    &mut local_events,
                    &http_client,
                    &emitter,
                    &hooks,
                )
                .await;
            if let Ok(mut guard) = events.lock() {
                guard.extend(local_events);
            }
            result.map(|result| LiveDownloadedBatch {
                stream_index,
                stream,
                state,
                result,
            })
        });
    }
    let mut downloaded = Vec::new();
    while let Some(joined) = handles.join_next().await {
        match joined {
            Ok(Ok(mut batch)) => {
                if let Some(output_path) = output_paths.get(batch.stream_index) {
                    batch.result.output_path = output_path.clone();
                }
                downloaded.push(batch);
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(Error::http("live download worker failed")),
        }
    }
    if let Ok(mut guard) = shared_events.lock() {
        events.append(&mut guard);
    }
    downloaded.sort_by_key(|batch| batch.stream_index);
    Ok(downloaded)
}

fn clean_ad_segments_in_batches(
    batches: &mut [LiveQueuedSegments],
    ad_keywords: &[String],
) -> Result<()> {
    if ad_keywords.is_empty() {
        return Ok(());
    }
    let mut streams = batches
        .iter()
        .map(|batch| batch.stream.clone())
        .collect::<Vec<_>>();
    clean_ad_segments(&mut streams, ad_keywords)?;
    for (batch, stream) in batches.iter_mut().zip(streams) {
        batch.stream = stream;
    }
    Ok(())
}

fn record_live_downloaded_indexed(
    downloaded: &[LiveDownloadedBatch],
    states: &mut [LiveStreamState],
    events: &mut Vec<ProgressEvent>,
) {
    for item in downloaded {
        let Some(state) = states.get_mut(item.stream_index) else {
            continue;
        };
        let recorded_duration_secs = state.recorded_duration_secs;
        let recorded_segments = state.recorded_segments;
        *state = item.state.clone();
        state.recorded_duration_secs = recorded_duration_secs;
        state.recorded_segments = recorded_segments;
        let segments = stream_media_segments(&item.stream);
        add_recorded_duration(state, &segments);
        let (recorded_segments, total_segments) = live_display_segment_counts(&item.stream, state);
        events.push(ProgressEvent::LiveRefresh {
            stream_id: Some(item.result.stream_id.clone()),
            label: Some(stream_short_label(&item.stream)),
            refreshed_duration: Duration::from_secs(state.refreshed_duration_secs),
            recorded_duration: Duration::from_secs(state.recorded_duration_secs),
            recorded_segments,
            total_segments,
            is_waiting: recorded_segments >= total_segments,
            recorded_size: None,
        });
    }
}

async fn refresh_live_streams(
    streams: &mut [Stream],
    source_kind: LoadedSourceKind,
    request: &DownloadRequest,
    config: &mut ParserConfig,
    transport: &DefaultHttpClient,
) -> Result<()> {
    match source_kind {
        LoadedSourceKind::Hls => hls_parser_for_transport(transport)
            .refresh_playlists(streams, config)
            .await
            .map(|_| ()),
        LoadedSourceKind::Dash => {
            let loaded = reload_live_manifest(source_kind, request, config, transport).await?;
            DashParser::new().refresh_streams(streams, &loaded.text, &loaded.final_url, config)
        }
        LoadedSourceKind::Mss => {
            let loaded = reload_live_manifest(source_kind, request, config, transport).await?;
            MssParser::new().refresh_streams(streams, &loaded.text, &loaded.final_url, config)
        }
        LoadedSourceKind::HttpLiveTs => Ok(()),
        LoadedSourceKind::BinaryData => Ok(()),
    }
}

async fn reload_live_manifest(
    expected_kind: LoadedSourceKind,
    request: &DownloadRequest,
    config: &mut ParserConfig,
    transport: &DefaultHttpClient,
) -> Result<crate::source::LoadedSource> {
    let original_url = config.original_url.clone();
    let current_url = if config.url.is_empty() {
        request.input.clone()
    } else {
        config.url.clone()
    };
    let loader = source_loader_for_transport(transport);
    match loader.load_source(&current_url, config).await {
        Ok(loaded) => {
            if !original_url.is_empty() {
                config.original_url = original_url;
            }
            ensure_loaded_kind(expected_kind, loaded)
        }
        Err(first_error) if !original_url.is_empty() && original_url != current_url => {
            let loaded = loader
                .load_source(&original_url, config)
                .await
                .map_err(|_| first_error)?;
            config.original_url = original_url;
            ensure_loaded_kind(expected_kind, loaded)
        }
        Err(error) => Err(error),
    }
}

fn ensure_loaded_kind(
    expected_kind: LoadedSourceKind,
    loaded: crate::source::LoadedSource,
) -> Result<crate::source::LoadedSource> {
    if loaded.kind == expected_kind {
        Ok(loaded)
    } else {
        Err(Error::live("live refresh source kind changed"))
    }
}

fn clear_stream_media_segments(stream: &mut Stream) {
    if let Some(playlist) = stream.playlist.as_mut() {
        let Some(part) = playlist.media_parts.first_mut() else {
            return;
        };
        part.media_segments.clear();
        playlist.media_parts.truncate(1);
    }
}

async fn wait_for_live_refresh(
    wait_seconds: u32,
    cancellation_token: &crate::cancellation::CancellationToken,
) -> Result<()> {
    let wait = Duration::from_secs(u64::from(wait_seconds.max(1)));
    let started = Instant::now();
    loop {
        cancellation_token.check()?;
        let elapsed = started.elapsed();
        if elapsed >= wait {
            return Ok(());
        }
        tokio::time::sleep(wait.saturating_sub(elapsed).min(Duration::from_millis(50))).await;
    }
}

async fn append_live_refresh_downloads(
    streams: &[Stream],
    results: &[StreamDownloadResult],
    options: &DownloadOptions,
    is_live_session: bool,
    live_audio_start_ms: Option<i64>,
    events: &mut Vec<ProgressEvent>,
    cancellation_token: &CancellationToken,
) -> Result<()> {
    if is_live_session && !options.live_real_time_merge {
        for result in results {
            if should_cleanup_stream_temp_dir(options, is_live_session) {
                cleanup_stream_temp_dir(&result.temp_dir, events).await?;
            }
        }
        return Ok(());
    }
    for (index, result) in results.iter().enumerate() {
        if let Some(stream) = streams.get(index) {
            decrypt_stream_files(stream, result, options, events, cancellation_token).await?;
        }
        if result.files.is_empty() {
            if should_cleanup_stream_temp_dir(options, is_live_session) {
                cleanup_stream_temp_dir(&result.temp_dir, events).await?;
            }
            continue;
        }
        if options.skip_merge {
            for path in &result.files {
                events.push(ProgressEvent::OutputArtifact(OutputArtifact::new(
                    path.clone(),
                )));
            }
            continue;
        }
        if let Some(stream) = streams.get(index)
            && should_format_subtitle(stream, options)
        {
            events.push(ProgressEvent::MergeProgress {
                stream_id: Some(result.stream_id.clone()),
                message: "appending refreshed live segments".to_string(),
            });
            append_live_subtitle_refresh(
                stream,
                result,
                options,
                SubtitleTimingContext {
                    is_live_session,
                    live_audio_start_ms,
                },
                events,
            )
            .await?;
            events.push(ProgressEvent::OutputArtifact(OutputArtifact::new(
                result.output_path.clone(),
            )));
            if should_cleanup_stream_temp_dir(options, is_live_session) {
                cleanup_stream_temp_dir(&result.temp_dir, events).await?;
            }
            continue;
        }
        events.push(ProgressEvent::MergeProgress {
            stream_id: Some(result.stream_id.clone()),
            message: "appending refreshed live segments".to_string(),
        });
        append_files(&result.files, &result.output_path).await?;
        events.push(ProgressEvent::OutputArtifact(OutputArtifact::new(
            result.output_path.clone(),
        )));
        if should_cleanup_stream_temp_dir(options, is_live_session) {
            cleanup_stream_temp_dir(&result.temp_dir, events).await?;
        }
    }
    Ok(())
}

fn should_cleanup_stream_temp_dir(options: &DownloadOptions, is_live_session: bool) -> bool {
    options.del_after_done && !(is_live_session && options.live_keep_segments)
}

fn should_cleanup_task_temp_root(options: &DownloadOptions, is_live_session: bool) -> bool {
    if !options.del_after_done {
        return false;
    }
    if options.skip_merge || options.skip_download {
        return false;
    }
    if is_live_session && options.live_keep_segments {
        return false;
    }
    true
}

async fn append_files(files: &[PathBuf], output_path: &Path) -> Result<()> {
    if let Some(parent) = output_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut output = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(output_path)
        .await?;
    for file in files {
        let mut input = tokio::fs::File::open(file).await?;
        tokio::io::copy(&mut input, &mut output).await?;
    }
    Ok(())
}

async fn execute_http_live_ts(
    request: &DownloadRequest,
    final_url: &str,
    headers: &BTreeMap<String, String>,
    dirs: &WorkDirectories,
    mut events: Vec<ProgressEvent>,
    log_plan: &LogFilePlan,
) -> Result<Vec<ProgressEvent>> {
    let stream = direct_http_live_stream(final_url, final_url);
    events.push(ProgressEvent::ManifestParsed { stream_count: 1 });
    push_extracted_stream_logs(
        &mut events,
        log_plan,
        request.options.log_level,
        std::slice::from_ref(&stream),
    )
    .await;
    events.push(ProgressEvent::StreamSelected {
        stream_id: stream_identity(&stream),
    });
    push_selected_stream_logs(
        &mut events,
        log_plan,
        request.options.log_level,
        std::slice::from_ref(&stream),
    )
    .await;
    if request.options.write_meta_json {
        push_session_log(
            &mut events,
            log_plan,
            request.options.log_level,
            LogLevel::Warn,
            "Writing meta json",
        )
        .await;
        let mut selected_file = BTreeMap::new();
        selected_file.insert(
            "meta_selected.json".to_string(),
            streams_metadata_json(std::slice::from_ref(&stream)),
        );
        let _ = write_raw_files(&selected_file, &dirs.temp_root).await?;
    }
    if request.options.skip_download {
        events.push(ProgressEvent::Finished { success: true });
        return Ok(events);
    }
    push_save_name_log(
        &mut events,
        log_plan,
        request.options.log_level,
        request.options.save_name.as_deref(),
    )
    .await;
    if let Some(limit) = request.options.live_record_limit {
        push_session_log(
            &mut events,
            log_plan,
            request.options.log_level,
            LogLevel::Warn,
            format!(
                "Live recording duration limit: {}",
                format_duration_short_for_log(limit)
            ),
        )
        .await;
    }
    let output_path = direct_http_live_output_path(final_url, &dirs.save_dir, &request.options);
    let dir_name = direct_http_live_dir_name(&request.options, &stream, 0);
    let save_name = direct_http_live_save_name(&request.options, &stream, final_url, 0);
    push_session_log(
        &mut events,
        log_plan,
        request.options.log_level,
        LogLevel::Debug,
        format!(
            "dirName: {}; saveDir: {}; saveName: {}",
            dir_name,
            dirs.save_dir.display(),
            save_name
        ),
    )
    .await;
    push_session_log(
        &mut events,
        log_plan,
        request.options.log_level,
        LogLevel::Debug,
        format_request_headers_debug_message(headers),
    )
    .await;
    events.push(ProgressEvent::StreamTaskCreated {
        stream_id: stream_identity(&stream),
        label: stream_full_label(&stream),
    });
    events.push(ProgressEvent::SegmentQueued {
        stream_id: stream_identity(&stream),
        segment_index: 0,
    });
    events.push(ProgressEvent::SegmentStarted {
        stream_id: stream_identity(&stream),
        segment_index: 0,
    });
    record_http_live_ts_to_file(
        final_url,
        headers,
        &request.options,
        &request.cancellation_token,
        &output_path,
        &mut events,
    )
    .await?;
    if let Ok(metadata) = tokio::fs::metadata(&output_path).await {
        push_session_log(
            &mut events,
            log_plan,
            request.options.log_level,
            LogLevel::Info,
            format!("File Size: {}", format_console_file_size(metadata.len())),
        )
        .await;
    }
    events.push(ProgressEvent::SegmentFinished {
        stream_id: stream_identity(&stream),
        segment_index: 0,
    });
    events.push(ProgressEvent::OutputArtifact(OutputArtifact::new(
        output_path,
    )));
    events.push(ProgressEvent::Finished { success: true });
    Ok(events)
}

#[allow(clippy::too_many_arguments)]
async fn record_http_live_ts_to_file(
    url: &str,
    headers: &BTreeMap<String, String>,
    options: &DownloadOptions,
    cancellation_token: &crate::cancellation::CancellationToken,
    output_path: &Path,
    events: &mut Vec<ProgressEvent>,
) -> Result<()> {
    if let Some(parent) = output_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let client = http_live_client(options)?;
    let mut current_url = url.to_string();
    cancellation_token.check()?;
    let mut request = client.get(&current_url);
    request = apply_request_headers(request, headers);
    let response = request
        .send()
        .await
        .map_err(|error| Error::http(error.to_string()))?;
    current_url = response.url().as_str().to_string();
    let status = response.status().as_u16();
    if !(200..=299).contains(&status) {
        let _ = response.bytes().await;
        return Err(Error::http(format!(
            "HTTP status {status} for {current_url}"
        )));
    }
    let mut reader = response;
    let mut output = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(output_path)
        .await?;
    let started = Instant::now();
    let mut last_progress = started;
    let mut last_progress_bytes = 0_u64;
    let mut state = HttpLiveTsState::default();
    let mut info_buffer = Vec::with_capacity(188 * 5000);
    let mut service_info_emitted = false;
    loop {
        cancellation_token.check()?;
        let Some(chunk) = reader
            .chunk()
            .await
            .map_err(|error| Error::http(error.to_string()))?
        else {
            break;
        };
        let size = chunk.len();
        if !service_info_emitted && info_buffer.len() < 188 * 5000 {
            let remaining = (188 * 5000_usize).saturating_sub(info_buffer.len());
            info_buffer.extend_from_slice(&chunk[..size.min(remaining)]);
            if let Some(info) = parse_http_live_ts_service_info(&info_buffer) {
                events.push(ProgressEvent::LiveServiceInfo {
                    program_id: info.program_id,
                    service_provider: non_empty_string(info.service_provider),
                    service_name: non_empty_string(info.service_name),
                });
                service_info_emitted = true;
            }
        }
        tokio::io::AsyncWriteExt::write_all(&mut output, &chunk).await?;
        let elapsed = started.elapsed();
        update_http_live_ts_state(&mut state, size, elapsed, options.live_record_limit);
        emit_http_live_progress(
            events,
            &state,
            &mut last_progress,
            &mut last_progress_bytes,
            false,
        );
        if state.stop_requested {
            break;
        }
    }
    emit_http_live_progress(
        events,
        &state,
        &mut last_progress,
        &mut last_progress_bytes,
        true,
    );
    tokio::io::AsyncWriteExt::flush(&mut output).await?;
    Ok(())
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct HttpLiveTsServiceInfo {
    program_id: String,
    service_provider: String,
    service_name: String,
}

fn parse_http_live_ts_service_info(data: &[u8]) -> Option<HttpLiveTsServiceInfo> {
    let mut info = HttpLiveTsServiceInfo::default();
    for offset in 0..data.len().saturating_sub(188) {
        if data.get(offset) != Some(&0x47) || data.get(offset + 188) != Some(&0x47) {
            continue;
        }
        let packet = data.get(offset..offset + 188)?;
        let header = u32::from_be_bytes([packet[0], packet[1], packet[2], packet[3]]);
        let pid = (header & 0x1fff00) >> 8;
        let payload = packet.get(4..)?;
        if pid == 0 {
            if let Some(program_id) = read_be_u16(payload.get(9..11)?) {
                info.program_id = program_id.to_string();
            }
        } else if pid == 0x0011
            && payload.get(1) == Some(&0x42)
            && let Some(section_length) =
                read_be_u16(payload.get(2..4)?).map(|value| value & 0x0fff)
        {
            let section_len = usize::from(section_length);
            let section = payload.get(4..4 + section_len)?;
            let descriptor_root = section.get(8..)?;
            let descriptor_loop_length =
                usize::from(read_be_u16(descriptor_root.get(3..5)?)? & 0x0fff);
            let descriptors = descriptor_root.get(5..5 + descriptor_loop_length)?;
            let provider_len = usize::from(*descriptors.get(3)?);
            let provider_start = 4;
            let provider_end = provider_start + provider_len;
            let name_len = usize::from(*descriptors.get(provider_end)?);
            let name_start = provider_end + 1;
            let name_end = name_start + name_len;
            info.service_provider =
                String::from_utf8_lossy(descriptors.get(provider_start..provider_end)?).to_string();
            info.service_name =
                String::from_utf8_lossy(descriptors.get(name_start..name_end)?).to_string();
        }
        if !info.program_id.is_empty()
            && (!info.service_name.is_empty() || !info.service_provider.is_empty())
        {
            return Some(info);
        }
    }
    None
}

fn read_be_u16(bytes: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes([*bytes.first()?, *bytes.get(1)?]))
}

fn non_empty_string(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}

fn emit_http_live_progress(
    events: &mut Vec<ProgressEvent>,
    state: &HttpLiveTsState,
    last_progress: &mut Instant,
    last_progress_bytes: &mut u64,
    force: bool,
) {
    let elapsed = last_progress.elapsed();
    if !force && elapsed < Duration::from_secs(1) {
        return;
    }
    let elapsed_secs = elapsed.as_secs().max(1);
    let bytes_delta = state
        .recording_size_bytes
        .saturating_sub(*last_progress_bytes);
    let first_report = *last_progress_bytes == 0 && state.recording_size_bytes > 0;
    let display_size = if force && first_report && state.recording_duration_secs == 0 {
        0
    } else {
        state.recording_size_bytes
    };
    let display_speed = if force && first_report && state.recording_duration_secs == 0 {
        0
    } else {
        bytes_delta / elapsed_secs
    };
    *last_progress = Instant::now();
    *last_progress_bytes = state.recording_size_bytes;
    events.push(ProgressEvent::AggregateProgress(AggregateProgress {
        downloaded_bytes: display_size,
        total_bytes: None,
        bytes_per_second: display_speed,
    }));
    let duration = Duration::from_secs(state.recording_duration_secs);
    events.push(ProgressEvent::LiveRefresh {
        stream_id: Some(HTTP_LIVE_TS_MARKER.to_string()),
        label: Some(format!("Vid Kbps | {HTTP_LIVE_TS_MARKER}")),
        refreshed_duration: duration,
        recorded_duration: duration,
        recorded_segments: 0,
        total_segments: 1,
        is_waiting: false,
        recorded_size: Some(display_size),
    });
}

fn format_console_file_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit_index = 0_usize;
    while value >= 1024.0 && unit_index + 1 < UNITS.len() {
        value /= 1024.0;
        unit_index += 1;
    }
    format!("{value:.2}{}", UNITS[unit_index])
}

fn http_live_client(_options: &DownloadOptions) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(100))
        .connect_timeout(Duration::from_secs(100))
        .redirect(reqwest::redirect::Policy::limited(50))
        .build()
        .map_err(|error| Error::http(error.to_string()))
}

fn direct_http_live_output_path(
    input: &str,
    save_dir: &Path,
    options: &DownloadOptions,
) -> PathBuf {
    let stream = direct_http_live_stream(input, input);
    let name = direct_http_live_save_name(options, &stream, input, 0);
    save_dir.join(format!("{name}.ts"))
}

fn direct_http_live_save_name(
    options: &DownloadOptions,
    stream: &Stream,
    input: &str,
    task_id: usize,
) -> String {
    if let Some(pattern) = &options.save_pattern
        && !pattern.trim().is_empty()
    {
        return format_save_pattern(pattern, stream, options.save_name.as_deref(), task_id);
    }
    if let Some(value) = options.save_name.as_ref().filter(|value| !value.is_empty()) {
        return format!(
            "{}.{}",
            valid_file_name(value, "_", false),
            stream.language.as_deref().unwrap_or_default()
        )
        .trim_end_matches('.')
        .to_string();
    }
    save_name_from_input(input, false)
}

fn direct_http_live_dir_name(options: &DownloadOptions, stream: &Stream, task_id: usize) -> String {
    let base_name = options
        .save_name
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|value| valid_file_name(value, "_", false))
        .unwrap_or_else(|| "haki-dl".to_string());
    format!(
        "{}_{}_{}_{}_{}_{}",
        base_name,
        task_id,
        valid_file_name(stream.group_id.as_deref().unwrap_or_default(), "-", false),
        stream.codecs.as_deref().unwrap_or_default(),
        stream
            .bandwidth
            .map(|value| value.to_string())
            .unwrap_or_default(),
        stream.language.as_deref().unwrap_or_default()
    )
}

fn direct_http_live_stream(input: &str, original_url: &str) -> Stream {
    Stream {
        group_id: Some(HTTP_LIVE_TS_MARKER.to_string()),
        url: input.to_string(),
        original_url: original_url.to_string(),
        playlist: Some(crate::manifest::Playlist::default()),
        ..Stream::default()
    }
}

async fn write_source_sidecars(
    directory: &Path,
    raw_files: &BTreeMap<String, String>,
    raw_streams: &[Stream],
) -> Result<Vec<PathBuf>> {
    let mut files = raw_files.clone();
    files.insert("meta.json".to_string(), streams_metadata_json(raw_streams));
    write_raw_files(&files, directory).await
}

fn annotate_live_refresh_events(
    events: &mut [ProgressEvent],
    stream: &Stream,
    stream_index: usize,
    is_waiting: bool,
    recorded_size: Option<u64>,
) {
    let stream_id_value = live_stream_task_id(stream, stream_index);
    let label_value = stream_short_label(stream);
    for event in events {
        if let ProgressEvent::LiveRefresh {
            stream_id,
            label,
            is_waiting: event_is_waiting,
            recorded_size: event_recorded_size,
            ..
        } = event
        {
            if stream_id.is_none() {
                *stream_id = Some(stream_id_value.clone());
            }
            if label.is_none() {
                *label = Some(label_value.clone());
            }
            *event_is_waiting = is_waiting;
            *event_recorded_size = recorded_size;
        }
    }
}

fn live_stream_task_id(stream: &Stream, task_id: usize) -> String {
    if !stream.id.is_empty() {
        return stream.id.clone();
    }
    stream
        .group_id
        .clone()
        .unwrap_or_else(|| format!("stream-{task_id}"))
}

fn format_duration_short_for_log(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds / 60) % 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours:02}h{minutes:02}m{seconds:02}s")
    } else {
        format!("{minutes:02}m{seconds:02}s")
    }
}

fn prepare_work_directories(options: &DownloadOptions) -> Result<WorkDirectories> {
    let save_dir = match &options.save_dir {
        Some(path) => path.clone(),
        None => std::env::current_dir()?,
    };
    let temp_base = match &options.tmp_dir {
        Some(path) => path.clone(),
        None => std::env::current_dir()?,
    };
    let temp_name = options
        .save_name
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|value| valid_file_name(value, "_", false))
        .unwrap_or_else(|| "haki-dl".to_string());
    let temp_root = temp_base.join(temp_name);
    Ok(WorkDirectories {
        temp_root,
        save_dir,
    })
}

async fn parse_loaded_manifest(
    text: &str,
    kind: LoadedSourceKind,
    final_url: &str,
    config: &ParserConfig,
) -> Result<Manifest> {
    match kind {
        LoadedSourceKind::Hls => {
            let parsed = HlsParser::new().parse(text, final_url, config).await?;
            let warnings = if parsed.is_master {
                vec!["Master List detected, try parse all streams".to_string()]
            } else {
                Vec::new()
            };
            Ok(Manifest {
                extractor_type: Some(ExtractorType::Hls),
                source: Some(final_url.to_string()),
                is_live: parsed.streams.iter().any(stream_is_live),
                streams: parsed.streams,
                warnings,
            })
        }
        LoadedSourceKind::Dash => {
            let parsed = DashParser::new().parse(text, final_url, config)?;
            Ok(Manifest {
                extractor_type: Some(ExtractorType::MpegDash),
                source: Some(final_url.to_string()),
                is_live: parsed.is_dynamic,
                streams: parsed.streams,
                warnings: Vec::new(),
            })
        }
        LoadedSourceKind::Mss => {
            let parsed = MssParser::new().parse(text, final_url, config)?;
            Ok(Manifest {
                extractor_type: Some(ExtractorType::Mss),
                source: Some(final_url.to_string()),
                is_live: parsed.is_live,
                streams: parsed.streams,
                warnings: parsed.warnings,
            })
        }
        LoadedSourceKind::HttpLiveTs => Err(Error::live(
            "direct HTTP live TS execution requires the live recorder pipeline",
        )),
        LoadedSourceKind::BinaryData => Err(Error::compatibility("Input not supported")),
    }
}

fn selected_requires_playlist_fetch(kind: LoadedSourceKind, selected: &[Stream]) -> bool {
    selected.iter().any(|stream| stream.playlist.is_none())
        || matches!(kind, LoadedSourceKind::Dash | LoadedSourceKind::Mss)
}

fn select_streams(streams: &[Stream], request: &DownloadRequest) -> Result<Vec<Stream>> {
    let mut streams = streams.to_vec();
    order_streams(&mut streams);
    let (mut basic_streams, mut audios, mut subtitles) = split_stream_groups(&streams);
    basic_streams = apply_drop_filter_list(basic_streams, &request.options.drop_video)?;
    audios = apply_drop_filter_list(audios, &request.options.drop_audio)?;
    subtitles = apply_drop_filter_list(subtitles, &request.options.drop_subtitle)?;
    let mut filtered = Vec::new();
    filtered.extend(basic_streams.clone());
    filtered.extend(audios.clone());
    filtered.extend(subtitles.clone());
    let selected = match &request.stream_selector {
        StreamSelector::Interactive if has_keep_filters(&request.options) => {
            let mut selected = Vec::new();
            selected.extend(apply_keep_filter_list(
                &basic_streams,
                &request.options.select_video,
            )?);
            selected.extend(apply_keep_filter_list(
                &audios,
                &request.options.select_audio,
            )?);
            selected.extend(apply_keep_filter_list(
                &subtitles,
                &request.options.select_subtitle,
            )?);
            selected
        }
        StreamSelector::Interactive => select_streams_interactively(&filtered, &request.options)?,
        StreamSelector::Auto => auto_select_streams(&filtered),
        StreamSelector::SubtitlesOnly => subtitle_only_streams(&filtered),
        StreamSelector::ExplicitIds(ids) => filtered
            .into_iter()
            .filter(|stream| ids.iter().any(|id| stream_matches_id(stream, id)))
            .collect(),
    };
    Ok(selected)
}

fn split_stream_groups(streams: &[Stream]) -> (Vec<Stream>, Vec<Stream>, Vec<Stream>) {
    let basic_streams = streams
        .iter()
        .filter(|stream| stream.media_type.is_none() || stream.media_type == Some(MediaType::Video))
        .cloned()
        .collect();
    let audios = streams
        .iter()
        .filter(|stream| stream.media_type == Some(MediaType::Audio))
        .cloned()
        .collect();
    let subtitles = streams
        .iter()
        .filter(|stream| stream.media_type == Some(MediaType::Subtitles))
        .cloned()
        .collect();
    (basic_streams, audios, subtitles)
}

fn has_keep_filters(options: &DownloadOptions) -> bool {
    !options.select_video.is_empty()
        || !options.select_audio.is_empty()
        || !options.select_subtitle.is_empty()
}

fn apply_drop_filter_list(
    mut streams: Vec<Stream>,
    filters: &[crate::config::StreamFilter],
) -> Result<Vec<Stream>> {
    for filter in filters {
        streams = filter_drop(&streams, Some(filter))?;
    }
    Ok(streams)
}

fn apply_keep_filter_list(
    streams: &[Stream],
    filters: &[crate::config::StreamFilter],
) -> Result<Vec<Stream>> {
    let mut selected = Vec::new();
    for filter in filters {
        selected.extend(filter_keep(streams, Some(filter))?);
    }
    Ok(selected)
}

fn select_streams_interactively(
    streams: &[Stream],
    options: &DownloadOptions,
) -> Result<Vec<Stream>> {
    if streams.len() <= 1
        || ((!std::io::stdin().is_terminal() || !std::io::stdout().is_terminal())
            && !options.force_ansi_console)
    {
        return Ok(interactive_default_streams(streams));
    }
    let defaults = interactive_default_streams(streams);
    let default_indexes = streams
        .iter()
        .enumerate()
        .filter_map(|(index, stream)| {
            defaults
                .iter()
                .any(|default| stream_identity(default) == stream_identity(stream))
                .then_some(index)
        })
        .collect::<Vec<_>>();

    #[cfg(feature = "cli")]
    let selected_indexes =
        crate::interactive_prompt::select_streams(streams, &default_indexes, options)?;
    #[cfg(not(feature = "cli"))]
    let selected_indexes = {
        let _ = options;
        default_indexes
    };

    let selected = selected_indexes
        .into_iter()
        .filter_map(|index| streams.get(index).cloned())
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Err(Error::config("interactive selection cannot be empty"));
    }
    Ok(selected)
}

fn validate_explicit_mp4forge_before_download(
    streams: &[Stream],
    options: &DownloadOptions,
) -> Result<()> {
    let Some(mux_options) = &options.mux_after_done else {
        return Ok(());
    };
    if mux_options.muxer != MuxerKind::Mp4forge {
        return Ok(());
    }
    if mux_options.format != MuxFormat::Mp4 {
        return Err(Error::mux(
            "mp4forge mux-after-done supports only mp4 output",
        ));
    }
    let has_fallback = mux_options.fallback_muxer.is_some();
    let matrix = Mp4forgeSupportMatrix::default();
    for stream in streams {
        if mux_options.skip_sub && stream.media_type == Some(MediaType::Subtitles) {
            continue;
        }
        if stream.media_type == Some(MediaType::Subtitles) {
            if has_fallback {
                continue;
            }
            return Err(Error::mux(
                "mp4forge mux-after-done does not support subtitle inputs; use fallback_muxer=ffmpeg or skip_sub=true",
            ));
        }
        let support = match mp4forge_support_for_stream(stream, &matrix) {
            Ok(support) => support,
            Err(error) if has_fallback => {
                let _ = error;
                continue;
            }
            Err(error) => {
                return Err(Error::mux(format!(
                    "{}; use fallback_muxer=ffmpeg for mp4forge mux-after-done",
                    error.compatibility_message()
                )));
            }
        };
        match support {
            crate::mux::Mp4forgeSupport::Supported => {}
            crate::mux::Mp4forgeSupport::Unsupported { reason } => {
                if !has_fallback {
                    return Err(Error::mux(format!(
                        "{reason}; use fallback_muxer=ffmpeg for mp4forge mux-after-done"
                    )));
                }
            }
            crate::mux::Mp4forgeSupport::RequiresProbe { reason } => {
                if !has_fallback {
                    return Err(Error::mux(format!(
                        "mp4forge requires media probing before download: {reason}; use fallback_muxer=ffmpeg for mp4forge mux-after-done"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn validate_mp4forge_decrypt_before_download(
    streams: &[Stream],
    options: &DownloadOptions,
) -> Result<()> {
    if options.decryption_engine != DecryptionEngine::Mp4forge
        || !decrypt_runtime_tool_required(options)
    {
        return Ok(());
    }
    for stream in streams {
        if !stream_requires_external_decrypt(stream) {
            continue;
        }
        validate_mp4forge_decrypt_stream(stream)?;
    }
    Ok(())
}

fn validate_mp4forge_decrypt_stream(stream: &Stream) -> Result<()> {
    if matches!(
        stream.media_type,
        Some(MediaType::Subtitles | MediaType::ClosedCaptions)
    ) {
        return Err(Error::decrypt(
            "mp4forge decrypt does not support subtitle inputs",
        ));
    }
    let extension = stream_mp4_family_extension(stream).ok_or_else(|| {
        Error::decrypt("mp4forge decrypt requires MP4-family container metadata before download")
    })?;
    if !mp4forge_decrypt_extension_supported(&extension) {
        return Err(Error::decrypt(
            "mp4forge decrypt supports only MP4-family encrypted inputs",
        ));
    }
    validate_mp4forge_decrypt_schemes(stream)?;
    Ok(())
}

fn validate_mp4forge_decrypt_schemes(stream: &Stream) -> Result<()> {
    for segment in stream_encryption_segments(stream) {
        if !matches!(
            segment.encryption.method,
            EncryptionMethod::Cenc | EncryptionMethod::SampleAes
        ) {
            continue;
        }
        if let Some(scheme) = segment.encryption.scheme.as_deref()
            && !scheme.is_empty()
            && !mp4forge_decrypt_scheme_supported(scheme)
        {
            return Err(Error::decrypt(format!(
                "mp4forge decrypt does not support encryption scheme: {scheme}"
            )));
        }
    }
    Ok(())
}

fn stream_mp4_family_extension(stream: &Stream) -> Option<String> {
    stream
        .extension
        .as_deref()
        .and_then(normalized_extension)
        .or_else(|| {
            stream
                .playlist
                .as_ref()
                .and_then(|playlist| playlist.media_init.as_ref())
                .and_then(|segment| normalized_extension_from_url(&segment.url))
        })
        .or_else(|| {
            stream.playlist.as_ref().and_then(|playlist| {
                playlist
                    .media_parts
                    .iter()
                    .flat_map(|part| part.media_segments.iter())
                    .find_map(|segment| normalized_extension_from_url(&segment.url))
            })
        })
}

fn normalized_extension(value: &str) -> Option<String> {
    value
        .trim()
        .trim_start_matches('.')
        .split(['?', '#'])
        .next()
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
}

fn normalized_extension_from_url(value: &str) -> Option<String> {
    let path = value.split(['?', '#']).next().unwrap_or(value);
    let name = path
        .rsplit(['/', '\\'])
        .next()
        .filter(|value| !value.is_empty())?;
    name.rsplit_once('.')
        .map(|(_, extension)| extension)
        .and_then(normalized_extension)
}

fn mp4forge_decrypt_extension_supported(extension: &str) -> bool {
    matches!(extension, "mp4" | "m4s" | "m4v" | "m4a" | "cmfv" | "cmfa")
}

fn mp4forge_decrypt_scheme_supported(scheme: &str) -> bool {
    matches!(
        scheme.to_ascii_lowercase().as_str(),
        "cenc" | "cens" | "cbc1" | "cbcs" | "piff"
    )
}

async fn finalize_downloads(
    streams: &[Stream],
    results: &[StreamDownloadResult],
    options: &DownloadOptions,
    is_live_session: bool,
    live_audio_start_ms: Option<i64>,
    events: &mut Vec<ProgressEvent>,
    cancellation_token: &CancellationToken,
) -> Result<Vec<OutputFile>> {
    let mut merged_outputs = Vec::new();
    for (index, result) in results.iter().enumerate() {
        let effective_options;
        let options = if result.disable_real_time_decryption && options.mp4_real_time_decryption {
            effective_options = DownloadOptions {
                mp4_real_time_decryption: false,
                ..options.clone()
            };
            &effective_options
        } else {
            options
        };
        if let Some(stream) = streams.get(index) {
            decrypt_stream_files(stream, result, options, events, cancellation_token).await?;
        }
        if options.skip_merge {
            for path in &result.files {
                events.push(ProgressEvent::OutputArtifact(OutputArtifact::new(
                    path.clone(),
                )));
            }
            continue;
        }
        let subtitle_write = if let Some(stream) = streams.get(index) {
            write_subtitle_output_if_needed(
                stream,
                &result.files,
                &result.output_path,
                &result.temp_dir,
                options,
                SubtitleTimingContext {
                    is_live_session,
                    live_audio_start_ms,
                },
                events,
            )
            .await?
        } else {
            SubtitleWriteOutcome::default()
        };
        let output_path = if subtitle_write.wrote {
            result.output_path.clone()
        } else {
            merge_stream_files(
                streams.get(index),
                result,
                options,
                is_live_session,
                events,
                cancellation_token,
            )
            .await?
        };
        events.push(ProgressEvent::OutputArtifact(OutputArtifact::new(
            output_path.clone(),
        )));
        if let Some(stream) = streams.get(index) {
            merged_outputs.push(output_file_from_stream(index, stream, result, &output_path));
        }
        if should_cleanup_stream_temp_dir(options, is_live_session)
            && !subtitle_write.retain_temp_dir
        {
            cleanup_stream_temp_dir(&result.temp_dir, events).await?;
        }
    }
    Ok(merged_outputs)
}

async fn merge_stream_files(
    stream: Option<&Stream>,
    result: &StreamDownloadResult,
    options: &DownloadOptions,
    is_live_session: bool,
    events: &mut Vec<ProgressEvent>,
    cancellation_token: &CancellationToken,
) -> Result<PathBuf> {
    if should_binary_merge_stream(stream, result, options, is_live_session) {
        events.push(ProgressEvent::MergeProgress {
            stream_id: Some(result.stream_id.clone()),
            message: "Binary merging...".to_string(),
        });
        let files = merge_input_files(stream, result, options);
        combine_files(&files, &result.output_path).await?;
        return external_decrypt_if_needed(
            stream,
            &result.output_path,
            options,
            events,
            cancellation_token,
        )
        .await;
    }

    let ffmpeg = options
        .ffmpeg_binary_path
        .as_deref()
        .ok_or_else(|| Error::config("ffmpeg runtime tool was not found"))?;
    let files = merge_input_files(stream, result, options);
    let merge_files = if files.len() >= 1800 {
        events.push(ProgressEvent::MergeProgress {
            stream_id: Some(result.stream_id.clone()),
            message: "Segments more than 1800, start partial merge...".to_string(),
        });
        partial_combine_files(&files).await?
    } else {
        files
    };
    let concat_list_path = if options.use_ffmpeg_concat_demuxer {
        let path = result.temp_dir.join("ffconcat.txt");
        write_ffmpeg_concat_list(&merge_files, &path).await?;
        Some(path)
    } else {
        None
    };
    let output_path = ffmpeg_merge_output_path(stream, &result.output_path);
    if let Some(parent) = output_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    events.push(ProgressEvent::MergeProgress {
        stream_id: Some(result.stream_id.clone()),
        message: "ffmpeg merging...".to_string(),
    });
    let metadata = FfmpegMergeMetadata {
        ddp_audio: ffmpeg_merge_ddp_audio_sidecar(&output_path).await?,
        ..FfmpegMergeMetadata::default()
    };
    let plan = plan_ffmpeg_merge(FfmpegMergeRequest {
        binary: ffmpeg,
        files: &merge_files,
        output_base_path: &output_path,
        format: ffmpeg_merge_format(stream),
        use_aac_filter: result.use_aac_filter || stream_needs_aac_filter(stream),
        fast_start: false,
        write_date: !options.no_date_info,
        use_concat_demuxer: options.use_ffmpeg_concat_demuxer,
        concat_list_path: concat_list_path.as_deref(),
        metadata: &metadata,
    })?;
    run_mux_command_plan(&plan, cancellation_token, events).await?;
    external_decrypt_if_needed(
        stream,
        &plan.output_path,
        options,
        events,
        cancellation_token,
    )
    .await
}

fn merge_input_files(
    stream: Option<&Stream>,
    result: &StreamDownloadResult,
    options: &DownloadOptions,
) -> Vec<PathBuf> {
    let mut files = result.files.clone();
    if options.mp4_real_time_decryption
        && !matches!(
            options.decryption_engine,
            DecryptionEngine::Mp4decrypt | DecryptionEngine::Mp4forge
        )
        && stream.is_some_and(stream_requires_external_decrypt)
        && stream
            .and_then(|stream| stream.playlist.as_ref())
            .and_then(|playlist| playlist.media_init.as_ref())
            .is_some()
        && !files.is_empty()
    {
        files.remove(0);
    }
    files
}

async fn write_ffmpeg_concat_list(files: &[PathBuf], path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut text = String::new();
    for file in files {
        let absolute = absolute_path(file)?;
        let normalized = absolute.to_string_lossy().replace('\\', "/");
        let escaped = normalized.replace('\'', "'\\''");
        text.push_str("file '");
        text.push_str(&escaped);
        text.push_str("'\n");
    }
    tokio::fs::write(path, text).await?;
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn should_binary_merge_stream(
    stream: Option<&Stream>,
    result: &StreamDownloadResult,
    options: &DownloadOptions,
    is_live_session: bool,
) -> bool {
    is_live_session
        || options.binary_merge
        || result.binary_merge_required
        || stream.is_some_and(|stream| stream.media_type == Some(MediaType::Subtitles))
}

fn ffmpeg_merge_format(stream: Option<&Stream>) -> MergeOutputFormat {
    if stream.is_some_and(|stream| stream.media_type == Some(MediaType::Audio)) {
        MergeOutputFormat::M4a
    } else {
        MergeOutputFormat::Mp4
    }
}

fn ffmpeg_merge_output_path(stream: Option<&Stream>, binary_output_path: &Path) -> PathBuf {
    let extension = match ffmpeg_merge_format(stream) {
        MergeOutputFormat::M4a => "m4a",
        _ => "mp4",
    };
    let output = binary_output_path.with_extension(extension);
    let final_output = match stream {
        Some(stream) => handle_file_collision(&output, stream),
        None => output,
    };
    let mut output_base = final_output;
    output_base.set_extension("");
    output_base
}

async fn ffmpeg_merge_ddp_audio_sidecar(output_base_path: &Path) -> Result<Option<PathBuf>> {
    let current_dir = std::env::current_dir()?;
    ffmpeg_merge_ddp_audio_sidecar_in(output_base_path, &current_dir).await
}

async fn ffmpeg_merge_ddp_audio_sidecar_in(
    output_base_path: &Path,
    current_dir: &Path,
) -> Result<Option<PathBuf>> {
    let Some(stem) = output_base_path.file_name() else {
        return Ok(None);
    };
    let mut sidecar = current_dir.to_path_buf();
    sidecar.push(stem);
    sidecar.set_extension("txt");
    if !tokio::fs::try_exists(&sidecar).await? {
        return Ok(None);
    }
    let text = tokio::fs::read_to_string(&sidecar).await?;
    let path = PathBuf::from(text.trim());
    if path.as_os_str().is_empty() {
        Ok(None)
    } else {
        Ok(Some(path))
    }
}

fn stream_needs_aac_filter(stream: Option<&Stream>) -> bool {
    stream
        .and_then(|stream| stream.codecs.as_deref())
        .is_some_and(|codecs| codecs.to_ascii_lowercase().contains("mp4a"))
}

async fn append_live_subtitle_refresh(
    stream: &Stream,
    result: &StreamDownloadResult,
    options: &DownloadOptions,
    timing: SubtitleTimingContext,
    events: &mut Vec<ProgressEvent>,
) -> Result<()> {
    let base_timestamp_ms = subtitle_base_timestamp_ms(stream, options, timing);
    let stream_id = Some(stream_identity(stream));
    events.push(ProgressEvent::SubtitleProgress {
        stream_id: stream_id.clone(),
        message: subtitle_extraction_message(stream, &result.files).await?,
    });
    let mut extraction =
        extract_subtitle_output(stream, &result.files, base_timestamp_ms, timing).await?;
    push_raw_console_lines(events, extraction.console_lines.drain(..));
    let mut subtitle = extraction.subtitle;
    let images = write_image_pngs(&mut subtitle, &result.temp_dir).await?;
    if !images.is_empty() {
        events.push(ProgressEvent::SubtitleProgress {
            stream_id: stream_id.clone(),
            message: "Processing Image Sub".to_string(),
        });
    }
    if !images.is_empty() && env::var(KEEP_IMAGE_SEGMENTS_ENV).ok().as_deref() != Some("1") {
        for path in &result.files {
            if tokio::fs::try_exists(path).await? {
                tokio::fs::remove_file(path).await?;
            }
        }
    }
    if let Some(parent) = result.output_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let formatted = format_subtitle(&subtitle, options.sub_format);
    let existing = match tokio::fs::read_to_string(&result.output_path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(Error::from(error)),
    };
    let merged = append_formatted_live_subtitle(&existing, &formatted, options.sub_format);
    tokio::fs::write(
        &result.output_path,
        subtitle_output_bytes(&merged, timing.is_live_session),
    )
    .await?;
    Ok(())
}

fn append_formatted_live_subtitle(
    existing: &str,
    refreshed: &str,
    format: SubtitleFormat,
) -> String {
    if existing.trim().is_empty() {
        return refreshed.to_string();
    }
    match format {
        SubtitleFormat::Srt => append_srt_text(existing, refreshed),
        SubtitleFormat::Vtt => append_vtt_text(existing, refreshed),
    }
}

fn append_srt_text(existing: &str, refreshed: &str) -> String {
    let mut output = trim_trailing_newlines(existing).to_string();
    let rewritten = reindex_srt_text(refreshed, count_srt_cues(existing));
    if !rewritten.trim().is_empty() {
        let newline = platform_newline();
        output.push_str(newline);
        output.push_str(newline);
        output.push_str(&rewritten);
        output.push_str(newline);
    }
    output
}

fn append_vtt_text(existing: &str, refreshed: &str) -> String {
    let mut output = trim_trailing_newlines(existing).to_string();
    let body = refreshed
        .strip_prefix("WEBVTT")
        .unwrap_or(refreshed)
        .trim_start_matches(['\r', '\n']);
    if !body.trim().is_empty() {
        let newline = platform_newline();
        output.push_str(newline);
        output.push_str(newline);
        output.push_str(body);
    }
    output
}

fn reindex_srt_text(text: &str, offset: usize) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let newline = platform_newline();
    let mut output = String::new();
    let mut index = 0_usize;
    let mut cue = 1_usize;
    while index < lines.len() {
        while index < lines.len() && lines[index].trim().is_empty() {
            index += 1;
        }
        if index >= lines.len() {
            break;
        }
        if index + 1 < lines.len()
            && lines[index].trim().parse::<usize>().is_ok()
            && lines[index + 1].contains(" --> ")
        {
            output.push_str(&(offset + cue).to_string());
            output.push_str(newline);
            output.push_str(lines[index + 1].trim_end());
            output.push_str(newline);
            index += 2;
            while index < lines.len() && !lines[index].trim().is_empty() {
                output.push_str(lines[index].trim_end());
                output.push_str(newline);
                index += 1;
            }
            output.push_str(newline);
            cue += 1;
            continue;
        }
        output.push_str(lines[index].trim_end());
        output.push_str(newline);
        index += 1;
    }
    if output.is_empty() {
        text.to_string()
    } else {
        output
    }
}

fn count_srt_cues(text: &str) -> usize {
    let lines = text.lines().collect::<Vec<_>>();
    lines
        .windows(2)
        .filter(|window| window[0].trim().parse::<usize>().is_ok() && window[1].contains(" --> "))
        .count()
}

fn trim_trailing_newlines(value: &str) -> &str {
    value.trim_end_matches(['\r', '\n'])
}

fn platform_newline() -> &'static str {
    #[cfg(windows)]
    {
        "\r\n"
    }
    #[cfg(not(windows))]
    {
        "\n"
    }
}

async fn write_subtitle_output_if_needed(
    stream: &Stream,
    files: &[PathBuf],
    output_path: &Path,
    temp_dir: &Path,
    options: &DownloadOptions,
    timing: SubtitleTimingContext,
    events: &mut Vec<ProgressEvent>,
) -> Result<SubtitleWriteOutcome> {
    if !should_format_subtitle(stream, options) {
        return Ok(SubtitleWriteOutcome::default());
    }
    let base_timestamp_ms = subtitle_base_timestamp_ms(stream, options, timing);
    let stream_id = Some(stream_identity(stream));
    events.push(ProgressEvent::SubtitleProgress {
        stream_id: stream_id.clone(),
        message: subtitle_extraction_message(stream, files).await?,
    });
    let mut extraction = extract_subtitle_output(stream, files, base_timestamp_ms, timing).await?;
    push_raw_console_lines(events, extraction.console_lines.drain(..));
    let mut subtitle = extraction.subtitle;
    let images = write_image_pngs(&mut subtitle, temp_dir).await?;
    if !images.is_empty() {
        events.push(ProgressEvent::SubtitleProgress {
            stream_id: stream_id.clone(),
            message: "Processing Image Sub".to_string(),
        });
    }
    if !images.is_empty() && env::var(KEEP_IMAGE_SEGMENTS_ENV).ok().as_deref() != Some("1") {
        for path in files {
            if tokio::fs::try_exists(path).await? {
                tokio::fs::remove_file(path).await?;
            }
        }
    }
    if let Some(parent) = output_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let formatted = format_subtitle(&subtitle, options.sub_format);
    tokio::fs::write(
        output_path,
        subtitle_output_bytes(&formatted, timing.is_live_session),
    )
    .await?;
    Ok(SubtitleWriteOutcome {
        wrote: true,
        retain_temp_dir: !images.is_empty(),
    })
}

fn subtitle_output_bytes(text: &str, is_live_session: bool) -> Vec<u8> {
    if is_live_session {
        return text.as_bytes().to_vec();
    }
    let mut bytes = Vec::with_capacity(3 + text.len());
    bytes.extend_from_slice(&[0xef, 0xbb, 0xbf]);
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SubtitleTimingContext {
    is_live_session: bool,
    live_audio_start_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SubtitleWriteOutcome {
    wrote: bool,
    retain_temp_dir: bool,
}

#[derive(Debug, Default)]
struct SubtitleExtraction {
    subtitle: WebVttSubtitle,
    console_lines: Vec<String>,
}

fn subtitle_base_timestamp_ms(
    stream: &Stream,
    options: &DownloadOptions,
    timing: SubtitleTimingContext,
) -> i64 {
    if options.live_fix_vtt_by_audio
        && stream.media_type == Some(MediaType::Subtitles)
        && should_format_raw_webvtt_subtitle(stream)
        && let Some(value) = timing.live_audio_start_ms
    {
        return value;
    }
    if timing.is_live_session
        && (should_format_ttml_subtitle(stream)
            || stream
                .codecs
                .as_deref()
                .is_some_and(|codecs| codecs.eq_ignore_ascii_case("stpp")))
        && let Some(value) = stream
            .publish_time
            .as_deref()
            .and_then(parse_manifest_timestamp_millis)
    {
        return value.saturating_sub(subtitle_batch_duration_ms(stream));
    }
    stream
        .skipped_duration
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| (value * 1000.0).min(i64::MAX as f64) as i64)
        .unwrap_or_default()
}

fn subtitle_batch_duration_ms(stream: &Stream) -> i64 {
    stream
        .playlist
        .as_ref()
        .map(|playlist| {
            playlist
                .media_parts
                .iter()
                .flat_map(|part| part.media_segments.iter())
                .map(|segment| segment.duration.max(0.0))
                .sum::<f64>()
        })
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| (value * 1000.0).min(i64::MAX as f64) as i64)
        .unwrap_or_default()
}

async fn extract_subtitle_output(
    stream: &Stream,
    files: &[PathBuf],
    base_timestamp_ms: i64,
    timing: SubtitleTimingContext,
) -> Result<SubtitleExtraction> {
    let init_path = stream
        .playlist
        .as_ref()
        .and_then(|playlist| playlist.media_init.as_ref())
        .and_then(|_| files.first());
    let media_files = if init_path.is_some() && files.len() > 1 {
        &files[1..]
    } else {
        files
    };
    if let Some(init_path) = init_path {
        let init = tokio::fs::read(init_path).await?;
        if let Some(timescale) = check_wvtt_init(&init)? {
            let extraction =
                extract_wvtt_from_files_with_console_lines(media_files, timescale).await?;
            let mut subtitle = extraction.subtitle;
            if !timing.is_live_session && base_timestamp_ms != 0 {
                subtitle.left_shift_ms(base_timestamp_ms);
            }
            return Ok(SubtitleExtraction {
                subtitle,
                console_lines: extraction.console_lines,
            });
        }
        if check_stpp_init(&init) {
            return extract_ttml_segmented_subtitle(
                stream,
                media_files,
                base_timestamp_ms,
                timing,
                TtmlExtractionKind::Mp4,
            )
            .await
            .map(subtitle_extraction_without_console_lines);
        }
    }
    if should_format_ttml_subtitle(stream) {
        return extract_ttml_segmented_subtitle(
            stream,
            media_files,
            base_timestamp_ms,
            timing,
            TtmlExtractionKind::Plain,
        )
        .await
        .map(subtitle_extraction_without_console_lines);
    }
    if should_format_raw_webvtt_subtitle(stream) {
        return extract_raw_webvtt_subtitle(stream, media_files, base_timestamp_ms, timing)
            .await
            .map(subtitle_extraction_without_console_lines);
    }
    Err(Error::subtitle(
        "unsupported subtitle format for automatic repair",
    ))
}

fn subtitle_extraction_without_console_lines(subtitle: WebVttSubtitle) -> SubtitleExtraction {
    SubtitleExtraction {
        subtitle,
        console_lines: Vec::new(),
    }
}

async fn subtitle_extraction_message(stream: &Stream, files: &[PathBuf]) -> Result<String> {
    let init_path = stream
        .playlist
        .as_ref()
        .and_then(|playlist| playlist.media_init.as_ref())
        .and_then(|_| files.first());
    if let Some(init_path) = init_path {
        let init = tokio::fs::read(init_path).await?;
        if check_wvtt_init(&init)?.is_some() {
            return Ok("Extracting VTT(mp4) subtitle...".to_string());
        }
        if check_stpp_init(&init) {
            return Ok("Extracting TTML(mp4) subtitle...".to_string());
        }
    }
    if should_format_ttml_subtitle(stream) {
        return Ok("Extracting TTML(raw) subtitle...".to_string());
    }
    Ok("Extracting VTT(raw) subtitle...".to_string())
}

async fn extract_raw_webvtt_subtitle(
    stream: &Stream,
    files: &[PathBuf],
    base_timestamp_ms: i64,
    timing: SubtitleTimingContext,
) -> Result<WebVttSubtitle> {
    let mut subtitle = None;
    let segments = stream_media_segments(stream);
    for (index, path) in files.iter().enumerate() {
        let bytes = tokio::fs::read(path).await?;
        let parse_base_timestamp_ms = if timing.is_live_session {
            base_timestamp_ms
        } else {
            0
        };
        let mut parsed = parse_webvtt_bytes(&bytes, parse_base_timestamp_ms)?;
        if parsed.mpegts_timestamp == 0
            && let Some(timestamp) =
                manual_subtitle_mpegts_timestamp(stream, &segments, index, !timing.is_live_session)
        {
            parsed.mpegts_timestamp = timestamp;
        }
        merge_subtitle_segment(&mut subtitle, parsed);
    }
    let mut subtitle = subtitle.unwrap_or_default();
    if !timing.is_live_session && base_timestamp_ms != 0 {
        subtitle.left_shift_ms(base_timestamp_ms);
    }
    Ok(subtitle)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TtmlExtractionKind {
    Plain,
    Mp4,
}

async fn extract_ttml_segmented_subtitle(
    stream: &Stream,
    files: &[PathBuf],
    base_timestamp_ms: i64,
    timing: SubtitleTimingContext,
    kind: TtmlExtractionKind,
) -> Result<WebVttSubtitle> {
    let mut subtitle = None;
    let segments = stream_media_segments(stream);
    for (index, path) in files.iter().enumerate() {
        let mut parsed = match kind {
            TtmlExtractionKind::Plain => {
                extract_ttml_from_files(
                    std::slice::from_ref(path),
                    0,
                    base_timestamp_ms_for_parse(timing, base_timestamp_ms),
                )
                .await?
            }
            TtmlExtractionKind::Mp4 => {
                extract_stpp_from_files(
                    std::slice::from_ref(path),
                    0,
                    base_timestamp_ms_for_parse(timing, base_timestamp_ms),
                )
                .await?
            }
        };
        if parsed.mpegts_timestamp == 0
            && let Some(timestamp) =
                manual_subtitle_mpegts_timestamp(stream, &segments, index, !timing.is_live_session)
        {
            parsed.mpegts_timestamp = timestamp;
        }
        merge_subtitle_segment(&mut subtitle, parsed);
    }
    let mut subtitle = subtitle.unwrap_or_default();
    if !timing.is_live_session && base_timestamp_ms != 0 {
        subtitle.left_shift_ms(base_timestamp_ms);
    }
    Ok(subtitle)
}

fn base_timestamp_ms_for_parse(timing: SubtitleTimingContext, base_timestamp_ms: i64) -> i64 {
    if timing.is_live_session {
        base_timestamp_ms
    } else {
        0
    }
}

fn merge_subtitle_segment(subtitle: &mut Option<WebVttSubtitle>, parsed: WebVttSubtitle) {
    if let Some(existing) = subtitle {
        existing.add_cues_from_one(parsed);
    } else {
        *subtitle = Some(parsed);
    }
}

fn should_format_subtitle(stream: &Stream, options: &DownloadOptions) -> bool {
    options.auto_subtitle_fix
        && stream.media_type == Some(MediaType::Subtitles)
        && (should_format_raw_webvtt_subtitle(stream)
            || should_format_ttml_subtitle(stream)
            || should_format_mp4_subtitle(stream))
}

fn should_format_ttml_subtitle(stream: &Stream) -> bool {
    stream
        .codecs
        .as_deref()
        .is_some_and(|codecs| codecs.eq_ignore_ascii_case("stpp"))
        || stream.extension.as_deref().is_some_and(|extension| {
            extension.eq_ignore_ascii_case("ttml") || extension.eq_ignore_ascii_case("dfxp")
        })
}

fn should_format_raw_webvtt_subtitle(stream: &Stream) -> bool {
    stream.extension.as_deref().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("vtt") || extension.eq_ignore_ascii_case("webvtt")
    })
}

fn should_format_mp4_subtitle(stream: &Stream) -> bool {
    stream.codecs.as_deref().is_some_and(|codecs| {
        codecs.eq_ignore_ascii_case("wvtt") || codecs.eq_ignore_ascii_case("stpp")
    }) || stream.extension.as_deref().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("m4s") || extension.eq_ignore_ascii_case("mp4")
    })
}

struct LivePipeMuxSession {
    senders: Vec<mpsc::Sender<LivePipeWriteJob>>,
    workers: Vec<thread::JoinHandle<Result<()>>>,
    child: Child,
    output_path: PathBuf,
    #[cfg(not(windows))]
    fifo_paths: Vec<PathBuf>,
}

struct LivePipeWriteJob {
    files: Vec<PathBuf>,
    ack: mpsc::Sender<std::result::Result<(), String>>,
}

impl LivePipeMuxSession {
    async fn start(
        streams: &[Stream],
        results: &[StreamDownloadResult],
        options: &DownloadOptions,
        _dirs: &WorkDirectories,
        events: &mut Vec<ProgressEvent>,
    ) -> Result<Self> {
        let files = live_pipe_output_files(streams, results);
        if files.is_empty() {
            return Err(Error::live(
                "live pipe mux requires at least one media stream",
            ));
        }
        let output_path = mux_output_base(&files, options)?.with_extension("ts");
        if let Some(parent) = output_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let LivePipeEnvironment {
            custom_destination,
            pipe_dir,
        } = live_pipe_environment();
        tokio::fs::create_dir_all(&pipe_dir).await?;
        let pipe_names = live_pipe_names(files.len());
        let endpoints = create_live_pipe_endpoints(&pipe_names, &pipe_dir)?;
        for pipe_name in &pipe_names {
            events.push(ProgressEvent::Log {
                level: LogLevel::Info,
                message: format!("Named pipe created: {pipe_name}"),
            });
        }
        let date_string = current_local_iso_timestamp();
        let plan = plan_live_pipe_mux(
            options
                .ffmpeg_binary_path
                .clone()
                .unwrap_or_else(|| PathBuf::from("ffmpeg")),
            &pipe_names,
            &output_path,
            &date_string,
            custom_destination.as_deref(),
            &pipe_dir,
            cfg!(windows),
        );
        events.push(ProgressEvent::MuxProgress {
            message: format!(
                "Mux with named pipe, to {}",
                output_path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("output.ts")
            ),
        });
        if custom_destination.is_some() {
            events.push(ProgressEvent::MuxProgress {
                message: plan.arguments.clone(),
            });
        }
        #[cfg(not(windows))]
        let fifo_paths = endpoints.clone();
        let workers = endpoints
            .into_iter()
            .map(spawn_live_pipe_writer)
            .collect::<Result<Vec<_>>>()?;
        let (senders, workers): (Vec<_>, Vec<_>) = workers.into_iter().unzip();
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let child = spawn_live_pipe_mux_command_plan(&plan)?;
        Ok(Self {
            senders,
            workers,
            child,
            output_path,
            #[cfg(not(windows))]
            fifo_paths,
        })
    }

    async fn write_stream_files_parallel(&self, jobs: Vec<(usize, Vec<PathBuf>)>) -> Result<()> {
        let senders = self.senders.clone();
        tokio::task::spawn_blocking(move || {
            let mut acks = Vec::with_capacity(jobs.len());
            for (stream_index, files) in jobs {
                let sender = senders
                    .get(stream_index)
                    .ok_or_else(|| Error::live("live pipe writer is missing"))?;
                let (ack, wait) = mpsc::channel();
                sender
                    .send(LivePipeWriteJob { files, ack })
                    .map_err(|_| Error::live("live pipe writer stopped"))?;
                acks.push(wait);
            }
            for ack in acks {
                match ack
                    .recv()
                    .map_err(|_| Error::live("live pipe writer stopped"))?
                {
                    Ok(()) => {}
                    Err(message) => return Err(Error::live(message)),
                }
            }
            Ok(())
        })
        .await
        .map_err(|_| Error::live("live pipe writer acknowledgement failed"))?
    }

    async fn finish(
        self,
        events: &mut Vec<ProgressEvent>,
        cancellation_token: &CancellationToken,
    ) -> Result<()> {
        let cancellation_token = cancellation_token.clone();
        let output_path = tokio::task::spawn_blocking(move || {
            let mut session = self;
            drop(session.senders);
            for worker in session.workers {
                match worker.join() {
                    Ok(result) => result?,
                    Err(_) => return Err(Error::live("live pipe writer failed")),
                }
            }
            let status =
                wait_live_pipe_child_with_cancellation(&mut session.child, &cancellation_token)?;
            #[cfg(not(windows))]
            cleanup_live_fifos(&session.fifo_paths)?;
            if !status.success() {
                return Err(Error::mux("live pipe mux process failed"));
            }
            if !session.output_path.exists() {
                return Err(Error::mux("live pipe mux did not create the output file"));
            }
            Ok(session.output_path)
        })
        .await
        .map_err(|_| Error::live("live pipe finalizer failed"))??;
        events.push(ProgressEvent::OutputArtifact(OutputArtifact::new(
            output_path,
        )));
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn stream_downloads_to_live_pipe(
    streams: &[Stream],
    results: &[StreamDownloadResult],
    options: &DownloadOptions,
    is_live_session: bool,
    live_audio_start_ms: Option<i64>,
    events: &mut Vec<ProgressEvent>,
    session: &mut LivePipeMuxSession,
    cancellation_token: &CancellationToken,
) -> Result<()> {
    let mut pipe_index = 0_usize;
    let mut pipe_jobs = Vec::new();
    let mut cleanup_dirs = Vec::new();
    for (index, result) in results.iter().enumerate() {
        let Some(stream) = streams.get(index) else {
            continue;
        };
        decrypt_stream_files(stream, result, options, events, cancellation_token).await?;
        if stream.media_type == Some(MediaType::Subtitles) {
            let subtitle_write = if !result.files.is_empty() && !options.skip_merge {
                write_subtitle_output_if_needed(
                    stream,
                    &result.files,
                    &result.output_path,
                    &result.temp_dir,
                    options,
                    SubtitleTimingContext {
                        is_live_session,
                        live_audio_start_ms,
                    },
                    events,
                )
                .await?
            } else {
                SubtitleWriteOutcome::default()
            };
            if subtitle_write.wrote {
                events.push(ProgressEvent::OutputArtifact(OutputArtifact::new(
                    result.output_path.clone(),
                )));
            }
            if should_cleanup_stream_temp_dir(options, is_live_session)
                && !subtitle_write.retain_temp_dir
            {
                cleanup_stream_temp_dir(&result.temp_dir, events).await?;
            }
            continue;
        }
        if result.files.is_empty() {
            if should_cleanup_stream_temp_dir(options, is_live_session) {
                cleanup_stream_temp_dir(&result.temp_dir, events).await?;
            }
            pipe_index = pipe_index.saturating_add(1);
            continue;
        }
        pipe_jobs.push((pipe_index, result.files.clone()));
        if should_cleanup_stream_temp_dir(options, is_live_session) {
            cleanup_dirs.push(result.temp_dir.clone());
        }
        pipe_index = pipe_index.saturating_add(1);
    }
    session.write_stream_files_parallel(pipe_jobs).await?;
    for temp_dir in cleanup_dirs {
        cleanup_stream_temp_dir(&temp_dir, events).await?;
    }
    Ok(())
}

fn live_pipe_indexes(streams: &[Stream]) -> Vec<Option<usize>> {
    let mut pipe_index = 0_usize;
    streams
        .iter()
        .map(|stream| {
            if stream.media_type == Some(MediaType::Subtitles) {
                None
            } else {
                let current = pipe_index;
                pipe_index = pipe_index.saturating_add(1);
                Some(current)
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn stream_indexed_downloads_to_live_pipe(
    downloaded: &[LiveDownloadedBatch],
    options: &DownloadOptions,
    is_live_session: bool,
    live_audio_start_ms: Option<i64>,
    events: &mut Vec<ProgressEvent>,
    session: &mut LivePipeMuxSession,
    pipe_indexes: &[Option<usize>],
    cancellation_token: &CancellationToken,
) -> Result<()> {
    let mut pipe_jobs = Vec::new();
    let mut cleanup_dirs = Vec::new();
    for item in downloaded {
        let stream = &item.stream;
        let result = &item.result;
        decrypt_stream_files(stream, result, options, events, cancellation_token).await?;
        if stream.media_type == Some(MediaType::Subtitles) {
            let subtitle_write = if !result.files.is_empty() && !options.skip_merge {
                write_subtitle_output_if_needed(
                    stream,
                    &result.files,
                    &result.output_path,
                    &result.temp_dir,
                    options,
                    SubtitleTimingContext {
                        is_live_session,
                        live_audio_start_ms,
                    },
                    events,
                )
                .await?
            } else {
                SubtitleWriteOutcome::default()
            };
            if subtitle_write.wrote {
                events.push(ProgressEvent::OutputArtifact(OutputArtifact::new(
                    result.output_path.clone(),
                )));
            }
            if should_cleanup_stream_temp_dir(options, is_live_session)
                && !subtitle_write.retain_temp_dir
            {
                cleanup_stream_temp_dir(&result.temp_dir, events).await?;
            }
            continue;
        }
        let pipe_index = pipe_indexes
            .get(item.stream_index)
            .and_then(|value| *value)
            .ok_or_else(|| Error::live("live pipe writer is missing"))?;
        if result.files.is_empty() {
            if should_cleanup_stream_temp_dir(options, is_live_session) {
                cleanup_stream_temp_dir(&result.temp_dir, events).await?;
            }
            continue;
        }
        pipe_jobs.push((pipe_index, result.files.clone()));
        if should_cleanup_stream_temp_dir(options, is_live_session) {
            cleanup_dirs.push(result.temp_dir.clone());
        }
    }
    session.write_stream_files_parallel(pipe_jobs).await?;
    for temp_dir in cleanup_dirs {
        cleanup_stream_temp_dir(&temp_dir, events).await?;
    }
    Ok(())
}

fn live_pipe_output_files(streams: &[Stream], results: &[StreamDownloadResult]) -> Vec<OutputFile> {
    streams
        .iter()
        .zip(results.iter())
        .enumerate()
        .filter_map(|(index, (stream, result))| {
            if stream.media_type == Some(MediaType::Subtitles) {
                return None;
            }
            Some(output_file_from_stream(
                index,
                stream,
                result,
                &result.output_path,
            ))
        })
        .collect()
}

fn live_pipe_names(count: usize) -> Vec<String> {
    let process = std::process::id();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    (0..count)
        .map(|index| format!("haki-dl-pipe-{process}-{stamp}-{index}"))
        .collect()
}

#[cfg(windows)]
type LivePipeEndpoint = interprocess::os::windows::named_pipe::PipeListener<
    interprocess::os::windows::named_pipe::pipe_mode::Bytes,
    interprocess::os::windows::named_pipe::pipe_mode::Bytes,
>;

#[cfg(not(windows))]
type LivePipeEndpoint = PathBuf;

#[cfg(windows)]
fn create_live_pipe_endpoints(names: &[String], _pipe_dir: &Path) -> Result<Vec<LivePipeEndpoint>> {
    use interprocess::os::windows::named_pipe::PipeListenerOptions;
    use interprocess::os::windows::named_pipe::pipe_mode::Bytes;

    names
        .iter()
        .map(|name| {
            let path = format!(r"\\.\pipe\{name}");
            PipeListenerOptions::new()
                .path(path)
                .create_duplex::<Bytes>()
                .map_err(|error| Error::live(error.to_string()))
        })
        .collect()
}

#[cfg(not(windows))]
fn create_live_pipe_endpoints(names: &[String], pipe_dir: &Path) -> Result<Vec<LivePipeEndpoint>> {
    use nix::sys::stat::Mode;
    use nix::unistd::mkfifo;

    let mut paths = Vec::with_capacity(names.len());
    for name in names {
        let path = pipe_dir.join(name);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR)
            .map_err(|error| Error::live(error.to_string()))?;
        paths.push(path);
    }
    Ok(paths)
}

enum LivePipeWriter {
    #[cfg(windows)]
    Windows(
        interprocess::os::windows::named_pipe::PipeStream<
            interprocess::os::windows::named_pipe::pipe_mode::Bytes,
            interprocess::os::windows::named_pipe::pipe_mode::Bytes,
        >,
    ),
    #[cfg(not(windows))]
    Unix(File),
}

#[cfg(windows)]
fn spawn_live_pipe_writer(
    endpoint: LivePipeEndpoint,
) -> Result<(
    mpsc::Sender<LivePipeWriteJob>,
    thread::JoinHandle<Result<()>>,
)> {
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let writer = endpoint
            .accept()
            .map(LivePipeWriter::Windows)
            .map_err(|error| Error::live(error.to_string()))?;
        run_live_pipe_writer(writer, receiver)
    });
    Ok((sender, worker))
}

#[cfg(not(windows))]
fn spawn_live_pipe_writer(
    endpoint: LivePipeEndpoint,
) -> Result<(
    mpsc::Sender<LivePipeWriteJob>,
    thread::JoinHandle<Result<()>>,
)> {
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let writer = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&endpoint)
            .map(LivePipeWriter::Unix)
            .map_err(Error::from)?;
        run_live_pipe_writer(writer, receiver)
    });
    Ok((sender, worker))
}

fn run_live_pipe_writer(
    mut writer: LivePipeWriter,
    receiver: mpsc::Receiver<LivePipeWriteJob>,
) -> Result<()> {
    for job in receiver {
        let result = write_live_pipe_job(&mut writer, &job.files);
        let ack = result.as_ref().map(|_| ()).map_err(ToString::to_string);
        let _ = job.ack.send(ack);
        result?;
    }
    Ok(())
}

fn write_live_pipe_job(writer: &mut LivePipeWriter, files: &[PathBuf]) -> Result<()> {
    for path in files {
        writer.write_file(path)?;
    }
    writer.flush_batch()
}

impl LivePipeWriter {
    fn write_file(&mut self, path: &Path) -> Result<()> {
        let mut input = File::open(path)?;
        match self {
            #[cfg(windows)]
            Self::Windows(writer) => {
                std::io::copy(&mut input, writer)?;
            }
            #[cfg(not(windows))]
            Self::Unix(writer) => {
                std::io::copy(&mut input, writer)?;
            }
        }
        Ok(())
    }

    fn flush_batch(&mut self) -> Result<()> {
        match self {
            #[cfg(windows)]
            Self::Windows(writer) => {
                writer.assume_flushed();
            }
            #[cfg(not(windows))]
            Self::Unix(writer) => {
                writer.flush()?;
            }
        }
        Ok(())
    }
}

#[cfg(not(windows))]
fn cleanup_live_fifos(paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

async fn run_mux_after_done(
    generated_outputs: &[OutputFile],
    options: &DownloadOptions,
    events: &mut Vec<ProgressEvent>,
    cancellation_token: &CancellationToken,
) -> Result<()> {
    let Some(mux_options) = &options.mux_after_done else {
        return Ok(());
    };
    let files = output_files_with_imports(
        generated_outputs.to_vec(),
        &options.mux_imports,
        mux_options.skip_sub,
    );
    if files.is_empty() {
        return Err(Error::mux(
            "mux-after-done requires at least one output file",
        ));
    }
    if !(mux_options.muxer == MuxerKind::Mp4forge && mux_options.fallback_muxer.is_some()) {
        validate_mp4forge_mux_after_done(
            mux_options.format,
            mux_options.muxer,
            &files,
            &Mp4forgeSupportMatrix::default(),
        )?;
    }
    let output_base = mux_output_base(generated_outputs, options)?;
    push_mux_after_done_start(
        events,
        &files,
        &output_base,
        mux_options.format,
        mux_options.muxer,
    );
    if mux_options.muxer == MuxerKind::Mp4forge {
        let output_path = output_base.with_extension("mp4");
        if let Err(mp4forge_error) = run_mp4forge_mux_after_done(&files, &output_path).await {
            let Some(fallback_muxer) = mux_options.fallback_muxer else {
                events.push(ProgressEvent::Log {
                    level: LogLevel::Error,
                    message: "Mux failed".to_string(),
                });
                return Err(mp4forge_error);
            };
            events.push(ProgressEvent::MuxProgress {
                message: format!(
                    "mp4forge mux-after-done failed; falling back to {}",
                    muxer_name(fallback_muxer)
                ),
            });
            let plan = mp4forge_fallback_mux_command_plan(
                mux_options,
                fallback_muxer,
                options,
                &files,
                &output_base,
            )?;
            if let Err(error) = run_mux_command_plan(&plan, cancellation_token, events).await {
                events.push(ProgressEvent::Log {
                    level: LogLevel::Error,
                    message: "Mux failed".to_string(),
                });
                return Err(Error::mux(format!(
                    "mp4forge mux-after-done failed; {} fallback failed: {error}",
                    muxer_name(fallback_muxer)
                )));
            }
            if !mux_options.keep {
                cleanup_mux_inputs(&files, &plan.output_path, events).await?;
            }
            let output_path = finalize_mux_output_path(plan.output_path.clone(), events).await?;
            events.push(ProgressEvent::OutputArtifact(OutputArtifact::new(
                output_path.clone(),
            )));
            return Ok(());
        }
        if !mux_options.keep {
            cleanup_mux_inputs(&files, &output_path, events).await?;
        }
        let output_path = finalize_mux_output_path(output_path, events).await?;
        events.push(ProgressEvent::OutputArtifact(OutputArtifact::new(
            output_path.clone(),
        )));
        return Ok(());
    }
    let plan = mux_command_plan(mux_options, options, &files, &output_base)?;
    if let Err(error) = run_mux_command_plan(&plan, cancellation_token, events).await {
        events.push(ProgressEvent::Log {
            level: LogLevel::Error,
            message: "Mux failed".to_string(),
        });
        return Err(error);
    }
    if !mux_options.keep {
        cleanup_mux_inputs(&files, &plan.output_path, events).await?;
    }
    let output_path = finalize_mux_output_path(plan.output_path.clone(), events).await?;
    events.push(ProgressEvent::OutputArtifact(OutputArtifact::new(
        output_path.clone(),
    )));
    Ok(())
}

fn push_mux_after_done_start(
    events: &mut Vec<ProgressEvent>,
    files: &[OutputFile],
    output_base: &Path,
    format: MuxFormat,
    muxer: MuxerKind,
) {
    for file in files {
        if let Some(name) = file.file_path.file_name().and_then(|value| value.to_str()) {
            events.push(ProgressEvent::MuxProgress {
                message: name.to_string(),
            });
        }
    }
    let target = output_base
        .file_name()
        .and_then(|value| value.to_str())
        .map(|name| format!("{name}{}", mux_extension(format)))
        .unwrap_or_else(|| format!("output{}", mux_extension(format)));
    let message = if muxer == MuxerKind::Mp4forge {
        format!("Muxing to {target} using mp4forge")
    } else {
        format!("Muxing to {target}")
    };
    events.push(ProgressEvent::MuxProgress { message });
}

fn mp4forge_fallback_mux_command_plan(
    mux_options: &MuxAfterDoneOptions,
    fallback_muxer: MuxerKind,
    options: &DownloadOptions,
    files: &[OutputFile],
    output_base: &Path,
) -> Result<MuxCommandPlan> {
    let fallback_options = MuxAfterDoneOptions {
        muxer: fallback_muxer,
        fallback_muxer: None,
        bin_path: None,
        ..mux_options.clone()
    };
    mux_command_plan(&fallback_options, options, files, output_base)
}

fn muxer_name(muxer: MuxerKind) -> &'static str {
    match muxer {
        MuxerKind::Ffmpeg => "ffmpeg",
        MuxerKind::Mkvmerge => "mkvmerge",
        MuxerKind::Mp4forge => "mp4forge",
    }
}

fn mux_command_plan(
    mux_options: &MuxAfterDoneOptions,
    options: &DownloadOptions,
    files: &[OutputFile],
    output_base: &Path,
) -> Result<MuxCommandPlan> {
    match mux_options.muxer {
        MuxerKind::Ffmpeg => plan_ffmpeg_mux(
            options
                .ffmpeg_binary_path
                .clone()
                .or_else(|| mux_options.bin_path.clone())
                .unwrap_or_else(|| PathBuf::from("ffmpeg")),
            files,
            output_base,
            mux_options.format,
            !options.no_date_info,
            None,
        ),
        MuxerKind::Mkvmerge => plan_mkvmerge_mux(
            mux_options
                .bin_path
                .clone()
                .or_else(|| options.mkvmerge_binary_path.clone())
                .unwrap_or_else(|| PathBuf::from("mkvmerge")),
            files,
            output_base,
        ),
        MuxerKind::Mp4forge => Err(Error::mux(
            "mp4forge mux-after-done uses a direct backend path",
        )),
    }
}

fn manual_subtitle_mpegts_timestamp(
    stream: &Stream,
    segments: &[MediaSegment],
    index: usize,
    include_skipped_duration: bool,
) -> Option<i64> {
    let segment = segments.get(index)?;
    let skipped_duration = if include_skipped_duration {
        stream
            .skipped_duration
            .filter(|duration| duration.is_finite() && *duration > 0.0)
            .unwrap_or_default()
    } else {
        0.0
    };
    let seconds = segments
        .iter()
        .filter(|candidate| candidate.index < segment.index)
        .map(|candidate| candidate.duration.max(0.0))
        .sum::<f64>()
        + skipped_duration;
    if seconds <= 0.0 || !seconds.is_finite() {
        return None;
    }
    Some((seconds.trunc() * 90_000.0).min(i64::MAX as f64) as i64)
}

#[cfg(feature = "mp4forge")]
async fn run_mp4forge_mux_after_done(files: &[OutputFile], output_path: &Path) -> Result<()> {
    let tracks = files
        .iter()
        .map(|file| mp4forge::mux::MuxTrackSpec::path(file.file_path.clone()))
        .collect::<Vec<_>>();
    let request = mp4forge::mux::MuxRequest::new(tracks);
    let output_path = output_path.to_path_buf();
    tokio::task::spawn_blocking(move || run_mp4forge_mux_on_worker_thread(request, output_path))
        .await
        .map_err(|_| Error::mux("mp4forge mux worker failed"))?
        .map_err(Error::mux)
}

#[cfg(feature = "mp4forge")]
fn run_mp4forge_mux_on_worker_thread(
    request: mp4forge::mux::MuxRequest,
    output_path: PathBuf,
) -> std::result::Result<(), String> {
    const STACK_SIZE: usize = 16 * 1024 * 1024;
    thread::Builder::new()
        .name("haki-dl-mp4forge-mux".to_string())
        .stack_size(STACK_SIZE)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            runtime.block_on(async {
                mp4forge::mux::mux_to_path_async(&request, &output_path)
                    .await
                    .map_err(|error| error.to_string())
            })
        })
        .map_err(|error| error.to_string())?
        .join()
        .map_err(|_| "mp4forge mux worker panicked".to_string())?
}

#[cfg(not(feature = "mp4forge"))]
async fn run_mp4forge_mux_after_done(_files: &[OutputFile], _output_path: &Path) -> Result<()> {
    Err(Error::mux(
        "mp4forge mux-after-done requires the mp4forge feature",
    ))
}

async fn run_mux_command_plan(
    plan: &MuxCommandPlan,
    cancellation_token: &CancellationToken,
    events: &mut Vec<ProgressEvent>,
) -> Result<()> {
    push_debug_event(
        events,
        format!("{}: {}", plan.program.display(), plan.arguments),
    );
    let args = split_command_arguments(&plan.arguments)?;
    let mut child = Command::new(&plan.program)
        .args(args)
        .current_dir(&plan.working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Error::mux(error.to_string()))?;
    let stderr_handle = child.stderr.take().map(|mut stderr| {
        tokio::spawn(async move {
            let mut stderr_bytes = Vec::new();
            stderr
                .read_to_end(&mut stderr_bytes)
                .await
                .map(|_| stderr_bytes)
        })
    });
    let status_result = wait_child_with_cancellation(&mut child, cancellation_token).await;
    let stderr_bytes = collect_mux_child_stderr(stderr_handle).await?;
    let status = status_result?;
    push_mux_stderr_warnings(&stderr_bytes, events);
    if !status.success() {
        return Err(Error::mux("media process failed"));
    }
    if !tokio::fs::try_exists(&plan.output_path).await? {
        return Err(Error::mux("media process did not create the output file"));
    }
    Ok(())
}

async fn wait_child_with_cancellation(
    child: &mut TokioChild,
    cancellation_token: &CancellationToken,
) -> Result<std::process::ExitStatus> {
    loop {
        if cancellation_token.is_cancelled() {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(Error::UserCancelled);
        }
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn wait_live_pipe_child_with_cancellation(
    child: &mut Child,
    cancellation_token: &CancellationToken,
) -> Result<std::process::ExitStatus> {
    loop {
        if cancellation_token.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::UserCancelled);
        }
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn spawn_live_pipe_mux_command_plan(plan: &MuxCommandPlan) -> Result<Child> {
    let args = split_command_arguments(&plan.arguments)?;
    StdCommand::new(&plan.program)
        .args(args)
        .current_dir(&plan.working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Error::mux(error.to_string()))
}

async fn collect_mux_child_stderr(
    handle: Option<tokio::task::JoinHandle<std::io::Result<Vec<u8>>>>,
) -> Result<Vec<u8>> {
    match handle {
        Some(handle) => match handle.await {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(error)) => Err(Error::mux(error.to_string())),
            Err(_) => Err(Error::mux("media process stderr reader failed")),
        },
        None => Ok(Vec::new()),
    }
}

fn push_mux_stderr_warnings(stderr_bytes: &[u8], events: &mut Vec<ProgressEvent>) {
    let stderr = redact_secrets(&String::from_utf8_lossy(stderr_bytes));
    for line in stderr.lines().map(str::trim_end) {
        if line.is_empty() {
            continue;
        }
        events.push(ProgressEvent::ExternalToolOutput {
            message: line.to_string(),
        });
    }
}

fn split_command_arguments(input: &str) -> Result<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && in_quote {
            match chars.peek().copied() {
                Some('"') => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                    continue;
                }
                _ => {
                    current.push(ch);
                    continue;
                }
            }
        }
        if ch == '"' {
            in_quote = !in_quote;
            continue;
        }
        if ch.is_whitespace() && !in_quote {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(ch);
    }
    if in_quote {
        return Err(Error::mux(
            "mux command arguments contain an unterminated quote",
        ));
    }
    if !current.is_empty() {
        args.push(current);
    }
    Ok(args)
}

fn mux_output_base(generated_outputs: &[OutputFile], options: &DownloadOptions) -> Result<PathBuf> {
    let first = generated_outputs
        .first()
        .ok_or_else(|| Error::mux("mux-after-done requires generated outputs"))?;
    let directory = first.file_path.parent().unwrap_or_else(|| Path::new(""));
    let name = match &options.save_name {
        Some(value) if !value.is_empty() => value.clone(),
        _ => first
            .file_path
            .file_stem()
            .and_then(|value| value.to_str())
            .map(str::to_string)
            .unwrap_or_else(|| "output".to_string()),
    };
    Ok(directory.join(format!("{name}.MUX")))
}

async fn finalize_mux_output_path(
    output_path: PathBuf,
    events: &mut Vec<ProgressEvent>,
) -> Result<PathBuf> {
    let Some(stem) = output_path.file_stem().and_then(|value| value.to_str()) else {
        return Ok(output_path);
    };
    let Some(base_stem) = stem.strip_suffix(".MUX") else {
        return Ok(output_path);
    };
    let mut final_path = output_path.clone();
    final_path.set_file_name(base_stem);
    if let Some(extension) = output_path.extension() {
        final_path.set_extension(extension);
    }
    if tokio::fs::try_exists(&final_path).await? {
        return Ok(output_path);
    }
    tokio::fs::rename(&output_path, &final_path).await?;
    if let Some(name) = final_path.file_name().and_then(|value| value.to_str()) {
        events.push(ProgressEvent::MuxProgress {
            message: format!("Rename to {name}"),
        });
    }
    Ok(final_path)
}

fn output_file_from_stream(
    index: usize,
    stream: &Stream,
    result: &StreamDownloadResult,
    path: &Path,
) -> OutputFile {
    let mut output = OutputFile::new(i32_from_usize(index), path.to_path_buf());
    output.media_type = stream.media_type;
    output.lang_code = stream.language.clone();
    output.description = stream.name.clone();
    output.media_infos = if result.media_infos.is_empty() {
        vec![MediaInfo {
            base_info: stream.codecs.clone(),
            bitrate: stream.bandwidth.map(|value| value.to_string()),
            resolution: stream.resolution.clone(),
            media_type: stream.media_type.map(media_type_name).map(str::to_string),
            ..MediaInfo::default()
        }]
    } else {
        result.media_infos.clone()
    };
    output
}

async fn cleanup_mux_inputs(
    generated_outputs: &[OutputFile],
    mux_output: &Path,
    events: &mut Vec<ProgressEvent>,
) -> Result<()> {
    events.push(ProgressEvent::MuxProgress {
        message: "Cleaning files...".to_string(),
    });
    for output in generated_outputs {
        if output.file_path != mux_output && tokio::fs::try_exists(&output.file_path).await? {
            tokio::fs::remove_file(&output.file_path).await?;
            events.push(ProgressEvent::Cleanup {
                path: output.file_path.clone(),
            });
        }
    }
    Ok(())
}

fn realtime_decrypt_hooks(
    options: &DownloadOptions,
    cancellation_token: &CancellationToken,
) -> DownloadHooks {
    if !options.mp4_real_time_decryption {
        return DownloadHooks::default();
    }
    let options = Arc::new(options.clone());
    let cancellation_token = cancellation_token.clone();
    let mp4forge_fragment_info_paths = Arc::new(Mutex::new(BTreeMap::<String, PathBuf>::new()));

    let segment_options = Arc::clone(&options);
    let segment_cancellation = cancellation_token.clone();
    let segment_fragment_info_paths = Arc::clone(&mp4forge_fragment_info_paths);
    let segment_completed: SegmentCompletedHook = Arc::new(move |context| {
        let options = Arc::clone(&segment_options);
        let cancellation_token = segment_cancellation.clone();
        let fragment_info_paths = Arc::clone(&segment_fragment_info_paths);
        Box::pin(async move {
            let mut events = Vec::new();
            if context.is_init {
                if !matches!(
                    options.decryption_engine,
                    DecryptionEngine::Mp4decrypt | DecryptionEngine::Mp4forge
                ) || !matches!(
                    context.segment.encryption.method,
                    EncryptionMethod::Cenc | EncryptionMethod::SampleAes
                ) {
                    return Ok(events);
                }
                if options.decryption_engine == DecryptionEngine::Mp4forge {
                    let copy_path =
                        copy_mp4forge_fragment_info_init(&context.actual_file_path).await?;
                    {
                        let mut guard = fragment_info_paths.lock().map_err(|_| {
                            Error::config("realtime decrypt state lock was poisoned")
                        })?;
                        guard.insert(context.stream_id.clone(), copy_path);
                    }
                }
                let decrypt_result = run_external_decrypt_in_place(
                    &context.stream,
                    Some(&context.segment),
                    &context.actual_file_path,
                    None,
                    &options,
                    &mut events,
                    &cancellation_token,
                )
                .await;
                if decrypt_result.is_err()
                    && options.decryption_engine == DecryptionEngine::Mp4forge
                {
                    let path = {
                        let mut guard = fragment_info_paths.lock().map_err(|_| {
                            Error::config("realtime decrypt state lock was poisoned")
                        })?;
                        guard.remove(&context.stream_id)
                    };
                    if let Some(path) = path
                        && tokio::fs::try_exists(&path).await?
                    {
                        tokio::fs::remove_file(path).await?;
                    }
                }
                decrypt_result?;
                return Ok(events);
            }

            if !matches!(
                context.segment.encryption.method,
                EncryptionMethod::Cenc | EncryptionMethod::SampleAes
            ) {
                return Ok(events);
            }
            let stored_fragment_info = if options.decryption_engine == DecryptionEngine::Mp4forge {
                let guard = fragment_info_paths
                    .lock()
                    .map_err(|_| Error::config("realtime decrypt state lock was poisoned"))?;
                guard.get(&context.stream_id).cloned()
            } else {
                None
            };
            let init_path = if options.decryption_engine == DecryptionEngine::Mp4forge {
                stored_fragment_info
                    .as_deref()
                    .or(context.init_file_path.as_deref())
            } else {
                context.init_file_path.as_deref()
            };
            run_external_decrypt_in_place(
                &context.stream,
                Some(&context.segment),
                &context.actual_file_path,
                init_path,
                &options,
                &mut events,
                &cancellation_token,
            )
            .await?;
            Ok(events)
        })
    });

    let cleanup_fragment_info_paths = Arc::clone(&mp4forge_fragment_info_paths);
    let stream_completed: StreamCompletedHook = Arc::new(move |stream_id| {
        let fragment_info_paths = Arc::clone(&cleanup_fragment_info_paths);
        Box::pin(async move {
            let path = {
                let mut guard = fragment_info_paths
                    .lock()
                    .map_err(|_| Error::config("realtime decrypt state lock was poisoned"))?;
                guard.remove(&stream_id)
            };
            if let Some(path) = path
                && tokio::fs::try_exists(&path).await?
            {
                tokio::fs::remove_file(path).await?;
            }
            Ok(Vec::new())
        })
    });

    DownloadHooks {
        segment_completed: Some(segment_completed),
        stream_completed: Some(stream_completed),
    }
}

async fn decrypt_stream_files(
    stream: &Stream,
    result: &StreamDownloadResult,
    _options: &DownloadOptions,
    _events: &mut Vec<ProgressEvent>,
    _cancellation_token: &CancellationToken,
) -> Result<()> {
    let Some(playlist) = &stream.playlist else {
        return Ok(());
    };
    let media_segments = playlist
        .media_parts
        .iter()
        .flat_map(|part| part.media_segments.iter())
        .collect::<Vec<_>>();
    let offset = usize::from(playlist.media_init.is_some());
    for (segment, path) in media_segments.iter().zip(result.files.iter().skip(offset)) {
        if matches!(
            segment.encryption.method,
            EncryptionMethod::Cenc | EncryptionMethod::SampleAes | EncryptionMethod::Unknown
        ) {
            continue;
        }
        decrypt_segment_file(segment, path).await?;
    }
    Ok(())
}

async fn copy_mp4forge_fragment_info_init(init_path: &Path) -> Result<PathBuf> {
    let parent = init_path.parent().unwrap_or_else(|| Path::new(""));
    let extension = init_path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{value}"))
        .unwrap_or_default();
    let copy_path = parent.join(format!("{}_mp4forge_init{}", unique_temp_stem(), extension));
    tokio::fs::copy(init_path, &copy_path).await?;
    Ok(copy_path)
}

async fn external_decrypt_if_needed(
    stream: Option<&Stream>,
    source_path: &Path,
    options: &DownloadOptions,
    events: &mut Vec<ProgressEvent>,
    cancellation_token: &CancellationToken,
) -> Result<PathBuf> {
    let Some(stream) = stream else {
        return Ok(source_path.to_path_buf());
    };
    if !stream_requires_external_decrypt(stream) {
        return Ok(source_path.to_path_buf());
    }
    if options.mp4_real_time_decryption {
        return Ok(source_path.to_path_buf());
    }
    run_external_decrypt_in_place(
        stream,
        None,
        source_path,
        None,
        options,
        events,
        cancellation_token,
    )
    .await?;
    Ok(source_path.to_path_buf())
}

async fn run_external_decrypt_in_place(
    stream: &Stream,
    segment: Option<&MediaSegment>,
    source_path: &Path,
    init_path: Option<&Path>,
    options: &DownloadOptions,
    events: &mut Vec<ProgressEvent>,
    cancellation_token: &CancellationToken,
) -> Result<()> {
    let file_info = read_mp4_protection_info_from_path(source_path).await?;
    let init_info =
        init_path.map(|path| async move { read_mp4_protection_info_from_path(path).await });
    let init_info = match init_info {
        Some(future) => future.await?,
        None => crate::decrypt::Mp4ProtectionInfo::default(),
    };
    let mut kid = segment
        .and_then(|segment| segment.encryption.kid.as_ref().map(|kid| bytes_to_hex(kid)))
        .or_else(|| stream_kid_hex(stream))
        .or_else(|| init_info.kid.clone())
        .or_else(|| file_info.kid.clone());
    let program = options
        .decryption_binary_path
        .clone()
        .unwrap_or_else(|| default_decrypt_program(options.decryption_engine));
    if kid.as_deref().unwrap_or_default().is_empty()
        && options.decryption_engine == DecryptionEngine::ShakaPackager
    {
        kid = read_shaka_missing_key_id(&program, source_path).await?;
    }
    let mut keys = custom_keys_to_pairs(&options.keys);
    if should_log_key_text_search(options.key_text_file.as_deref(), kid.as_deref()).await {
        events.push(ProgressEvent::DecryptProgress {
            stream_id: Some(stream_identity(stream)),
            message: "Trying to search for KEY from text file...".to_string(),
        });
    }
    if let Some(found) =
        search_key_text_file(options.key_text_file.as_deref(), kid.as_deref()).await?
    {
        events.push(ProgressEvent::DecryptProgress {
            stream_id: Some(stream_identity(stream)),
            message: format!("OK {found}"),
        });
        keys.push(found);
    }
    let selected = select_key_pair(
        &keys,
        kid.as_deref(),
        init_info.is_multi_drm || file_info.is_multi_drm,
    );
    let Some(selected) = selected else {
        events.push(ProgressEvent::Warning {
            message: "external decrypt key is missing".to_string(),
        });
        return Ok(());
    };
    let stream_id = Some(stream_identity(stream));
    push_decrypt_protection_logs(
        events,
        stream_id.clone(),
        &init_info,
        &file_info,
        kid.as_deref(),
    );
    events.push(ProgressEvent::DecryptProgress {
        stream_id: stream_id.clone(),
        message: format!(
            "Decrypting using {}...",
            decryption_engine_name(options.decryption_engine)
        ),
    });
    let dest = decrypted_output_path(source_path);
    if options.decryption_engine == DecryptionEngine::Mp4forge {
        push_debug_event(events, "FileName: mp4forge (in-process)");
        push_debug_event(
            events,
            format!(
                "Arguments: {}",
                mp4forge_decrypt_debug_arguments(&keys, &selected, source_path, &dest, init_path)
            ),
        );
        run_mp4forge_decrypt_in_place(source_path, &dest, init_path, &keys, &selected).await?;
        tokio::fs::remove_file(source_path).await?;
        tokio::fs::rename(&dest, source_path).await?;
        return Ok(());
    }
    let prepared_source =
        prepare_external_decrypt_source(options.decryption_engine, source_path, init_path).await?;
    let mut command_source = prepared_source
        .as_deref()
        .unwrap_or(source_path)
        .to_path_buf();
    let mut command_dest = dest.clone();
    let mp4decrypt_temp = if options.decryption_engine == DecryptionEngine::Mp4decrypt {
        let temp = prepare_mp4decrypt_temp_paths(source_path);
        tokio::fs::rename(source_path, &temp.encrypted).await?;
        command_source = temp.encrypted.clone();
        command_dest = temp.decrypted.clone();
        Some(temp)
    } else {
        None
    };
    let command_init = if options.decryption_engine == DecryptionEngine::Mp4decrypt {
        init_path
    } else {
        None
    };
    let command = external_decrypt_command(
        options.decryption_engine,
        &keys,
        &selected,
        kid.as_deref(),
        &command_source,
        &command_dest,
        command_init,
    )?;
    let command_arguments = command.arguments.join(" ");
    push_debug_event(
        events,
        format!(
            "FileName: {}",
            redact_secrets(&program.display().to_string())
        ),
    );
    push_debug_event(events, format!("Arguments: {command_arguments}"));
    let mut process = Command::new(program);
    if let Some(directory) = &command.working_directory {
        process.current_dir(directory);
    }
    let mut child = match process
        .args(&command.arguments)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_error) => {
            restore_mp4decrypt_temp(source_path, mp4decrypt_temp.as_ref()).await?;
            cleanup_prepared_decrypt_source(prepared_source.as_deref()).await?;
            events.push(ProgressEvent::Log {
                level: LogLevel::Error,
                message: "Decryption failed".to_string(),
            });
            return Ok(());
        }
    };
    let stderr_handle = child.stderr.take().map(|mut stderr| {
        tokio::spawn(async move {
            let mut stderr_bytes = Vec::new();
            stderr
                .read_to_end(&mut stderr_bytes)
                .await
                .map(|_| stderr_bytes)
        })
    });
    let status = match wait_child_with_cancellation(&mut child, cancellation_token).await {
        Ok(status) => status,
        Err(error) => {
            let _ = collect_child_stderr(stderr_handle).await;
            restore_mp4decrypt_temp(source_path, mp4decrypt_temp.as_ref()).await?;
            cleanup_prepared_decrypt_source(prepared_source.as_deref()).await?;
            return Err(error);
        }
    };
    let _ = collect_child_stderr(stderr_handle).await?;
    if !status.success() {
        restore_mp4decrypt_temp(source_path, mp4decrypt_temp.as_ref()).await?;
        cleanup_prepared_decrypt_source(prepared_source.as_deref()).await?;
        events.push(ProgressEvent::Log {
            level: LogLevel::Error,
            message: "Decryption failed".to_string(),
        });
        return Ok(());
    }
    let produced_dest = mp4decrypt_temp
        .as_ref()
        .map(|temp| temp.decrypted.as_path())
        .unwrap_or(dest.as_path());
    if !tokio::fs::try_exists(produced_dest).await? {
        restore_mp4decrypt_temp(source_path, mp4decrypt_temp.as_ref()).await?;
        cleanup_prepared_decrypt_source(prepared_source.as_deref()).await?;
        events.push(ProgressEvent::Log {
            level: LogLevel::Error,
            message: "Decryption failed".to_string(),
        });
        return Ok(());
    }
    finalize_mp4decrypt_temp(source_path, &dest, mp4decrypt_temp.as_ref()).await?;
    cleanup_prepared_decrypt_source(prepared_source.as_deref()).await?;
    tokio::fs::remove_file(source_path).await?;
    tokio::fs::rename(&dest, source_path).await?;
    Ok(())
}

async fn should_log_key_text_search(path: Option<&Path>, kid: Option<&str>) -> bool {
    let Some(path) = path else {
        return false;
    };
    if kid.is_none_or(|value| value.is_empty()) {
        return false;
    }
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_file())
}

async fn collect_child_stderr(
    handle: Option<tokio::task::JoinHandle<std::io::Result<Vec<u8>>>>,
) -> Result<Vec<u8>> {
    match handle {
        Some(handle) => match handle.await {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(error)) => Err(Error::Io(error)),
            Err(_) => Err(Error::decrypt("external decrypt stderr reader failed")),
        },
        None => Ok(Vec::new()),
    }
}

fn mp4forge_decrypt_debug_arguments(
    keys: &[String],
    selected: &SelectedKey,
    source_path: &Path,
    dest: &Path,
    init_path: Option<&Path>,
) -> String {
    let mut args = Vec::new();
    for key in normalized_mp4decrypt_keys(keys, selected) {
        args.push("--key".to_string());
        args.push(key);
    }
    if let Some(init_path) = init_path {
        args.push("--fragments-info".to_string());
        args.push(quoted_debug_path_arg(init_path));
    }
    args.push(quoted_debug_path_arg(source_path));
    args.push(quoted_debug_path_arg(dest));
    args.join(" ")
}

fn quoted_debug_path_arg(path: &Path) -> String {
    let value = path.to_string_lossy().replace('"', "\\\"");
    format!("\"{value}\"")
}

#[cfg(feature = "mp4forge")]
async fn run_mp4forge_decrypt_in_place(
    source_path: &Path,
    dest: &Path,
    init_path: Option<&Path>,
    keys: &[String],
    selected: &SelectedKey,
) -> Result<()> {
    let mut options = mp4forge::decrypt::DecryptOptions::new();
    for key in normalized_mp4decrypt_keys(keys, selected) {
        options
            .add_key_spec(&key)
            .map_err(|error| Error::decrypt(error.to_string()))?;
    }
    if let Some(init_path) = init_path {
        options.set_fragments_info_bytes(tokio::fs::read(init_path).await?);
    }
    mp4forge::decrypt::decrypt_file_async(source_path, dest, &options)
        .await
        .map_err(|error| Error::decrypt(error.to_string()))?;
    if !tokio::fs::try_exists(dest).await? {
        return Err(Error::decrypt(
            "mp4forge decrypt did not create the output file",
        ));
    }
    Ok(())
}

#[cfg(not(feature = "mp4forge"))]
async fn run_mp4forge_decrypt_in_place(
    _source_path: &Path,
    _dest: &Path,
    _init_path: Option<&Path>,
    _keys: &[String],
    _selected: &SelectedKey,
) -> Result<()> {
    Err(Error::decrypt(
        "mp4forge decryption requires the mp4forge feature",
    ))
}

struct Mp4decryptTempPaths {
    encrypted: PathBuf,
    decrypted: PathBuf,
}

fn prepare_mp4decrypt_temp_paths(source_path: &Path) -> Mp4decryptTempPaths {
    let work_dir = source_path.parent().unwrap_or_else(|| Path::new(""));
    let extension = source_path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{value}"))
        .unwrap_or_default();
    let stem = unique_temp_stem();
    Mp4decryptTempPaths {
        encrypted: work_dir.join(format!("{stem}{extension}")),
        decrypted: work_dir.join(format!("{stem}_dec{extension}")),
    }
}

async fn restore_mp4decrypt_temp(
    source_path: &Path,
    temp: Option<&Mp4decryptTempPaths>,
) -> Result<()> {
    let Some(temp) = temp else {
        return Ok(());
    };
    if tokio::fs::try_exists(&temp.encrypted).await? && !tokio::fs::try_exists(source_path).await? {
        tokio::fs::rename(&temp.encrypted, source_path).await?;
    }
    if tokio::fs::try_exists(&temp.decrypted).await? {
        tokio::fs::remove_file(&temp.decrypted).await?;
    }
    Ok(())
}

async fn finalize_mp4decrypt_temp(
    source_path: &Path,
    dest: &Path,
    temp: Option<&Mp4decryptTempPaths>,
) -> Result<()> {
    let Some(temp) = temp else {
        return Ok(());
    };
    if tokio::fs::try_exists(&temp.encrypted).await? && !tokio::fs::try_exists(source_path).await? {
        tokio::fs::rename(&temp.encrypted, source_path).await?;
    }
    if tokio::fs::try_exists(dest).await? {
        tokio::fs::remove_file(dest).await?;
    }
    tokio::fs::rename(&temp.decrypted, dest).await?;
    Ok(())
}

fn unique_temp_stem() -> String {
    let process = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("haki-dl-dec-{process}-{nanos}")
}

fn stream_requires_external_decrypt(stream: &Stream) -> bool {
    stream_encryption_methods(stream)
        .into_iter()
        .any(|method| matches!(method, EncryptionMethod::Cenc | EncryptionMethod::SampleAes))
}

async fn prepare_external_decrypt_source(
    engine: DecryptionEngine,
    source_path: &Path,
    init_path: Option<&Path>,
) -> Result<Option<PathBuf>> {
    if engine == DecryptionEngine::Mp4decrypt || init_path.is_none() {
        return Ok(None);
    }
    let prepared = source_path.with_extension("itmp");
    let init_path = init_path.ok_or_else(|| Error::decrypt("missing init path"))?;
    combine_files(
        &[init_path.to_path_buf(), source_path.to_path_buf()],
        &prepared,
    )
    .await?;
    Ok(Some(prepared))
}

async fn cleanup_prepared_decrypt_source(path: Option<&Path>) -> Result<()> {
    if let Some(path) = path
        && tokio::fs::try_exists(path).await?
    {
        tokio::fs::remove_file(path).await?;
    }
    Ok(())
}

async fn read_mp4_protection_info_from_path(
    path: &Path,
) -> Result<crate::decrypt::Mp4ProtectionInfo> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut data = Vec::new();
    let mut limited = tokio::io::AsyncReadExt::take(&mut file, 1024 * 1024);
    limited.read_to_end(&mut data).await?;
    Ok(read_mp4_protection_info(&data))
}

async fn read_shaka_missing_key_id(program: &Path, source_path: &Path) -> Result<Option<String>> {
    let tmp_output = source_path.with_extension("tmp.webm");
    let key_id = "00000000000000000000000000000000";
    let output = Command::new(program)
        .arg("--quiet")
        .arg("--enable_raw_key_decryption")
        .arg(format!(
            "input={},stream=0,output={}",
            source_path.display(),
            tmp_output.display()
        ))
        .arg("--keys")
        .arg(format!("key_id={key_id}:key={key_id}"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|error| Error::decrypt(error.to_string()))?;
    if tokio::fs::try_exists(&tmp_output).await? {
        tokio::fs::remove_file(&tmp_output).await?;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let regex = regex::Regex::new(r"Key for key_id=([0-9a-f]+) was not found")
        .map_err(|error| Error::decrypt(error.to_string()))?;
    Ok(regex
        .captures(&stderr)
        .and_then(|captures| captures.get(1))
        .map(|capture| capture.as_str().to_string())
        .filter(|value| !value.is_empty()))
}

fn stream_kid_hex(stream: &Stream) -> Option<String> {
    for segment in stream_encryption_segments(stream) {
        if let Some(kid) = &segment.encryption.kid {
            return Some(bytes_to_hex(kid));
        }
    }
    None
}

fn stream_encryption_methods(stream: &Stream) -> Vec<EncryptionMethod> {
    stream_encryption_segments(stream)
        .into_iter()
        .map(|segment| segment.encryption.method)
        .collect()
}

fn stream_media_segments(stream: &Stream) -> Vec<MediaSegment> {
    stream
        .playlist
        .as_ref()
        .map(|playlist| {
            playlist
                .media_parts
                .iter()
                .flat_map(|part| part.media_segments.iter().cloned())
                .collect()
        })
        .unwrap_or_default()
}

fn stream_encryption_segments(stream: &Stream) -> Vec<&MediaSegment> {
    let mut segments = Vec::new();
    if let Some(playlist) = &stream.playlist {
        if let Some(init) = &playlist.media_init {
            segments.push(init);
        }
        segments.extend(
            playlist
                .media_parts
                .iter()
                .flat_map(|part| part.media_segments.iter()),
        );
    }
    segments
}

fn external_decrypt_command(
    engine: DecryptionEngine,
    keys: &[String],
    selected: &SelectedKey,
    kid: Option<&str>,
    source: &Path,
    dest: &Path,
    init: Option<&Path>,
) -> Result<ExternalDecryptCommand> {
    let key_value = selected
        .key_pair
        .split_once(':')
        .map(|(_, key)| key)
        .unwrap_or(selected.key_pair.as_str());
    let arguments = match engine {
        DecryptionEngine::Mp4forge => {
            return Err(Error::decrypt("mp4forge decryption is handled in process"));
        }
        DecryptionEngine::Mp4decrypt => {
            let mut args = Vec::new();
            for key in normalized_mp4decrypt_keys(keys, selected) {
                args.push("--key".to_string());
                args.push(match selected.track_id.as_deref() {
                    Some(track_id) => {
                        let value = key.split_once(':').map(|(_, key)| key).unwrap_or(&key);
                        format!("{track_id}:{value}")
                    }
                    None => key,
                });
            }
            if let Some(init) = init {
                args.push("--fragments-info".to_string());
                args.push(mp4decrypt_fragment_info_arg(source, init));
            }
            args.push(mp4decrypt_path_arg(source));
            args.push(mp4decrypt_path_arg(dest));
            args
        }
        DecryptionEngine::ShakaPackager => {
            let key_id = selected
                .track_id
                .as_deref()
                .map(|_| "00000000000000000000000000000000")
                .or_else(|| {
                    selected
                        .key_pair
                        .split_once(':')
                        .map(|(key_id, _)| key_id)
                        .filter(|key_id| !key_id.is_empty())
                })
                .or(kid)
                .unwrap_or_default();
            let label = selected
                .track_id
                .as_deref()
                .map(|track_id| format!("label={track_id}:"))
                .unwrap_or_default();
            vec![
                "--quiet".to_string(),
                "--enable_raw_key_decryption".to_string(),
                format!(
                    "input={},stream=0,output={}",
                    source.display(),
                    dest.display()
                ),
                "--keys".to_string(),
                format!("{label}key_id={key_id}:key={key_value}"),
            ]
        }
        DecryptionEngine::Ffmpeg => vec![
            "-loglevel".to_string(),
            "error".to_string(),
            "-nostdin".to_string(),
            "-decryption_key".to_string(),
            key_value.to_string(),
            "-i".to_string(),
            source.to_string_lossy().to_string(),
            "-c".to_string(),
            "copy".to_string(),
            dest.to_string_lossy().to_string(),
        ],
    };
    Ok(ExternalDecryptCommand {
        arguments,
        working_directory: if engine == DecryptionEngine::Mp4decrypt {
            source
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .map(Path::to_path_buf)
        } else {
            None
        },
    })
}

fn normalized_mp4decrypt_keys(keys: &[String], selected: &SelectedKey) -> Vec<String> {
    if keys.len() == 1 && !keys[0].contains(':') && selected.key_pair.contains(':') {
        return vec![selected.key_pair.clone()];
    }
    keys.to_vec()
}

fn mp4decrypt_path_arg(path: &Path) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

fn mp4decrypt_fragment_info_arg(source: &Path, init: &Path) -> String {
    if source.parent() == init.parent() {
        return mp4decrypt_path_arg(init);
    }
    init.to_string_lossy().to_string()
}

fn decryption_engine_name(engine: DecryptionEngine) -> &'static str {
    match engine {
        DecryptionEngine::Mp4forge => "mp4forge",
        DecryptionEngine::Mp4decrypt => "MP4DECRYPT",
        DecryptionEngine::ShakaPackager => "SHAKA_PACKAGER",
        DecryptionEngine::Ffmpeg => "FFMPEG",
    }
}

fn default_decrypt_program(engine: DecryptionEngine) -> PathBuf {
    match engine {
        DecryptionEngine::Mp4forge => PathBuf::from("mp4forge"),
        DecryptionEngine::Mp4decrypt => PathBuf::from("mp4decrypt"),
        DecryptionEngine::ShakaPackager => PathBuf::from("shaka-packager"),
        DecryptionEngine::Ffmpeg => PathBuf::from("ffmpeg"),
    }
}

fn decrypted_output_path(source_path: &Path) -> PathBuf {
    let parent = source_path.parent().unwrap_or_else(|| Path::new(""));
    let stem = source_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("output");
    let extension = source_path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{value}"))
        .unwrap_or_default();
    parent.join(format!("{stem}_dec{extension}"))
}

async fn decrypt_segment_file(segment: &MediaSegment, path: &Path) -> Result<()> {
    match segment.encryption.method {
        EncryptionMethod::None => Ok(()),
        EncryptionMethod::Aes128
        | EncryptionMethod::Aes128Ecb
        | EncryptionMethod::Chacha20
        | EncryptionMethod::SampleAesCtr => {
            decrypt_hls_segment_file(
                path,
                segment.encryption.method,
                segment.encryption.key.as_deref(),
                segment.encryption.iv.as_deref(),
            )
            .await
        }
        EncryptionMethod::SampleAes | EncryptionMethod::Cenc => Err(Error::decrypt(
            "encrypted stream requires an external decrypt pipeline",
        )),
        EncryptionMethod::Unknown => Ok(()),
    }
}

async fn cleanup_stream_temp_dir(path: &Path, events: &mut Vec<ProgressEvent>) -> Result<()> {
    if tokio::fs::try_exists(path).await? {
        tokio::fs::remove_dir_all(path).await?;
        events.push(ProgressEvent::Cleanup {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

async fn cleanup_task_temp_root(path: &Path, events: &mut Vec<ProgressEvent>) -> Result<()> {
    if tokio::fs::try_exists(path).await? {
        tokio::fs::remove_dir_all(path).await?;
        events.push(ProgressEvent::Cleanup {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn stream_is_live(stream: &Stream) -> bool {
    stream
        .playlist
        .as_ref()
        .is_some_and(|playlist| playlist.is_live)
}

fn stream_matches_id(stream: &Stream, id: &str) -> bool {
    stream.id == id
        || stream.group_id.as_deref() == Some(id)
        || stream.url == id
        || stream.name.as_deref() == Some(id)
}

fn stream_identity(stream: &Stream) -> String {
    if !stream.id.is_empty() {
        return stream.id.clone();
    }
    stream
        .group_id
        .clone()
        .unwrap_or_else(|| stream.url.clone())
}

fn media_type_name(media_type: crate::manifest::MediaType) -> &'static str {
    match media_type {
        crate::manifest::MediaType::Audio => "audio",
        crate::manifest::MediaType::Video => "video",
        crate::manifest::MediaType::Subtitles => "subtitles",
        crate::manifest::MediaType::ClosedCaptions => "closed_captions",
    }
}

fn i32_from_usize(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}
