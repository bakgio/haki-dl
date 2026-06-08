//! Smooth Streaming manifest parsing and initialization generation.

use std::time::{SystemTime, UNIX_EPOCH};

use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::manifest::{
    EncryptionInfo, EncryptionMethod, ExtractorType, KeySource, MediaPart, MediaSegment, MediaType,
    MssData, Playlist, Stream,
};
use crate::processor::{DefaultUrlProcessor, ParserConfig, UrlProcessor, combine_url};

const PLAYREADY_SYSTEM_ID: &str = "9A04F079-9840-4286-AB92-E65BE0885F95";
const WIDEVINE_SYSTEM_ID: &str = "EDEF8BA9-79D6-4ACE-A3C8-27DCD51D21ED";
const START_CODE: &str = "00000001";

/// Parsed Smooth Streaming manifest result.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MssManifest {
    /// Streams exposed by the manifest.
    pub streams: Vec<Stream>,
    /// Non-fatal parser warnings.
    pub warnings: Vec<String>,
    /// Root manifest timescale.
    pub timescale: i32,
    /// Root manifest duration in timescale units.
    pub duration: i64,
    /// Whether the manifest is live.
    pub is_live: bool,
}

/// Generated initialization segment metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MssGeneratedInit {
    /// Initialization segment bytes.
    pub bytes: Vec<u8>,
    /// Codec private data used for byte generation.
    pub codec_private_data: String,
    /// Codec string derived while generating codec configuration boxes.
    pub codecs: Option<String>,
}

/// Smooth Streaming parser.
pub struct MssParser {
    url_processors: Vec<Box<dyn UrlProcessor>>,
}

impl Default for MssParser {
    fn default() -> Self {
        Self {
            url_processors: vec![Box::<DefaultUrlProcessor>::default()],
        }
    }
}

impl MssParser {
    /// Creates a parser with default processors.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses a Smooth Streaming manifest from text.
    pub fn parse(&self, raw_text: &str, url: &str, config: &ParserConfig) -> Result<MssManifest> {
        if !raw_text.contains("<SmoothStreamingMedia") {
            return Err(Error::protocol(
                "Smooth Streaming input must contain SmoothStreamingMedia",
            ));
        }
        let document = roxmltree::Document::parse(raw_text)
            .map_err(|error| Error::protocol(error.to_string()))?;
        let root = document
            .descendants()
            .find(|node| node.has_tag_name("SmoothStreamingMedia"))
            .ok_or_else(|| Error::protocol("SmoothStreamingMedia element not found"))?;

        let timescale = parse_optional_i32(
            attr(root, "TimeScale"),
            "Smooth Streaming TimeScale",
            10_000_000,
        )?;
        let duration = parse_optional_i64(attr(root, "Duration"), "Smooth Streaming Duration", 0)?;
        let is_live = match attr(root, "IsLive") {
            Some(value) => parse_bool(value)?,
            None => false,
        };
        let protection = parse_protection(root)?;
        let base_url = if config.base_url.is_empty() {
            url
        } else {
            &config.base_url
        };

        let mut streams = Vec::new();
        let mut warnings = Vec::new();
        for stream_index in children(root, "StreamIndex") {
            self.parse_stream_index(
                stream_index,
                url,
                base_url,
                timescale,
                duration,
                is_live,
                protection.as_ref(),
                config,
                &mut streams,
                &mut warnings,
            )?;
        }
        apply_default_external_tracks(&mut streams);
        Ok(MssManifest {
            streams,
            warnings,
            timescale,
            duration,
            is_live,
        })
    }

    /// Refreshes selected streams from a newer Smooth Streaming manifest.
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

