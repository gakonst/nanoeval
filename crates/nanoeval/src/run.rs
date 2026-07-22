use std::{
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::EvalError;

/// Stable metadata and native storage for one reusable evaluation run.
#[derive(Clone, Debug)]
pub struct EvalRun {
    id: Uuid,
    started_at: DateTime<Utc>,
    directory: PathBuf,
    parent_directory: PathBuf,
    max_concurrency: usize,
}

impl EvalRun {
    pub(crate) fn create(
        parent_directory: &Path,
        max_concurrency: usize,
    ) -> Result<Self, EvalError> {
        fs::create_dir_all(parent_directory)?;
        let parent_directory = fs::canonicalize(parent_directory)?;
        let id = Uuid::new_v4();
        let directory = parent_directory.join(id.to_string());
        fs::create_dir_all(&directory)?;
        Ok(Self {
            id,
            started_at: Utc::now(),
            directory,
            parent_directory,
            max_concurrency,
        })
    }

    #[must_use]
    pub const fn id(&self) -> Uuid {
        self.id
    }

    #[must_use]
    pub const fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    #[must_use]
    pub fn parent_directory(&self) -> &Path {
        &self.parent_directory
    }

    #[must_use]
    pub const fn max_concurrency(&self) -> usize {
        self.max_concurrency
    }
}
