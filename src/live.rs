//! Live playlist recording planners and state helpers.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::DownloadOptions;
use crate::datetime::parse_manifest_timestamp_seconds;
use crate::error::Result;
use crate::event::ProgressEvent;
use crate::manifest::{MediaSegment, MediaType, Stream};
use crate::mux::MuxCommandPlan;
use crate::selection::{save_name_from_input_with_suffix, valid_file_name};

/// Supported live segment identity mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiveIdentityMode {
    /// Match by program date-time when every segment has it.
    ProgramDateTime,
    /// Match by generated segment name or index.
    SegmentName,
}

/// Mutable live state for one stream.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LiveStreamState {
    /// Last emitted segment file name.
    pub last_file_name: String,
    /// Last emitted program date-time timestamp in seconds.
    pub last_date_time_unix: i64,
    /// Maximum emitted media index.
    pub max_index: i64,
    /// Recorded duration in seconds.
    pub recorded_duration_secs: u64,
    /// Refreshed duration in seconds.
    pub refreshed_duration_secs: u64,
    /// Recorded segment count.
    pub recorded_segments: u64,
    /// Refreshed segment count.
    pub refreshed_segments: u64,
    /// Whether the record limit has been reached.
    pub record_limit_reached: bool,
    /// Whether all segment paths in the first window share the same file name.
    pub all_same_path: Option<bool>,
}

/// Live refresh filtering result.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveRefreshResult {
    /// New segments to schedule.
    pub new_segments: Vec<MediaSegment>,
    /// Identity mode used by the refresh.
    pub identity_mode: LiveIdentityMode,
    /// Events emitted for API/CLI progress.
    pub events: Vec<ProgressEvent>,
}

/// Direct HTTP live TS recording counters.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HttpLiveTsState {
    /// Recorded duration in seconds.
    pub recording_duration_secs: u64,
    /// Recorded size in bytes.
    pub recording_size_bytes: u64,
    /// Whether the record limit has been reached.
    pub stop_requested: bool,
}

/// Live startup side effects applied by the compatibility workflow.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveOptionEffects {
    /// Concurrent stream recording is forced.
    pub concurrent_download: bool,
    /// MP4 real-time decryption is forced.
    pub mp4_real_time_decryption: bool,
    /// VTT-by-audio repair remains enabled.
    pub live_fix_vtt_by_audio: bool,
}

/// Synchronizes streams by trimming each stream to the live startup window.
pub fn sync_live_startup_windows(streams: &mut [Stream], take_last_count: i32) {
    let active_indices = streams
        .iter()
        .enumerate()
        .filter_map(|(index, stream)| {
            stream
                .playlist
                .as_ref()
                .and_then(|playlist| playlist.media_parts.first())
                .filter(|part| !part.media_segments.is_empty())
                .map(|_| index)
        })
        .collect::<Vec<_>>();
    if active_indices.is_empty() {
        return;
    }
    let all_have_date_time = active_indices.iter().all(|index| {
        streams[*index]
            .playlist
            .as_ref()
            .and_then(|playlist| playlist.media_parts.first())
            .is_some_and(|part| {
                part.media_segments
                    .iter()
                    .all(|segment| segment.program_date_time.is_some())
            })
    });

    if all_have_date_time {
        if let Some(start) = active_indices
            .iter()
            .filter_map(|index| first_part_min_datetime(&streams[*index]))
            .max()
        {
            for index in &active_indices {
                trim_parts_by_datetime(&mut streams[*index], start);
            }
        }
    } else if let Some(start) = active_indices
        .iter()
        .filter_map(|index| first_part_min_index(&streams[*index]))
        .max()
    {
        for index in &active_indices {
            trim_parts_by_index(&mut streams[*index], start);
        }
    }

    trim_live_take_window(streams, &active_indices, take_last_count);
}

/// Computes the live playlist refresh wait in seconds.
pub fn compute_live_wait_seconds(streams: &[Stream], override_wait: Option<i32>) -> u32 {
    if let Some(value) = override_wait {
        return u32::try_from(value.max(1)).unwrap_or(1);
    }
    let min_duration = streams
        .iter()
        .filter_map(|stream| stream.playlist.as_ref())
        .filter_map(|playlist| playlist.media_parts.first())
        .map(|part| {
            part.media_segments
                .iter()
                .map(|segment| segment.duration)
                .sum::<f64>()
        })
        .filter(|value| *value > 0.0)
        .fold(None, |acc: Option<f64>, value| match acc {
            Some(current) => Some(current.min(value)),
            None => Some(value),
        })
        .unwrap_or(6.0);
    let wait = (min_duration / 2.0).floor() as i64 - 2;
    wait.max(1) as u32
}

/// Applies live-mode option side effects.
pub fn live_option_effects(
    options: &DownloadOptions,
    selected_streams: &[Stream],
) -> LiveOptionEffects {
    LiveOptionEffects {
        concurrent_download: true,
        mp4_real_time_decryption: true,
        live_fix_vtt_by_audio: options.live_fix_vtt_by_audio
            && selected_streams
                .iter()
                .any(|stream| stream.media_type == Some(MediaType::Audio)),
    }
}

