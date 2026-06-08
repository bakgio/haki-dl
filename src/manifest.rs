//! Manifest, stream, playlist, segment, and encryption models.

use std::cmp::Ordering;

/// Media type associated with a stream or artifact.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MediaType {
    /// Audio stream.
    Audio,
    /// Video stream.
    Video,
    /// Subtitle stream.
    Subtitles,
    /// Closed captions stream.
    ClosedCaptions,
}

/// Source extractor family.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtractorType {
    /// MPEG-DASH manifest.
    MpegDash,
    /// HLS manifest.
    Hls,
    /// Direct HTTP live TS input.
    HttpLive,
    /// Smooth Streaming manifest.
    Mss,
}

/// Yes/no choice flag used by manifest metadata.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Choice {
    /// Explicit no.
    No,
    /// Explicit yes.
    Yes,
}

/// DASH role value.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoleType {
    Subtitle,
    Main,
    Alternate,
    Supplementary,
    Commentary,
    Dub,
    Description,
    Sign,
    Metadata,
    ForcedSubtitle,
    /// Numeric role value without a named variant.
    Numeric(i32),
}

impl RoleType {
    /// Converts an integer role value into a named role when one exists.
    pub const fn from_number(value: i32) -> Self {
        match value {
            0 => Self::Subtitle,
            1 => Self::Main,
            2 => Self::Alternate,
            3 => Self::Supplementary,
            4 => Self::Commentary,
            5 => Self::Dub,
            6 => Self::Description,
            7 => Self::Sign,
            8 => Self::Metadata,
            9 => Self::ForcedSubtitle,
            other => Self::Numeric(other),
        }
    }

    /// Parses a role enum token by name or integer value.
    pub fn parse_enum_token(value: &str) -> Option<Self> {
        let trimmed = value.trim();
        parse_role_name(trimmed).or_else(|| trimmed.parse::<i32>().ok().map(Self::from_number))
    }
}

fn parse_role_name(value: &str) -> Option<RoleType> {
    match value.to_ascii_lowercase().as_str() {
        "subtitle" => Some(RoleType::Subtitle),
        "main" => Some(RoleType::Main),
        "alternate" => Some(RoleType::Alternate),
        "supplementary" => Some(RoleType::Supplementary),
        "commentary" => Some(RoleType::Commentary),
        "dub" => Some(RoleType::Dub),
        "description" => Some(RoleType::Description),
        "sign" => Some(RoleType::Sign),
        "metadata" => Some(RoleType::Metadata),
        "forcedsubtitle" => Some(RoleType::ForcedSubtitle),
        _ => None,
    }
}

/// Segment encryption method.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EncryptionMethod {
    /// No encryption.
    #[default]
    None,
    Aes128,
    Aes128Ecb,
    SampleAes,
    SampleAesCtr,
    Cenc,
    Chacha20,
    Unknown,
}

impl EncryptionMethod {
    /// Parses the method grammar used by manifests.
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(normalize_token).as_deref() {
            Some("none") => Self::None,
            Some("aes128") => Self::Aes128,
            Some("aes128ecb") => Self::Aes128Ecb,
            Some("sampleaes") => Self::SampleAes,
            Some("sampleaesctr") => Self::SampleAesCtr,
            Some("cenc") => Self::Cenc,
            Some("chacha20") => Self::Chacha20,
            Some(_) => Self::Unknown,
            None => Self::Unknown,
        }
    }
}

/// Origin of key material.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum KeySource {
    /// No key material.
    #[default]
    None,
    /// Inline manifest or CLI material.
    Inline,
    /// Key URI from a manifest.
    Uri,
    /// Key file from CLI/API.
    File,
    /// Key text file lookup.
    KeyTextFile,
    /// API-provided custom material.
    Custom,
}

/// Encryption metadata attached to init or media segments.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EncryptionInfo {
    /// Encryption method.
    pub method: EncryptionMethod,
    /// Content key bytes when already known.
    pub key: Option<Vec<u8>>,
    /// Initialization vector bytes when known.
    pub iv: Option<Vec<u8>>,
    /// Key identifier bytes when known.
    pub kid: Option<Vec<u8>>,
    /// Protection scheme such as cenc/cbcs when known.
    pub scheme: Option<String>,
    /// Raw protection data needed by downstream decryptors.
    pub protection_data: Option<Vec<u8>>,
    /// Key material source.
    pub source: KeySource,
}

