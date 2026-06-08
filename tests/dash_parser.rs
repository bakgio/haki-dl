use std::error::Error;

use haki_dl::{DashParser, EncryptionMethod, MediaType, ParserConfig, RoleType};

#[test]
fn dash_segment_template_timeline_expands_segments() -> Result<(), Box<dyn Error>> {
    let mpd = r#"<MPD type="static" mediaPresentationDuration="PT6S" minimumUpdatePeriod="PT5S" publishTime="1997-01-01T10:00:00Z">
  <BaseURL>http://cdn.example/base/</BaseURL>
  <Period id="p0" duration="PT5S">
    <AdaptationSet mimeType="video/mp4" codecs="avc1.640028" frameRate="30000/1001" videoRange="PQ">
      <Representation id="v1" bandwidth="1000" width="1920" height="1080">
        <SegmentTemplate timescale="1000" initialization="init-$RepresentationID$.mp4" media="chunk-$Time$.m4s">
          <SegmentTimeline>
            <S t="0" d="2000" r="1"/>
            <S d="1000"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let parsed = DashParser::new().parse(
        mpd,
        "http://origin.example/manifest.mpd",
        &ParserConfig::default(),
    )?;
    let stream = &parsed.streams[0];
    let playlist = stream.playlist.as_ref().ok_or("missing playlist")?;

    assert_eq!(parsed.minimum_update_period, Some(5.0));
    assert_eq!(parsed.publish_time.as_deref(), Some("1997-01-01T10:00:00Z"));
    assert_eq!(stream.group_id.as_deref(), Some("v1"));
    assert_eq!(stream.resolution.as_deref(), Some("1920x1080"));
    assert_eq!(stream.frame_rate, Some(29.97));
    assert_eq!(stream.video_range.as_deref(), Some("PQ"));
    assert_eq!(
        playlist
            .media_init
            .as_ref()
            .map(|segment| segment.url.as_str()),
        Some("http://cdn.example/base/init-v1.mp4")
    );
    assert_eq!(playlist.segments_count(), 3);
    assert_eq!(
        playlist.media_parts[0].media_segments[0]
            .name_from_var
            .as_deref(),
        Some("0")
    );
    assert_eq!(
        playlist.media_parts[0].media_segments[1]
            .name_from_var
            .as_deref(),
        Some("2000")
    );
    assert_eq!(playlist.media_parts[0].media_segments[2].duration, 1.0);
    Ok(())
}

#[test]
fn dash_segment_list_ranges_channels_and_protection() -> Result<(), Box<dyn Error>> {
    let mpd = r#"<MPD xmlns:cenc="urn:mpeg:cenc:2013" mediaPresentationDuration="PT4S">
  <Period id="p0">
    <AdaptationSet mimeType="audio/mp4" lang="en">
      <AudioChannelConfiguration value="2"/>
      <Representation id="a1" bandwidth="128000" codecs="mp4a.40.2">
        <ContentProtection><cenc:pssh>AAAA</cenc:pssh></ContentProtection>
        <SegmentList timescale="48000" duration="96000">
          <Initialization sourceURL="init.mp4" range="0-99"/>
          <SegmentURL media="a1-1.m4s" mediaRange="100-199"/>
          <SegmentURL media="a1-2.m4s" mediaRange="200-299"/>
        </SegmentList>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let parsed = DashParser::new().parse(
        mpd,
        "http://cdn.example/audio/manifest.mpd",
        &ParserConfig::default(),
    )?;
    let stream = &parsed.streams[0];
    let playlist = stream.playlist.as_ref().ok_or("missing playlist")?;

    assert_eq!(stream.media_type, Some(MediaType::Audio));
    assert_eq!(stream.language.as_deref(), Some("en"));
    assert_eq!(stream.channels.as_deref(), Some("2"));
    assert_eq!(
        playlist
            .media_init
            .as_ref()
            .and_then(|segment| segment.stop_range()),
        Some(99)
    );
    assert_eq!(playlist.segments_count(), 2);
    assert_eq!(playlist.media_parts[0].media_segments[0].duration, 2.0);
    assert_eq!(
        playlist.media_parts[0].media_segments[0].stop_range(),
        Some(199)
    );
    assert_eq!(
        playlist.media_parts[0].media_segments[0].encryption.method,
        EncryptionMethod::Cenc
    );
    assert_eq!(
        playlist.media_parts[0].media_segments[0]
            .encryption
            .protection_data,
        Some(b"AAAA".to_vec())
    );
    Ok(())
}

