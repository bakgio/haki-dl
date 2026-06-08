//! Stream selection, filtering, range clipping, ad cleanup, and naming helpers.

use std::collections::BTreeSet;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use regex::Regex;
use time::OffsetDateTime;

use crate::config::{CustomRange, StreamFilter};
use crate::error::{Error, Result};
use crate::manifest::{MediaSegment, MediaType, Stream, compare_streams_compatible};

/// Applies compatibility ordering to a stream list.
pub fn order_streams(streams: &mut [Stream]) {
    streams.sort_by(compare_streams_compatible);
}

/// Selects the default automatic stream set: best basic stream, best audio per language, and all subtitles.
pub fn auto_select_streams(streams: &[Stream]) -> Vec<Stream> {
    if streams.len() <= 1 {
        return streams.to_vec();
    }
    let mut selected = Vec::new();
    let mut basic_streams = streams
        .iter()
        .filter(|stream| media_is_video(stream))
        .cloned()
        .collect::<Vec<_>>();
    order_streams(&mut basic_streams);
    if let Some(stream) = basic_streams.first() {
        selected.push(stream.clone());
    }

    let mut audio_languages = BTreeSet::new();
    let mut audios = streams
        .iter()
        .filter(|stream| stream.media_type == Some(MediaType::Audio))
        .cloned()
        .collect::<Vec<_>>();
    order_streams(&mut audios);
    for stream in audios {
        let language = stream.language.clone().unwrap_or_default();
        if audio_languages.insert(language) {
            selected.push(stream);
        }
    }

    let mut subtitles = streams
        .iter()
        .filter(|stream| stream.media_type == Some(MediaType::Subtitles))
        .cloned()
        .collect::<Vec<_>>();
    order_streams(&mut subtitles);
    selected.extend(subtitles);
    unique_streams(selected)
}

/// Selects subtitle streams only.
pub fn subtitle_only_streams(streams: &[Stream]) -> Vec<Stream> {
    streams
        .iter()
        .filter(|stream| stream.media_type == Some(MediaType::Subtitles))
        .cloned()
        .collect()
}

/// Returns the same default choices that the interactive prompt preselects.
pub fn interactive_default_streams(streams: &[Stream]) -> Vec<Stream> {
    if streams.len() <= 1 {
        return streams.to_vec();
    }
    let Some(first) = streams.first() else {
        return Vec::new();
    };
    let mut selected = vec![first.clone()];
    if let Some(audio_id) = &first.audio_id
        && let Some(audio) = streams.iter().find(|stream| {
            stream.media_type == Some(MediaType::Audio)
                && stream.group_id.as_deref() == Some(audio_id.as_str())
        })
    {
        selected.push(audio.clone());
    }
    if let Some(subtitle_id) = &first.subtitle_id
        && let Some(subtitle) = streams.iter().find(|stream| {
            stream.media_type == Some(MediaType::Subtitles)
                && stream.group_id.as_deref() == Some(subtitle_id.as_str())
        })
    {
        selected.push(subtitle.clone());
    }
    if let Some(fallback) = streams
        .iter()
        .find(|stream| stream.media_type.is_none())
        .or_else(|| {
            streams
                .iter()
                .find(|stream| stream.media_type == Some(MediaType::Audio))
        })
        .or_else(|| {
            streams
                .iter()
                .find(|stream| stream.media_type == Some(MediaType::Subtitles))
        })
    {
        selected.push(fallback.clone());
    }
    unique_streams(selected)
}

