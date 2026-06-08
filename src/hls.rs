//! HLS manifest parsing.

use crate::attribute::{hls_attribute, non_empty_hls_attribute};
use crate::config::LogLevel;
use crate::datetime::parse_manifest_timestamp;
use crate::error::{Error, Result};
use crate::http::DefaultHttpClient;
use crate::manifest::{
    EncryptionInfo, EncryptionMethod, ExtractorType, KeySource, MediaPart, MediaSegment, MediaType,
    Playlist, Stream,
};
use crate::numeric::{parse_manifest_f64, refresh_interval_i32_value};
use crate::processor::{
    ContentProcessor, DefaultHlsContentProcessor, DefaultHlsKeyProcessor, DefaultUrlProcessor,
    KeyProcessor, ParserConfig, UrlProcessor, resolve_media_url,
};
use crate::source::SourceLoader;

const ALLOW_HLS_MULTI_EXT_MAP_WARNING: &str = "Multiple #EXT-X-MAP tags are now allowed for detection. However, this software may not handle them correctly. Please manually verify the content's integrity";

/// Parsed HLS manifest result.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HlsManifest {
    /// Streams exposed by the playlist.
    pub streams: Vec<Stream>,
    /// Whether the source was a master playlist.
    pub is_master: bool,
    /// Whether the workflow must force binary merge due unsupported encryption.
    pub requires_binary_merge: bool,
}

/// HLS parser with default URL and key processors.
pub struct HlsParser {
    source_loader: SourceLoader,
    url_processors: Vec<Box<dyn UrlProcessor>>,
    key_processors: Vec<Box<dyn KeyProcessor>>,
}

impl Default for HlsParser {
    fn default() -> Self {
        Self {
            source_loader: SourceLoader::new(),
            url_processors: vec![Box::<DefaultUrlProcessor>::default()],
            key_processors: vec![Box::<DefaultHlsKeyProcessor>::default()],
        }
    }
}

impl HlsParser {
    /// Creates a parser with default processors.
    pub fn new() -> Self {
        Self::default()
    }

    /// Uses an explicit source loader for selected media playlist requests.
    pub fn with_source_loader(mut self, source_loader: SourceLoader) -> Self {
        self.source_loader = source_loader;
        self
    }

    /// Uses one shared HTTP transport for media playlists and key requests.
    pub fn with_http(mut self, http: DefaultHttpClient) -> Self {
        self.source_loader = SourceLoader::new().with_http(http.clone());
        self.key_processors = vec![Box::new(DefaultHlsKeyProcessor::new(http))];
        self
    }

    /// Parses HLS text from a known URL after default manifest text processing.
    pub async fn parse(
        &self,
        raw_text: &str,
        url: &str,
        config: &ParserConfig,
    ) -> Result<HlsManifest> {
        let mut effective_config = config.clone();
        if effective_config.url.is_empty() {
            effective_config.url = url.to_string();
        }
        push_allow_hls_multi_ext_map_warning(&effective_config);
        let text = preprocess_hls_text(raw_text, &effective_config)?;
        let text = text.as_str();
        if !text.starts_with("#EXTM3U") {
            return Err(Error::protocol("HLS input must start with #EXTM3U"));
        }
        if is_master_playlist(text) {
            let streams = self.parse_master(text, url, &effective_config)?;
            Ok(HlsManifest {
                streams,
                is_master: true,
                requires_binary_merge: false,
            })
        } else {
            let playlist = self
                .parse_media_playlist(text, url, &effective_config)
                .await?;
            let extension = if playlist.media_init.is_some() {
                "mp4"
            } else {
                "ts"
            };
            let requires_binary_merge = playlist_has_unknown_encryption(&playlist);
            Ok(HlsManifest {
                streams: vec![Stream {
                    url: url.to_string(),
                    extension: Some(extension.to_string()),
                    playlist: Some(playlist),
                    ..Stream::default()
                }],
                is_master: false,
                requires_binary_merge,
            })
        }
    }