    /// Applies configured URL processors to already-selected Smooth Streaming streams.
    pub fn process_stream_urls(&self, streams: &mut [Stream], config: &ParserConfig) -> Result<()> {
        for stream in streams {
            let Some(playlist) = &mut stream.playlist else {
                continue;
            };
            if let Some(init) = &mut playlist.media_init
                && !init.url.starts_with("base64://")
                && !init.url.starts_with("hex://")
            {
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
    fn parse_stream_index(
        &self,
        stream_index: roxmltree::Node<'_, '_>,
        manifest_url: &str,
        base_url: &str,
        root_timescale: i32,
        root_duration: i64,
        is_live: bool,
        protection: Option<&MssProtection>,
        config: &ParserConfig,
        streams: &mut Vec<Stream>,
        warnings: &mut Vec<String>,
    ) -> Result<()> {
        let stream_type = attr(stream_index, "Type").unwrap_or_default();
        let name = attr(stream_index, "Name").map(str::to_string);
        let language = attr(stream_index, "Language")
            .filter(|value| value.len() == 3)
            .map(str::to_string);
        let stream_url_pattern = attr(stream_index, "Url").map(str::to_string);

        for quality in children(stream_index, "QualityLevel") {
            let four_cc = attr(quality, "FourCC")
                .ok_or_else(|| Error::protocol("Smooth Streaming QualityLevel requires FourCC"))?
                .to_ascii_uppercase();
            if !MssInitGenerator::can_handle(&four_cc) {
                warnings.push(format!("{four_cc} not supported! Skiped."));
                continue;
            }

            let pattern = attr(quality, "Url")
                .map(str::to_string)
                .or_else(|| stream_url_pattern.clone())
                .ok_or_else(|| Error::protocol("Smooth Streaming stream requires Url"))?;
            let pattern = normalize_url_pattern(&pattern);
            let bitrate = i64::from(parse_optional_i32(
                attr(quality, "Bitrate"),
                "Smooth Streaming Bitrate",
                0,
            )?);
            let width =
                parse_optional_i32(attr(quality, "MaxWidth"), "Smooth Streaming MaxWidth", 0)?;
            let height =
                parse_optional_i32(attr(quality, "MaxHeight"), "Smooth Streaming MaxHeight", 0)?;
            let channels = attr(quality, "Channels").map(str::to_string);
            let codec_private_data = attr(quality, "CodecPrivateData")
                .unwrap_or_default()
                .to_string();
            let mut stream = Stream {
                id: attr(quality, "Index")
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{four_cc}:{bitrate}")),
                media_type: media_type(stream_type),
                group_id: name
                    .clone()
                    .or_else(|| attr(quality, "Index").map(str::to_string)),
                language: language.clone(),
                bandwidth: Some(bitrate),
                codecs: parse_codecs(&four_cc, &codec_private_data),
                resolution: if width == 0 {
                    None
                } else {
                    Some(format!("{width}x{height}"))
                },
                channels,
                extension: Some("m4s".to_string()),
                publish_time: Some(current_publish_time()),
                period_id: attr(quality, "Index").map(str::to_string),
                url: manifest_url.to_string(),
                original_url: config.original_url.clone(),
                playlist: Some(Playlist {
                    url: manifest_url.to_string(),
                    is_live,
                    media_init: Some(MediaSegment {
                        index: -1,
                        url: if codec_private_data.is_empty() {
                            String::new()
                        } else {
                            format!("hex://{codec_private_data}")
                        },
                        ..MediaSegment::default()
                    }),
                    media_parts: vec![MediaPart::default()],
                    ..Playlist::default()
                }),
                mss_data: Some(MssData {
                    four_cc: four_cc.clone(),
                    codec_private_data,
                    stream_type: stream_type.to_string(),
                    timescale: root_timescale,
                    sampling_rate: parse_optional_i32(
                        attr(quality, "SamplingRate"),
                        "Smooth Streaming SamplingRate",
                        48_000,
                    )?,
                    channels: parse_optional_i32(
                        attr(quality, "Channels"),
                        "Smooth Streaming Channels",
                        2,
                    )?,
                    bits_per_sample: parse_optional_i32(
                        attr(quality, "BitsPerSample"),
                        "Smooth Streaming BitsPerSample",
                        16,
                    )?,
                    nal_unit_length_field: parse_optional_i32(
                        attr(quality, "NALUnitLengthField"),
                        "Smooth Streaming NALUnitLengthField",
                        4,
                    )?,
                    duration: root_duration,
                    is_protection: protection.is_some(),
                    protection_system_id: protection
                        .map(|value| value.system_id.clone())
                        .unwrap_or_default(),
                    protection_data: protection
                        .map(|value| value.data_hex.clone())
                        .unwrap_or_default(),
                }),
                ..Stream::default()
            };

            self.expand_chunks(
                stream_index,
                base_url,
                &pattern,
                bitrate,
                root_timescale,
                root_duration,
                &mut stream,
            )?;

            let generated = MssInitGenerator::generate(&stream)?;
            if let Some(codecs) = generated.codecs {
                stream.codecs = Some(codecs);
            }
            if let Some(playlist) = &mut stream.playlist
                && let Some(init) = &mut playlist.media_init
            {
                init.url = format!("base64://{}", base64_encode(&generated.bytes));
            }
            if let Some(protection) = protection
                && stream_type != "text"
            {
                apply_protection(&mut stream, protection)?;
            }
            streams.push(stream);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn expand_chunks(
        &self,
        stream_index: roxmltree::Node<'_, '_>,
        base_url: &str,
        pattern: &str,
        bitrate: i64,
        root_timescale: i32,
        root_duration: i64,
        stream: &mut Stream,
    ) -> Result<()> {
        let mut current_time = 0_i64;
        let mut segment_index = 0_i64;
        let timescale = f64::from(root_timescale);
        for chunk in children(stream_index, "c") {
            if let Some(value) = attr(chunk, "t") {
                let start = parse_i64(value, "Smooth Streaming chunk t")?;
                current_time = start;
            }
            let duration = parse_optional_i64(attr(chunk, "d"), "Smooth Streaming chunk d", 0)?;
            let repeat = parse_optional_i64(attr(chunk, "r"), "Smooth Streaming chunk r", 0)?;
            let repeat = if repeat > 0 { repeat - 1 } else { repeat };
            self.push_chunk_segment(
                stream,
                base_url,
                pattern,
                bitrate,
                current_time,
                duration,
                timescale,
                segment_index,
            )?;
            segment_index += 1;
            let repeat = if repeat < 0 && duration != 0 {
                ((root_duration as f64 / duration as f64).ceil() as i64 - 1).max(0)
            } else if repeat < 0 {
                0
            } else {
                repeat
            };
            for _ in 0..repeat {
                current_time += duration;
                self.push_chunk_segment(
                    stream,
                    base_url,
                    pattern,
                    bitrate,
                    current_time,
                    duration,
                    timescale,
                    segment_index,
                )?;
                segment_index += 1;
            }
            current_time += duration;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn push_chunk_segment(
        &self,
        stream: &mut Stream,
        base_url: &str,
        pattern: &str,
        bitrate: i64,
        current_time: i64,
        duration: i64,
        timescale: f64,
        segment_index: i64,
    ) -> Result<()> {
        let combined = combine_url(base_url, pattern);
        let resolved = replace_mss_vars(&combined, bitrate, current_time);
        let segment = MediaSegment {
            index: segment_index,
            duration: duration as f64 / timescale,
            name_from_var: if combined.contains("{start_time}") {
                Some(current_time.to_string())
            } else {
                None
            },
            url: resolved,
            ..MediaSegment::default()
        };
        if let Some(part) = stream
            .playlist
            .as_mut()
            .and_then(|playlist| playlist.media_parts.first_mut())
        {
            part.media_segments.push(segment);
        }
        Ok(())
    }

    fn process_resolved_url(&self, url: &str, config: &ParserConfig) -> Result<String> {
        let mut resolved = url.to_string();
        for processor in &self.url_processors {
            if processor.can_process(ExtractorType::Mss, &resolved, config) {
                resolved = processor.process(&resolved, config)?;
            }
        }
        Ok(resolved)
    }
}

/// Smooth Streaming initialization generator.
pub struct MssInitGenerator;

impl MssInitGenerator {
    /// Returns true when this FourCC is supported for initialization generation.
    pub fn can_handle(four_cc: &str) -> bool {
        matches!(
            four_cc,
            "HVC1"
                | "HEV1"
                | "AACL"
                | "AACH"
                | "EC-3"
                | "H264"
                | "AVC1"
                | "DAVC"
                | "TTML"
                | "DVHE"
                | "DVH1"
        )
    }

    /// Generates an initialization segment for a parsed Smooth Streaming stream.
    pub fn generate(stream: &Stream) -> Result<MssGeneratedInit> {
        Self::generate_with_track_id(stream, 2)
    }

    /// Generates an initialization segment using the track ID from the first Smooth Streaming fragment when available.
    pub fn generate_with_first_segment(
        stream: &Stream,
        first_segment: &[u8],
    ) -> Result<MssGeneratedInit> {
        Self::generate_with_track_id(stream, first_tfhd_track_id(first_segment).unwrap_or(2))
    }

    fn generate_with_track_id(stream: &Stream, track_id: u32) -> Result<MssGeneratedInit> {
        let data = stream
            .mss_data
            .as_ref()
            .ok_or_else(|| Error::protocol("Smooth Streaming stream is missing metadata"))?;
        if !Self::can_handle(&data.four_cc) {
            return Err(Error::compatibility(
                "Smooth Streaming FourCC is not supported",
            ));
        }
        let protection = if data.is_protection {
            Some(ProtectionContext::from_mss_data(data)?)
        } else {
            None
        };
        let codec_private_data = if data.codec_private_data.is_empty() {
            Self::generate_aac_codec_private_data(&data.four_cc, data.sampling_rate, data.channels)
                .unwrap_or_default()
        } else {
            data.codec_private_data.clone()
        };
        let mut builder = InitBuilder {
            stream,
            data,
            codec_private_data,
            protection,
            codecs_override: None,
            creation_time: current_unix_seconds(),
            track_id,
        };
        let mut bytes = Vec::new();
        bytes.extend(builder.gen_ftyp());
        bytes.extend(builder.gen_moov()?);
        Ok(MssGeneratedInit {
            bytes,
            codec_private_data: builder.codec_private_data,
            codecs: builder.codecs_override,
        })
    }

    /// Generates AAC codec private data for AAC-compatible Smooth Streaming FourCC values.
    pub fn generate_aac_codec_private_data(
        four_cc: &str,
        sampling_rate: i32,
        channels: i32,
    ) -> Option<String> {
        if four_cc == "AACH" {
            let object_type = 0x05_u8;
            let index_freq = sampling_frequency_index(sampling_rate);
            let extension = sampling_frequency_index(sampling_rate.saturating_mul(2));
            let mut bytes = [0_u8; 4];
            bytes[0] = (object_type << 3) | (index_freq >> 1);
            bytes[1] = (index_freq << 7) | (u8::try_from(channels).ok()? << 3) | (extension >> 1);
            bytes[2] = (extension << 7) | (0x02 << 2);
            bytes[3] = 0;
            return Some(format!(
                "{:016X}{:016X}",
                u16::from_be_bytes([bytes[0], bytes[1]]),
                u16::from_be_bytes([bytes[2], bytes[3]])
            ));
        }
        if four_cc.starts_with("AAC") {
            let object_type = 0x02_u8;
            let index_freq = sampling_frequency_index(sampling_rate);
            let channel_value = u8::try_from(channels).ok()?;
            let first = (object_type << 3) | (index_freq >> 1);
            let second = (index_freq << 7) | (channel_value << 3);
            return Some(format!("{:016X}", u16::from_be_bytes([first, second])));
        }
        None
    }
}

struct InitBuilder<'a> {
    stream: &'a Stream,
    data: &'a MssData,
    codec_private_data: String,
    protection: Option<ProtectionContext>,
    codecs_override: Option<String>,
    creation_time: u64,
    track_id: u32,
}

impl InitBuilder<'_> {
    fn gen_ftyp(&self) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend(b"isml");
        push_u32(&mut payload, 1);
        payload.extend(b"iso5");
        payload.extend(b"iso6");
        payload.extend(b"piff");
        payload.extend(b"msdh");
        box_bytes("ftyp", payload)
    }

    fn gen_moov(&mut self) -> Result<Vec<u8>> {
        let mut payload = Vec::new();
        payload.extend(self.gen_mvhd());
        payload.extend(self.gen_trak()?);
        payload.extend(self.gen_mvex());
        if let Some(protection) = &self.protection {
            payload.extend(gen_pssh_box(&protection.system_id, &protection.data));
            payload.extend(gen_widevine_pssh_box(&protection.kid));
        }
        Ok(box_bytes("moov", payload))
    }

    fn gen_mvhd(&self) -> Vec<u8> {
        let mut payload = Vec::new();
        push_u64(&mut payload, self.creation_time);
        push_u64(&mut payload, self.creation_time);
        push_u32(&mut payload, u32_from_i32(self.data.timescale));
        push_u64(&mut payload, u64_from_i64(self.data.duration));
        push_u16(&mut payload, 1);
        push_u16(&mut payload, 0);
        push_u8(&mut payload, 1);
        push_u8(&mut payload, 0);
        push_u16(&mut payload, 0);
        push_u32(&mut payload, 0);
        push_u32(&mut payload, 0);
        payload.extend(unity_matrix());
        for _ in 0..6 {
            push_u32(&mut payload, 0);
        }
        push_u32(&mut payload, u32::MAX);
        full_box("mvhd", 1, 0, payload)
    }

    fn gen_trak(&mut self) -> Result<Vec<u8>> {
        let mut payload = Vec::new();
        payload.extend(self.gen_tkhd());
        payload.extend(self.gen_mdia()?);
        Ok(box_bytes("trak", payload))
    }

    fn gen_tkhd(&self) -> Vec<u8> {
        let (width, height) = dimensions(self.stream);
        let mut payload = Vec::new();
        push_u64(&mut payload, self.creation_time);
        push_u64(&mut payload, self.creation_time);
        push_u32(&mut payload, self.track_id);
        push_u32(&mut payload, 0);
        push_u64(&mut payload, u64_from_i64(self.data.duration));
        push_u32(&mut payload, 0);
        push_u32(&mut payload, 0);
        push_i16(&mut payload, 0);
        push_i16(&mut payload, 0);
        push_u8(
            &mut payload,
            if self.data.stream_type == "audio" {
                1
            } else {
                0
            },
        );
        push_u8(&mut payload, 0);
        push_u16(&mut payload, 0);
        payload.extend(unity_matrix());
        push_u16(&mut payload, u16_from_i32(width));
        push_u16(&mut payload, 0);
        push_u16(&mut payload, u16_from_i32(height));
        push_u16(&mut payload, 0);
        full_box("tkhd", 1, 0x7, payload)
    }

    fn gen_mdia(&mut self) -> Result<Vec<u8>> {
        let mut payload = Vec::new();
        payload.extend(self.gen_mdhd());
        payload.extend(self.gen_hdlr()?);
        payload.extend(self.gen_minf()?);
        Ok(box_bytes("mdia", payload))
    }

    fn gen_mdhd(&self) -> Vec<u8> {
        let mut payload = Vec::new();
        push_u64(&mut payload, self.creation_time);
        push_u64(&mut payload, self.creation_time);
        push_u32(&mut payload, u32_from_i32(self.data.timescale));
        push_u64(&mut payload, u64_from_i64(self.data.duration));
        push_u16(&mut payload, language_code(self.stream.language.as_deref()));
        push_u16(&mut payload, 0);
        full_box("mdhd", 1, 0, payload)
    }

    fn gen_hdlr(&self) -> Result<Vec<u8>> {
        let handler = match self.data.stream_type.as_str() {
            "audio" => b"soun".as_slice(),
            "video" => b"vide".as_slice(),
            "text" => b"subt".as_slice(),
            _ => {
                return Err(Error::compatibility(
                    "Smooth Streaming stream type is not supported",
                ));
            }
        };
        let mut payload = Vec::new();
        push_u32(&mut payload, 0);
        payload.extend(handler);
        push_u32(&mut payload, 0);
        push_u32(&mut payload, 0);
        push_u32(&mut payload, 0);
        payload.extend(
            self.stream
                .group_id
                .as_deref()
                .unwrap_or("HAKI handler")
                .as_bytes(),
        );
        push_u8(&mut payload, 0);
        Ok(full_box("hdlr", 0, 0, payload))
    }

    fn gen_minf(&mut self) -> Result<Vec<u8>> {
        let mut payload = Vec::new();
        match self.data.stream_type.as_str() {
            "audio" => payload.extend(full_box("smhd", 0, 0, vec![0, 0, 0, 0])),
            "video" => payload.extend(full_box("vmhd", 0, 1, vec![0, 0, 0, 0, 0, 0, 0, 0])),
            "text" => payload.extend(full_box("sthd", 0, 0, Vec::new())),
            _ => {
                return Err(Error::compatibility(
                    "Smooth Streaming stream type is not supported",
                ));
            }
        }
        let mut dref_payload = Vec::new();
        push_u32(&mut dref_payload, 1);
        dref_payload.extend(full_box("url ", 0, 1, Vec::new()));
        payload.extend(box_bytes("dinf", full_box("dref", 0, 0, dref_payload)));
        payload.extend(self.gen_stbl()?);
        Ok(box_bytes("minf", payload))
    }

    fn gen_stbl(&mut self) -> Result<Vec<u8>> {
        let mut payload = Vec::new();
        payload.extend(full_box("stts", 0, 0, vec![0, 0, 0, 0]));
        payload.extend(full_box("stsc", 0, 0, vec![0, 0, 0, 0]));
        payload.extend(full_box("stco", 0, 0, vec![0, 0, 0, 0]));
        payload.extend(full_box("stsz", 0, 0, vec![0, 0, 0, 0, 0, 0, 0, 0]));
        payload.extend(full_box("stsd", 0, 0, self.gen_stsd()?));
        Ok(box_bytes("stbl", payload))
    }

    fn gen_stsd(&mut self) -> Result<Vec<u8>> {
        let mut payload = Vec::new();
        push_u32(&mut payload, 1);
        payload.extend(self.sample_entry()?);
        Ok(payload)
    }

    fn sample_entry(&mut self) -> Result<Vec<u8>> {
        match self.data.stream_type.as_str() {
            "audio" => self.audio_sample_entry(),
            "video" => self.video_sample_entry(),
            "text" => self.text_sample_entry(),
            _ => Err(Error::compatibility(
                "Smooth Streaming stream type is not supported",
            )),
        }
    }

    fn audio_sample_entry(&self) -> Result<Vec<u8>> {
        let mut payload = sample_entry_header();
        push_u32(&mut payload, 0);
        push_u32(&mut payload, 0);
        push_u16(&mut payload, u16_from_i32(self.data.channels));
        push_u16(&mut payload, u16_from_i32(self.data.bits_per_sample));
        push_u16(&mut payload, 0);
        push_u16(&mut payload, 0);
        push_u16(&mut payload, u16_from_i32(self.data.sampling_rate));
        push_u16(&mut payload, 0);
        payload.extend(self.gen_esds()?);
        if self.data.four_cc.starts_with("AAC") {
            if let Some(protection) = &self.protection {
                payload.extend(gen_sinf("mp4a", protection));
                return Ok(box_bytes("enca", payload));
            }
            return Ok(box_bytes("mp4a", payload));
        }
        if self.data.four_cc == "EC-3" {
            if let Some(protection) = &self.protection {
                payload.extend(gen_sinf("ec-3", protection));
                return Ok(box_bytes("enca", payload));
            }
            return Ok(box_bytes("ec-3", payload));
        }
        Err(Error::compatibility(
            "Smooth Streaming audio FourCC is not supported",
        ))
    }

    fn gen_esds(&self) -> Result<Vec<u8>> {
        let config = hex_to_bytes(&self.codec_private_data)?;
        let bandwidth = self.stream.bandwidth.unwrap_or(0);
        let mut payload = Vec::new();
        push_u8(&mut payload, 0x03);
        push_u8(&mut payload, u8_from_usize(20 + config.len()));
        push_u8(&mut payload, ((self.track_id & 0xff00) >> 8) as u8);
        push_u8(&mut payload, (self.track_id & 0x00ff) as u8);
        push_u8(&mut payload, 0);
        push_u8(&mut payload, 0x04);
        push_u8(&mut payload, u8_from_usize(15 + config.len()));
        push_u8(&mut payload, 0x40);
        push_u8(&mut payload, (0x05 << 2) | 1);
        push_u8(&mut payload, 0xff);
        push_u8(&mut payload, 0xff);
        push_u8(&mut payload, 0xff);
        push_u32(&mut payload, u32_from_i64_bits(bandwidth));
        push_u32(&mut payload, u32_from_i64_bits(bandwidth));
        push_u8(&mut payload, 0x05);
        push_u8(&mut payload, u8_from_usize(config.len()));
        payload.extend(config);
        Ok(full_box("esds", 0, 0, payload))
    }

    fn video_sample_entry(&mut self) -> Result<Vec<u8>> {
        let (width, height) = dimensions(self.stream);
        let mut payload = sample_entry_header();
        push_u16(&mut payload, 0);
        push_u16(&mut payload, 0);
        push_u32(&mut payload, 0);
        push_u32(&mut payload, 0);
        push_u32(&mut payload, 0);
        push_u16(&mut payload, u16_from_i32(width));
        push_u16(&mut payload, u16_from_i32(height));
        push_u16(&mut payload, 0x48);
        push_u16(&mut payload, 0);
        push_u16(&mut payload, 0x48);
        push_u16(&mut payload, 0);
        push_u32(&mut payload, 0);
        push_u16(&mut payload, 1);
        payload.extend([0_u8; 32]);
        push_u16(&mut payload, 0x18);
        push_u16(&mut payload, u16::MAX);

        if matches!(self.data.four_cc.as_str(), "H264" | "AVC1" | "DAVC") {
            payload.extend(self.gen_avcc()?);
            if let Some(protection) = &self.protection {
                payload.extend(gen_sinf("avc1", protection));
                return Ok(box_bytes("encv", payload));
            }
            return Ok(box_bytes("avc1", payload));
        }
        if matches!(self.data.four_cc.as_str(), "HVC1" | "HEV1") {
            payload.extend(self.gen_hvcc("hvc1")?);
            if let Some(protection) = &self.protection {
                payload.extend(gen_sinf("hvc1", protection));
                return Ok(box_bytes("encv", payload));
            }
            return Ok(box_bytes("hvc1", payload));
        }
        if matches!(self.data.four_cc.as_str(), "DVHE" | "DVH1") {
            payload.extend(self.gen_hvcc("dvh1")?);
            if let Some(protection) = &self.protection {
                payload.extend(gen_sinf("dvh1", protection));
                return Ok(box_bytes("encv", payload));
            }
            return Ok(box_bytes("dvh1", payload));
        }
        Err(Error::compatibility(
            "Smooth Streaming video FourCC is not supported",
        ))
    }

    fn gen_avcc(&self) -> Result<Vec<u8>> {
        let units = split_start_code_units(&self.codec_private_data)?;
        let sps = units
            .iter()
            .find(|unit| unit.first().is_some_and(|byte| byte & 0x1f == 7))
            .ok_or_else(|| Error::protocol("AVC codec private data is missing SPS"))?;
        let pps = units
            .iter()
            .find(|unit| unit.first().is_some_and(|byte| byte & 0x1f == 8))
            .ok_or_else(|| Error::protocol("AVC codec private data is missing PPS"))?;
        if sps.len() < 4 {
            return Err(Error::protocol("AVC SPS is too short"));
        }
        let mut payload = Vec::new();
        push_u8(&mut payload, 1);
        payload.extend(&sps[1..4]);
        push_u8(
            &mut payload,
            0xfc | u8_from_i32(self.data.nal_unit_length_field.saturating_sub(1)),
        );
        push_u8(&mut payload, 1);
        push_u16(&mut payload, u16_from_usize(sps.len()));
        payload.extend(sps);
        push_u8(&mut payload, 1);
        push_u16(&mut payload, u16_from_usize(pps.len()));
        payload.extend(pps);
        Ok(box_bytes("avcC", payload))
    }

    fn gen_hvcc(&mut self, code: &str) -> Result<Vec<u8>> {
        let units = split_start_code_units(&self.codec_private_data)?;
        let vps = units
            .iter()
            .find(|unit| unit.first().is_some_and(|byte| byte >> 1 == 0x20))
            .ok_or_else(|| Error::protocol("HEVC codec private data is missing VPS"))?;
        let sps = units
            .iter()
            .find(|unit| unit.first().is_some_and(|byte| byte >> 1 == 0x21))
            .ok_or_else(|| Error::protocol("HEVC codec private data is missing SPS"))?;
        let pps = units
            .iter()
            .find(|unit| unit.first().is_some_and(|byte| byte >> 1 == 0x22))
            .ok_or_else(|| Error::protocol("HEVC codec private data is missing PPS"))?;
        let cleaned_sps = remove_emulation_prevention(sps);
        if cleaned_sps.len() < 13 {
            return Err(Error::protocol("HEVC SPS is too short"));
        }
        let first = *cleaned_sps
            .get(2)
            .ok_or_else(|| Error::protocol("HEVC SPS is too short"))?;
        let _max_sub_layers_minus_one = (first & 0x0e) >> 1;
        let profile = *cleaned_sps
            .get(3)
            .ok_or_else(|| Error::protocol("HEVC SPS is too short"))?;
        let general_profile_space = (profile & 0xc0) >> 6;
        let general_tier_flag = (profile & 0x20) >> 5;
        let general_profile_idc = profile & 0x1f;
        let compatibility = u32::from_be_bytes([
            cleaned_sps[4],
            cleaned_sps[5],
            cleaned_sps[6],
            cleaned_sps[7],
        ]);
        let constraint = cleaned_sps
            .get(8..14)
            .ok_or_else(|| Error::protocol("HEVC SPS is too short"))?;
        let general_level_idc = *cleaned_sps
            .get(14)
            .ok_or_else(|| Error::protocol("HEVC SPS is too short"))?;
        self.codecs_override = Some(format!(
            "{code}.{}{general_profile_idc}.{:x}.{}{general_level_idc}.{}",
            ["", "A", "B", "C"][usize::from(general_profile_space)],
            compatibility,
            if general_tier_flag == 1 { "H" } else { "L" },
            bytes_to_hex_upper(
                &constraint
                    .iter()
                    .copied()
                    .filter(|byte| *byte != 0)
                    .collect::<Vec<_>>()
            )
        ));

        let mut payload = Vec::new();
        push_u8(&mut payload, 1);
        push_u8(
            &mut payload,
            (general_profile_space << 6) | (general_tier_flag << 5) | general_profile_idc,
        );
        push_u32(&mut payload, compatibility);
        payload.extend(constraint);
        push_u8(&mut payload, general_profile_idc);
        push_u16(&mut payload, 0xf000);
        push_u8(&mut payload, 0xfc);
        push_u8(&mut payload, 0xfc);
        push_u8(&mut payload, 0xf8);
        push_u8(&mut payload, 0xf8);
        push_u16(&mut payload, 0);
        push_u8(
            &mut payload,
            u8_from_i32(self.data.nal_unit_length_field.saturating_sub(1)),
        );
        push_u8(&mut payload, 0x03);
        push_nal_array(&mut payload, 0x20, vps);
        push_nal_array(&mut payload, 0x21, sps);
        push_nal_array(&mut payload, 0x22, pps);
        Ok(box_bytes("hvcC", payload))
    }

    fn text_sample_entry(&self) -> Result<Vec<u8>> {
        if self.data.four_cc != "TTML" {
            return Err(Error::compatibility(
                "Smooth Streaming text FourCC is not supported",
            ));
        }
        let mut payload = sample_entry_header();
        payload.extend(b"http://www.w3.org/ns/ttml\0");
        payload.extend(b"\0");
        payload.extend(b"\0");
        Ok(box_bytes("stpp", payload))
    }

    fn gen_mvex(&self) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend(full_box("mehd", 1, 0, {
            let mut data = Vec::new();
            push_u64(&mut data, u64_from_i64(self.data.duration));
            data
        }));
        payload.extend(full_box("trex", 0, 0, {
            let mut data = Vec::new();
            push_u32(&mut data, self.track_id);
            push_u32(&mut data, 1);
            push_u32(&mut data, 0);
            push_u32(&mut data, 0);
            push_u32(&mut data, 0);
            data
        }));
        box_bytes("mvex", payload)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MssProtection {
    system_id: String,
    data_hex: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProtectionContext {
    system_id: String,
    data: Vec<u8>,
    kid: Vec<u8>,
}

impl ProtectionContext {
    fn from_mss_data(data: &MssData) -> Result<Self> {
        if data
            .protection_system_id
            .eq_ignore_ascii_case(WIDEVINE_SYSTEM_ID)
        {
            return Err(Error::compatibility(
                "Smooth Streaming Widevine protection is not supported",
            ));
        }
        let protection_data = hex_to_bytes(&data.protection_data)?;
        let kid = extract_playready_kid(&data.protection_system_id, &protection_data)?;
        Ok(Self {
            system_id: data.protection_system_id.clone(),
            data: protection_data,
            kid,
        })
    }
}

fn parse_protection(root: roxmltree::Node<'_, '_>) -> Result<Option<MssProtection>> {
    let Some(protection) = first_child(root, "Protection") else {
        return Ok(None);
    };
    let Some(header) = first_child(protection, "ProtectionHeader") else {
        return Ok(None);
    };
    let system_id = attr(header, "SystemID")
        .unwrap_or(PLAYREADY_SYSTEM_ID)
        .to_string();
    let data = base64_decode(header.text().unwrap_or_default().trim())?;
    Ok(Some(MssProtection {
        system_id,
        data_hex: bytes_to_hex_upper(&data),
    }))
}

fn apply_protection(stream: &mut Stream, protection: &MssProtection) -> Result<()> {
    let protection_data = hex_to_bytes(&protection.data_hex)?;
    let encryption = EncryptionInfo {
        method: EncryptionMethod::Cenc,
        protection_data: Some(protection_data),
        scheme: Some("cenc".to_string()),
        source: KeySource::Inline,
        ..EncryptionInfo::default()
    };
    if let Some(playlist) = &mut stream.playlist {
        if let Some(init) = &mut playlist.media_init {
            init.encryption = encryption.clone();
        }
        for part in &mut playlist.media_parts {
            for segment in &mut part.media_segments {
                segment.encryption = encryption.clone();
            }
        }
    }
    Ok(())
}

fn apply_default_external_tracks(streams: &mut [Stream]) {
    let first_audio = streams
        .iter()
        .find(|stream| stream.media_type == Some(MediaType::Audio))
        .and_then(|stream| stream.group_id.clone());
    let first_subtitle = streams
        .iter()
        .find(|stream| stream.media_type == Some(MediaType::Subtitles))
        .and_then(|stream| stream.group_id.clone());
    for stream in streams
        .iter_mut()
        .filter(|stream| stream.resolution.is_some())
    {
        if stream.audio_id.is_none() {
            stream.audio_id = first_audio.clone();
        }
        if stream.subtitle_id.is_none() {
            stream.subtitle_id = first_subtitle.clone();
        }
    }
}

fn stream_identity(stream: &Stream) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        stream.media_type,
        stream.group_id,
        stream.resolution,
        stream.bandwidth,
        stream.codecs,
        stream.language,
        stream.channels
    )
}

fn parse_codecs(four_cc: &str, private_data: &str) -> Option<String> {
    if four_cc == "TTML" {
        return Some("stpp".to_string());
    }
    if private_data.is_empty() {
        return None;
    }
    match four_cc {
        "H264" | "X264" | "DAVC" | "AVC1" => Some(parse_avc_codecs(private_data)),
        "AAC" | "AACL" | "AACH" | "AACP" => Some(parse_aac_codecs(four_cc, private_data)),
        _ => Some(four_cc.to_ascii_lowercase()),
    }
}

fn parse_avc_codecs(private_data: &str) -> String {
    if let Some(index) = private_data.find("00000001") {
        let after = index + "00000001".len();
        if private_data
            .get(after..after + 2)
            .is_some_and(|value| value.eq_ignore_ascii_case("67"))
            && let Some(profile) = private_data.get(after + 2..after + 8)
        {
            return format!("avc1.{profile}");
        }
    }
    "avc1.4D401E".to_string()
}

fn parse_aac_codecs(four_cc: &str, private_data: &str) -> String {
    let profile = if four_cc == "AACH" {
        5
    } else {
        private_data
            .get(..2)
            .and_then(|value| u8::from_str_radix(value, 16).ok())
            .map(|byte| (byte & 0xf8) >> 3)
            .unwrap_or(2)
    };
    format!("mp4a.40.{profile}")
}

fn media_type(value: &str) -> Option<MediaType> {
    match value {
        "audio" => Some(MediaType::Audio),
        "text" => Some(MediaType::Subtitles),
        _ => None,
    }
}

fn normalize_url_pattern(pattern: &str) -> String {
    pattern
        .replace("{bitrate}", "{Bitrate}")
        .replace("{start time}", "{start_time}")
}

fn replace_mss_vars(value: &str, bitrate: i64, current_time: i64) -> String {
    value
        .replace("{Bitrate}", &bitrate.to_string())
        .replace("{start_time}", &current_time.to_string())
}

fn current_publish_time() -> String {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

fn current_unix_seconds() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs(),
        Err(_) => 0,
    }
}

fn sampling_frequency_index(sampling_rate: i32) -> u8 {
    match sampling_rate {
        96_000 => 0x0,
        88_200 => 0x1,
        64_000 => 0x2,
        48_000 => 0x3,
        44_100 => 0x4,
        32_000 => 0x5,
        24_000 => 0x6,
        22_050 => 0x7,
        16_000 => 0x8,
        12_000 => 0x9,
        11_025 => 0xa,
        8_000 => 0xb,
        7_350 => 0xc,
        _ => 0x0,
    }
}

fn split_start_code_units(value: &str) -> Result<Vec<Vec<u8>>> {
    let mut units = Vec::new();
    for part in value.split(START_CODE).filter(|part| !part.is_empty()) {
        units.push(hex_to_bytes(part)?);
    }
    Ok(units)
}

fn remove_emulation_prevention(value: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(value.len());
    for byte in value {
        output.push(*byte);
        if output.ends_with(&[0x00, 0x00, 0x03]) {
            let _ = output.pop();
        }
    }
    output
}

fn extract_playready_kid(system_id: &str, protection_data: &[u8]) -> Result<Vec<u8>> {
    if !system_id.eq_ignore_ascii_case(PLAYREADY_SYSTEM_ID) {
        return Ok(vec![0; 16]);
    }
    let hex = bytes_to_hex_upper(protection_data);
    let compact = hex.replace("00", "");
    let text = String::from_utf8_lossy(&hex_to_bytes(&compact)?).to_string();
    let start = text
        .find("<KID>")
        .ok_or_else(|| Error::protocol("Smooth Streaming protection KID is missing"))?
        + "<KID>".len();
    let end = text
        .get(start..)
        .and_then(|tail| tail.find('<').map(|offset| start + offset))
        .ok_or_else(|| Error::protocol("Smooth Streaming protection KID is invalid"))?;
    let encoded = text
        .get(start..end)
        .ok_or_else(|| Error::protocol("Smooth Streaming protection KID is invalid"))?;
    let mut kid = base64_decode(encoded)?;
    if kid.len() != 16 {
        return Err(Error::protocol(
            "Smooth Streaming protection KID length is invalid",
        ));
    }
    kid.swap(0, 3);
    kid.swap(1, 2);
    kid.swap(4, 5);
    kid.swap(6, 7);
    Ok(kid)
}

fn gen_sinf(codec: &str, protection: &ProtectionContext) -> Vec<u8> {
    let frma = box_bytes("frma", codec.as_bytes().to_vec());
    let mut schm_payload = Vec::new();
    schm_payload.extend(b"cenc");
    schm_payload.extend([0, 1, 0, 0]);
    let schm = full_box("schm", 0, 0, schm_payload);
    let mut tenc_payload = Vec::new();
    tenc_payload.extend([0, 0, 1, 8]);
    tenc_payload.extend(&protection.kid);
    let tenc = full_box("tenc", 0, 0, tenc_payload);
    let schi = box_bytes("schi", tenc);
    let mut payload = Vec::new();
    payload.extend(frma);
    payload.extend(schm);
    payload.extend(schi);
    box_bytes("sinf", payload)
}

fn gen_pssh_box(system_id: &str, data: &[u8]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend(hex_to_bytes(&system_id.replace('-', "")).unwrap_or_default());
    push_u32(&mut payload, u32_from_usize(data.len()));
    payload.extend(data);
    full_box("pssh", 0, 0, payload)
}

fn gen_widevine_pssh_box(kid: &[u8]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend(hex_to_bytes(&WIDEVINE_SYSTEM_ID.replace('-', "")).unwrap_or_default());
    let data = format!(
        "08011210{}1A046E647265220400000000",
        bytes_to_hex_upper(kid)
    );
    let pssh_data = hex_to_bytes(&data).unwrap_or_default();
    push_u32(&mut payload, u32_from_usize(pssh_data.len()));
    payload.extend(pssh_data);
    full_box("pssh", 0, 0, payload)
}

fn push_nal_array(payload: &mut Vec<u8>, nal_type: u8, data: &[u8]) {
    push_u8(payload, nal_type);
    push_u16(payload, 1);
    push_u16(payload, u16_from_usize(data.len()));
    payload.extend(data);
}

fn sample_entry_header() -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend([0_u8; 6]);
    push_u16(&mut payload, 1);
    payload
}

fn box_bytes(box_type: &str, payload: Vec<u8>) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(payload.len() + 8);
    push_u32(&mut bytes, u32_from_usize(payload.len() + 8));
    bytes.extend(box_type.as_bytes().iter().take(4));
    bytes.extend(payload);
    bytes
}

fn full_box(box_type: &str, version: u8, flags: u32, payload: Vec<u8>) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(payload.len() + 4);
    push_u8(&mut bytes, version);
    let flag_bytes = flags.to_be_bytes();
    bytes.extend(&flag_bytes[1..]);
    bytes.extend(payload);
    box_bytes(box_type, bytes)
}