/// Keeps streams matching a filter.
pub fn filter_keep(streams: &[Stream], filter: Option<&StreamFilter>) -> Result<Vec<Stream>> {
    let Some(filter) = filter else {
        return Ok(Vec::new());
    };
    let mut inputs = streams.to_vec();
    inputs = filter_regex(inputs, |stream| stream.group_id.as_deref(), &filter.id)?;
    inputs = filter_regex(
        inputs,
        |stream| stream.language.as_deref(),
        &filter.language,
    )?;
    inputs = filter_regex(inputs, |stream| stream.name.as_deref(), &filter.name)?;
    inputs = filter_regex(inputs, |stream| stream.codecs.as_deref(), &filter.codecs)?;
    inputs = filter_regex(
        inputs,
        |stream| stream.resolution.as_deref(),
        &filter.resolution,
    )?;
    inputs = filter_regex_owned(
        inputs,
        |stream| stream.frame_rate.map(|value| value.to_string()),
        &filter.frame_rate,
    )?;
    inputs = filter_regex(
        inputs,
        |stream| stream.channels.as_deref(),
        &filter.channels,
    )?;
    inputs = filter_regex(
        inputs,
        |stream| stream.video_range.as_deref(),
        &filter.range,
    )?;
    inputs = filter_regex(inputs, |stream| Some(stream.url.as_str()), &filter.url)?;

    if let Some(max) = filter.segment_count_max
        && inputs.iter().all(|stream| stream.segments_count() > 0)
    {
        inputs.retain(|stream| i64_from_usize(stream.segments_count()) < max);
    }
    if let Some(min) = filter.segment_count_min
        && inputs.iter().all(|stream| stream.segments_count() > 0)
    {
        inputs.retain(|stream| i64_from_usize(stream.segments_count()) > min);
    }
    if let Some(min) = filter.playlist_duration_min {
        inputs.retain(|stream| stream.total_duration().is_some_and(|value| value > min));
    }
    if let Some(max) = filter.playlist_duration_max {
        inputs.retain(|stream| stream.total_duration().is_some_and(|value| value < max));
    }
    if let Some(min) = filter.bandwidth_min {
        inputs.retain(|stream| stream.bandwidth.is_some_and(|value| value >= min));
    }
    if let Some(max) = filter.bandwidth_max {
        inputs.retain(|stream| stream.bandwidth.is_some_and(|value| value <= max));
    }
    if let Some(role) = filter.role {
        inputs.retain(|stream| stream.role == Some(role));
    }
    Ok(apply_for_choice(inputs, &filter.for_choice))
}

/// Drops streams matching a filter.
pub fn filter_drop(streams: &[Stream], filter: Option<&StreamFilter>) -> Result<Vec<Stream>> {
    let Some(filter) = filter else {
        return Ok(streams.to_vec());
    };
    let selected = filter_keep(streams, Some(filter))?;
    let selected_ids = selected
        .iter()
        .map(drop_display_identity)
        .collect::<BTreeSet<_>>();
    Ok(streams
        .iter()
        .filter(|stream| !selected_ids.contains(&drop_display_identity(stream)))
        .cloned()
        .collect())
}

/// Applies media-type-specific keep and drop filters.
pub fn apply_stream_filters(
    streams: &[Stream],
    select_video: &[StreamFilter],
    select_audio: &[StreamFilter],
    select_subtitle: &[StreamFilter],
    drop_video: &[StreamFilter],
    drop_audio: &[StreamFilter],
    drop_subtitle: &[StreamFilter],
) -> Result<Vec<Stream>> {
    let has_keep_filters =
        !select_video.is_empty() || !select_audio.is_empty() || !select_subtitle.is_empty();
    let basic = streams
        .iter()
        .filter(|stream| media_is_video(stream))
        .cloned()
        .collect::<Vec<_>>();
    let audios = streams
        .iter()
        .filter(|stream| stream.media_type == Some(MediaType::Audio))
        .cloned()
        .collect::<Vec<_>>();
    let subtitles = streams
        .iter()
        .filter(|stream| stream.media_type == Some(MediaType::Subtitles))
        .cloned()
        .collect::<Vec<_>>();

    let basic = apply_drop_filters_to_group(basic, drop_video)?;
    let audios = apply_drop_filters_to_group(audios, drop_audio)?;
    let subtitles = apply_drop_filters_to_group(subtitles, drop_subtitle)?;

    let mut selected = Vec::new();
    if has_keep_filters {
        selected.extend(apply_keep_filters_to_group(&basic, select_video)?);
        selected.extend(apply_keep_filters_to_group(&audios, select_audio)?);
        selected.extend(apply_keep_filters_to_group(&subtitles, select_subtitle)?);
    } else {
        selected.extend(basic);
        selected.extend(audios);
        selected.extend(subtitles);
    }
    Ok(unique_streams(selected))
}

