//! Backend selection policy.

/// Optional backend selected for an operation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BackendSelection {
    /// Choose the compatibility backend for the operation.
    #[default]
    CompatibilityDefault,
    /// Use an external ffmpeg process.
    Ffmpeg,
    /// Use an external mkvmerge process.
    Mkvmerge,
    /// Use the optional MP4 backend.
    Mp4forge,
}

/// Dependency and backend policy for request planning.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BackendPolicy {
    /// MP4 backend policy.
    pub mp4: Mp4BackendPolicy,
}

/// Policy for the optional MP4 backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mp4BackendPolicy {
    /// Whether explicit MP4 backend selection is allowed.
    pub enabled: bool,
    /// Published crate version or Git revision recorded by the build.
    pub source_version: Option<String>,
}

impl Default for Mp4BackendPolicy {
    fn default() -> Self {
        Self {
            enabled: mp4forge_feature_enabled(),
            source_version: None,
        }
    }
}

fn mp4forge_feature_enabled() -> bool {
    #[cfg(feature = "mp4forge")]
    {
        true
    }
    #[cfg(not(feature = "mp4forge"))]
    {
        false
    }
}