#[test]
fn dash_segment_list_uses_base_url_for_ranged_init_and_media_without_urls()
-> Result<(), Box<dyn Error>> {
    let mpd = r#"<MPD mediaPresentationDuration="PT20S">
  <Period id="p0">
    <AdaptationSet mimeType="audio/mp4">
      <Representation id="a1" bandwidth="325257" codecs="opus" audioSamplingRate="48000">
        <BaseURL>https://cdn.example/audio.opus.mp4?token=fixture</BaseURL>
        <SegmentList timescale="48000" duration="480000">
          <Initialization range="0-1019"/>
          <SegmentURL mediaRange="1352-402287"/>
          <SegmentURL mediaRange="402288-802423"/>
        </SegmentList>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let parsed = DashParser::new().parse(
        mpd,
        "https://origin.example/path/manifest.mpd",
        &ParserConfig::default(),
    )?;
    let playlist = parsed.streams[0]
        .playlist
        .as_ref()
        .ok_or("missing playlist")?;
    let init = playlist.media_init.as_ref().ok_or("missing init")?;
    let first = &playlist.media_parts[0].media_segments[0];
    let second = &playlist.media_parts[0].media_segments[1];

    assert_eq!(playlist.segments_count(), 2);
    assert_eq!(init.url, "https://cdn.example/audio.opus.mp4?token=fixture");
    assert_eq!(init.start_range, Some(0));
    assert_eq!(init.stop_range(), Some(1019));
    assert_eq!(first.url, init.url);
    assert_eq!(first.start_range, Some(1352));
    assert_eq!(first.stop_range(), Some(402287));
    assert_eq!(first.duration, 10.0);
    assert_eq!(second.url, init.url);
    assert_eq!(second.start_range, Some(402288));
    assert_eq!(second.stop_range(), Some(802423));
    Ok(())
}

#[test]
fn dash_content_protection_preserves_default_kid_and_widevine_pssh() -> Result<(), Box<dyn Error>> {
    let mpd = r#"<MPD xmlns:cenc="urn:mpeg:cenc:2013" mediaPresentationDuration="PT2S">
  <Period id="p0">
    <AdaptationSet mimeType="video/mp4" codecs="avc1.640028">
      <ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011" value="cenc" cenc:default_KID="00112233-4455-6677-8899-aabbccddeeff"/>
      <ContentProtection schemeIdUri="urn:uuid:9a04f079-9840-4286-ab92-e65be0885f95"><cenc:pssh>PLAYREADY</cenc:pssh></ContentProtection>
      <ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"><cenc:pssh>WIDEVINE</cenc:pssh></ContentProtection>
      <Representation id="v1" bandwidth="1000">
        <SegmentList duration="2"><SegmentURL media="seg.m4s"/></SegmentList>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let parsed = DashParser::new().parse(
        mpd,
        "https://cdn.example/manifest.mpd",
        &ParserConfig::default(),
    )?;
    let segment = &parsed.streams[0]
        .playlist
        .as_ref()
        .ok_or("missing playlist")?
        .media_parts[0]
        .media_segments[0];

    assert_eq!(segment.encryption.method, EncryptionMethod::Cenc);
    assert_eq!(
        segment.encryption.kid,
        Some(vec![
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ])
    );
    assert_eq!(segment.encryption.scheme.as_deref(), Some("cenc"));
    assert_eq!(
        segment.encryption.protection_data.as_deref(),
        Some(b"WIDEVINE".as_slice())
    );
    Ok(())
}