/// Applies a custom segment or time range to selected streams.
pub fn apply_custom_range(streams: &mut [Stream], custom_range: Option<&CustomRange>) {
    let Some(custom_range) = custom_range else {
        return;
    };
    for stream in streams {
        let mut skipped_duration = 0.0;
        let Some(playlist) = &mut stream.playlist else {
            continue;
        };
        let all_segments = playlist
            .media_parts
            .iter()
            .flat_map(|part| part.media_segments.iter().cloned())
            .collect::<Vec<_>>();
        for part in &mut playlist.media_parts {
            let new_segments = match custom_range {
                CustomRange::Segment {
                    start_index,
                    end_index,
                    ..
                } => part
                    .media_segments
                    .iter()
                    .filter(|segment| segment.index >= *start_index && segment.index <= *end_index)
                    .cloned()
                    .collect::<Vec<_>>(),
                CustomRange::Time {
                    start_seconds,
                    end_seconds,
                    ..
                } => part
                    .media_segments
                    .iter()
                    .filter(|segment| {
                        let before = duration_before_index(&all_segments, segment.index);
                        before >= *start_seconds && before <= *end_seconds
                    })
                    .cloned()
                    .collect::<Vec<_>>(),
            };
            if let Some(first) = new_segments.first() {
                skipped_duration += part
                    .media_segments
                    .iter()
                    .filter(|segment| segment.index < first.index)
                    .map(|segment| segment.duration)
                    .sum::<f64>();
            }
            part.media_segments = new_segments;
        }
        stream.skipped_duration = Some(skipped_duration);
    }
}

/// Removes ad segments whose URL matches any supplied regex pattern.
pub fn clean_ad_segments(streams: &mut [Stream], keywords: &[String]) -> Result<()> {
    if keywords.is_empty() {
        return Ok(());
    }
    let regexes = keywords
        .iter()
        .map(|keyword| Regex::new(keyword).map_err(|error| Error::config(error.to_string())))
        .collect::<Result<Vec<_>>>()?;
    for stream in streams {
        let Some(playlist) = &mut stream.playlist else {
            continue;
        };
        for part in &mut playlist.media_parts {
            part.media_segments
                .retain(|segment| regexes.iter().all(|regex| !regex.is_match(&segment.url)));
        }
        playlist
            .media_parts
            .retain(|part| !part.media_segments.is_empty());
    }
    Ok(())
}

/// Returns a file-system-safe file name.
pub fn valid_file_name(input: &str, replacement: &str, filter_slash: bool) -> String {
    let mut output = input.to_string();
    for invalid in invalid_file_name_chars() {
        output = output.replace(invalid, replacement);
    }
    if filter_slash {
        output = output.replace('/', replacement);
        output = output.replace('\\', replacement);
    }
    output.trim_matches('.').to_string()
}

/// Derives a save name from a local path or URL.
pub fn save_name_from_input(input: &str, add_suffix: bool) -> String {
    save_name_from_input_with_suffix(input, suffix_now(), add_suffix)
}

/// Derives a save name using an explicit suffix for deterministic planning and tests.
pub fn save_name_from_input_with_suffix(input: &str, suffix: String, add_suffix: bool) -> String {
    let base = if Path::new(input).exists() {
        Path::new(input)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_string()
    } else {
        let path = uri_local_path(input);
        Path::new(&path)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_string()
    };
    if add_suffix {
        valid_file_name(&format!("{base}_{suffix}"), "_", false)
    } else {
        valid_file_name(&format!("{base}_"), "_", false)
    }
}