impl EncryptionInfo {
    /// Returns true when the segment uses encryption.
    pub fn is_encrypted(&self) -> bool {
        self.method != EncryptionMethod::None
    }
}

/// Inclusive byte range over a segment resource.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteRange {
    /// First byte position.
    pub start: i64,
    /// Expected byte length.
    pub length: i64,
}

impl ByteRange {
    /// Returns the inclusive final byte position.
    pub fn end(&self) -> Option<i64> {
        Some(self.start.wrapping_add(self.length).wrapping_sub(1))
    }
}

/// One media segment or initialization segment.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug)]
pub struct MediaSegment {
    /// Segment index.
    pub index: i64,
    /// Segment duration in seconds.
    pub duration: f64,
    /// Optional title.
    pub title: Option<String>,
    /// Program date-time as source text until a time crate is introduced.
    pub program_date_time: Option<String>,
    /// Byte-range start.
    pub start_range: Option<i64>,
    /// Expected byte length.
    pub expected_length: Option<i64>,
    /// Encryption metadata.
    pub encryption: EncryptionInfo,
    /// Segment URL.
    pub url: String,
    /// DASH name generated from a template variable.
    pub name_from_var: Option<String>,
}

impl Default for MediaSegment {
    fn default() -> Self {
        Self {
            index: 0,
            duration: 0.0,
            title: None,
            program_date_time: None,
            start_range: None,
            expected_length: None,
            encryption: EncryptionInfo::default(),
            url: String::new(),
            name_from_var: None,
        }
    }
}

impl MediaSegment {
    /// Returns the inclusive byte-range end when start and length are known.
    pub fn stop_range(&self) -> Option<i64> {
        match (self.start_range, self.expected_length) {
            (Some(start), Some(length)) => Some(start.wrapping_add(length).wrapping_sub(1)),
            _ => None,
        }
    }

    /// Returns true when the segment is encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.encryption.is_encrypted()
    }
}

impl PartialEq for MediaSegment {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
            && (self.duration - other.duration).abs() < 0.001
            && self.title == other.title
            && self.start_range == other.start_range
            && self.stop_range() == other.stop_range()
            && self.expected_length == other.expected_length
            && self.url == other.url
    }
}

/// Segment group separated by discontinuities.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaPart {
    /// Segments in this part.
    pub media_segments: Vec<MediaSegment>,
}

impl MediaPart {
    /// Sum of segment durations in seconds.
    pub fn total_duration(&self) -> f64 {
        self.media_segments
            .iter()
            .map(|segment| segment.duration)
            .sum()
    }
}

/// Playlist attached to a stream.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, PartialEq)]
pub struct Playlist {
    /// Playlist URL.
    pub url: String,
    /// Whether the playlist is live.
    pub is_live: bool,
    /// Live refresh interval in milliseconds.
    pub refresh_interval_ms: f64,
    /// Target duration in seconds.
    pub target_duration: Option<f64>,
    /// Initialization segment.
    pub media_init: Option<MediaSegment>,
    /// Discontinuity-separated media parts.
    pub media_parts: Vec<MediaPart>,
}

impl Default for Playlist {
    fn default() -> Self {
        Self {
            url: String::new(),
            is_live: false,
            refresh_interval_ms: 15_000.0,
            target_duration: None,
            media_init: None,
            media_parts: Vec::new(),
        }
    }
}

impl Playlist {
    /// Sum of all media segment durations in seconds.
    pub fn total_duration(&self) -> f64 {
        self.media_parts.iter().map(MediaPart::total_duration).sum()
    }

    /// Total number of media segments.
    pub fn segments_count(&self) -> usize {
        self.media_parts
            .iter()
            .map(|part| part.media_segments.len())
            .sum()
    }
}

/// Smooth Streaming metadata.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MssData {
    pub four_cc: String,
    pub codec_private_data: String,
    pub stream_type: String,
    pub timescale: i32,
    pub sampling_rate: i32,
    pub channels: i32,
    pub bits_per_sample: i32,
    pub nal_unit_length_field: i32,
    pub duration: i64,
    pub is_protection: bool,
    pub protection_system_id: String,
    pub protection_data: String,
}