#[test]
fn dash_rejects_invalid_publish_time() {
    for value in ["not-a-date", "1997-02-31T00:00:00Z"] {
        let mpd = format!(
            r#"<MPD publishTime="{value}" mediaPresentationDuration="PT2S">
  <Period id="p0">
    <AdaptationSet mimeType="video/mp4">
      <Representation id="v1" bandwidth="1000">
        <SegmentList duration="2"><SegmentURL media="seg.m4s"/></SegmentList>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#
        );

        let error = DashParser::new()
            .parse(
                &mpd,
                "https://cdn.example/manifest.mpd",
                &ParserConfig::default(),
            )
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();

        assert!(
            error.contains("invalid DASH publish time"),
            "{value}: {error}"
        );
    }
}

#[test]
fn dash_rejects_invalid_present_numeric_and_time_attributes() {
    let cases = [
        r#"<MPD mediaPresentationDuration="PT2S"><Period><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="bad"><SegmentList duration="2"><SegmentURL media="seg.m4s"/></SegmentList></Representation></AdaptationSet></Period></MPD>"#,
        r#"<MPD mediaPresentationDuration="PT2S"><Period><AdaptationSet mimeType="video/mp4" frameRate="30000/bad"><Representation id="v1" bandwidth="1000"><SegmentList duration="2"><SegmentURL media="seg.m4s"/></SegmentList></Representation></AdaptationSet></Period></MPD>"#,
        r#"<MPD mediaPresentationDuration="PT2S"><Period><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="1000" frameRate="30000/0"><SegmentList duration="2"><SegmentURL media="seg.m4s"/></SegmentList></Representation></AdaptationSet></Period></MPD>"#,
        r#"<MPD mediaPresentationDuration="PT2S"><Period><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="1000"><SegmentList timescale="bad" duration="2"><SegmentURL media="seg.m4s"/></SegmentList></Representation></AdaptationSet></Period></MPD>"#,
        r#"<MPD mediaPresentationDuration="PT2S"><Period><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="1000"><SegmentList timescale="1" duration="bad"><SegmentURL media="seg.m4s"/></SegmentList></Representation></AdaptationSet></Period></MPD>"#,
        r#"<MPD mediaPresentationDuration="PT2S"><Period><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="1000"><SegmentList timescale="1" duration="1"><SegmentURL media="seg.m4s" mediaRange="bad"/></SegmentList></Representation></AdaptationSet></Period></MPD>"#,
        r#"<MPD mediaPresentationDuration="PT2S"><Period><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="1000"><SegmentTemplate timescale="bad" duration="1" media="seg-$Number$.m4s"/></Representation></AdaptationSet></Period></MPD>"#,
        r#"<MPD mediaPresentationDuration="PT2S"><Period><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="1000"><SegmentTemplate timescale="1" duration="bad" media="seg-$Number$.m4s"/></Representation></AdaptationSet></Period></MPD>"#,
        r#"<MPD mediaPresentationDuration="PT2S"><Period><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="1000"><SegmentTemplate timescale="1" duration="1" startNumber="bad" media="seg-$Number$.m4s"/></Representation></AdaptationSet></Period></MPD>"#,
        r#"<MPD type="dynamic" availabilityStartTime="1997-01-01T00:00:00Z" timeShiftBufferDepth="PT2S"><Period><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="1000"><SegmentTemplate timescale="1" duration="1" presentationTimeOffset="bad" media="seg-$Number$.m4s"/></Representation></AdaptationSet></Period></MPD>"#,
        r#"<MPD type="dynamic" availabilityStartTime="1997-02-31T00:00:00Z" timeShiftBufferDepth="PT2S"><Period><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="1000"><SegmentTemplate timescale="1" duration="1" media="seg-$Number$.m4s"/></Representation></AdaptationSet></Period></MPD>"#,
        r#"<MPD mediaPresentationDuration="PT2S"><Period><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="1000"><SegmentTemplate timescale="1" media="seg-$Time$.m4s"><SegmentTimeline><S t="bad" d="1"/></SegmentTimeline></SegmentTemplate></Representation></AdaptationSet></Period></MPD>"#,
        r#"<MPD mediaPresentationDuration="PT2S"><Period><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="1000"><SegmentTemplate timescale="1" media="seg-$Time$.m4s"><SegmentTimeline><S t="0" d="bad"/></SegmentTimeline></SegmentTemplate></Representation></AdaptationSet></Period></MPD>"#,
        r#"<MPD mediaPresentationDuration="PT2S"><Period><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="1000"><SegmentTemplate timescale="1" media="seg-$Time$.m4s"><SegmentTimeline><S t="0" d="1" r="bad"/></SegmentTimeline></SegmentTemplate></Representation></AdaptationSet></Period></MPD>"#,
        r#"<MPD timeShiftBufferDepth="not-duration"><Period><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="1000"><SegmentList duration="1"><SegmentURL media="seg.m4s"/></SegmentList></Representation></AdaptationSet></Period></MPD>"#,
        r#"<MPD mediaPresentationDuration="not-duration"><Period><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="1000"><SegmentList duration="1"><SegmentURL media="seg.m4s"/></SegmentList></Representation></AdaptationSet></Period></MPD>"#,
    ];

    for mpd in cases {
        assert!(
            DashParser::new()
                .parse(
                    mpd,
                    "https://cdn.example/manifest.mpd",
                    &ParserConfig::default()
                )
                .is_err(),
            "{mpd}"
        );
    }
}