fn uri_local_path(input: &str) -> String {
    let without_query = input.split('?').next().unwrap_or(input);
    let without_fragment = without_query.split('#').next().unwrap_or(without_query);
    let raw_path = if let Some(rest) = without_fragment.strip_prefix("file://") {
        file_uri_path(rest)
    } else if let Some(scheme_index) = without_fragment.find("://") {
        let after_scheme = &without_fragment[scheme_index + 3..];
        after_scheme
            .find('/')
            .map(|index| after_scheme[index..].to_string())
            .unwrap_or_default()
    } else {
        without_fragment.to_string()
    };
    percent_decode_lossy(&raw_path)
}

fn file_uri_path(rest: &str) -> String {
    let path = if rest.starts_with('/') {
        rest.to_string()
    } else {
        rest.find('/')
            .map(|index| rest[index..].to_string())
            .unwrap_or_default()
    };
    #[cfg(windows)]
    {
        let bytes = path.as_bytes();
        if bytes.len() > 2 && bytes[0] == b'/' && bytes[2] == b':' {
            return path[1..].to_string();
        }
    }
    path
}

fn percent_decode_lossy(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            output.push((high << 4) | low);
            index += 3;
            continue;
        }
        output.push(bytes[index]);
        index += 1;
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

/// Formats a save pattern without the extension.
pub fn format_save_pattern(
    save_pattern: &str,
    stream: &Stream,
    save_name: Option<&str>,
    task_id: usize,
) -> String {
    let mut result = save_pattern.to_string();
    let replacements = [
        ("<SaveName>", save_name.unwrap_or_default().to_string()),
        ("<Id>", task_id.to_string()),
        ("<Codecs>", stream.codecs.clone().unwrap_or_default()),
        ("<Language>", stream.language.clone().unwrap_or_default()),
        (
            "<Bandwidth>",
            stream
                .bandwidth
                .map(|value| value.to_string())
                .unwrap_or_default(),
        ),
        (
            "<Resolution>",
            stream.resolution.clone().unwrap_or_default(),
        ),
        (
            "<FrameRate>",
            stream
                .frame_rate
                .map(|value| value.to_string())
                .unwrap_or_default(),
        ),
        ("<Channels>", stream.channels.clone().unwrap_or_default()),
        (
            "<VideoRange>",
            stream.video_range.clone().unwrap_or_default(),
        ),
        (
            "<MediaType>",
            stream
                .media_type
                .map(|value| media_type_name(value).to_string())
                .unwrap_or_default(),
        ),
        ("<GroupId>", stream.group_id.clone().unwrap_or_default()),
    ];
    for (from, to) in replacements {
        result = result.replace(from, &to);
    }
    result = result.replace("__", "_").replace("..", ".");
    valid_file_name(result.trim_matches('_').trim_matches('.'), "_", false)
}

/// Returns a collision-free output path using stream metadata attempts before copy suffixes.
pub fn handle_file_collision(original_path: &Path, stream: &Stream) -> PathBuf {
    handle_file_collision_with_reserved(original_path, stream, &HashSet::new())
}

/// Returns a collision-free output path while also avoiding paths planned in this session.
pub fn handle_file_collision_with_reserved(
    original_path: &Path,
    stream: &Stream,
    reserved: &HashSet<PathBuf>,
) -> PathBuf {
    if !original_path.exists() && !reserved.contains(original_path) {
        return original_path.to_path_buf();
    }
    let dir = original_path.parent().unwrap_or_else(|| Path::new(""));
    let name = original_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let ext = original_path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{value}"))
        .unwrap_or_default();
    for candidate in collision_attempts(name, &ext, stream) {
        let path = dir.join(candidate);
        if !path.exists() && !reserved.contains(&path) {
            return path;
        }
    }
    let mut output = original_path.to_path_buf();
    while output.exists() || reserved.contains(&output) {
        let stem = output
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        let ext = output
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!(".{value}"))
            .unwrap_or_default();
        output = dir.join(format!("{stem}.copy{ext}"));
    }
    output
}