fn first_tfhd_track_id(bytes: &[u8]) -> Option<u32> {
    find_tfhd_track_id(bytes, 0, bytes.len())
}

fn find_tfhd_track_id(bytes: &[u8], start: usize, end: usize) -> Option<u32> {
    let mut offset = start;
    while offset.checked_add(8).is_some_and(|value| value <= end) {
        let size = read_box_size(bytes, offset)?;
        if size < 8 {
            return None;
        }
        let box_end = offset.checked_add(size)?;
        if box_end > end {
            return None;
        }
        let box_type = bytes.get(offset + 4..offset + 8)?;
        if box_type == b"tfhd" {
            return read_u32_at(bytes, offset + 12);
        }
        if matches!(box_type, b"moof" | b"traf")
            && let Some(track_id) = find_tfhd_track_id(bytes, offset + 8, box_end)
        {
            return Some(track_id);
        }
        offset = box_end;
    }
    None
}

fn read_box_size(bytes: &[u8], offset: usize) -> Option<usize> {
    read_u32_at(bytes, offset).map(|value| value as usize)
}

fn read_u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    let data = bytes.get(offset..offset + 4)?;
    Some(u32::from_be_bytes([data[0], data[1], data[2], data[3]]))
}

fn unity_matrix() -> Vec<u8> {
    let mut payload = Vec::new();
    push_u32(&mut payload, 0x0001_0000);
    push_u32(&mut payload, 0);
    push_u32(&mut payload, 0);
    push_u32(&mut payload, 0);
    push_u32(&mut payload, 0x0001_0000);
    push_u32(&mut payload, 0);
    push_u32(&mut payload, 0);
    push_u32(&mut payload, 0);
    push_u32(&mut payload, 0x4000_0000);
    payload
}