#[test]
fn dash_preserves_absent_defaults_and_plain_frame_rate_behavior() -> Result<(), Box<dyn Error>> {
    let mpd = r#"<MPD>
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <Representation id="v1" frameRate="25">
        <SegmentList>
          <SegmentURL media="seg.m4s"/>
        </SegmentList>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let parsed = DashParser::new().parse(
        mpd,
        "https://cdn.example/manifest.mpd",
        &ParserConfig::default(),
    )?;
    let stream = &parsed.streams[0];
    let segment = &stream
        .playlist
        .as_ref()
        .ok_or("missing playlist")?
        .media_parts[0]
        .media_segments[0];

    assert_eq!(stream.bandwidth, Some(0));
    assert_eq!(stream.frame_rate, None);
    assert_eq!(segment.duration, 0.0);
    Ok(())
}

#[test]
fn dash_negative_timeline_repeat_expands_to_period_duration() -> Result<(), Box<dyn Error>> {
    let mpd = r#"<MPD mediaPresentationDuration="PT5S">
  <Period id="p0" duration="PT5S">
    <AdaptationSet mimeType="video/mp4">
      <Representation id="v1" bandwidth="1000">
        <SegmentTemplate timescale="1000" media="t-$Time$.m4s">
          <SegmentTimeline><S t="0" d="1000" r="-1"/></SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let parsed = DashParser::new().parse(
        mpd,
        "http://cdn.example/v/manifest.mpd",
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
        Some("4000")
    );
    Ok(())
}

#[test]
fn dash_representation_media_type_carries_within_adaptation_set() -> Result<(), Box<dyn Error>> {
    let mpd = r#"<MPD mediaPresentationDuration="PT2S">
  <Period id="p0" duration="PT2S">
    <AdaptationSet>
      <Representation id="a1" contentType="audio" mimeType="audio/mp4" bandwidth="128000">
        <SegmentList timescale="1" duration="1"><SegmentURL media="a1.m4s"/></SegmentList>
      </Representation>
      <Representation id="a2" bandwidth="64000">
        <SegmentList timescale="1" duration="1"><SegmentURL media="a2.m4s"/></SegmentList>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let parsed = DashParser::new().parse(
        mpd,
        "http://cdn.example/path/manifest.mpd",
        &ParserConfig::default(),
    )?;

    assert_eq!(parsed.streams[0].media_type, Some(MediaType::Audio));
    assert_eq!(parsed.streams[1].media_type, Some(MediaType::Audio));
    Ok(())
}