    /// Fetches missing media playlists for selected streams.
    pub async fn fetch_playlists(
        &self,
        streams: &mut [Stream],
        config: &mut ParserConfig,
    ) -> Result<bool> {
        self.fetch_playlists_inner(streams, config, false).await
    }

    async fn fetch_playlists_inner(
        &self,
        streams: &mut [Stream],
        config: &mut ParserConfig,
        force_refresh: bool,
    ) -> Result<bool> {
        let master_url = config.url.clone();
        let mut requires_binary_merge = false;
        for stream in streams {
            if !force_refresh && stream.playlist.is_some() {
                continue;
            }
            let loaded = match self.source_loader.load_source(&stream.url, config).await {
                Ok(loaded) => loaded,
                Err(error) if should_refresh_from_master(&master_url, stream) => {
                    config.push_diagnostic(
                        LogLevel::Warn,
                        "Can not load m3u8. Try refreshing url from master url...",
                    );
                    if self
                        .refresh_stream_url_from_master(stream, &master_url, config)
                        .await?
                    {
                        self.source_loader.load_source(&stream.url, config).await?
                    } else {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            };
            push_allow_hls_multi_ext_map_warning(config);
            let playlist = self
                .parse_media_playlist(&loaded.text, &loaded.final_url, config)
                .await?;
            requires_binary_merge |= playlist_has_unknown_encryption(&playlist);
            if stream.media_type == Some(MediaType::Subtitles) {
                if playlist_contains_extension(&playlist, ".ttml") {
                    stream.extension = Some("ttml".to_string());
                } else if playlist_contains_extension(&playlist, ".vtt")
                    || playlist_contains_extension(&playlist, ".webvtt")
                {
                    stream.extension = Some("vtt".to_string());
                }
            } else {
                stream.extension = Some(if playlist.media_init.is_some() {
                    "m4s".to_string()
                } else {
                    "ts".to_string()
                });
            }
            match &mut stream.playlist {
                Some(existing) if existing.media_init.is_some() => {
                    existing.media_parts = playlist.media_parts;
                    existing.is_live = playlist.is_live;
                    existing.refresh_interval_ms = playlist.refresh_interval_ms;
                    existing.target_duration = playlist.target_duration;
                }
                _ => stream.playlist = Some(playlist),
            }
        }
        Ok(requires_binary_merge)
    }

    /// Refreshes selected playlists.
    pub async fn refresh_playlists(
        &self,
        streams: &mut [Stream],
        config: &mut ParserConfig,
    ) -> Result<bool> {
        self.fetch_playlists_inner(streams, config, true).await
    }

    fn parse_master(&self, text: &str, url: &str, config: &ParserConfig) -> Result<Vec<Stream>> {
        let mut streams = Vec::new();
        let mut pending_variant: Option<Stream> = None;
        for line in text.lines().filter(|line| !line.is_empty()) {
            if let Some(value) = line.strip_prefix("#EXT-X-STREAM-INF:") {
                let average_bandwidth = hls_attribute(value, "AVERAGE-BANDWIDTH")?;
                let bandwidth_value = if average_bandwidth.as_deref().is_none_or(str::is_empty) {
                    hls_attribute(value, "BANDWIDTH")?
                } else {
                    average_bandwidth
                };
                let bandwidth = Some(parse_hls_int_as_i64(
                    bandwidth_value.as_deref(),
                    "HLS bandwidth",
                )?);
                let audio_id = non_empty_hls_attribute(value, "AUDIO")?;
                let mut codecs = hls_attribute(value, "CODECS")?;
                if codecs.is_some() && audio_id.is_some() {
                    codecs = codecs.and_then(|value| value.split(',').next().map(str::to_string));
                }
                let frame_rate = match hls_attribute(value, "FRAME-RATE")? {
                    Some(value) if !value.is_empty() => {
                        Some(parse_manifest_f64(&value, "HLS frame rate")?)
                    }
                    _ => None,
                };
                pending_variant = Some(Stream {
                    original_url: config.original_url.clone(),
                    bandwidth,
                    codecs,
                    resolution: hls_attribute(value, "RESOLUTION")?,
                    frame_rate,
                    audio_id,
                    video_id: non_empty_hls_attribute(value, "VIDEO")?,
                    subtitle_id: non_empty_hls_attribute(value, "SUBTITLES")?,
                    video_range: non_empty_hls_attribute(value, "VIDEO-RANGE")?,
                    ..Stream::default()
                });
            } else if let Some(value) = line.strip_prefix("#EXT-X-MEDIA:") {
                if let Some(stream) = self.parse_media_rendition(value, url, config)? {
                    streams.push(stream);
                }
            } else if line.starts_with('#') {
                continue;
            } else if let Some(mut stream) = pending_variant.take() {
                stream.url = self.process_url(line, url, config)?;
                stream.id = stream.url.clone();
                streams.push(stream);
            }
        }
        Ok(deduplicate_master_streams(streams))
    }

    fn parse_media_rendition(
        &self,
        value: &str,
        url: &str,
        config: &ParserConfig,
    ) -> Result<Option<Stream>> {
        let media_type_value = hls_attribute(value, "TYPE")?
            .ok_or_else(|| Error::protocol("HLS media rendition requires TYPE"))?;
        let media_type = match media_type_value.replace('-', "_").as_str() {
            "AUDIO" => Some(MediaType::Audio),
            "VIDEO" => Some(MediaType::Video),
            "SUBTITLES" => Some(MediaType::Subtitles),
            "CLOSED_CAPTIONS" => return Ok(None),
            _ => None,
        };
        let Some(uri) = non_empty_hls_attribute(value, "URI")? else {
            return Ok(None);
        };
        let stream_url = self.process_url(&uri, url, config)?;
        let characteristics =
            non_empty_hls_attribute(value, "CHARACTERISTICS")?.and_then(|value| {
                value
                    .split(',')
                    .next_back()
                    .and_then(|item| item.split('.').next_back())
                    .map(str::to_string)
            });
        Ok(Some(Stream {
            id: stream_url.clone(),
            media_type,
            group_id: hls_attribute(value, "GROUP-ID")?,
            language: non_empty_hls_attribute(value, "LANGUAGE")?,
            name: non_empty_hls_attribute(value, "NAME")?,
            default: None,
            forced: None,
            channels: non_empty_hls_attribute(value, "CHANNELS")?,
            characteristics,
            url: stream_url,
            original_url: config.original_url.clone(),
            ..Stream::default()
        }))
    }

    async fn parse_media_playlist(
        &self,
        text: &str,
        url: &str,
        config: &ParserConfig,
    ) -> Result<Playlist> {
        let allow_multi_map = config
            .custom_parser_args
            .get("AllowHlsMultiExtMap")
            .is_some_and(|value| value == "true");
        let mut playlist = Playlist {
            url: url.to_string(),
            ..Playlist::default()
        };
        let mut parts: Vec<MediaPart> = Vec::new();
        let mut current_segments: Vec<MediaSegment> = Vec::new();
        let mut pending_segment = MediaSegment::default();
        let mut expect_segment = false;
        let mut is_endlist = false;
        let mut segment_index = 0_i64;
        let mut current_encryption = initial_encryption(config);
        let mut last_key_line = String::new();
        let mut last_range_next = None;
        let mut provider_ad_removed = false;
        let mut inside_provider_ad = false;

        for line in text.lines().filter(|line| !line.is_empty()) {
            if let Some(value) = line.strip_prefix("#EXT-X-BYTERANGE:") {
                let range = parse_byte_range(value, last_range_next)?;
                pending_segment.expected_length = Some(range.0);
                pending_segment.start_range = Some(range.1);
                expect_segment = true;
            } else if let Some(value) = line.strip_prefix("#EXT-X-PLAYLIST-TYPE:") {
                is_endlist = value.trim().ends_with("VOD");
            } else if line.starts_with("#UPLYNK-SEGMENT") {
                if line.contains(",ad") {
                    inside_provider_ad = true;
                } else if line.contains(",segment") {
                    inside_provider_ad = false;
                }
                continue;
            }
            if inside_provider_ad {
                continue;
            }
            if let Some(value) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
                playlist.target_duration = Some(parse_manifest_f64(value, "HLS target duration")?);
            } else if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
                segment_index = parse_i64(value.trim(), "HLS media sequence")?;
            } else if let Some(value) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
                let value = value.trim();
                if parse_manifest_timestamp(value).is_none() {
                    return Err(Error::protocol("invalid HLS program date time"));
                }
                pending_segment.program_date_time = Some(value.to_string());
            } else if line.starts_with("#EXT-X-DISCONTINUITY") {
                if provider_ad_removed && let Some(part) = parts.pop() {
                    current_segments = part.media_segments;
                    provider_ad_removed = false;
                    continue;
                }
                if !current_segments.is_empty() {
                    parts.push(MediaPart {
                        media_segments: current_segments,
                    });
                    current_segments = Vec::new();
                    last_range_next = None;
                }
            } else if line.starts_with("#EXT-X-KEY:") {
                if line != last_key_line {
                    current_encryption = self.parse_key(line, url, text, config).await?;
                }
                last_key_line = line.to_string();
            } else if let Some(value) = line.strip_prefix("#EXTINF:") {
                let (duration, title) = parse_extinf(value)?;
                pending_segment.duration = duration;
                pending_segment.title = title;
                pending_segment.index = segment_index;
                apply_encryption(&mut pending_segment, &current_encryption, segment_index);
                expect_segment = true;
                segment_index += 1;
            } else if line.starts_with("#EXT-X-ENDLIST") {
                if !current_segments.is_empty() {
                    parts.push(MediaPart {
                        media_segments: current_segments,
                    });
                    current_segments = Vec::new();
                }
                is_endlist = true;
            } else if let Some(value) = line.strip_prefix("#EXT-X-MAP:") {
                if playlist.media_init.is_none() || provider_ad_removed {
                    playlist.media_init = Some(self.parse_map(
                        value,
                        url,
                        config,
                        &current_encryption,
                        segment_index,
                    )?);
                } else {
                    if !current_segments.is_empty() {
                        parts.push(MediaPart {
                            media_segments: current_segments,
                        });
                        current_segments = Vec::new();
                        last_range_next = None;
                    }
                    if !allow_multi_map {
                        is_endlist = true;
                        break;
                    }
                }
            } else if line.starts_with('#') {
                continue;
            } else if expect_segment {
                let segment_url = self.process_url(line, url, config)?;
                pending_segment.url = segment_url;
                if is_provider_ad_segment(&pending_segment.url) {
                    segment_index = segment_index.saturating_sub(1);
                    provider_ad_removed = true;
                    pending_segment = MediaSegment::default();
                    expect_segment = false;
                    continue;
                }
                if let (Some(start), Some(length)) =
                    (pending_segment.start_range, pending_segment.expected_length)
                {
                    last_range_next = Some(start + length);
                }
                current_segments.push(pending_segment);
                pending_segment = MediaSegment::default();
                expect_segment = false;
            }
        }
        if !is_endlist {
            parts.push(MediaPart {
                media_segments: current_segments,
            });
        }
        playlist.media_parts = parts;
        playlist.is_live = !is_endlist;
        if playlist.is_live {
            playlist.refresh_interval_ms =
                refresh_interval_i32_value(playlist.target_duration.unwrap_or(5.0) * 2.0 * 1000.0);
        }
        Ok(playlist)
    }

    fn parse_map(
        &self,
        value: &str,
        url: &str,
        config: &ParserConfig,
        current_encryption: &EncryptionInfo,
        segment_index: i64,
    ) -> Result<MediaSegment> {
        let uri = hls_attribute(value, "URI")?
            .ok_or_else(|| Error::protocol("#EXT-X-MAP requires URI"))?;
        let mut segment = MediaSegment {
            index: -1,
            url: self.process_url(&uri, url, config)?,
            ..MediaSegment::default()
        };
        if let Some(range) = hls_attribute(value, "BYTERANGE")? {
            let (length, start) = parse_byte_range(&range, Some(0))?;
            segment.expected_length = Some(length);
            segment.start_range = Some(start);
        }
        apply_encryption(&mut segment, current_encryption, segment_index);
        Ok(segment)
    }

    async fn parse_key(
        &self,
        line: &str,
        url: &str,
        text: &str,
        config: &ParserConfig,
    ) -> Result<EncryptionInfo> {
        for processor in &self.key_processors {
            if processor.can_process(ExtractorType::Hls, line, url, text, config) {
                return processor.process(line, url, text, config).await;
            }
        }
        Err(Error::protocol(
            "no HLS key processor accepted the key line",
        ))
    }

    fn process_url(&self, value: &str, url: &str, config: &ParserConfig) -> Result<String> {
        let processors = self
            .url_processors
            .iter()
            .map(|processor| processor.as_ref())
            .collect::<Vec<_>>();
        resolve_media_url(ExtractorType::Hls, value, url, config, &processors)
    }

    async fn refresh_stream_url_from_master(
        &self,
        stream: &mut Stream,
        master_url: &str,
        config: &ParserConfig,
    ) -> Result<bool> {
        if master_url.is_empty() {
            return Ok(false);
        }
        let mut master_config = config.clone();
        let loaded = self
            .source_loader
            .load_source(master_url, &mut master_config)
            .await?;
        let refreshed = self.parse_master(&loaded.text, &loaded.final_url, &master_config)?;
        if let Some(replacement) = refreshed
            .into_iter()
            .find(|candidate| stream_refresh_identity_matches(stream, candidate))
        {
            let old_url = stream.url.clone();
            config.push_diagnostic(LogLevel::Debug, format!("{old_url} => {}", replacement.url));
            stream.url = replacement.url;
            return Ok(true);
        }
        Ok(false)
    }
}

