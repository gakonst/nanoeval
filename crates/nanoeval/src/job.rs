use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{EvalError, sweep::RunManifest};

const RUN_FILE: &str = "run.json";

/// Stable metadata and native storage for one reusable evaluator.
#[derive(Clone, Debug)]
pub(crate) struct EvalJob {
    id: Uuid,
    started_at: DateTime<Utc>,
    directory: PathBuf,
    parent_directory: PathBuf,
}

impl EvalJob {
    pub(crate) fn create(parent_directory: &Path) -> Result<Self, EvalError> {
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

    pub fn bind_run(&self, run: &RunManifest) -> Result<(), EvalError> {
        let path = self.directory.join(RUN_FILE);
        let encoded = serde_json::to_vec_pretty(run)?;
        if path.exists() {
            return Self::verify_run(&path, run);
        }

        let mut temporary = tempfile::NamedTempFile::new_in(&self.directory)?;
        temporary.write_all(&encoded)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        match temporary.persist_noclobber(&path) {
            Ok(_) => Ok(()),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                Self::verify_run(&path, run)
            }
            Err(error) => Err(error.error.into()),
        }
    }

    fn verify_run(path: &Path, expected: &RunManifest) -> Result<(), EvalError> {
        let retained: RunManifest = serde_json::from_slice(&fs::read(path)?)?;
        if retained == *expected {
            Ok(())
        } else {
            Err(EvalError::RunConflict(path.to_path_buf()))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use nanocodex::Nanocodex;
    use tempfile::tempdir;

    use super::*;
    use crate::{Sweep, Task};

    #[test]
    fn atomically_binds_one_finite_run() {
        let output = tempdir().unwrap();
        let job = EvalJob::create(output.path()).unwrap();
        let first = sweep(1);
        let first_run = first.manifest();

        job.bind_run(&first_run).unwrap();
        job.bind_run(&first_run).unwrap();
        let retained: RunManifest =
            serde_json::from_slice(&fs::read(job.directory().join(RUN_FILE)).unwrap()).unwrap();
        assert_eq!(retained, first_run);

        let error = job.bind_run(&sweep(2).manifest()).unwrap_err();
        assert!(matches!(error, EvalError::RunConflict(_)));
    }

    fn sweep(trials: u16) -> Sweep {
        Sweep::builder()
            .task(
                Task::load(
                    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tasks/write-greeting"),
                )
                .unwrap(),
            )
            .trials(trials)
            .agent("test", Nanocodex::builder("test-key"))
            .unwrap()
            .build()
            .unwrap()
    }
}