#[test]
fn dash_segment_template_replaces_number_bandwidth_and_representation_tokens()
-> Result<(), Box<dyn Error>> {
    let mpd = r#"<MPD mediaPresentationDuration="PT2S">
  <Period id="p0" duration="PT2S">
    <AdaptationSet mimeType="video/mp4">
      <Representation id="v-main" bandwidth="2500000">
        <SegmentTemplate timescale="1" duration="1" startNumber="5" initialization="$RepresentationID$-$Bandwidth$-init.mp4" media="$RepresentationID$-$Bandwidth$-$Number%05d$.m4s"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let parsed = DashParser::new().parse(
        mpd,
        "http://cdn.example/path/manifest.mpd",
        &ParserConfig::default(),
    )?;
    let playlist = parsed.streams[0]
        .playlist
        .as_ref()
        .ok_or("missing playlist")?;

    assert_eq!(
        playlist
            .media_init
            .as_ref()
            .map(|segment| segment.url.as_str()),
        Some("http://cdn.example/path/v-main-2500000-init.mp4")
    );
    assert_eq!(
        playlist.media_parts[0].media_segments[0].url,
        "http://cdn.example/path/v-main-2500000-00005.m4s"
    );
    assert_eq!(
        playlist.media_parts[0].media_segments[1].url,
        "http://cdn.example/path/v-main-2500000-00006.m4s"
    );
    Ok(())
}

#[test]
fn dash_segment_template_uses_closest_timeline_only() -> Result<(), Box<dyn Error>> {
    let mpd = r#"<MPD mediaPresentationDuration="PT2S">
  <Period id="p0" duration="PT2S">
    <AdaptationSet mimeType="video/mp4">
      <SegmentTemplate timescale="1" duration="1" media="outer-$Time$.m4s">
        <SegmentTimeline><S t="0" d="1" r="4"/></SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v-main" bandwidth="2500000">
        <SegmentTemplate startNumber="7" media="inner-$Number$.m4s"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let parsed = DashParser::new().parse(
        mpd,
        "http://cdn.example/path/manifest.mpd",
        &ParserConfig::default(),
    )?;
    let playlist = parsed.streams[0]
        .playlist
        .as_ref()
        .ok_or("missing playlist")?;

    assert_eq!(playlist.segments_count(), 2);
    assert_eq!(
        playlist.media_parts[0].media_segments[0].url,
        "http://cdn.example/path/inner-7.m4s"
    );
    assert_eq!(
        playlist.media_parts[0].media_segments[1].url,
        "http://cdn.example/path/inner-8.m4s"
    );
    Ok(())
}

#[test]
fn dash_refresh_updates_selected_stream_playlist_by_identity() -> Result<(), Box<dyn Error>> {
    let first = r#"<MPD mediaPresentationDuration="PT1S">
  <Period id="p0" duration="PT1S"><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="1000" width="640" height="360"><SegmentTemplate timescale="1" duration="1" initialization="old-init.mp4" media="old-$Number$.m4s"/></Representation></AdaptationSet></Period>
</MPD>"#;
    let second = r#"<MPD mediaPresentationDuration="PT1S" publishTime="1997-01-01T10:00:00Z">
  <Period id="p1" duration="PT1S"><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="1000" width="640" height="360"><SegmentTemplate timescale="1" duration="1" initialization="new-init.mp4" media="new-$Number$.m4s"/></Representation></AdaptationSet></Period>
</MPD>"#;
    let parser = DashParser::new();
    let mut streams = parser
        .parse(
            first,
            "http://cdn.example/v/manifest.mpd",
            &ParserConfig::default(),
        )?
        .streams;

    parser.refresh_streams(
        &mut streams,
        second,
        "http://cdn.example/v/manifest.mpd",
        &ParserConfig::default(),
    )?;
    let playlist = streams[0].playlist.as_ref().ok_or("missing playlist")?;

    assert!(
        playlist.media_parts[0].media_segments[0]
            .url
            .contains("new-1.m4s")
    );
    assert_eq!(
        playlist
            .media_init
            .as_ref()
            .map(|segment| segment.url.as_str()),
        Some("http://cdn.example/v/old-init.mp4")
    );
    assert_eq!(streams[0].publish_time, None);
    Ok(())
}

