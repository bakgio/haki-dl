//! Cooperative cancellation primitives.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Shared cancellation token for API callers and CLI signal handlers.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    /// Creates a new non-cancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks the token as cancelled.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Returns whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Returns an error when cancellation has been requested.
    pub fn check(&self) -> crate::Result<()> {
        if self.is_cancelled() {
            Err(crate::Error::UserCancelled)
        } else {
            Ok(())
        }
    }
}