fn apply_keep_filters_to_group(
    streams: &[Stream],
    filters: &[StreamFilter],
) -> Result<Vec<Stream>> {
    let mut selected = Vec::new();
    for filter in filters {
        selected.extend(filter_keep(streams, Some(filter))?);
    }
    Ok(selected)
}

fn apply_drop_filters_to_group(
    selected: Vec<Stream>,
    filters: &[StreamFilter],
) -> Result<Vec<Stream>> {
    let mut output = selected;
    for filter in filters {
        output = filter_drop(&output, Some(filter))?;
    }
    Ok(output)
}

fn filter_regex(
    streams: Vec<Stream>,
    accessor: impl Fn(&Stream) -> Option<&str>,
    pattern: &Option<String>,
) -> Result<Vec<Stream>> {
    let Some(pattern) = pattern else {
        return Ok(streams);
    };
    let regex = Regex::new(pattern).map_err(|error| Error::config(error.to_string()))?;
    Ok(streams
        .into_iter()
        .filter(|stream| accessor(stream).is_some_and(|value| regex.is_match(value)))
        .collect())
}

fn filter_regex_owned(
    streams: Vec<Stream>,
    accessor: impl Fn(&Stream) -> Option<String>,
    pattern: &Option<String>,
) -> Result<Vec<Stream>> {
    let Some(pattern) = pattern else {
        return Ok(streams);
    };
    let regex = Regex::new(pattern).map_err(|error| Error::config(error.to_string()))?;
    Ok(streams
        .into_iter()
        .filter(|stream| accessor(stream).is_some_and(|value| regex.is_match(&value)))
        .collect())
}

fn apply_for_choice(mut streams: Vec<Stream>, choice: &str) -> Vec<Stream> {
    if choice.is_empty() || choice == "all" {
        return streams;
    }
    if choice == "best" {
        streams.truncate(1);
        return streams;
    }
    if choice == "worst" {
        return streams.into_iter().rev().take(1).collect::<Vec<_>>();
    }
    if let Some(number) = choice
        .strip_prefix("best")
        .and_then(|value| value.parse::<usize>().ok())
    {
        streams.truncate(number);
        return streams;
    }
    if let Some(number) = choice
        .strip_prefix("worst")
        .and_then(|value| value.parse::<usize>().ok())
    {
        let len = streams.len();
        return streams
            .into_iter()
            .skip(len.saturating_sub(number))
            .collect();
    }
    streams.truncate(1);
    streams
}

fn unique_streams(streams: Vec<Stream>) -> Vec<Stream> {
    let mut seen = BTreeSet::new();
    let mut output = Vec::new();
    for stream in streams {
        if seen.insert(display_identity(&stream)) {
            output.push(stream);
        }
    }
    output
}

fn duration_before_index(segments: &[MediaSegment], index: i64) -> f64 {
    segments
        .iter()
        .filter(|segment| segment.index < index)
        .map(|segment| segment.duration)
        .sum()
}

fn media_is_video(stream: &Stream) -> bool {
    stream.media_type.is_none() || stream.media_type == Some(MediaType::Video)
}

fn display_identity(stream: &Stream) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{}|{}",
        stream.media_type,
        stream.group_id,
        stream.language,
        stream.name,
        stream.codecs,
        stream.bandwidth,
        stream.resolution,
        stream.frame_rate,
        stream.channels,
        stream.video_range,
        stream.characteristics,
        stream.role,
        stream.segments_count(),
        stream.total_duration().unwrap_or_default()
    )
}