#[test]
fn dash_segment_base_roles_and_subtitle_extension() -> Result<(), Box<dyn Error>> {
    let mpd = r#"<MPD mediaPresentationDuration="PT10S">
  <Period id="p0">
    <AdaptationSet mimeType="application/ttml+xml" lang="en">
      <Role value="forced-subtitle"/>
      <Representation id="s1">
        <SegmentBase>
          <Initialization sourceURL="sub.mp4" range="0-10"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let parsed = DashParser::new().parse(
        mpd,
        "http://cdn.example/subs/manifest.mpd",
        &ParserConfig::default(),
    )?;
    let stream = &parsed.streams[0];
    let playlist = stream.playlist.as_ref().ok_or("missing playlist")?;

    assert_eq!(stream.media_type, Some(MediaType::Subtitles));
    assert_eq!(stream.role, Some(RoleType::ForcedSubtitle));
    assert_eq!(stream.extension.as_deref(), Some("ttml"));
    assert_eq!(
        playlist
            .media_init
            .as_ref()
            .and_then(|segment| segment.stop_range()),
        Some(10)
    );
    assert_eq!(playlist.segments_count(), 1);
    assert_eq!(playlist.media_parts[0].media_segments[0].duration, 10.0);
    Ok(())
}

#[test]
fn dash_dynamic_mpd_sets_live_refresh_metadata() -> Result<(), Box<dyn Error>> {
    let mpd = r#"<MPD type="dynamic" timeShiftBufferDepth="PT30S" availabilityStartTime="1997-01-01T10:00:00Z">
  <Period id="p0" duration="PT2S">
    <AdaptationSet mimeType="video/mp4">
      <Representation id="v1" bandwidth="1000">
        <SegmentTemplate timescale="1" duration="1" startNumber="10" media="live-$Number$.m4s"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let parsed = DashParser::new().parse(
        mpd,
        "http://cdn.example/live/manifest.mpd",
        &ParserConfig::default(),
    )?;
    let playlist = parsed.streams[0]
        .playlist
        .as_ref()
        .ok_or("missing playlist")?;

    assert!(parsed.is_dynamic);
    assert!(playlist.is_live);
    assert_eq!(playlist.refresh_interval_ms, 15_000.0);
    assert_eq!(playlist.media_parts[0].media_segments[0].index, 10);
    Ok(())
}

