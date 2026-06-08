//! Progress counters used by events and API callbacks.

/// Aggregate progress counters across a request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AggregateProgress {
    /// Downloaded bytes across all active streams.
    pub downloaded_bytes: u64,
    /// Expected total bytes when known.
    pub total_bytes: Option<u64>,
    /// Current aggregate bytes per second.
    pub bytes_per_second: u64,
}

/// Per-stream progress counters.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StreamProgress {
    /// Stable stream identifier within the session.
    pub stream_id: String,
    /// Downloaded bytes for this stream.
    pub downloaded_bytes: u64,
    /// Expected stream bytes when known.
    pub total_bytes: Option<u64>,
    /// Current stream bytes per second.
    pub bytes_per_second: u64,
    /// Number of consecutive low-speed ticks observed by the compatibility speed state.
    pub low_speed_count: u32,
    /// Completed segment count.
    pub completed_segments: u64,
    /// Expected segment count when known.
    pub total_segments: Option<u64>,
}

/// Per-segment progress counters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentProgress {
    /// Stable stream identifier within the session.
    pub stream_id: String,
    /// Segment index within the stream.
    pub segment_index: u64,
    /// Downloaded segment bytes.
    pub downloaded_bytes: u64,
    /// Expected segment bytes when known.
    pub total_bytes: Option<u64>,
    /// Current stream bytes per second.
    pub bytes_per_second: u64,
    /// Number of consecutive low-speed ticks observed by the compatibility speed state.
    pub low_speed_count: u32,
    /// Current retry attempt.
    pub retry_attempt: u32,
}