/// Stream metadata used by selectors and progress events.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stream {
    /// Stable stream identifier.
    pub id: String,
    /// Stream media type when known.
    pub media_type: Option<MediaType>,
    /// Group identifier.
    pub group_id: Option<String>,
    /// Language tag when known.
    pub language: Option<String>,
    /// Human-readable stream name when known.
    pub name: Option<String>,
    /// Default flag when known.
    pub default: Option<Choice>,
    /// Forced subtitle/audio flag when known.
    pub forced: Option<Choice>,
    /// Duration skipped by selection/range logic.
    pub skipped_duration: Option<f64>,
    /// Smooth Streaming metadata.
    pub mss_data: Option<MssData>,
    /// Bandwidth in bits per second when known.
    pub bandwidth: Option<i64>,
    /// Codec string when known.
    pub codecs: Option<String>,
    /// Resolution label when known.
    pub resolution: Option<String>,
    /// Frame rate when known.
    pub frame_rate: Option<f64>,
    /// Channel label when known.
    pub channels: Option<String>,
    /// Stream extension hint when known.
    pub extension: Option<String>,
    /// DASH role when known.
    pub role: Option<RoleType>,
    /// Video range or color range metadata.
    pub video_range: Option<String>,
    /// HLS/DASH characteristics metadata.
    pub characteristics: Option<String>,
    /// Publish time as source text until a time crate is introduced.
    pub publish_time: Option<String>,
    /// Associated external audio group ID.
    pub audio_id: Option<String>,
    /// Associated external video group ID.
    pub video_id: Option<String>,
    /// Associated external subtitle group ID.
    pub subtitle_id: Option<String>,
    /// DASH period ID.
    pub period_id: Option<String>,
    /// Current URL.
    pub url: String,
    /// Original URL before URL processing.
    pub original_url: String,
    /// Expanded playlist.
    pub playlist: Option<Playlist>,
}

impl Stream {
    /// Number of media segments in the playlist.
    pub fn segments_count(&self) -> usize {
        self.playlist.as_ref().map_or(0, Playlist::segments_count)
    }

    /// Sum of media segment durations in seconds.
    pub fn total_duration(&self) -> Option<f64> {
        self.playlist.as_ref().map(Playlist::total_duration)
    }
}

/// Parsed manifest model.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Manifest {
    /// Extractor family that produced this manifest.
    pub extractor_type: Option<ExtractorType>,
    /// Source URL or path.
    pub source: Option<String>,
    /// Streams exposed by the manifest.
    pub streams: Vec<Stream>,
    /// Non-fatal parser warnings.
    pub warnings: Vec<String>,
    /// Whether any selected playlist should be treated as live.
    pub is_live: bool,
}

/// Sorts streams using compatibility ordering.
pub fn sort_streams_compatible(streams: &mut [Stream]) {
    streams.sort_by(compare_streams_compatible);
}

/// Compares streams using media type, bandwidth descending, then audio channel order descending.
pub fn compare_streams_compatible(left: &Stream, right: &Stream) -> Ordering {
    media_rank(left.media_type)
        .cmp(&media_rank(right.media_type))
        .then_with(|| compare_option_i64_desc(left.bandwidth, right.bandwidth))
        .then_with(|| audio_channel_order(right).cmp(&audio_channel_order(left)))
}

/// Parses the audio channel ordering key used by selection.
pub fn audio_channel_order(stream: &Stream) -> i32 {
    let Some(channels) = &stream.channels else {
        return 0;
    };
    let first = channels.split('/').next().unwrap_or_default();
    first.parse::<i32>().unwrap_or_default()
}

fn media_rank(media_type: Option<MediaType>) -> i32 {
    match media_type {
        None => -1,
        Some(MediaType::Audio) => 0,
        Some(MediaType::Video) => 1,
        Some(MediaType::Subtitles) => 2,
        Some(MediaType::ClosedCaptions) => 3,
    }
}

fn compare_option_i64_desc(left: Option<i64>, right: Option<i64>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => right.cmp(&left),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn normalize_token(value: &str) -> String {
    value
        .chars()
        .filter(|ch| *ch != '-' && *ch != '_' && *ch != ' ')
        .flat_map(char::to_lowercase)
        .collect()
}

/// API stream selection model.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum StreamSelector {
    /// Preserve CLI-interactive behavior where available.
    #[default]
    Interactive,
    /// Auto-select the best basic stream, audio languages, and subtitles.
    Auto,
    /// Select subtitles only.
    SubtitlesOnly,
    /// Select explicit stream identifiers.
    ExplicitIds(Vec<String>),
}
