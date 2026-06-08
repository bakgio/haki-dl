use haki_dl::{EncryptionInfo, EncryptionMethod, KeySource, MediaPart, MediaSegment, Playlist};

#[test]
fn playlist_derives_duration_counts_ranges_and_encryption_state() {
    let encrypted = EncryptionInfo {
        method: EncryptionMethod::Aes128,
        key: Some(vec![1; 16]),
        iv: Some(vec![2; 16]),
        kid: None,
        scheme: None,
        protection_data: None,
        source: KeySource::Inline,
    };
    let first = MediaSegment {
        index: 5,
        duration: 2.0,
        title: Some("intro".to_string()),
        program_date_time: Some("1997-01-01T10:00:00Z".to_string()),
        start_range: Some(100),
        expected_length: Some(50),
        encryption: encrypted,
        url: "seg-5.m4s".to_string(),
        name_from_var: Some("v-5".to_string()),
    };
    let nearly_equal = MediaSegment {
        duration: 2.0005,
        encryption: EncryptionInfo::default(),
        program_date_time: None,
        name_from_var: None,
        ..first.clone()
    };
    let second = MediaSegment {
        index: 6,
        duration: 3.5,
        url: "seg-6.m4s".to_string(),
        ..MediaSegment::default()
    };
    let playlist = Playlist {
        url: "media.m3u8".to_string(),
        media_parts: vec![MediaPart {
            media_segments: vec![first.clone(), second],
        }],
        ..Playlist::default()
    };

    assert_eq!(first.stop_range(), Some(149));
    assert_eq!(
        MediaSegment {
            start_range: Some(5),
            expected_length: Some(0),
            ..MediaSegment::default()
        }
        .stop_range(),
        Some(4)
    );
    assert_eq!(
        MediaSegment {
            start_range: Some(5),
            expected_length: Some(-2),
            ..MediaSegment::default()
        }
        .stop_range(),
        Some(2)
    );
    assert_eq!(
        haki_dl::ByteRange {
            start: 5,
            length: 0
        }
        .end(),
        Some(4)
    );
    assert!(first.is_encrypted());
    assert_eq!(first, nearly_equal);
    assert_eq!(playlist.segments_count(), 2);
    assert_eq!(playlist.total_duration(), 5.5);
}
