use std::error::Error;

use haki_dl::{
    CustomRange, MediaPart, MediaSegment, MediaType, Playlist, RoleType, Stream, StreamFilter,
    apply_custom_range, apply_stream_filters, clean_ad_segments, filter_drop, filter_keep,
};

#[test]
fn role_and_video_range_filters_are_applied() -> Result<(), Box<dyn Error>> {
    let streams = vec![
        Stream {
            group_id: Some("main".to_string()),
            video_range: Some("PQ".to_string()),
            media_type: Some(MediaType::Video),
            role: Some(RoleType::Main),
            ..Stream::default()
        },
        Stream {
            group_id: Some("commentary".to_string()),
            video_range: Some("SDR".to_string()),
            media_type: Some(MediaType::Video),
            role: Some(RoleType::Commentary),
            ..Stream::default()
        },
        Stream {
            group_id: Some("forced".to_string()),
            video_range: Some("PQ".to_string()),
            media_type: Some(MediaType::Video),
            role: Some(RoleType::ForcedSubtitle),
            ..Stream::default()
        },
        Stream {
            group_id: Some("numeric".to_string()),
            video_range: Some("PQ".to_string()),
            media_type: Some(MediaType::Video),
            role: Some(RoleType::Numeric(10)),
            ..Stream::default()
        },
    ];
    let filter = StreamFilter {
        for_choice: "all".to_string(),
        range: Some("P.".to_string()),
        role: Some(RoleType::Main),
        ..StreamFilter::default()
    };
    let kept = filter_keep(&streams, Some(&filter))?;

    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].group_id.as_deref(), Some("main"));

    let forced = filter_keep(
        &streams,
        Some(&StreamFilter {
            for_choice: "all".to_string(),
            role: Some(RoleType::ForcedSubtitle),
            ..StreamFilter::default()
        }),
    )?;
    assert_eq!(forced.len(), 1);
    assert_eq!(forced[0].group_id.as_deref(), Some("forced"));

    let numeric = filter_keep(
        &streams,
        Some(&StreamFilter {
            for_choice: "all".to_string(),
            role: Some(RoleType::Numeric(10)),
            ..StreamFilter::default()
        }),
    )?;
    assert_eq!(numeric.len(), 1);
    assert_eq!(numeric[0].group_id.as_deref(), Some("numeric"));

    let no_role = filter_keep(
        &streams,
        Some(&StreamFilter {
            for_choice: "all".to_string(),
            ..StreamFilter::default()
        }),
    )?;
    assert_eq!(no_role.len(), streams.len());
    Ok(())
}

#[test]
fn drop_filter_distinguishes_streams_by_display_metadata() -> Result<(), Box<dyn Error>> {
    let streams = vec![
        Stream {
            media_type: Some(MediaType::Video),
            group_id: Some("same".to_string()),
            resolution: Some("1920x1080".to_string()),
            bandwidth: Some(1_000_000),
            video_range: Some("SDR".to_string()),
            ..Stream::default()
        },
        Stream {
            media_type: Some(MediaType::Video),
            group_id: Some("same".to_string()),
            resolution: Some("1920x1080".to_string()),
            bandwidth: Some(2_000_000),
            video_range: Some("PQ".to_string()),
            ..Stream::default()
        },
    ];
    let remaining = filter_drop(
        &streams,
        Some(&StreamFilter {
            for_choice: "all".to_string(),
            bandwidth_max: Some(1_500_000),
            ..StreamFilter::default()
        }),
    )?;

    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].bandwidth, Some(2_000_000));
    assert_eq!(remaining[0].video_range.as_deref(), Some("PQ"));
    Ok(())
}

#[test]
fn drop_filter_uses_display_identity_for_selected_streams() -> Result<(), Box<dyn Error>> {
    let streams = vec![
        Stream {
            media_type: Some(MediaType::Video),
            group_id: Some("same".to_string()),
            language: Some("eng".to_string()),
            resolution: Some("1920x1080".to_string()),
            bandwidth: Some(2_000_000),
            ..Stream::default()
        },
        Stream {
            media_type: Some(MediaType::Video),
            group_id: Some("same".to_string()),
            language: Some("fra".to_string()),
            resolution: Some("1920x1080".to_string()),
            bandwidth: Some(2_000_000),
            ..Stream::default()
        },
    ];
    let remaining = filter_drop(
        &streams,
        Some(&StreamFilter {
            for_choice: "all".to_string(),
            language: Some("eng".to_string()),
            ..StreamFilter::default()
        }),
    )?;

    assert!(remaining.is_empty());
    Ok(())
}