fn push_allow_hls_multi_ext_map_warning(config: &ParserConfig) {
    if config
        .custom_parser_args
        .get("AllowHlsMultiExtMap")
        .is_some_and(|value| value == "true")
    {
        config.push_diagnostic(LogLevel::Warn, ALLOW_HLS_MULTI_EXT_MAP_WARNING);
    }
}

fn preprocess_hls_text(raw_text: &str, config: &ParserConfig) -> Result<String> {
    let trimmed = raw_text.trim().trim_start_matches('\u{feff}').trim_start();
    if !trimmed.starts_with("#EXTM3U") {
        return Ok(trimmed.to_string());
    }
    let processor = DefaultHlsContentProcessor;
    if processor.can_process(ExtractorType::Hls, trimmed, config) {
        processor.process(trimmed, config)
    } else {
        Ok(trimmed.to_string())
    }
}

fn should_refresh_from_master(master_url: &str, stream: &Stream) -> bool {
    !master_url.is_empty() && master_url != stream.url
}

fn stream_refresh_identity_matches(left: &Stream, right: &Stream) -> bool {
    left.media_type == right.media_type
        && left.group_id == right.group_id
        && left.language == right.language
        && left.name == right.name
        && left.codecs == right.codecs
        && left.resolution == right.resolution
        && left.frame_rate == right.frame_rate
        && left.channels == right.channels
        && left.video_range == right.video_range
        && left.bandwidth == right.bandwidth
}

