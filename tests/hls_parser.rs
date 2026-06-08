use std::error::Error;

use haki_dl::{EncryptionMethod, HlsParser, MediaType, ParserConfig, streams_metadata_json};

#[tokio::test]
async fn media_playlist_parses_keys_maps_ranges_discontinuity_and_live()
-> Result<(), Box<dyn Error>> {
    let mut config = ParserConfig {
        original_url: "http://media.example/path/master.m3u8".to_string(),
        ..ParserConfig::default()
    };
    let text = r#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-MEDIA-SEQUENCE:42
#EXT-X-KEY:METHOD=AES-128,URI="base64:AAECAwQFBgcICQoLDA0ODw=="
#EXT-X-MAP:URI="init.mp4",BYTERANGE="100@0"
#EXTINF:2.5,first
seg42.m4s
#EXT-X-PROGRAM-DATE-TIME:1997-01-01T10:00:00Z
#EXTINF:3.5,second
#EXT-X-BYTERANGE:50@100
seg43.m4s
#EXT-X-DISCONTINUITY
#EXTINF:4,
seg44.m4s
"#;

    let parsed = HlsParser::new()
        .parse(text, "http://media.example/path/media.m3u8", &config)
        .await?;
    let playlist = parsed.streams[0]
        .playlist
        .as_ref()
        .ok_or("missing playlist")?;

    assert!(!parsed.is_master);
    assert!(parsed.streams[0].original_url.is_empty());
    assert_eq!(parsed.streams[0].extension.as_deref(), Some("mp4"));
    assert!(playlist.is_live);
    assert_eq!(playlist.refresh_interval_ms, 8_000.0);
    assert_eq!(playlist.media_parts.len(), 2);
    assert_eq!(playlist.segments_count(), 3);
    assert_eq!(
        playlist
            .media_init
            .as_ref()
            .map(|segment| segment.stop_range()),
        Some(Some(99))
    );
    let first = &playlist.media_parts[0].media_segments[0];
    assert_eq!(first.index, 42);
    assert_eq!(first.encryption.method, EncryptionMethod::Aes128);
    assert_eq!(
        first.encryption.key,
        Some(vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15])
    );
    assert_eq!(first.encryption.iv.as_ref().map(|iv| iv[15]), Some(42));
    let second = &playlist.media_parts[0].media_segments[1];
    assert_eq!(second.title.as_deref(), None);
    assert_eq!(
        second.program_date_time.as_deref(),
        Some("1997-01-01T10:00:00Z")
    );
    assert_eq!(second.stop_range(), Some(149));
    assert!(!parsed.requires_binary_merge);
    let metadata = streams_metadata_json(&parsed.streams);
    assert!(metadata.contains("\"OriginalUrl\": \"\""));
    assert!(!metadata.contains("http://media.example/path/master.m3u8"));
    config.custom_parser_args.clear();
    Ok(())
}

#[tokio::test]
async fn media_playlist_endlist_and_unknown_encryption_signal_binary_merge()
-> Result<(), Box<dyn Error>> {
    let text = r#"#EXTM3U
#EXT-X-KEY:METHOD=FAIRPLAY,URI="base64:AAECAwQFBgcICQoLDA0ODw=="
#EXTINF:2,
seg.ts
#EXT-X-ENDLIST
"#;
    let parsed = HlsParser::new()
        .parse(
            text,
            "http://media.example/live.m3u8",
            &ParserConfig::default(),
        )
        .await?;
    let playlist = parsed.streams[0]
        .playlist
        .as_ref()
        .ok_or("missing playlist")?;

    assert!(!playlist.is_live);
    assert!(parsed.requires_binary_merge);
    assert_eq!(
        playlist.media_parts[0].media_segments[0].encryption.method,
        EncryptionMethod::Unknown
    );
    Ok(())
}

