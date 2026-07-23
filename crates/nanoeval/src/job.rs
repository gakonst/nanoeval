use std::{
    fs::{self, File, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::{DateTime, Utc};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{EvalError, sweep::RunManifest};

const JOB_FILE: &str = "job.json";
const LOCK_FILE: &str = ".nanoeval.lock";
const RUN_FILE: &str = "run.json";

/// Stable metadata and native storage for one reusable evaluator.
#[derive(Clone, Debug)]
pub(crate) struct EvalJob {
    id: Uuid,
    started_at: DateTime<Utc>,
    directory: PathBuf,
    parent_directory: PathBuf,
    resumed: bool,
    _lease: Arc<JobLease>,
}

#[derive(Debug, Deserialize, Serialize)]
struct JobIdentity {
    id: Uuid,
    started_at: DateTime<Utc>,
}

#[derive(Debug)]
struct JobLease {
    _file: File,
}

impl EvalJob {
    pub(crate) fn create(parent_directory: &Path) -> Result<Self, EvalError> {
        fs::create_dir_all(parent_directory)?;
        let parent_directory = fs::canonicalize(parent_directory)?;
        let id = Uuid::new_v4();
        let directory = parent_directory.join(id.to_string());
        fs::create_dir_all(&directory)?;
        let lease = Self::lease(&directory)?;
        let started_at = Utc::now();
        Self::write_json(&directory, JOB_FILE, &JobIdentity { id, started_at })?;
        Ok(Self {
            id,
            started_at,
            directory,
            parent_directory,
            resumed: false,
            _lease: Arc::new(lease),
        })
    }

    pub(crate) fn resume_or_create(
        parent_directory: &Path,
        run: &RunManifest,
    ) -> Result<Self, EvalError> {
        fs::create_dir_all(parent_directory)?;
        let parent_directory = fs::canonicalize(parent_directory)?;
        let mut candidates = fs::read_dir(&parent_directory)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .filter_map(|directory| {
                let identity = Self::read_json::<JobIdentity>(&directory.join(JOB_FILE)).ok()?;
                let retained = Self::read_json::<RunManifest>(&directory.join(RUN_FILE)).ok()?;
                let completed = Self::completed_trial_count(&directory).ok()?;
                (retained == *run && completed < run.attempt_count()).then_some((
                    identity.started_at,
                    identity,
                    directory,
                ))
            })
            .collect::<Vec<_>>();
        candidates.sort_unstable_by_key(|(started_at, _, _)| *started_at);

        let Some((_, identity, directory)) = candidates.pop() else {
            return Self::create(&parent_directory);
        };
        let lease = Self::lease(&directory).map_err(|error| match error {
            EvalError::Io(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                EvalError::RunActive(directory.clone())
            }
            other => other,
        })?;
        Ok(Self {
            id: identity.id,
            started_at: identity.started_at,
            directory,
            parent_directory,
            resumed: true,
            _lease: Arc::new(lease),
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
    pub const fn resumed(&self) -> bool {
        self.resumed
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

    pub fn completed_attempt(&self, trial_prefix: &str) -> Result<bool, EvalError> {
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name
                .strip_prefix(trial_prefix)
                .is_some_and(|suffix| suffix.starts_with("__"))
                && entry.path().join("result.json").is_file()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn verify_run(path: &Path, expected: &RunManifest) -> Result<(), EvalError> {
        let retained: RunManifest = serde_json::from_slice(&fs::read(path)?)?;
        if retained == *expected {
            Ok(())
        } else {
            Err(EvalError::RunConflict(path.to_path_buf()))
        }
    }

    fn completed_trial_count(directory: &Path) -> Result<usize, EvalError> {
        let mut completed = 0;
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() && entry.path().join("result.json").is_file() {
                completed += 1;
            }
        }
        Ok(completed)
    }

    fn lease(directory: &Path) -> Result<JobLease, EvalError> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(directory.join(LOCK_FILE))?;
        file.try_lock_exclusive()?;
        Ok(JobLease { _file: file })
    }

    fn read_json<T>(path: &Path) -> Result<T, EvalError>
    where
        T: for<'de> Deserialize<'de>,
    {
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }

    fn write_json(
        directory: &Path,
        filename: &str,
        value: &impl Serialize,
    ) -> Result<(), EvalError> {
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer_pretty(&mut temporary, value)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        temporary
            .persist_noclobber(directory.join(filename))
            .map_err(|error| error.error)?;
        Ok(())
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
        assert!(!job.resumed());
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

    #[test]
    fn resumes_the_latest_matching_incomplete_job() {
        let output = tempdir().unwrap();
        let run = sweep(2).manifest();
        let first = EvalJob::resume_or_create(output.path(), &run).unwrap();
        first.bind_run(&run).unwrap();
        let first_id = first.id();
        drop(first);

        let resumed = EvalJob::resume_or_create(output.path(), &run).unwrap();
        assert!(resumed.resumed());
        assert_eq!(resumed.id(), first_id);
    }

    #[test]
    fn refuses_to_open_an_incomplete_job_that_is_still_active() {
        let output = tempdir().unwrap();
        let run = sweep(2).manifest();
        let active = EvalJob::resume_or_create(output.path(), &run).unwrap();
        active.bind_run(&run).unwrap();

        let error = EvalJob::resume_or_create(output.path(), &run).unwrap_err();
        assert!(
            matches!(error, EvalError::RunActive(directory) if directory == active.directory())
        );
    }

    #[test]
    fn recognizes_only_a_durable_terminal_trial_for_a_coordinate() {
        let output = tempdir().unwrap();
        let job = EvalJob::create(output.path()).unwrap();
        let abandoned = job.directory().join("task__agent__001__abandoned");
        fs::create_dir_all(&abandoned).unwrap();
        assert!(!job.completed_attempt("task__agent__001").unwrap());

        fs::write(abandoned.join("result.json"), "{}").unwrap();
        assert!(job.completed_attempt("task__agent__001").unwrap());
        assert!(!job.completed_attempt("task__agent__002").unwrap());
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
