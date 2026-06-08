use std::error::Error;

use haki_dl::{
    EncryptionMethod, MediaType, MssInitGenerator, MssParser, ParserConfig, SourceLoader,
};

#[tokio::test]
async fn mss_manifest_parses_streams_chunks_tracks_and_init() -> Result<(), Box<dyn Error>> {
    let manifest = format!(
        r#"<SmoothStreamingMedia TimeScale="10000000" Duration="30000000" IsLive="false">
  <StreamIndex Type="video" Name="video" Url="QualityLevels({{bitrate}})/Fragments(video={{start time}})">
    <QualityLevel Index="v1" Bitrate="1000000" FourCC="H264" MaxWidth="1280" MaxHeight="720" CodecPrivateData="{avc}"/>
    <c t="0" d="10000000" r="2"/>
    <c d="10000000"/>
  </StreamIndex>
  <StreamIndex Type="audio" Name="audio" Language="eng" Url="QualityLevels({{Bitrate}})/Fragments(audio={{start_time}})">
    <QualityLevel Index="a1" Bitrate="128000" FourCC="AACL" SamplingRate="48000" Channels="2"/>
    <c t="0" d="10000000" r="1"/>
  </StreamIndex>
  <StreamIndex Type="text" Name="text" Language="en-US" Url="text/{{start_time}}">
    <QualityLevel Index="s1" Bitrate="0" FourCC="TTML"/>
    <c t="0" d="10000000" r="1"/>
  </StreamIndex>
</SmoothStreamingMedia>"#,
        avc = avc_private_data()
    );

    let parsed = MssParser::new().parse(
        &manifest,
        "http://media.example/path/stream.ism/Manifest",
        &ParserConfig::default(),
    )?;
    let video = parsed
        .streams
        .iter()
        .find(|stream| stream.resolution.as_deref() == Some("1280x720"))
        .ok_or("missing video")?;
    let audio = parsed
        .streams
        .iter()
        .find(|stream| stream.media_type == Some(MediaType::Audio))
        .ok_or("missing audio")?;
    let subtitle = parsed
        .streams
        .iter()
        .find(|stream| stream.media_type == Some(MediaType::Subtitles))
        .ok_or("missing subtitle")?;

    assert_eq!(parsed.timescale, 10_000_000);
    assert_eq!(parsed.duration, 30_000_000);
    assert!(!parsed.is_live);
    assert_eq!(video.audio_id.as_deref(), Some("audio"));
    assert_eq!(video.subtitle_id.as_deref(), Some("text"));
    assert_eq!(video.codecs.as_deref(), Some("avc1.42001E"));
    assert_eq!(video.segments_count(), 3);
    let video_playlist = video.playlist.as_ref().ok_or("missing playlist")?;
    assert_eq!(
        video_playlist.media_parts[0].media_segments[0]
            .name_from_var
            .as_deref(),
        Some("0")
    );
    assert_eq!(
        video_playlist.media_parts[0].media_segments[1].url,
        "http://media.example/path/stream.ism/QualityLevels(1000000)/Fragments(video=10000000)"
    );
    assert_eq!(audio.language.as_deref(), Some("eng"));
    assert_eq!(audio.segments_count(), 1);
    assert_eq!(subtitle.language, None);
    assert_eq!(subtitle.codecs.as_deref(), Some("stpp"));
    assert_init_has_boxes(video).await?;
    assert_init_has_boxes(audio).await?;
    assert_init_has_boxes(subtitle).await?;
    Ok(())
}