#[tokio::test]
async fn media_playlist_applies_key_rotation_and_none_method() -> Result<(), Box<dyn Error>> {
    let text = r#"#EXTM3U
#EXT-X-KEY:METHOD=AES-128,URI="base64:AAECAwQFBgcICQoLDA0ODw=="
#EXTINF:2,
seg0.ts
#EXT-X-KEY:METHOD=AES-128,URI="base64:Dw4NDAsKCQgHBgUEAwIBAA=="
#EXTINF:2,
seg1.ts
#EXT-X-KEY:METHOD=NONE
#EXTINF:2,
seg2.ts
#EXT-X-ENDLIST
"#;
    let parsed = HlsParser::new()
        .parse(
            text,
            "http://media.example/media.m3u8",
            &ParserConfig::default(),
        )
        .await?;
    let segments = &parsed.streams[0]
        .playlist
        .as_ref()
        .ok_or("missing playlist")?
        .media_parts[0]
        .media_segments;

    assert_eq!(segments[0].encryption.method, EncryptionMethod::Aes128);
    assert_eq!(segments[0].encryption.iv.as_ref().map(|iv| iv[15]), Some(0));
    assert_eq!(
        segments[0].encryption.key,
        Some(vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15])
    );
    assert_eq!(
        segments[1].encryption.key,
        Some(vec![15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0])
    );
    assert_eq!(segments[2].encryption.method, EncryptionMethod::None);
    assert!(segments[2].encryption.key.is_none());
    Ok(())
}

#[tokio::test]
async fn media_playlist_derives_implicit_iv_from_signed_segment_index() -> Result<(), Box<dyn Error>>
{
    let text = r#"#EXTM3U
#EXT-X-MEDIA-SEQUENCE:-2
#EXT-X-KEY:METHOD=AES-128,URI="base64:AAECAwQFBgcICQoLDA0ODw=="
#EXTINF:2,
neg2.ts
#EXTINF:2,
neg1.ts
#EXT-X-ENDLIST
"#;
    let parsed = HlsParser::new()
        .parse(
            text,
            "http://media.example/signed-index.m3u8",
            &ParserConfig::default(),
        )
        .await?;
    let segments = &parsed.streams[0]
        .playlist
        .as_ref()
        .ok_or("missing playlist")?
        .media_parts[0]
        .media_segments;

    assert_eq!(segments[0].index, -2);
    assert_eq!(segments[1].index, -1);
    assert_eq!(
        segments[0].encryption.iv.as_deref(),
        Some(
            &[
                0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 254
            ][..]
        )
    );
    assert_eq!(
        segments[1].encryption.iv.as_deref(),
        Some(
            &[
                0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255
            ][..]
        )
    );

    let custom_config = ParserConfig {
        custom_iv: Some(vec![7; 16]),
        ..ParserConfig::default()
    };
    let custom = HlsParser::new()
        .parse(
            text,
            "http://media.example/signed-index.m3u8",
            &custom_config,
        )
        .await?;
    assert_eq!(
        custom.streams[0]
            .playlist
            .as_ref()
            .ok_or("missing playlist")?
            .media_parts[0]
            .media_segments[0]
            .encryption
            .iv
            .as_deref(),
        Some(&[7; 16][..])
    );
    Ok(())
}

#[tokio::test]
async fn master_playlist_parses_variants_renditions_and_skips_closed_captions()
-> Result<(), Box<dyn Error>> {
    let config = ParserConfig {
        original_url: "http://cdn.example/master.m3u8?marker=1".to_string(),
        ..ParserConfig::default()
    };
    let text = r#"#EXTM3U
#EXT-X-MEDIA:TYPE=VIDEO,GROUP-ID="vid",NAME="Angle",URI="video/angle.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",LANGUAGE="en",NAME="English",DEFAULT=YES,CHANNELS="6/JOC",URI="audio/en.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",LANGUAGE="en",NAME="English Duplicate",URI="audio/en.m3u8"
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="sub",LANGUAGE="en",NAME="English CC",FORCED=YES,CHARACTERISTICS="public.accessibility.describes-video",URI="subs/en.m3u8"
#EXT-X-MEDIA:TYPE=CLOSED-CAPTIONS,GROUP-ID="cc",LANGUAGE="en",NAME="CC"
#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=500,URI="iframe.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1000,AVERAGE-BANDWIDTH=900,CODECS="avc1.640028,mp4a.40.2",RESOLUTION=1920x1080,FRAME-RATE=25,AUDIO="aud",SUBTITLES="sub",VIDEO-RANGE=PQ
video/main.m3u8
"#;

    let parsed = HlsParser::new()
        .parse(text, "http://cdn.example/path/master.m3u8", &config)
        .await?;

    assert!(parsed.is_master);
    assert_eq!(parsed.streams.len(), 4);
    assert_eq!(parsed.streams[0].media_type, Some(MediaType::Video));
    assert_eq!(parsed.streams[1].media_type, Some(MediaType::Audio));
    assert_eq!(parsed.streams[2].media_type, Some(MediaType::Subtitles));
    let variant = parsed
        .streams
        .iter()
        .find(|stream| stream.media_type.is_none())
        .ok_or("missing variant")?;
    assert_eq!(variant.original_url.as_str(), config.original_url.as_str());
    assert_eq!(variant.bandwidth, Some(900));
    assert_eq!(variant.codecs.as_deref(), Some("avc1.640028"));
    assert_eq!(variant.audio_id.as_deref(), Some("aud"));
    let audio = parsed
        .streams
        .iter()
        .find(|stream| stream.media_type == Some(MediaType::Audio))
        .ok_or("missing audio")?;
    assert_eq!(audio.original_url.as_str(), config.original_url.as_str());
    assert_eq!(audio.default, None);
    assert_eq!(audio.channels.as_deref(), Some("6/JOC"));
    assert_eq!(
        parsed
            .streams
            .iter()
            .filter(|stream| stream.media_type == Some(MediaType::Audio))
            .count(),
        1
    );
    let subtitle = parsed
        .streams
        .iter()
        .find(|stream| stream.media_type == Some(MediaType::Subtitles))
        .ok_or("missing subtitles")?;
    assert_eq!(subtitle.original_url.as_str(), config.original_url.as_str());
    assert_eq!(subtitle.forced, None);
    assert_eq!(subtitle.characteristics.as_deref(), Some("describes-video"));
    Ok(())
}

