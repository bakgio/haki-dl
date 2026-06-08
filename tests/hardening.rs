use haki_dl::{
    BoundedEventQueue, MediaPart, MediaSegment, Playlist, ProgressEvent, ResourceLimits, Stream,
    estimate_segment_count, validate_cleanup_paths, validate_manifest_size,
    validate_resource_limits,
};
use std::error::Error;

mod support;
use support::TempDirectory;

#[test]
fn resource_limits_reject_large_stream_and_manifest_shapes() {
    let stream = stream_with_segments(3);
    let limits = ResourceLimits {
        max_concurrent_streams: 1,
        max_segments_per_stream: 2,
        max_event_queue: 2,
        max_manifest_bytes: 4,
    };

    assert!(validate_resource_limits(std::slice::from_ref(&stream), &limits).is_err());
    assert!(validate_manifest_size(5, &limits).is_err());
    assert_eq!(estimate_segment_count(&[stream]), 3);
}

#[test]
fn bounded_event_queue_enforces_backpressure() {
    let mut queue = BoundedEventQueue::new(1);
    assert!(queue.push(ProgressEvent::PlanningStarted).is_ok());
    assert!(queue.push(ProgressEvent::ManifestLoading).is_err());
    assert_eq!(queue.events().len(), 1);
}

#[test]
fn cleanup_validation_rejects_paths_outside_temp_root() -> Result<(), Box<dyn Error>> {
    let temp = TempDirectory::new("hardening-cleanup")?;
    let inside = temp.path().join("inside.bin");
    std::fs::write(&inside, b"x")?;
    validate_cleanup_paths(temp.path(), std::slice::from_ref(&inside))?;

    let outside = temp
        .path()
        .parent()
        .ok_or("missing parent")?
        .join("outside.bin");
    assert!(validate_cleanup_paths(temp.path(), &[outside]).is_err());
    Ok(())
}

fn stream_with_segments(count: usize) -> Stream {
    Stream {
        playlist: Some(Playlist {
            media_parts: vec![MediaPart {
                media_segments: (0..count)
                    .map(|index| MediaSegment {
                        index: index as i64,
                        ..MediaSegment::default()
                    })
                    .collect(),
            }],
            ..Playlist::default()
        }),
        ..Stream::default()
    }
}