#[test]
fn mss_init_regeneration_uses_first_fragment_track_id() -> Result<(), Box<dyn Error>> {
    let parsed = MssParser::new().parse(
        r#"<SmoothStreamingMedia TimeScale="10000000" Duration="10000000" IsLive="false">
  <StreamIndex Type="audio" Name="audio" Url="a/{{Bitrate}}/{{start_time}}">
    <QualityLevel Index="a1" Bitrate="128000" FourCC="AACL" SamplingRate="48000" Channels="2"/>
    <c t="0" d="10000000"/>
  </StreamIndex>
</SmoothStreamingMedia>"#,
        "http://media.example/path/stream.ism/Manifest",
        &ParserConfig::default(),
    )?;
    let stream = parsed.streams.first().ok_or("missing stream")?;
    let init =
        MssInitGenerator::generate_with_first_segment(stream, &fragment_with_tfhd_track_id(1))?;

    assert_eq!(box_u32_field(&init.bytes, b"tkhd", 28), Some(1));
    assert_eq!(box_u32_field(&init.bytes, b"trex", 12), Some(1));
    assert_eq!(box_u16_field(&init.bytes, b"esds", 14), Some(1));
    Ok(())
}

#[test]
fn mss_negative_repeat_expands_to_manifest_duration() -> Result<(), Box<dyn Error>> {
    let manifest = format!(
        r#"<SmoothStreamingMedia TimeScale="10000000" Duration="50000000">
  <StreamIndex Type="video" Name="video" Url="v/{{Bitrate}}/{{start_time}}">
    <QualityLevel Index="v1" Bitrate="1000000" FourCC="H264" MaxWidth="1280" MaxHeight="720" CodecPrivateData="{avc}"/>
    <c t="0" d="10000000" r="-1"/>
  </StreamIndex>
</SmoothStreamingMedia>"#,
        avc = avc_private_data()
    );

    let parsed = MssParser::new().parse(
        &manifest,
        "http://media.example/path/stream.ism/Manifest",
        &ParserConfig::default(),
    )?;
    let playlist = parsed.streams[0]
        .playlist
        .as_ref()
        .ok_or("missing playlist")?;

    assert_eq!(playlist.segments_count(), 5);
    assert_eq!(
        playlist.media_parts[0].media_segments[4]
            .name_from_var
            .as_deref(),
        Some("40000000")
    );
    Ok(())
}

