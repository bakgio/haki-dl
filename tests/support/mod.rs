use std::io;
use std::path::Path;

use tempfile::{Builder, TempDir};

pub struct TempDirectory {
    dir: TempDir,
}

impl TempDirectory {
    pub fn new(prefix: &str) -> io::Result<Self> {
        Ok(Self {
            dir: Builder::new()
                .prefix(&format!("{}-", sanitize_prefix(prefix)))
                .tempdir()?,
        })
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

fn sanitize_prefix(prefix: &str) -> String {
    let sanitized = prefix
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "haki-dl".to_string()
    } else {
        sanitized
    }
}