fn is_master_playlist(text: &str) -> bool {
    text.contains("#EXT-X-STREAM-INF")
}

fn deduplicate_master_streams(streams: Vec<Stream>) -> Vec<Stream> {
    let mut seen = std::collections::BTreeSet::new();
    streams
        .into_iter()
        .filter(|stream| seen.insert(stream.url.clone()))
        .collect()
}

fn initial_encryption(config: &ParserConfig) -> EncryptionInfo {
    let mut info = EncryptionInfo::default();
    if let Some(method) = config.custom_method {
        info.method = method;
    }
    if let Some(key) = &config.custom_key
        && !key.is_empty()
    {
        info.key = Some(key.clone());
        info.source = KeySource::Custom;
    }
    if let Some(iv) = &config.custom_iv
        && !iv.is_empty()
    {
        info.iv = Some(iv.clone());
    }
    info
}

fn apply_encryption(segment: &mut MediaSegment, current: &EncryptionInfo, segment_index: i64) {
    if current.method == EncryptionMethod::None {
        return;
    }
    segment.encryption = current.clone();
    if segment.encryption.iv.is_none() {
        segment.encryption.iv = Some(default_iv(segment_index));
    }
}

fn default_iv(segment_index: i64) -> Vec<u8> {
    let value = if segment_index >= 0 {
        segment_index as u128
    } else {
        u128::from(segment_index as u64)
    };
    value.to_be_bytes().to_vec()
}

