//! DASH MPD parsing.

use crate::datetime::{parse_manifest_timestamp, parse_manifest_timestamp_seconds};
use crate::error::{Error, Result};
use crate::manifest::{
    EncryptionInfo, EncryptionMethod, ExtractorType, KeySource, MediaPart, MediaSegment, MediaType,
    Playlist, RoleType, Stream,
};
use crate::numeric::parse_manifest_f64;
use crate::processor::{
    ContentProcessor, DefaultDashContentProcessor, DefaultUrlProcessor, ParserConfig,
    SignedDashUrlProcessor, UrlProcessor, combine_url,
};
use std::time::{SystemTime, UNIX_EPOCH};

/// Parsed DASH manifest result.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DashManifest {
    /// Streams exposed by the MPD.
    pub streams: Vec<Stream>,
    /// Whether the MPD type is dynamic.
    pub is_dynamic: bool,
    /// MPD publish time as source text.
    pub publish_time: Option<String>,
    /// Minimum update period in seconds.
    pub minimum_update_period: Option<f64>,
}

/// DASH parser.
pub struct DashParser {
    url_processors: Vec<Box<dyn UrlProcessor>>,
}

impl Default for DashParser {
    fn default() -> Self {
        Self {
            url_processors: vec![
                Box::<SignedDashUrlProcessor>::default(),
                Box::<DefaultUrlProcessor>::default(),
            ],
        }
    }
}

impl DashParser {
    /// Creates a parser with default processors.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses an MPD document after default manifest text processing.
    pub fn parse(&self, raw_text: &str, url: &str, config: &ParserConfig) -> Result<DashManifest> {
        let mut effective_config = config.clone();
        if effective_config.url.is_empty() {
            effective_config.url = url.to_string();
        }
        let text = preprocess_dash_text(raw_text, &effective_config)?;
        let raw_text = text.as_str();
        if !raw_text.contains("<MPD") {
            return Err(Error::protocol("DASH input must contain an MPD document"));
        }
        let document = roxmltree::Document::parse(raw_text)
            .map_err(|error| Error::protocol(error.to_string()))?;
        let mpd = document
            .descendants()
            .find(|node| node.has_tag_name("MPD"))
            .ok_or_else(|| Error::protocol("MPD element not found"))?;
        let is_dynamic = attr(mpd, "type").is_some_and(|value| value == "dynamic");
        let publish_time = attr(mpd, "publishTime")
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if let Some(value) = publish_time.as_deref()
            && parse_manifest_timestamp(value).is_none()
        {
            return Err(Error::protocol("invalid DASH publish time"));
        }
        let minimum_update_period = attr(mpd, "minimumUpdatePeriod")
            .map(|value| parse_duration(value, "minimumUpdatePeriod"))
            .transpose()?;
        let media_duration = attr(mpd, "mediaPresentationDuration")
            .map(|value| parse_duration(value, "mediaPresentationDuration"))
            .transpose()?;
        let time_shift = match attr(mpd, "timeShiftBufferDepth") {
            Some(value) if !value.is_empty() => parse_duration(value, "timeShiftBufferDepth")?,
            _ => 60.0,
        };
        let availability_start_time = attr(mpd, "availabilityStartTime");
        let mut base_url = if config.base_url.is_empty() {
            url.to_string()
        } else {
            config.base_url.clone()
        };
        base_url = extend_base_url(mpd, &base_url);

        let mut streams = Vec::new();
        for period in children(mpd, "Period") {
            let period_id = attr(period, "id").map(str::to_string);
            let period_duration = attr(period, "duration")
                .map(|value| parse_duration(value, "Period duration"))
                .transpose()?
                .or(media_duration);
            let period_base = extend_base_url(period, &base_url);
            for adaptation in children(period, "AdaptationSet") {
                let adaptation_base = extend_base_url(adaptation, &period_base);
                let mut carried_media_type = attr(adaptation, "contentType")
                    .or_else(|| attr(adaptation, "mimeType"))
                    .map(str::to_string);
                for representation in children(adaptation, "Representation") {
                    if carried_media_type.is_none() {
                        carried_media_type = Some(
                            attr(representation, "contentType")
                                .or_else(|| attr(representation, "mimeType"))
                                .unwrap_or_default()
                                .to_string(),
                        );
                    }
                    let representation_base = extend_base_url(representation, &adaptation_base);
                    let mut stream = self.parse_representation(
                        mpd,
                        adaptation,
                        representation,
                        carried_media_type.as_deref(),
                        &representation_base,
                        period_id.clone(),
                        period_duration,
                        is_dynamic,
                        time_shift,
                        availability_start_time,
                        publish_time.clone(),
                        url,
                        config,
                    )?;
                    if has_content_protection(adaptation) || has_content_protection(representation)
                    {
                        apply_content_protection(&mut stream, adaptation, representation);
                    }
                    merge_or_push_stream(&mut streams, stream, is_dynamic);
                }
            }
        }
        apply_dash_default_external_tracks(&mut streams);
        normalize_dash_extensions(&mut streams);
        Ok(DashManifest {
            streams,
            is_dynamic,
            publish_time,
            minimum_update_period,
        })
    }