#[tokio::test]
async fn master_playlist_ignores_empty_rendition_values_for_empty_attributes()
-> Result<(), Box<dyn Error>> {
    let text = r#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="skip",LANGUAGE="en",NAME="Skip",URI=""
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",LANGUAGE="",NAME="",CHANNELS="",CHARACTERISTICS="",URI="audio/empty.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1000,CODECS="avc1.640028,mp4a.40.2",RESOLUTION=1280x720,AUDIO="",SUBTITLES="",VIDEO="",VIDEO-RANGE=""
video/main.m3u8
"#;

    let parsed = HlsParser::new()
        .parse(
            text,
            "http://cdn.example/path/master.m3u8",
            &ParserConfig::default(),
        )
        .await?;

    assert_eq!(parsed.streams.len(), 2);
    let audio = parsed
        .streams
        .iter()
        .find(|stream| stream.media_type == Some(MediaType::Audio))
        .ok_or("missing audio")?;
    assert_eq!(audio.group_id.as_deref(), Some("aud"));
    assert!(audio.language.is_none());
    assert!(audio.name.is_none());
    assert!(audio.channels.is_none());
    assert!(audio.characteristics.is_none());

    let variant = parsed
        .streams
        .iter()
        .find(|stream| stream.media_type.is_none())
        .ok_or("missing variant")?;
    assert_eq!(variant.codecs.as_deref(), Some("avc1.640028,mp4a.40.2"));
    assert!(variant.audio_id.is_none());
    assert!(variant.video_id.is_none());
    assert!(variant.subtitle_id.is_none());
    assert!(variant.video_range.is_none());
    Ok(())
}

#[tokio::test]
async fn hls_media_rendition_requires_type_but_keeps_unknown_type() -> Result<(), Box<dyn Error>> {
    let missing_type = r#"#EXTM3U
#EXT-X-MEDIA:GROUP-ID="aud",NAME="Missing",URI="audio/missing.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1000
video/main.m3u8
"#;
    assert!(
        HlsParser::new()
            .parse(
                missing_type,
                "http://cdn.example/path/master.m3u8",
                &ParserConfig::default()
            )
            .await
            .is_err()
    );

    let unknown_type = r#"#EXTM3U
#EXT-X-MEDIA:TYPE=DATA,GROUP-ID="data",NAME="Data",URI="data/main.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1000
video/main.m3u8
"#;
    let parsed = HlsParser::new()
        .parse(
            unknown_type,
            "http://cdn.example/path/master.m3u8",
            &ParserConfig::default(),
        )
        .await?;

    assert_eq!(parsed.streams.len(), 2);
    assert_eq!(parsed.streams[0].media_type, None);
    assert_eq!(parsed.streams[0].group_id.as_deref(), Some("data"));
    assert!(parsed.streams[0].url.ends_with("/data/main.m3u8"));
    Ok(())
}
