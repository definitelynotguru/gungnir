//! Typed errors for the gungnir library.

use crate::id::EntryId;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("frontmatter error in {path}: {source}")]
    Yaml {
        path: std::path::PathBuf,
        #[source]
        source: serde_yaml::Error,
    },

    #[error("entry not found: {0}")]
    NotFound(EntryId),

    #[error("duplicate entry id: {0}")]
    Duplicate(EntryId),

    #[error("invalid entry: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, Error>;