    /// Refreshes selected streams from a newer MPD by matching stable stream identity.
    pub fn refresh_streams(
        &self,
        streams: &mut [Stream],
        raw_text: &str,
        url: &str,
        config: &ParserConfig,
    ) -> Result<()> {
        let refreshed = self.parse(raw_text, url, config)?;
        for stream in streams.iter_mut() {
            if let Some(new_stream) = refreshed
                .streams
                .iter()
                .find(|candidate| stream_identity(candidate) == stream_identity(stream))
                .or_else(|| {
                    refreshed.streams.iter().find(|candidate| {
                        candidate
                            .playlist
                            .as_ref()
                            .and_then(|playlist| playlist.media_init.as_ref())
                            .map(|segment| segment.url.as_str())
                            == stream
                                .playlist
                                .as_ref()
                                .and_then(|playlist| playlist.media_init.as_ref())
                                .map(|segment| segment.url.as_str())
                    })
                })
                && let (Some(existing), Some(updated)) =
                    (stream.playlist.as_mut(), new_stream.playlist.as_ref())
            {
                existing.media_parts = updated.media_parts.clone();
            }
        }
        self.process_stream_urls(streams, config)?;
        Ok(())
    }

    /// Applies configured URL processors to already-selected DASH streams.
    pub fn process_stream_urls(&self, streams: &mut [Stream], config: &ParserConfig) -> Result<()> {
        for stream in streams {
            let Some(playlist) = &mut stream.playlist else {
                continue;
            };
            if let Some(init) = &mut playlist.media_init {
                init.url = self.process_resolved_url(&init.url, config)?;
            }
            for part in &mut playlist.media_parts {
                for segment in &mut part.media_segments {
                    segment.url = self.process_resolved_url(&segment.url, config)?;
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn parse_representation(
        &self,
        mpd: roxmltree::Node<'_, '_>,
        adaptation: roxmltree::Node<'_, '_>,
        representation: roxmltree::Node<'_, '_>,
        carried_media_type: Option<&str>,
        base_url: &str,
        period_id: Option<String>,
        period_duration: Option<f64>,
        is_dynamic: bool,
        time_shift: f64,
        availability_start_time: Option<&str>,
        publish_time: Option<String>,
        url: &str,
        config: &ParserConfig,
    ) -> Result<Stream> {
        let mime_type = attr(representation, "mimeType").or_else(|| attr(adaptation, "mimeType"));
        let codecs = attr(representation, "codecs")
            .or_else(|| attr(adaptation, "codecs"))
            .map(str::to_string);
        let parsed_role = role_type(representation).or_else(|| role_type(adaptation));
        let role = parsed_role.map(|role| role.value);
        let forces_subtitle_role = parsed_role
            .map(|role| role.forces_subtitle_media_type)
            .unwrap_or(false);
        let mut media_type = media_type(carried_media_type, codecs.as_deref());
        if forces_subtitle_role {
            media_type = Some(MediaType::Subtitles);
        }
        let extension = extension_from_mime(mime_type, forces_subtitle_role);
        let mut playlist = Playlist {
            url: url.to_string(),
            is_live: is_dynamic,
            refresh_interval_ms: time_shift * 500.0,
            media_parts: vec![MediaPart::default()],
            ..Playlist::default()
        };
        self.fill_segments(
            mpd,
            adaptation,
            representation,
            base_url,
            period_duration,
            is_dynamic,
            time_shift,
            availability_start_time,
            &mut playlist,
        )?;
        if playlist.media_parts[0].media_segments.is_empty() {
            playlist.media_parts[0].media_segments.push(MediaSegment {
                index: 0,
                duration: period_duration.unwrap_or(0.0),
                url: base_url.to_string(),
                ..MediaSegment::default()
            });
        }
        let group_id = dash_group_id(representation);
        Ok(Stream {
            id: group_id.clone().unwrap_or_else(|| base_url.to_string()),
            media_type,
            group_id,
            bandwidth: Some(parse_optional_i32_to_i64(
                attr(representation, "bandwidth"),
                "DASH bandwidth",
                0,
            )?),
            codecs,
            language: filter_language(
                attr(representation, "lang").or_else(|| attr(adaptation, "lang")),
            ),
            name: child_text(adaptation, "Label"),
            frame_rate: source_frame_rate(adaptation, representation)?,
            resolution: resolution(representation),
            channels: audio_channels(adaptation).or_else(|| audio_channels(representation)),
            extension,
            role,
            video_range: attr(representation, "videoRange")
                .or_else(|| attr(adaptation, "videoRange"))
                .map(str::to_string),
            publish_time,
            period_id,
            url: url.to_string(),
            original_url: config.original_url.clone(),
            playlist: Some(playlist),
            ..Stream::default()
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn fill_segments(
        &self,
        _mpd: roxmltree::Node<'_, '_>,
        adaptation: roxmltree::Node<'_, '_>,
        representation: roxmltree::Node<'_, '_>,
        base_url: &str,
        period_duration: Option<f64>,
        is_dynamic: bool,
        time_shift: f64,
        availability_start_time: Option<&str>,
        playlist: &mut Playlist,
    ) -> Result<()> {
        if let Some(segment_base) = first_child(representation, "SegmentBase") {
            self.parse_segment_base(segment_base, base_url, period_duration, playlist)?;
        }
        if let Some(segment_list) = first_child(representation, "SegmentList") {
            self.parse_segment_list(segment_list, base_url, playlist)?;
        }
        if let Some(segment_template) = first_child(representation, "SegmentTemplate")
            .or_else(|| first_child(adaptation, "SegmentTemplate"))
        {
            let outer = first_child(adaptation, "SegmentTemplate").unwrap_or(segment_template);
            self.parse_segment_template(
                segment_template,
                outer,
                representation,
                base_url,
                period_duration,
                is_dynamic,
                time_shift,
                availability_start_time,
                playlist,
            )?;
        }
        Ok(())
    }

    fn parse_segment_base(
        &self,
        segment_base: roxmltree::Node<'_, '_>,
        base_url: &str,
        duration: Option<f64>,
        playlist: &mut Playlist,
    ) -> Result<()> {
        if let Some(initialization) = first_child(segment_base, "Initialization") {
            if let Some(source_url) = attr(initialization, "sourceURL") {
                playlist.media_init = Some(init_segment(
                    self.resolve_url(source_url, base_url),
                    attr(initialization, "range"),
                )?);
            } else {
                playlist.media_parts[0].media_segments.push(MediaSegment {
                    index: 0,
                    duration: duration.unwrap_or(0.0),
                    url: base_url.to_string(),
                    ..MediaSegment::default()
                });
            }
        }
        Ok(())
    }

    fn parse_segment_list(
        &self,
        segment_list: roxmltree::Node<'_, '_>,
        base_url: &str,
        playlist: &mut Playlist,
    ) -> Result<()> {
        if let Some(initialization) = first_child(segment_list, "Initialization") {
            let init_url = attr(initialization, "sourceURL")
                .map(|source_url| self.resolve_url(source_url, base_url))
                .unwrap_or_else(|| base_url.to_string());
            playlist.media_init = Some(init_segment(init_url, attr(initialization, "range"))?);
        }
        let timescale =
            parse_optional_i32(attr(segment_list, "timescale"), "SegmentList timescale", 1)? as f64;
        let duration =
            parse_optional_i64(attr(segment_list, "duration"), "SegmentList duration", 0)? as f64;
        for (index, segment_url) in children(segment_list, "SegmentURL").enumerate() {
            let segment_url_value = attr(segment_url, "media")
                .map(|media| self.resolve_url(media, base_url))
                .unwrap_or_else(|| base_url.to_string());
            let mut segment = MediaSegment {
                index: i64::try_from(index).unwrap_or(0),
                duration: duration / timescale,
                url: segment_url_value,
                ..MediaSegment::default()
            };
            if let Some(range) = attr(segment_url, "mediaRange") {
                apply_range(&mut segment, range)?;
            }
            playlist.media_parts[0].media_segments.push(segment);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn parse_segment_template(
        &self,
        segment_template: roxmltree::Node<'_, '_>,
        outer: roxmltree::Node<'_, '_>,
        representation: roxmltree::Node<'_, '_>,
        base_url: &str,
        period_duration: Option<f64>,
        is_dynamic: bool,
        time_shift: f64,
        availability_start_time: Option<&str>,
        playlist: &mut Playlist,
    ) -> Result<()> {
        let representation_id = attr(representation, "id").unwrap_or_default();
        let bandwidth = attr(representation, "bandwidth").unwrap_or_default();
        let timescale = parse_optional_i32(
            attr(segment_template, "timescale").or_else(|| attr(outer, "timescale")),
            "SegmentTemplate timescale",
            1,
        )? as f64;
        let start_number = parse_optional_i64(
            attr(segment_template, "startNumber").or_else(|| attr(outer, "startNumber")),
            "SegmentTemplate startNumber",
            1,
        )?;
        let presentation_time_offset = parse_optional_i64(
            attr(segment_template, "presentationTimeOffset")
                .or_else(|| attr(outer, "presentationTimeOffset")),
            "SegmentTemplate presentationTimeOffset",
            0,
        )?;
        let initialization =
            attr(segment_template, "initialization").or_else(|| attr(outer, "initialization"));
        if let Some(initialization) = initialization {
            let init = replace_template(initialization, representation_id, bandwidth, None, None);
            playlist.media_init = Some(MediaSegment {
                index: -1,
                url: self.resolve_url(&init, base_url),
                ..MediaSegment::default()
            });
        }
        let Some(media_template) = attr(segment_template, "media").or_else(|| attr(outer, "media"))
        else {
            return Err(Error::protocol("SegmentTemplate media is invalid"));
        };
        if let Some(timeline) = first_child(segment_template, "SegmentTimeline") {
            self.expand_timeline(
                timeline,
                media_template,
                representation_id,
                bandwidth,
                start_number,
                timescale,
                period_duration,
                base_url,
                playlist,
            )?;
        } else if let Some(duration_value) =
            attr(segment_template, "duration").or_else(|| attr(outer, "duration"))
        {
            let duration = parse_i64(duration_value, "SegmentTemplate duration")? as f64;
            if duration == 0.0 {
                return Err(Error::protocol("SegmentTemplate duration must be positive"));
            }
            if duration < 0.0 {
                return Ok(());
            }
            let total = period_duration
                .map(|seconds| ((seconds * timescale) / duration).ceil() as i64)
                .unwrap_or(0);
            let mut first_number = start_number;
            let mut count = if is_dynamic && total == 0 { 0 } else { total };
            if count == 0 && is_dynamic {
                let available = availability_start_time
                    .and_then(parse_manifest_timestamp_seconds)
                    .ok_or_else(|| Error::protocol("invalid DASH availability start time"))?;
                let offset_seconds = (presentation_time_offset / 1000) as f64 / 1000.0;
                let elapsed = (unix_now_seconds() - available) as f64 - offset_seconds;
                first_number += (((elapsed - time_shift) * timescale) / duration) as i64;
                count = ((time_shift * timescale) / duration) as i64;
            }
            for offset in 0..count {
                let number = first_number + offset;
                let media = replace_template(
                    media_template,
                    representation_id,
                    bandwidth,
                    Some(number),
                    None,
                );
                playlist.media_parts[0].media_segments.push(MediaSegment {
                    index: if is_dynamic { number } else { offset },
                    duration: duration / timescale,
                    name_from_var: if media_template.contains("$Number") {
                        Some(number.to_string())
                    } else {
                        None
                    },
                    url: self.resolve_url(&media, base_url),
                    ..MediaSegment::default()
                });
            }
        } else {
            return Err(Error::protocol("SegmentTemplate duration is invalid"));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn expand_timeline(
        &self,
        timeline: roxmltree::Node<'_, '_>,
        media_template: &str,
        representation_id: &str,
        bandwidth: &str,
        mut number: i64,
        timescale: f64,
        period_duration: Option<f64>,
        base_url: &str,
        playlist: &mut Playlist,
    ) -> Result<()> {
        let mut current_time = 0_i64;
        let mut segment_index = 0_i64;
        for item in children(timeline, "S") {
            if let Some(value) = attr(item, "t") {
                let start = parse_i64(value, "SegmentTimeline t")?;
                current_time = start;
            }
            let duration = parse_optional_i64(attr(item, "d"), "SegmentTimeline d", 0)?;
            let repeat = parse_optional_i64(attr(item, "r"), "SegmentTimeline r", 0)?;
            let repeat = if repeat < 0 {
                period_duration
                    .map(|seconds| ((seconds * timescale) / duration as f64).ceil() as i64 - 1)
                    .unwrap_or(0)
            } else {
                repeat
            };
            for _ in 0..=repeat.max(0) {
                let media = replace_template(
                    media_template,
                    representation_id,
                    bandwidth,
                    Some(number),
                    Some(current_time),
                );
                playlist.media_parts[0].media_segments.push(MediaSegment {
                    index: segment_index,
                    duration: duration as f64 / timescale,
                    name_from_var: if media_template.contains("$Time") {
                        Some(current_time.to_string())
                    } else {
                        None
                    },
                    url: self.resolve_url(&media, base_url),
                    ..MediaSegment::default()
                });
                current_time += duration;
                number += 1;
                segment_index += 1;
            }
        }
        Ok(())
    }

    fn resolve_url(&self, value: &str, base_url: &str) -> String {
        combine_url(base_url, value)
    }

    fn process_resolved_url(&self, url: &str, config: &ParserConfig) -> Result<String> {
        let mut resolved = url.to_string();
        let processors = self
            .url_processors
            .iter()
            .map(|processor| processor.as_ref())
            .collect::<Vec<_>>();
        for processor in processors {
            if processor.can_process(ExtractorType::MpegDash, &resolved, config) {
                resolved = processor.process(&resolved, config)?;
            }
        }
        Ok(resolved)
    }
}

fn merge_or_push_stream(streams: &mut Vec<Stream>, stream: Stream, is_dynamic: bool) {
    if is_dynamic {
        streams.push(stream);
        return;
    }
    if let Some(existing) = streams.iter_mut().find(|existing| {
        existing.period_id != stream.period_id
            && existing.group_id == stream.group_id
            && existing.resolution == stream.resolution
            && existing.media_type == stream.media_type
    }) {
        if let (Some(existing_playlist), Some(mut playlist)) =
            (existing.playlist.as_mut(), stream.playlist)
        {
            let existing_last = existing_playlist
                .media_parts
                .last_mut()
                .and_then(|part| part.media_segments.last_mut());
            let new_last_url = playlist
                .media_parts
                .first()
                .and_then(|part| part.media_segments.last())
                .map(|segment| segment.url.clone());
            if let (Some(existing_last), Some(new_last_url)) = (existing_last, new_last_url)
                && existing_last.url == new_last_url
            {
                let added_duration = playlist
                    .media_parts
                    .first()
                    .map(|part| {
                        part.media_segments
                            .iter()
                            .map(|segment| segment.duration)
                            .sum()
                    })
                    .unwrap_or(0.0);
                existing_last.duration += added_duration;
                return;
            }
            let start_index = existing_playlist
                .media_parts
                .last()
                .and_then(|part| part.media_segments.last())
                .map(|segment| segment.index + 1)
                .unwrap_or(0);
            for part in &mut playlist.media_parts {
                for segment in &mut part.media_segments {
                    segment.index += start_index;
                }
            }
            existing_playlist
                .media_parts
                .append(&mut playlist.media_parts);
        }
    } else {
        streams.push(stream);
    }
}

fn stream_identity(stream: &Stream) -> String {
    format!(
        "{:?}|{:?}|{:?}",
        stream.group_id, stream.resolution, stream.media_type
    )
}

fn dash_group_id(representation: roxmltree::Node<'_, '_>) -> Option<String> {
    let mut id = attr(representation, "id").map(str::to_string)?;
    if let Some(volume_adjust) = attr(representation, "volumeAdjust") {
        id.push('-');
        id.push_str(volume_adjust);
    }
    Some(id)
}

fn filter_language(value: Option<&str>) -> Option<String> {
    let value = value?;
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '-'))
    {
        Some(value.to_string())
    } else {
        Some("und".to_string())
    }
}

fn apply_dash_default_external_tracks(streams: &mut [Stream]) {
    let audio_id = streams
        .iter()
        .filter(|stream| stream.media_type == Some(MediaType::Audio))
        .max_by_key(|stream| stream.bandwidth.unwrap_or(0))
        .and_then(|stream| stream.group_id.clone());
    let subtitle_id = streams
        .iter()
        .filter(|stream| stream.media_type == Some(MediaType::Subtitles))
        .max_by_key(|stream| stream.bandwidth.unwrap_or(0))
        .and_then(|stream| stream.group_id.clone());
    for stream in streams
        .iter_mut()
        .filter(|stream| stream.resolution.is_some())
    {
        if stream.audio_id.is_none() {
            stream.audio_id = audio_id.clone();
        }
        if stream.subtitle_id.is_none() {
            stream.subtitle_id = subtitle_id.clone();
        }
    }
}

fn normalize_dash_extensions(streams: &mut [Stream]) {
    for stream in streams {
        let segment_count = stream.segments_count();
        if stream.media_type == Some(MediaType::Subtitles) {
            if stream.extension.as_deref() == Some("mp4") {
                stream.extension = Some("m4s".to_string());
            }
        } else if stream.extension.is_none() || segment_count > 1 {
            stream.extension = Some("m4s".to_string());
        }
    }
}

fn preprocess_dash_text(raw_text: &str, config: &ParserConfig) -> Result<String> {
    let trimmed = raw_text.trim().trim_start_matches('\u{feff}').trim_start();
    let processor = DefaultDashContentProcessor;
    if processor.can_process(ExtractorType::MpegDash, trimmed, config) {
        processor.process(trimmed, config)
    } else {
        Ok(trimmed.to_string())
    }
}

fn apply_content_protection(
    stream: &mut Stream,
    adaptation: roxmltree::Node<'_, '_>,
    representation: roxmltree::Node<'_, '_>,
) {
    let protection =
        content_protection_info(adaptation).merge_missing(content_protection_info(representation));
    let encryption = EncryptionInfo {
        method: EncryptionMethod::Cenc,
        kid: protection.kid,
        scheme: protection.scheme,
        protection_data: protection.protection_data,
        source: KeySource::Inline,
        ..EncryptionInfo::default()
    };
    if let Some(playlist) = &mut stream.playlist {
        if let Some(init) = &mut playlist.media_init {
            init.encryption = encryption.clone();
        }
        for segment in &mut playlist.media_parts[0].media_segments {
            segment.encryption = encryption.clone();
        }
    }
}

fn has_content_protection(node: roxmltree::Node<'_, '_>) -> bool {
    children(node, "ContentProtection").next().is_some()
}

#[derive(Clone, Debug, Default)]
struct DashProtectionInfo {
    kid: Option<Vec<u8>>,
    scheme: Option<String>,
    protection_data: Option<Vec<u8>>,
}

impl DashProtectionInfo {
    fn merge_missing(mut self, other: Self) -> Self {
        if self.kid.is_none() {
            self.kid = other.kid;
        }
        if self.scheme.is_none() {
            self.scheme = other.scheme;
        }
        if self.protection_data.is_none() {
            self.protection_data = other.protection_data;
        }
        self
    }
}

fn content_protection_info(node: roxmltree::Node<'_, '_>) -> DashProtectionInfo {
    let mut info = DashProtectionInfo::default();
    let mut first_pssh = None;
    let mut widevine_pssh = None;
    for protection in children(node, "ContentProtection") {
        if info.kid.is_none() {
            info.kid = content_protection_kid(protection);
        }
        if info.scheme.is_none() {
            info.scheme = content_protection_scheme(protection);
        }
        let pssh = children(protection, "pssh")
            .find_map(|pssh| pssh.text().map(|text| text.as_bytes().to_vec()));
        if first_pssh.is_none() {
            first_pssh = pssh.clone();
        }
        if is_widevine_protection(protection) && pssh.is_some() {
            widevine_pssh = pssh;
        }
    }
    info.protection_data = widevine_pssh.or(first_pssh);
    info
}

fn content_protection_kid(node: roxmltree::Node<'_, '_>) -> Option<Vec<u8>> {
    node.attributes()
        .find(|attribute| attribute.name().eq_ignore_ascii_case("default_KID"))
        .and_then(|attribute| parse_kid_hex(attribute.value()))
}

fn content_protection_scheme(node: roxmltree::Node<'_, '_>) -> Option<String> {
    if let Some(value) = attr(node, "value")
        && !value.trim().is_empty()
    {
        return Some(value.trim().to_ascii_lowercase());
    }
    attr(node, "schemeIdUri")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
}

fn is_widevine_protection(node: roxmltree::Node<'_, '_>) -> bool {
    attr(node, "schemeIdUri")
        .map(|value| normalize_token(value).contains("edef8ba979d64acea3c827dcd51d21ed"))
        .unwrap_or(false)
}

fn parse_kid_hex(value: &str) -> Option<Vec<u8>> {
    let first = value
        .split_whitespace()
        .next()
        .unwrap_or(value)
        .trim_matches('{')
        .trim_matches('}');
    let hex = first.chars().filter(|ch| *ch != '-').collect::<String>();
    if hex.len() != 32 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = Vec::with_capacity(16);
    let mut chars = hex.chars();
    while let Some(high) = chars.next() {
        let low = chars.next()?;
        let high = hex_value(high)?;
        let low = hex_value(low)?;
        bytes.push((high << 4) | low);
    }
    Some(bytes)
}

fn hex_value(ch: char) -> Option<u8> {
    match ch {
        '0'..='9' => Some(ch as u8 - b'0'),
        'a'..='f' => Some(ch as u8 - b'a' + 10),
        'A'..='F' => Some(ch as u8 - b'A' + 10),
        _ => None,
    }
}

fn init_segment(url: String, range: Option<&str>) -> Result<MediaSegment> {
    let mut segment = MediaSegment {
        index: -1,
        url,
        ..MediaSegment::default()
    };
    if let Some(range) = range {
        apply_range(&mut segment, range)?;
    }
    Ok(segment)
}

fn apply_range(segment: &mut MediaSegment, range: &str) -> Result<()> {
    let (start, stop) = range
        .split_once('-')
        .ok_or_else(|| Error::protocol("range must use start-stop syntax"))?;
    let start = start
        .parse::<i64>()
        .map_err(|_| Error::protocol("range start is invalid"))?;
    let stop = stop
        .parse::<i64>()
        .map_err(|_| Error::protocol("range stop is invalid"))?;
    segment.start_range = Some(start);
    segment.expected_length = Some(stop - start + 1);
    Ok(())
}

fn media_type(source_type: Option<&str>, codecs: Option<&str>) -> Option<MediaType> {
    if matches!(codecs, Some("stpp" | "wvtt")) {
        return Some(MediaType::Subtitles);
    }
    let value = source_type.unwrap_or_default();
    match value.split('/').next().unwrap_or_default() {
        "audio" => Some(MediaType::Audio),
        "text" => Some(MediaType::Subtitles),
        _ => None,
    }
}

fn extension_from_mime(mime_type: Option<&str>, forces_subtitle_role: bool) -> Option<String> {
    if forces_subtitle_role && mime_type.is_some_and(|value| value.contains("ttml")) {
        return Some("ttml".to_string());
    }
    mime_type.and_then(|value| value.split('/').nth(1).map(str::to_string))
}

#[derive(Clone, Copy)]
struct ParsedRole {
    value: RoleType,
    forces_subtitle_media_type: bool,
}

fn role_type(node: roxmltree::Node<'_, '_>) -> Option<ParsedRole> {
    let role = first_child(node, "Role")?;
    let value = attr(role, "value")?;
    parse_role_value(value)
}

fn parse_role_value(value: &str) -> Option<ParsedRole> {
    if let Some(role) = RoleType::parse_enum_token(value) {
        return Some(ParsedRole {
            value: role,
            forces_subtitle_media_type: role == RoleType::Subtitle,
        });
    }
    if value.contains('-') {
        let role = RoleType::parse_enum_token(&value.replace('-', ""))?;
        return Some(ParsedRole {
            value: role,
            forces_subtitle_media_type: role == RoleType::ForcedSubtitle,
        });
    }
    None
}

fn resolution(representation: roxmltree::Node<'_, '_>) -> Option<String> {
    let width = attr(representation, "width")?;
    let height = attr(representation, "height")?;
    Some(format!("{width}x{height}"))
}

fn audio_channels(node: roxmltree::Node<'_, '_>) -> Option<String> {
    first_child(node, "AudioChannelConfiguration")
        .and_then(|node| attr(node, "value").map(str::to_string))
}

fn source_frame_rate(
    adaptation: roxmltree::Node<'_, '_>,
    representation: roxmltree::Node<'_, '_>,
) -> Result<Option<f64>> {
    match parse_frame_rate(attr(adaptation, "frameRate"))? {
        Some(value) => Ok(Some(value)),
        None => parse_frame_rate(attr(representation, "frameRate")),
    }
}

fn parse_frame_rate(value: Option<&str>) -> Result<Option<f64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some((num, den)) = value.split_once('/') else {
        return Ok(None);
    };
    let num = parse_manifest_f64(num, "DASH frameRate numerator")?;
    let den = parse_manifest_f64(den, "DASH frameRate denominator")?;
    let divided = num / den;
    if divided.is_nan() {
        return Ok(Some(f64::NAN));
    }
    if divided.is_infinite() {
        return Err(Error::protocol("DASH frameRate is invalid"));
    }
    let value = (divided * 1000.0).round() / 1000.0;
    if value.is_finite() {
        Ok(Some(value))
    } else {
        Err(Error::protocol("DASH frameRate is invalid"))
    }
}

fn replace_template(
    template: &str,
    representation_id: &str,
    bandwidth: &str,
    number: Option<i64>,
    time: Option<i64>,
) -> String {
    let mut value = template
        .replace("$RepresentationID$", representation_id)
        .replace("$Bandwidth$", bandwidth);
    if let Some(number) = number {
        value = value.replace("$Number$", &number.to_string());
        while let Some(replaced) = replace_formatted_number_token_once(&value, number) {
            value = replaced;
        }
    }
    if let Some(time) = time {
        value = value.replace("$Time$", &time.to_string());
    }
    value
}

fn replace_formatted_number_token_once(input: &str, value: i64) -> Option<String> {
    let needle = "$Number%";
    let start = input.find(needle)?;
    let width_start = start + needle.len();
    let rest = input.get(width_start..)?;
    let width_end_rel = rest.find("d$")?;
    let width_text = rest.get(..width_end_rel).unwrap_or_default();
    let width = width_text
        .trim_start_matches('0')
        .parse::<usize>()
        .unwrap_or(0);
    let token_end = width_start + width_end_rel + 2;
    let before = input.get(..start).unwrap_or_default();
    let after = input.get(token_end..).unwrap_or_default();
    Some(format!("{before}{value:0width$}{after}"))
}

fn extend_base_url(node: roxmltree::Node<'_, '_>, base_url: &str) -> String {
    match child_text(node, "BaseURL") {
        Some(value) => combine_url(base_url, &normalize_provider_base_url(&value)),
        None => base_url.to_string(),
    }
}

fn normalize_provider_base_url(value: &str) -> String {
    if value.contains("kkbox.com.tw/") {
        value.replace("//https:%2F%2F", "//")
    } else {
        value.to_string()
    }
}

fn parse_duration(value: &str, field: &str) -> Result<f64> {
    if !value.starts_with('P') {
        return Err(Error::protocol(format!("{field} duration is invalid")));
    }
    let mut number = String::new();
    let mut in_time = false;
    let mut total = 0.0;
    for ch in value.chars().skip(1) {
        if ch == 'T' {
            in_time = true;
            continue;
        }
        if ch.is_ascii_digit() || ch == '.' {
            number.push(ch);
            continue;
        }
        if number.is_empty() {
            return Err(Error::protocol(format!("{field} duration is invalid")));
        }
        let parsed = parse_f64(&number, field)?;
        number.clear();
        total += match (in_time, ch) {
            (false, 'D') => parsed * 86_400.0,
            (true, 'H') => parsed * 3_600.0,
            (true, 'M') => parsed * 60.0,
            (true, 'S') => parsed,
            _ => return Err(Error::protocol(format!("{field} duration is invalid"))),
        };
    }
    if !number.is_empty() {
        return Err(Error::protocol(format!("{field} duration is invalid")));
    }
    Ok(total)
}

fn parse_optional_i32(value: Option<&str>, field: &str, default: i32) -> Result<i32> {
    match value {
        Some(value) => parse_i32(value, field),
        None => Ok(default),
    }
}

fn parse_optional_i64(value: Option<&str>, field: &str, default: i64) -> Result<i64> {
    match value {
        Some(value) => parse_i64(value, field),
        None => Ok(default),
    }
}

fn parse_optional_i32_to_i64(value: Option<&str>, field: &str, default: i32) -> Result<i64> {
    let value = parse_optional_i32(value, field, default)?;
    Ok(i64::from(value))
}

fn parse_i32(value: &str, field: &str) -> Result<i32> {
    value
        .parse::<i32>()
        .map_err(|_| Error::protocol(format!("{field} is invalid")))
}

fn parse_i64(value: &str, field: &str) -> Result<i64> {
    value
        .parse::<i64>()
        .map_err(|_| Error::protocol(format!("{field} is invalid")))
}

fn parse_f64(value: &str, field: &str) -> Result<f64> {
    let parsed = value
        .parse::<f64>()
        .map_err(|_| Error::protocol(format!("{field} is invalid")))?;
    if parsed.is_finite() {
        Ok(parsed)
    } else {
        Err(Error::protocol(format!("{field} is invalid")))
    }
}

fn unix_now_seconds() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

fn children<'a>(
    node: roxmltree::Node<'a, 'a>,
    name: &'static str,
) -> impl Iterator<Item = roxmltree::Node<'a, 'a>> {
    node.children()
        .filter(move |child| child.is_element() && child.tag_name().name() == name)
}

fn first_child<'a>(
    node: roxmltree::Node<'a, 'a>,
    name: &'static str,
) -> Option<roxmltree::Node<'a, 'a>> {
    children(node, name).next()
}

fn child_text(node: roxmltree::Node<'_, '_>, name: &'static str) -> Option<String> {
    first_child(node, name)
        .and_then(|child| child.text())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn attr<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|attribute| attribute.name() == name)
        .map(|attribute| attribute.value())
}

fn normalize_token(value: &str) -> String {
    value
        .chars()
        .filter(|ch| *ch != '-' && *ch != '_' && *ch != ' ')
        .flat_map(char::to_lowercase)
        .collect()
}