#[tokio::test]
async fn mss_playready_protection_applies_to_non_text_and_writes_pssh() -> Result<(), Box<dyn Error>>
{
    let protection =
        playready_header_base64(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
    let manifest = format!(
        r#"<SmoothStreamingMedia TimeScale="10000000" Duration="10000000">
  <Protection><ProtectionHeader SystemID="9A04F079-9840-4286-AB92-E65BE0885F95">{protection}</ProtectionHeader></Protection>
  <StreamIndex Type="video" Name="video" Url="v/{{Bitrate}}/{{start_time}}">
    <QualityLevel Index="v1" Bitrate="1000000" FourCC="H264" MaxWidth="1280" MaxHeight="720" CodecPrivateData="{avc}"/>
    <c t="0" d="10000000" r="1"/>
  </StreamIndex>
  <StreamIndex Type="text" Name="text" Url="t/{{start_time}}">
    <QualityLevel Index="s1" Bitrate="0" FourCC="TTML"/>
    <c t="0" d="10000000" r="1"/>
  </StreamIndex>
</SmoothStreamingMedia>"#,
        avc = avc_private_data()
    );

    let parsed = MssParser::new().parse(
        &manifest,
        "http://media.example/path/stream.ism/Manifest",
        &ParserConfig::default(),
    )?;
    let video = parsed
        .streams
        .iter()
        .find(|stream| stream.resolution.as_deref() == Some("1280x720"))
        .ok_or("missing video")?;
    let subtitle = parsed
        .streams
        .iter()
        .find(|stream| stream.media_type == Some(MediaType::Subtitles))
        .ok_or("missing subtitle")?;
    let video_playlist = video.playlist.as_ref().ok_or("missing video playlist")?;
    let subtitle_playlist = subtitle
        .playlist
        .as_ref()
        .ok_or("missing subtitle playlist")?;
    let init_bytes = load_init(video).await?;

    assert_eq!(
        video_playlist
            .media_init
            .as_ref()
            .map(|segment| segment.encryption.method),
        Some(EncryptionMethod::Cenc)
    );
    assert_eq!(
        video_playlist.media_parts[0].media_segments[0]
            .encryption
            .method,
        EncryptionMethod::Cenc
    );
    assert_eq!(
        subtitle_playlist.media_parts[0].media_segments[0]
            .encryption
            .method,
        EncryptionMethod::None
    );
    assert!(contains_box(&init_bytes, b"pssh"));
    assert!(contains_box(&init_bytes, b"tenc"));
    Ok(())
}

#[test]
fn mss_refresh_updates_media_parts_by_identity() -> Result<(), Box<dyn Error>> {
    let first = format!(
        r#"<SmoothStreamingMedia TimeScale="10000000" Duration="10000000" IsLive="true">
  <StreamIndex Type="video" Name="video" Url="old/{{Bitrate}}/{{start_time}}">
    <QualityLevel Index="v1" Bitrate="1000000" FourCC="H264" MaxWidth="1280" MaxHeight="720" CodecPrivateData="{avc}"/>
    <c t="0" d="10000000" r="1"/>
  </StreamIndex>
</SmoothStreamingMedia>"#,
        avc = avc_private_data()
    );
    let second = format!(
        r#"<SmoothStreamingMedia TimeScale="10000000" Duration="10000000" IsLive="true">
  <StreamIndex Type="video" Name="video" Url="new/{{Bitrate}}/{{start_time}}">
    <QualityLevel Index="v1" Bitrate="1000000" FourCC="H264" MaxWidth="1280" MaxHeight="720" CodecPrivateData="{avc}"/>
    <c t="10000000" d="10000000" r="1"/>
  </StreamIndex>
</SmoothStreamingMedia>"#,
        avc = avc_private_data()
    );
    let parser = MssParser::new();
    let mut streams = parser
        .parse(
            &first,
            "http://cdn.example/live/manifest.ism",
            &ParserConfig::default(),
        )?
        .streams;

    parser.refresh_streams(
        &mut streams,
        &second,
        "http://cdn.example/live/manifest.ism",
        &ParserConfig::default(),
    )?;
    let playlist = streams[0].playlist.as_ref().ok_or("missing playlist")?;

    assert!(
        playlist.media_parts[0].media_segments[0]
            .url
            .contains("new/1000000/10000000")
    );
    assert_eq!(
        playlist.media_parts[0].media_segments[0]
            .name_from_var
            .as_deref(),
        Some("10000000")
    );
    Ok(())
}

async fn assert_init_has_boxes(stream: &haki_dl::Stream) -> Result<(), Box<dyn Error>> {
    let bytes = load_init(stream).await?;
    let top = top_level_boxes(&bytes)?;
    assert_eq!(top, vec!["ftyp".to_string(), "moov".to_string()]);
    assert_eq!(bytes.get(8..12), Some(b"isml".as_slice()));
    assert!(contains_box(&bytes, b"trak"));
    assert!(contains_box(&bytes, b"stsd"));
    Ok(())
}

async fn load_init(stream: &haki_dl::Stream) -> Result<Vec<u8>, Box<dyn Error>> {
    let url = stream
        .playlist
        .as_ref()
        .and_then(|playlist| playlist.media_init.as_ref())
        .map(|segment| segment.url.as_str())
        .ok_or("missing init")?;
    Ok(SourceLoader::new()
        .load_segment_bytes(url, &ParserConfig::default())
        .await?)
}

fn top_level_boxes(bytes: &[u8]) -> Result<Vec<String>, Box<dyn Error>> {
    let mut names = Vec::new();
    let mut offset = 0_usize;
    while offset + 8 <= bytes.len() {
        let size = read_u32(bytes, offset)?;
        let name = bytes
            .get(offset + 4..offset + 8)
            .ok_or("box type missing")?;
        names.push(String::from_utf8_lossy(name).to_string());
        if size < 8 {
            return Err("invalid box size".into());
        }
        offset += size;
    }
    Ok(names)
}

fn contains_box(bytes: &[u8], name: &[u8; 4]) -> bool {
    let mut offset = 0_usize;
    while offset + 8 <= bytes.len() {
        let Some(size) = read_u32(bytes, offset).ok() else {
            return false;
        };
        if bytes.get(offset + 4..offset + 8) == Some(name.as_slice()) {
            return true;
        }
        if size < 8 || offset + size > bytes.len() {
            offset += 1;
        } else if matches!(
            bytes.get(offset + 4..offset + 8),
            Some(
                b"moov"
                    | b"trak"
                    | b"mdia"
                    | b"minf"
                    | b"stbl"
                    | b"stsd"
                    | b"enca"
                    | b"encv"
                    | b"sinf"
                    | b"schi"
            )
        ) {
            if contains_box(&bytes[offset + 8..offset + size], name) {
                return true;
            }
            offset += size;
        } else {
            offset += size;
        }
    }
    false
}

fn box_u32_field(bytes: &[u8], name: &[u8; 4], field_offset: usize) -> Option<u32> {
    let mut offset = 0_usize;
    while offset + 8 <= bytes.len() {
        let size = read_u32(bytes, offset).ok()?;
        if size < 8 || offset + size > bytes.len() {
            offset += 1;
            continue;
        }
        if bytes.get(offset + 4..offset + 8) == Some(name.as_slice()) {
            let field = bytes.get(offset + field_offset..offset + field_offset + 4)?;
            return Some(u32::from_be_bytes([field[0], field[1], field[2], field[3]]));
        }
        if matches!(
            bytes.get(offset + 4..offset + 8),
            Some(b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"mvex")
        ) && let Some(value) =
            box_u32_field(&bytes[offset + 8..offset + size], name, field_offset)
        {
            return Some(value);
        }
        offset += size;
    }
    None
}

fn box_u16_field(bytes: &[u8], name: &[u8; 4], field_offset: usize) -> Option<u16> {
    let mut offset = 0_usize;
    while offset + 8 <= bytes.len() {
        let size = read_u32(bytes, offset).ok()?;
        if size < 8 || offset + size > bytes.len() {
            offset += 1;
            continue;
        }
        if bytes.get(offset + 4..offset + 8) == Some(name.as_slice()) {
            let field = bytes.get(offset + field_offset..offset + field_offset + 2)?;
            return Some(u16::from_be_bytes([field[0], field[1]]));
        }
        if matches!(
            bytes.get(offset + 4..offset + 8),
            Some(
                b"moov"
                    | b"trak"
                    | b"mdia"
                    | b"minf"
                    | b"stbl"
                    | b"stsd"
                    | b"mp4a"
                    | b"enca"
                    | b"sinf"
                    | b"schi"
                    | b"mvex"
            )
        ) && let Some(value) =
            box_u16_field(&bytes[offset + 8..offset + size], name, field_offset)
        {
            return Some(value);
        }
        offset += size;
    }
    None
}

fn fragment_with_tfhd_track_id(track_id: u32) -> Vec<u8> {
    mp4_box(
        b"moof",
        mp4_box(
            b"traf",
            full_mp4_box(b"tfhd", 0, 0, track_id.to_be_bytes().to_vec()),
        ),
    )
}

fn full_mp4_box(name: &[u8; 4], version: u8, flags: u32, payload: Vec<u8>) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(version);
    let flags = flags.to_be_bytes();
    body.extend(&flags[1..]);
    body.extend(payload);
    mp4_box(name, body)
}

fn mp4_box(name: &[u8; 4], payload: Vec<u8>) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend(
        u32::try_from(payload.len() + 8)
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    bytes.extend(name);
    bytes.extend(payload);
    bytes
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<usize, Box<dyn Error>> {
    let data = bytes.get(offset..offset + 4).ok_or("u32 out of bounds")?;
    Ok(u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize)
}

fn avc_private_data() -> &'static str {
    "000000016742001E89ABCDEF0000000168CE06E2"
}

fn playready_header_base64(kid: &[u8; 16]) -> String {
    let kid_base64 = base64_encode(kid);
    let xml = format!("<WRMHEADER><KID>{kid_base64}</KID></WRMHEADER>");
    let mut utf16 = Vec::new();
    for unit in xml.encode_utf16() {
        utf16.extend(unit.to_le_bytes());
    }
    base64_encode(&utf16)
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
