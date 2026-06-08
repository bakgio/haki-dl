//! Stable error type for the public API.

use std::fmt;

/// Result alias used by public APIs.
pub type Result<T> = std::result::Result<T, Error>;

/// Error categories returned by public APIs.
#[derive(Debug)]
pub enum Error {
    /// A manifest or media protocol rule failed.
    Protocol { message: String },
    /// An HTTP operation failed.
    Http { message: String },
    /// A filesystem or stream operation failed.
    Io(std::io::Error),
    /// A decryption operation failed.
    Decrypt { message: String },
    /// A mux or merge operation failed.
    Mux { message: String },
    /// A subtitle operation failed.
    Subtitle { message: String },
    /// A live-recording operation failed.
    Live { message: String },
    /// Configuration or request validation failed.
    Config { message: String },
    /// The caller cancelled the operation.
    UserCancelled,
    /// A compatibility profile rejected the requested behavior.
    Compatibility { message: String },
}

impl Error {
    /// Creates a protocol error.
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol {
            message: message.into(),
        }
    }

    /// Creates an HTTP error.
    pub fn http(message: impl Into<String>) -> Self {
        Self::Http {
            message: message.into(),
        }
    }

    /// Creates a decryption error.
    pub fn decrypt(message: impl Into<String>) -> Self {
        Self::Decrypt {
            message: message.into(),
        }
    }

    /// Creates a mux error.
    pub fn mux(message: impl Into<String>) -> Self {
        Self::Mux {
            message: message.into(),
        }
    }

    /// Creates a subtitle error.
    pub fn subtitle(message: impl Into<String>) -> Self {
        Self::Subtitle {
            message: message.into(),
        }
    }

    /// Creates a live-recording error.
    pub fn live(message: impl Into<String>) -> Self {
        Self::Live {
            message: message.into(),
        }
    }

    /// Creates a configuration error.
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config {
            message: message.into(),
        }
    }

    /// Creates a compatibility error.
    pub fn compatibility(message: impl Into<String>) -> Self {
        Self::Compatibility {
            message: message.into(),
        }
    }

    pub(crate) fn compatibility_message(&self) -> String {
        match self {
            Self::Protocol { message }
            | Self::Http { message }
            | Self::Decrypt { message }
            | Self::Mux { message }
            | Self::Subtitle { message }
            | Self::Live { message }
            | Self::Config { message }
            | Self::Compatibility { message } => message.clone(),
            Self::Io(error) => error.to_string(),
            Self::UserCancelled => "operation cancelled".to_string(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol { message } => write!(f, "protocol error: {message}"),
            Self::Http { message } => write!(f, "http error: {message}"),
            Self::Io(error) => write!(f, "io error: {error}"),
            Self::Decrypt { message } => write!(f, "decrypt error: {message}"),
            Self::Mux { message } => write!(f, "mux error: {message}"),
            Self::Subtitle { message } => write!(f, "subtitle error: {message}"),
            Self::Live { message } => write!(f, "live error: {message}"),
            Self::Config { message } => write!(f, "config error: {message}"),
            Self::UserCancelled => f.write_str("operation cancelled"),
            Self::Compatibility { message } => write!(f, "compatibility error: {message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