fn language_code(value: Option<&str>) -> u16 {
    let language = value.unwrap_or("und").as_bytes();
    if language.len() < 3 {
        return language_code(Some("und"));
    }
    let a = u16::from(language[0].saturating_sub(0x60));
    let b = u16::from(language[1].saturating_sub(0x60));
    let c = u16::from(language[2].saturating_sub(0x60));
    (a << 10) | (b << 5) | c
}

fn dimensions(stream: &Stream) -> (i32, i32) {
    let Some(resolution) = &stream.resolution else {
        return (0, 0);
    };
    let Some((width, height)) = resolution.split_once('x') else {
        return (0, 0);
    };
    (
        width.parse::<i32>().unwrap_or(0),
        height.parse::<i32>().unwrap_or(0),
    )
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(Error::protocol(
            "Smooth Streaming IsLive value must be true or false",
        )),
    }
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

fn attr<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|attribute| attribute.name() == name)
        .map(|attribute| attribute.value())
}

fn hex_to_bytes(value: &str) -> Result<Vec<u8>> {
    if value.len() & 1 != 0 {
        return Err(Error::config("hex length must be even"));
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    let mut chars = value.chars();
    while let Some(high) = chars.next() {
        let low = chars
            .next()
            .ok_or_else(|| Error::config("hex length must be even"))?;
        let high = hex_value(high).ok_or_else(|| Error::config("hex is invalid"))?;
        let low = hex_value(low).ok_or_else(|| Error::config("hex is invalid"))?;
        bytes.push((high << 4) | low);
    }
    Ok(bytes)
}

fn hex_value(ch: char) -> Option<u8> {
    match ch {
        '0'..='9' => Some(ch as u8 - b'0'),
        'a'..='f' => Some(ch as u8 - b'a' + 10),
        'A'..='F' => Some(ch as u8 - b'A' + 10),
        _ => None,
    }
}

fn bytes_to_hex_upper(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
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

fn base64_decode(value: &str) -> Result<Vec<u8>> {
    crate::base64::decode_base64(value).map_err(Error::config)
}

fn push_u8(bytes: &mut Vec<u8>, value: u8) {
    bytes.push(value);
}

fn push_i16(bytes: &mut Vec<u8>, value: i16) {
    bytes.extend(value.to_be_bytes());
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend(value.to_be_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend(value.to_be_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend(value.to_be_bytes());
}

fn u8_from_i32(value: i32) -> u8 {
    u8::try_from(value).unwrap_or(u8::MAX)
}

fn u8_from_usize(value: usize) -> u8 {
    u8::try_from(value).unwrap_or(u8::MAX)
}

fn u16_from_i32(value: i32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn u16_from_usize(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn u32_from_i32(value: i32) -> u32 {
    u32::try_from(value).unwrap_or_default()
}

fn u32_from_i64_bits(value: i64) -> u32 {
    (value as i32) as u32
}

fn u32_from_usize(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn u64_from_i64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}
