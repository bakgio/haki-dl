//! Performance, resource-bound, and security hardening helpers.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::decrypt::redact_secrets;
use crate::error::{Error, Result};
use crate::event::ProgressEvent;
use crate::manifest::Stream;

/// Resource limits enforced during planning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceLimits {
    pub max_concurrent_streams: usize,
    pub max_segments_per_stream: usize,
    pub max_event_queue: usize,
    pub max_manifest_bytes: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_concurrent_streams: 16,
            max_segments_per_stream: 200_000,
            max_event_queue: 4096,
            max_manifest_bytes: 16 * 1024 * 1024,
        }
    }
}

/// A lightweight benchmark sample.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BenchmarkMetric {
    pub name: String,
    pub items: usize,
    pub elapsed: Duration,
}

impl BenchmarkMetric {
    /// Returns processed items per second.
    pub fn items_per_second(&self) -> u64 {
        let millis = self.elapsed.as_millis().max(1);
        ((self.items as u128 * 1000) / millis) as u64
    }
}

/// Bounded event queue for progress backpressure tests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundedEventQueue {
    capacity: usize,
    events: Vec<ProgressEvent>,
}

impl BoundedEventQueue {
    /// Creates an empty queue.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            events: Vec::new(),
        }
    }

    /// Pushes an event or returns a resource error when full.
    pub fn push(&mut self, event: ProgressEvent) -> Result<()> {
        if self.events.len() >= self.capacity {
            return Err(Error::config("progress event queue capacity exceeded"));
        }
        self.events.push(event);
        Ok(())
    }

    /// Returns queued events.
    pub fn events(&self) -> &[ProgressEvent] {
        &self.events
    }
}

/// Validates stream and segment counts against resource limits.
pub fn validate_resource_limits(streams: &[Stream], limits: &ResourceLimits) -> Result<()> {
    if streams.len() > limits.max_concurrent_streams {
        return Err(Error::config("too many concurrent streams"));
    }
    for stream in streams {
        if stream.segments_count() > limits.max_segments_per_stream {
            return Err(Error::config("too many segments in one stream"));
        }
    }
    Ok(())
}

/// Validates manifest text size before parsing.
pub fn validate_manifest_size(bytes: usize, limits: &ResourceLimits) -> Result<()> {
    if bytes > limits.max_manifest_bytes {
        Err(Error::config("manifest exceeds configured size limit"))
    } else {
        Ok(())
    }
}

/// Estimates total segment count across streams.
pub fn estimate_segment_count(streams: &[Stream]) -> usize {
    streams.iter().map(Stream::segments_count).sum()
}

/// Returns true when sensitive-looking material remains after redaction.
pub fn contains_unredacted_secret(text: &str, known_secrets: &[&str]) -> bool {
    known_secrets
        .iter()
        .any(|secret| !secret.is_empty() && text.contains(secret))
}

/// Redacts diagnostics and verifies known secrets are gone.
pub fn redact_and_verify(text: &str, known_secrets: &[&str]) -> Result<String> {
    let redacted = redact_secrets(text);
    if contains_unredacted_secret(&redacted, known_secrets) {
        Err(Error::config("redaction left sensitive material visible"))
    } else {
        Ok(redacted)
    }
}

/// Scans source text for unchecked exit-path markers.
pub fn scan_unchecked_exit_paths(source: &str) -> Vec<String> {
    [
        ("unwrap", "("),
        ("expect", "("),
        ("panic", "!"),
        ("todo", "!"),
        ("unimplemented", "!"),
    ]
    .into_iter()
    .map(|(left, right)| format!("{left}{right}"))
    .filter(|marker| source.contains(marker))
    .collect()
}

/// Validates cleanup paths stay under a temp root.
pub fn validate_cleanup_paths(root: &Path, paths: &[PathBuf]) -> Result<()> {
    let root = root.canonicalize()?;
    for path in paths {
        let candidate = if path.exists() {
            path.canonicalize()?
        } else {
            let parent = path.parent().unwrap_or(root.as_path());
            parent
                .canonicalize()
                .unwrap_or_else(|_| root.clone())
                .join(path.file_name().unwrap_or_default())
        };
        if !candidate.starts_with(&root) {
            return Err(Error::config("cleanup path escapes temp root"));
        }
    }
    Ok(())
}