#[test]
fn drop_filters_are_applied_before_keep_filters() -> Result<(), Box<dyn Error>> {
    let streams = vec![
        video("v-high", 3_000_000, "1920x1080"),
        video("v-low", 1_000_000, "1280x720"),
    ];
    let selected = apply_stream_filters(
        &streams,
        &[StreamFilter {
            for_choice: "best".to_string(),
            ..StreamFilter::default()
        }],
        &[],
        &[],
        &[StreamFilter {
            for_choice: "all".to_string(),
            id: Some("v-high".to_string()),
            ..StreamFilter::default()
        }],
        &[],
        &[],
    )?;

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].group_id.as_deref(), Some("v-low"));
    Ok(())
}

#[test]
fn custom_ranges_and_ad_cleanup_mutate_playlists() -> Result<(), Box<dyn Error>> {
    let mut by_segment = vec![video_with_urls(
        "v",
        &["seg0.ts", "seg1.ts", "seg2.ts", "seg3.ts"],
    )];
    apply_custom_range(
        &mut by_segment,
        Some(&CustomRange::Segment {
            input: "1-2".to_string(),
            start_index: 1,
            end_index: 2,
        }),
    );
    let playlist = by_segment[0].playlist.as_ref().ok_or("missing playlist")?;
    assert_eq!(playlist.segments_count(), 2);
    assert_eq!(by_segment[0].skipped_duration, Some(2.0));

    let mut by_time = vec![video_with_urls(
        "v",
        &["seg0.ts", "seg1.ts", "seg2.ts", "seg3.ts"],
    )];
    apply_custom_range(
        &mut by_time,
        Some(&CustomRange::Time {
            input: "2-4".to_string(),
            start_seconds: 2.0,
            end_seconds: 4.0,
        }),
    );
    let playlist = by_time[0].playlist.as_ref().ok_or("missing playlist")?;
    assert_eq!(playlist.segments_count(), 2);
    assert_eq!(
        playlist.media_parts[0].media_segments[0].url.as_str(),
        "seg1.ts"
    );

    let high_index = i64::from(i32::MAX) + 1;
    let mut high_indexes = vec![Stream {
        media_type: Some(MediaType::Video),
        group_id: Some("high-index".to_string()),
        playlist: Some(Playlist {
            media_parts: vec![MediaPart {
                media_segments: vec![
                    segment(high_index - 1, "before.ts"),
                    segment(high_index, "inside-a.ts"),
                    segment(high_index + 1, "inside-b.ts"),
                ],
            }],
            ..Playlist::default()
        }),
        ..Stream::default()
    }];
    apply_custom_range(
        &mut high_indexes,
        Some(&CustomRange::Segment {
            input: format!("{high_index}-"),
            start_index: high_index,
            end_index: i64::MAX,
        }),
    );
    let playlist = high_indexes[0]
        .playlist
        .as_ref()
        .ok_or("missing playlist")?;
    assert_eq!(playlist.segments_count(), 2);
    assert_eq!(
        playlist.media_parts[0]
            .media_segments
            .iter()
            .map(|segment| segment.url.as_str())
            .collect::<Vec<_>>(),
        vec!["inside-a.ts", "inside-b.ts"]
    );

    let mut with_ads = vec![Stream {
        group_id: Some("ad-test".to_string()),
        playlist: Some(Playlist {
            media_parts: vec![
                MediaPart {
                    media_segments: vec![segment(0, "media/ad/seg0.ts")],
                },
                MediaPart {
                    media_segments: vec![segment(1, "media/main/seg1.ts")],
                },
            ],
            ..Playlist::default()
        }),
        ..Stream::default()
    }];
    clean_ad_segments(&mut with_ads, &["/ad/".to_string()])?;
    let playlist = with_ads[0].playlist.as_ref().ok_or("missing playlist")?;

    assert_eq!(playlist.media_parts.len(), 1);
    assert_eq!(playlist.segments_count(), 1);
    assert_eq!(
        playlist.media_parts[0].media_segments[0].url.as_str(),
        "media/main/seg1.ts"
    );
    Ok(())
}

fn video(group_id: &str, bandwidth: i64, resolution: &str) -> Stream {
    Stream {
        media_type: Some(MediaType::Video),
        group_id: Some(group_id.to_string()),
        bandwidth: Some(bandwidth),
        resolution: Some(resolution.to_string()),
        ..Stream::default()
    }
}

fn video_with_urls(group_id: &str, urls: &[&str]) -> Stream {
    Stream {
        media_type: Some(MediaType::Video),
        group_id: Some(group_id.to_string()),
        playlist: Some(Playlist {
            media_parts: vec![MediaPart {
                media_segments: urls
                    .iter()
                    .enumerate()
                    .map(|(index, url)| MediaSegment {
                        index: index as i64,
                        duration: 2.0,
                        url: (*url).to_string(),
                        ..MediaSegment::default()
                    })
                    .collect(),
            }],
            ..Playlist::default()
        }),
        ..Stream::default()
    }
}

fn segment(index: i64, url: &str) -> MediaSegment {
    MediaSegment {
        index,
        duration: 2.0,
        url: url.to_string(),
        ..MediaSegment::default()
    }
}