#[test]
fn dash_frame_rate_uses_adaptation_value_before_representation() -> Result<(), Box<dyn Error>> {
    let both_slash = r#"<MPD mediaPresentationDuration="PT1S">
  <Period id="p0" duration="PT1S">
    <AdaptationSet mimeType="video/mp4" frameRate="24000/1001">
      <Representation id="v1" bandwidth="1000" frameRate="30000/1001">
        <SegmentTemplate timescale="1" duration="1" media="seg-$Number$.m4s"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
    let parsed = DashParser::new().parse(
        both_slash,
        "http://cdn.example/v/manifest.mpd",
        &ParserConfig::default(),
    )?;
    assert_eq!(parsed.streams[0].frame_rate, Some(23.976));

    let adaptation_plain = r#"<MPD mediaPresentationDuration="PT1S">
  <Period id="p0" duration="PT1S">
    <AdaptationSet mimeType="video/mp4" frameRate="25">
      <Representation id="v1" bandwidth="1000" frameRate="30000/1001">
        <SegmentTemplate timescale="1" duration="1" media="seg-$Number$.m4s"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
    let parsed = DashParser::new().parse(
        adaptation_plain,
        "http://cdn.example/v/manifest.mpd",
        &ParserConfig::default(),
    )?;
    assert_eq!(parsed.streams[0].frame_rate, Some(29.97));

    let invalid_adaptation = r#"<MPD mediaPresentationDuration="PT1S">
  <Period id="p0" duration="PT1S">
    <AdaptationSet mimeType="video/mp4" frameRate="24000/bad">
      <Representation id="v1" bandwidth="1000" frameRate="30000/1001">
        <SegmentTemplate timescale="1" duration="1" media="seg-$Number$.m4s"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
    assert!(
        DashParser::new()
            .parse(
                invalid_adaptation,
                "http://cdn.example/v/manifest.mpd",
                &ParserConfig::default()
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn dash_zero_duration_template_is_rejected() {
    let mpd = r#"<MPD mediaPresentationDuration="PT1S">
  <Period id="p0" duration="PT1S">
    <AdaptationSet mimeType="video/mp4">
      <Representation id="v1" bandwidth="1000">
        <SegmentTemplate timescale="1" duration="0" media="bad-$Number$.m4s"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    assert!(
        DashParser::new()
            .parse(
                mpd,
                "http://cdn.example/v/manifest.mpd",
                &ParserConfig::default()
            )
            .is_err()
    );
}

#[test]
fn dash_template_without_media_is_rejected() -> Result<(), Box<dyn Error>> {
    let missing_media = r#"<MPD mediaPresentationDuration="PT1S">
  <Period id="p0" duration="PT1S">
    <AdaptationSet mimeType="video/mp4">
      <Representation id="v1" bandwidth="1000">
        <SegmentTemplate timescale="1" duration="1" initialization="init.mp4"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    assert!(
        DashParser::new()
            .parse(
                missing_media,
                "http://cdn.example/v/manifest.mpd",
                &ParserConfig::default()
            )
            .is_err()
    );

    let inherited_media = r#"<MPD mediaPresentationDuration="PT1S">
  <Period id="p0" duration="PT1S">
    <AdaptationSet mimeType="video/mp4">
      <SegmentTemplate timescale="1" duration="1" media="outer-$Number$.m4s"/>
      <Representation id="v1" bandwidth="1000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
    let parsed = DashParser::new().parse(
        inherited_media,
        "http://cdn.example/v/manifest.mpd",
        &ParserConfig::default(),
    )?;
    let segment = &parsed.streams[0]
        .playlist
        .as_ref()
        .ok_or("missing playlist")?
        .media_parts[0]
        .media_segments[0];
    assert_eq!(segment.url, "http://cdn.example/v/outer-1.m4s");
    Ok(())
}

#[test]
fn dash_static_periods_with_same_stream_merge_into_parts() -> Result<(), Box<dyn Error>> {
    let mpd = r#"<MPD mediaPresentationDuration="PT4S">
  <Period id="p0" duration="PT2S">
    <AdaptationSet mimeType="video/mp4">
      <Representation id="v1" bandwidth="1000" width="640" height="360">
        <SegmentTemplate timescale="1" duration="1" startNumber="1" media="p0-$Number$.m4s"/>
      </Representation>
    </AdaptationSet>
  </Period>
  <Period id="p1" duration="PT2S">
    <AdaptationSet mimeType="video/mp4">
      <Representation id="v1" bandwidth="1000" width="640" height="360">
        <SegmentTemplate timescale="1" duration="1" startNumber="1" media="p1-$Number$.m4s"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let parsed = DashParser::new().parse(
        mpd,
        "http://cdn.example/v/manifest.mpd",
        &ParserConfig::default(),
    )?;
    let playlist = parsed.streams[0]
        .playlist
        .as_ref()
        .ok_or("missing playlist")?;

    assert_eq!(parsed.streams.len(), 1);
    assert_eq!(playlist.media_parts.len(), 2);
    assert_eq!(playlist.segments_count(), 4);
    assert_eq!(playlist.media_parts[1].media_segments[0].index, 2);
    assert!(
        playlist.media_parts[1].media_segments[0]
            .url
            .contains("p1-1.m4s")
    );
    Ok(())
}

#[test]
fn dash_static_periods_with_same_last_url_extend_duration() -> Result<(), Box<dyn Error>> {
    let mpd = r#"<MPD mediaPresentationDuration="PT4S">
  <Period id="p0" duration="PT2S">
    <AdaptationSet mimeType="video/mp4">
      <Representation id="v1" bandwidth="1000" width="640" height="360">
        <SegmentTemplate timescale="1" duration="2" startNumber="1" media="same.m4s"/>
      </Representation>
    </AdaptationSet>
  </Period>
  <Period id="p1" duration="PT2S">
    <AdaptationSet mimeType="video/mp4">
      <Representation id="v1" bandwidth="1000" width="640" height="360">
        <SegmentTemplate timescale="1" duration="2" startNumber="1" media="same.m4s"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let parsed = DashParser::new().parse(
        mpd,
        "http://cdn.example/v/manifest.mpd",
        &ParserConfig::default(),
    )?;
    let playlist = parsed.streams[0]
        .playlist
        .as_ref()
        .ok_or("missing playlist")?;

    assert_eq!(parsed.streams.len(), 1);
    assert_eq!(playlist.media_parts.len(), 1);
    assert_eq!(playlist.segments_count(), 1);
    assert_eq!(playlist.media_parts[0].media_segments[0].duration, 4.0);
    Ok(())
}

#[test]
fn dash_applies_track_defaults_language_filter_and_extension_normalization()
-> Result<(), Box<dyn Error>> {
    let mpd = r#"<MPD mediaPresentationDuration="PT2S">
  <Period id="p0" duration="PT2S">
    <AdaptationSet mimeType="audio/mp4" lang="en@bad">
      <Representation id="a-low" bandwidth="64000"><SegmentTemplate timescale="1" duration="2" media="a-low-$Number$.m4s"/></Representation>
      <Representation id="a-high" bandwidth="128000" volumeAdjust="plus"><SegmentTemplate timescale="1" duration="2" media="a-high-$Number$.m4s"/></Representation>
    </AdaptationSet>
    <AdaptationSet mimeType="application/mp4" codecs="wvtt">
      <Role value="subtitle"/>
      <Representation id="s1" bandwidth="100"><SegmentTemplate timescale="1" duration="2" media="s-$Number$.m4s"/></Representation>
    </AdaptationSet>
    <AdaptationSet mimeType="video/mp4">
      <Representation id="v1" bandwidth="1000" width="640" height="360"><SegmentTemplate timescale="1" duration="1" media="v-$Number$.m4s"/></Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let parsed = DashParser::new().parse(
        mpd,
        "http://cdn.example/path/manifest.mpd",
        &ParserConfig::default(),
    )?;
    let video = parsed
        .streams
        .iter()
        .find(|stream| stream.media_type.is_none())
        .ok_or("missing video")?;
    let high_audio = parsed
        .streams
        .iter()
        .find(|stream| stream.group_id.as_deref() == Some("a-high-plus"))
        .ok_or("missing adjusted audio")?;
    let subtitle = parsed
        .streams
        .iter()
        .find(|stream| stream.media_type == Some(MediaType::Subtitles))
        .ok_or("missing subtitle")?;

    assert_eq!(high_audio.language.as_deref(), Some("und"));
    assert_eq!(video.audio_id.as_deref(), Some("a-high-plus"));
    assert_eq!(video.subtitle_id.as_deref(), Some("s1"));
    assert_eq!(video.extension.as_deref(), Some("m4s"));
    assert_eq!(subtitle.extension.as_deref(), Some("m4s"));
    Ok(())
}