fn parse_extinf(value: &str) -> Result<(f64, Option<String>)> {
    let (duration, _) = value.split_once(',').unwrap_or((value, ""));
    let duration = parse_manifest_f64(duration, "#EXTINF duration")?;
    Ok((duration, None))
}

fn parse_i64(value: &str, field: &str) -> Result<i64> {
    value
        .parse::<i64>()
        .map_err(|_| Error::protocol(format!("{field} is invalid")))
}

fn parse_byte_range(value: &str, fallback_start: Option<i64>) -> Result<(i64, i64)> {
    if value.split('@').nth(2).is_some() {
        let start = fallback_start.ok_or_else(|| Error::protocol("byte range start is missing"))?;
        return Ok((0, start));
    }
    let (length, start) = match value.split_once('@') {
        Some((length, start)) => (length.trim(), Some(start.trim())),
        None => (value.trim(), None),
    };
    let length = length
        .parse::<i64>()
        .map_err(|_| Error::protocol("byte range length is invalid"))?;
    let start = match start {
        Some(start) => start
            .parse::<i64>()
            .map_err(|_| Error::protocol("byte range start is invalid"))?,
        None => fallback_start.ok_or_else(|| Error::protocol("byte range start is missing"))?,
    };
    Ok((length, start))
}

fn parse_hls_int_as_i64(value: Option<&str>, label: &str) -> Result<i64> {
    let parsed = match value {
        Some(value) => value
            .trim()
            .parse::<i32>()
            .map_err(|_| Error::protocol(format!("{label} is invalid")))?,
        None => 0,
    };
    Ok(i64::from(parsed))
}

fn playlist_has_unknown_encryption(playlist: &Playlist) -> bool {
    playlist
        .media_init
        .as_ref()
        .is_some_and(|segment| segment.encryption.method == EncryptionMethod::Unknown)
        || playlist.media_parts.iter().any(|part| {
            part.media_segments
                .iter()
                .any(|segment| segment.encryption.method == EncryptionMethod::Unknown)
        })
}

fn playlist_contains_extension(playlist: &Playlist, extension: &str) -> bool {
    playlist.media_parts.iter().any(|part| {
        part.media_segments
            .iter()
            .any(|segment| segment.url.contains(extension))
    })
}

fn is_provider_ad_segment(url: &str) -> bool {
    (url.contains("ccode=") && url.contains("/ad/") && url.contains("duration="))
        || (url.contains("ccode=0902") && url.contains("duration="))
}