/// Filters a refreshed stream playlist to only unseen live segments and updates stream state.
pub fn filter_new_live_segments(
    stream: &mut Stream,
    state: &mut LiveStreamState,
    extractor_is_hls: bool,
    record_limit: Option<Duration>,
) -> Result<LiveRefreshResult> {
    let Some(playlist) = &mut stream.playlist else {
        return Ok(LiveRefreshResult {
            new_segments: Vec::new(),
            identity_mode: LiveIdentityMode::SegmentName,
            events: Vec::new(),
        });
    };
    let Some(part) = playlist.media_parts.first_mut() else {
        return Ok(LiveRefreshResult {
            new_segments: Vec::new(),
            identity_mode: LiveIdentityMode::SegmentName,
            events: Vec::new(),
        });
    };
    let all_has_datetime = part
        .media_segments
        .iter()
        .all(|segment| segment.program_date_time.is_some());
    let all_same_path = match state.all_same_path {
        Some(value) => value,
        None => {
            let names = part
                .media_segments
                .iter()
                .map(|segment| save_name_from_input_with_suffix(&segment.url, String::new(), false))
                .collect::<Vec<_>>();
            let value = names.len() > 1 && names.windows(2).all(|window| window[0] == window[1]);
            state.all_same_path = Some(value);
            value
        }
    };
    let identity_mode = if all_has_datetime && state.last_date_time_unix != 0 {
        LiveIdentityMode::ProgramDateTime
    } else {
        LiveIdentityMode::SegmentName
    };
    let start = match identity_mode {
        LiveIdentityMode::ProgramDateTime => part.media_segments.iter().position(|segment| {
            segment
                .program_date_time
                .as_deref()
                .and_then(parse_manifest_timestamp_seconds)
                == Some(state.last_date_time_unix)
        }),
        LiveIdentityMode::SegmentName => part.media_segments.iter().position(|segment| {
            live_segment_name(segment, extractor_is_hls, all_has_datetime, all_same_path)
                == state.last_file_name
        }),
    };
    let mut new_segments = start
        .map(|index| {
            part.media_segments
                .iter()
                .skip(index + 1)
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| {
            if state.last_file_name.is_empty() && state.last_date_time_unix == 0 {
                part.media_segments.clone()
            } else {
                Vec::new()
            }
        });
    if let Some(new_min) = new_segments.iter().map(|segment| segment.index).min()
        && new_min < state.max_index
    {
        let offset = state.max_index - new_min + 1;
        for segment in &mut new_segments {
            segment.index += offset;
        }
    }
    if let Some(max_index) = new_segments.iter().map(|segment| segment.index).max() {
        state.max_index = max_index;
    }
    if let Some(last) = new_segments.last() {
        state.last_file_name =
            live_segment_name(last, extractor_is_hls, all_has_datetime, all_same_path);
        state.last_date_time_unix = last
            .program_date_time
            .as_deref()
            .and_then(parse_manifest_timestamp_seconds)
            .unwrap_or_default();
    }
    let refreshed = new_segments
        .iter()
        .map(|segment| segment.duration.max(0.0))
        .sum::<f64>() as u64;
    state.refreshed_duration_secs = state.refreshed_duration_secs.saturating_add(refreshed);
    state.refreshed_segments = state
        .refreshed_segments
        .saturating_add(new_segments.len() as u64);
    if let Some(limit) = record_limit
        && state.refreshed_duration_secs >= limit.as_secs()
    {
        state.record_limit_reached = true;
    }
    part.media_segments = new_segments.clone();
    playlist.media_parts.truncate(1);
    let events = vec![ProgressEvent::LiveRefresh {
        stream_id: None,
        label: None,
        refreshed_duration: Duration::from_secs(state.refreshed_duration_secs),
        recorded_duration: Duration::from_secs(state.recorded_duration_secs),
        recorded_segments: state.recorded_segments,
        total_segments: state.refreshed_segments,
        is_waiting: false,
        recorded_size: None,
    }];
    Ok(LiveRefreshResult {
        new_segments,
        identity_mode,
        events,
    })
}

/// Updates the recorded-duration counter after writing segments.
pub fn add_recorded_duration(state: &mut LiveStreamState, segments: &[MediaSegment]) {
    let recorded = segments
        .iter()
        .map(|segment| segment.duration.max(0.0))
        .sum::<f64>() as u64;
    state.recorded_duration_secs = state.recorded_duration_secs.saturating_add(recorded);
    state.recorded_segments = state
        .recorded_segments
        .saturating_add(segments.len() as u64);
}

/// Updates direct HTTP live TS counters for one received chunk.
pub fn update_http_live_ts_state(
    state: &mut HttpLiveTsState,
    bytes: usize,
    elapsed: Duration,
    record_limit: Option<Duration>,
) {
    state.recording_size_bytes = state.recording_size_bytes.saturating_add(bytes as u64);
    state.recording_duration_secs = elapsed.as_secs();
    if let Some(limit) = record_limit
        && elapsed >= limit
    {
        state.stop_requested = true;
    }
}

/// Plans the live pipe mux command without creating pipes or launching a process.
pub fn plan_live_pipe_mux(
    binary: impl Into<PathBuf>,
    pipe_names: &[String],
    output_path: &Path,
    date_string: &str,
    custom_destination: Option<&str>,
    pipe_dir: &Path,
    windows: bool,
) -> MuxCommandPlan {
    let mut command = String::from("-y -fflags +genpts -loglevel quiet ");
    if custom_destination.is_some() {
        command.push_str(" -re ");
    }
    for pipe_name in pipe_names {
        let input = if windows {
            format!("\\\\.\\pipe\\{pipe_name}")
        } else {
            pipe_dir.join(pipe_name).display().to_string()
        };
        command.push_str(&format!(" -i \"{input}\" "));
    }
    for index in 0..pipe_names.len() {
        command.push_str(&format!(" -map {index} "));
    }
    command.push_str(" -strict unofficial -c copy ");
    command.push_str(&format!(" -metadata date=\"{date_string}\" "));
    command.push_str(" -ignore_unknown -copy_unknown ");
    match custom_destination {
        Some(value) if value.trim_start().starts_with('-') => command.push_str(value),
        Some(value) => command.push_str(&format!(" -f mpegts -shortest \"{value}\"")),
        None => command.push_str(&format!(
            " -f mpegts -shortest \"{}\"",
            output_path.display()
        )),
    }
    MuxCommandPlan {
        program: binary.into(),
        arguments: command,
        working_directory: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        output_path: output_path.to_path_buf(),
    }
}

/// Returns the live segment file-name stem used for duplicate detection.
pub fn live_segment_name(
    segment: &MediaSegment,
    extractor_is_hls: bool,
    all_has_datetime: bool,
    all_same_path: bool,
) -> String {
    if let Some(name) = &segment.name_from_var
        && !name.is_empty()
    {
        return name.clone();
    }
    let mut name = save_name_from_input_with_suffix(&segment.url, String::new(), false);
    if all_same_path {
        name = valid_file_name(
            segment.url.split('?').next_back().unwrap_or_default(),
            "_",
            false,
        );
    }
    if extractor_is_hls && all_has_datetime {
        segment
            .program_date_time
            .as_deref()
            .and_then(parse_manifest_timestamp_seconds)
            .map(|value| value.to_string())
            .unwrap_or(name)
    } else if extractor_is_hls {
        segment.index.to_string()
    } else {
        name
    }
}

fn first_part_min_datetime(stream: &Stream) -> Option<i64> {
    stream
        .playlist
        .as_ref()?
        .media_parts
        .first()?
        .media_segments
        .iter()
        .filter_map(|segment| {
            segment
                .program_date_time
                .as_deref()
                .and_then(parse_manifest_timestamp_seconds)
        })
        .min()
}

fn first_part_min_index(stream: &Stream) -> Option<i64> {
    stream
        .playlist
        .as_ref()?
        .media_parts
        .first()?
        .media_segments
        .iter()
        .map(|segment| segment.index)
        .min()
}

fn trim_parts_by_datetime(stream: &mut Stream, start: i64) {
    let Some(playlist) = &mut stream.playlist else {
        return;
    };
    for part in &mut playlist.media_parts {
        part.media_segments.retain(|segment| {
            segment
                .program_date_time
                .as_deref()
                .and_then(parse_manifest_timestamp_seconds)
                .is_some_and(|value| value >= start)
        });
    }
}

fn trim_parts_by_index(stream: &mut Stream, start: i64) {
    let Some(playlist) = &mut stream.playlist else {
        return;
    };
    for part in &mut playlist.media_parts {
        part.media_segments.retain(|segment| segment.index >= start);
    }
}

fn trim_live_take_window(streams: &mut [Stream], active_indices: &[usize], take_last_count: i32) {
    let counts = active_indices
        .iter()
        .filter_map(|index| {
            streams[*index]
                .playlist
                .as_ref()
                .and_then(|playlist| playlist.media_parts.first())
                .map(|part| part.media_segments.len())
        })
        .collect::<Vec<_>>();
    if counts.is_empty()
        || counts
            .iter()
            .all(|count| (*count as i64) <= take_last_count as i64)
    {
        return;
    }
    let skip_count =
        counts.iter().min().copied().unwrap_or_default() as i64 - take_last_count as i64 + 1;
    let skip_count = usize::try_from(skip_count.max(0)).unwrap_or(usize::MAX);
    for index in active_indices {
        let Some(playlist) = &mut streams[*index].playlist else {
            continue;
        };
        for part in &mut playlist.media_parts {
            if skip_count < part.media_segments.len() {
                part.media_segments = part.media_segments[skip_count..].to_vec();
            } else {
                part.media_segments.clear();
            }
        }
    }
}