fn drop_display_identity(stream: &Stream) -> String {
    let segments = segments_count_label(stream.segments_count());
    let duration = stream.total_duration().unwrap_or_default();
    match stream.media_type {
        Some(MediaType::Audio) => format!(
            "audio|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{}|{}",
            encrypted_method_names(stream),
            stream.group_id,
            stream.bandwidth.map(|value| value / 1000),
            stream.name,
            stream.codecs,
            stream.language,
            stream.channels.as_ref().map(|value| format!("{value}CH")),
            stream.role,
            segments,
            duration,
        ),
        Some(MediaType::Subtitles) => format!(
            "subtitles|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{}|{}",
            encrypted_method_names(stream),
            stream.group_id,
            stream.language,
            stream.name,
            stream.codecs,
            stream.characteristics,
            stream.role,
            segments,
            duration,
        ),
        _ => format!(
            "video|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{}|{}",
            encrypted_method_names(stream),
            stream.resolution,
            stream.bandwidth.map(|value| value / 1000),
            stream.group_id,
            stream.frame_rate,
            stream.codecs,
            stream.video_range,
            stream.role,
            segments,
            duration,
        ),
    }
}

fn segments_count_label(count: usize) -> String {
    match count {
        0 => String::new(),
        1 => "1 Segment".to_string(),
        value => format!("{value} Segments"),
    }
}

fn encrypted_method_names(stream: &Stream) -> Vec<String> {
    let mut methods = BTreeSet::new();
    if let Some(playlist) = &stream.playlist {
        for method in playlist
            .media_parts
            .iter()
            .flat_map(|part| part.media_segments.iter())
            .map(|segment| segment.encryption.method)
            .filter(|method| *method != crate::manifest::EncryptionMethod::None)
        {
            methods.insert(format!("{method:?}"));
        }
    }
    methods.into_iter().collect()
}

fn collision_attempts(name: &str, ext: &str, stream: &Stream) -> Vec<String> {
    let mut attempts = Vec::new();
    if stream.media_type == Some(MediaType::Video) {
        if let Some(resolution) = &stream.resolution {
            attempts.push(format!("{name}.{resolution}{ext}"));
        }
        if let Some(bandwidth) = stream.bandwidth {
            attempts.push(format!(
                "{name}.{:.1}Mbps{ext}",
                bandwidth as f64 / 1_000_000.0
            ));
        }
        if let (Some(resolution), Some(bandwidth)) = (&stream.resolution, stream.bandwidth) {
            attempts.push(format!(
                "{name}.{resolution}.{:.1}Mbps{ext}",
                bandwidth as f64 / 1_000_000.0
            ));
        }
    } else if stream.media_type == Some(MediaType::Audio) {
        if let Some(language) = &stream.language {
            attempts.push(format!("{name}.{language}{ext}"));
        }
        if let Some(channels) = &stream.channels {
            attempts.push(format!("{name}.{channels}ch{ext}"));
        }
        if let (Some(language), Some(channels)) = (&stream.language, &stream.channels) {
            attempts.push(format!("{name}.{language}.{channels}ch{ext}"));
        }
        if let Some(bandwidth) = stream.bandwidth {
            attempts.push(format!("{name}.{}kbps{ext}", bandwidth / 1000));
        }
    } else if stream.media_type == Some(MediaType::Subtitles)
        && let Some(language) = &stream.language
    {
        attempts.push(format!("{name}.{language}{ext}"));
    }
    attempts
}

fn invalid_file_name_chars() -> Vec<&'static str> {
    vec![
        "\"", "<", ">", "|", "\0", "\u{1}", "\u{2}", "\u{3}", "\u{4}", "\u{5}", "\u{6}", "\u{7}",
        "\u{8}", "\u{9}", "\n", "\u{b}", "\u{c}", "\r", "\u{e}", "\u{f}", "\u{10}", "\u{11}",
        "\u{12}", "\u{13}", "\u{14}", "\u{15}", "\u{16}", "\u{17}", "\u{18}", "\u{19}", "\u{1a}",
        "\u{1b}", "\u{1c}", "\u{1d}", "\u{1e}", "\u{1f}", ":", "*", "?", "\\", "/",
    ]
}

fn media_type_name(media_type: MediaType) -> &'static str {
    match media_type {
        MediaType::Audio => "AUDIO",
        MediaType::Video => "VIDEO",
        MediaType::Subtitles => "SUBTITLES",
        MediaType::ClosedCaptions => "CLOSED_CAPTIONS",
    }
}

fn suffix_now() -> String {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

fn i64_from_usize(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
