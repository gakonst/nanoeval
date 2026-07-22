use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use sha2::{Digest, Sha256};

use crate::HarborError;

const PACKAGE_FILES: [&str; 3] = ["task.toml", "instruction.md", "README.md"];
const PACKAGE_DIRECTORIES: [&str; 4] = ["environment", "tests", "solution", "steps"];

/// Matches `dirhash(directory, "sha256")`, which Harbor currently records as
/// `TrialResult.task_checksum`.
pub(crate) fn directory_hash(root: &Path) -> Result<String, HarborError> {
    let mut ancestors = HashSet::new();
    hash_directory(root, &mut ancestors, &|path| fs::canonicalize(path))?
        .ok_or_else(|| HarborError::EmptyTask(root.to_path_buf()))
}

/// Matches Harbor's `Packager.compute_content_hash`, which is recorded with a
/// `sha256:` prefix in job and trial locks.
pub(crate) fn package_content_hash(root: &Path) -> Result<String, HarborError> {
    let matcher = package_ignore_matcher(root)?;
    let mut files = Vec::new();
    for name in PACKAGE_FILES {
        let path = root.join(name);
        if path.is_file() && !is_ignored(root, &path, matcher.as_ref()) {
            files.push(path);
        }
    }
    for name in PACKAGE_DIRECTORIES {
        let directory = root.join(name);
        if directory.is_dir() {
            collect_package_files(root, &directory, matcher.as_ref(), &mut files)?;
        }
    }
    files.sort_by_key(|path| relative_name(root, path));

    let mut outer = Sha256::new();
    for path in files {
        let relative = relative_name(root, &path);
        let file_hash = hex_digest(&fs::read(&path)?);
        outer.update(relative.as_bytes());
        outer.update([0]);
        outer.update(file_hash.as_bytes());
        outer.update(b"\n");
    }
    Ok(format!("{:x}", outer.finalize()))
}

fn hash_directory(
    directory: &Path,
    ancestors: &mut HashSet<PathBuf>,
    canonicalize: &impl Fn(&Path) -> std::io::Result<PathBuf>,
) -> Result<Option<String>, HarborError> {
    let canonical = canonicalize(directory)?;
    if !ancestors.insert(canonical.clone()) {
        return Err(HarborError::CyclicTaskDirectory(directory.to_path_buf()));
    }

    let result = (|| {
        let mut descriptors = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::metadata(&path)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if metadata.is_dir() {
                if let Some(hash) = hash_directory(&path, ancestors, canonicalize)? {
                    descriptors.push(format!("dirhash:{hash}\0name:{name}"));
                }
            } else if metadata.is_file() {
                let hash = hex_digest(&fs::read(path)?);
                descriptors.push(format!("data:{hash}\0name:{name}"));
            }
        }
        if descriptors.is_empty() {
            return Ok(None);
        }
        descriptors.sort();
        Ok(Some(hex_digest(descriptors.join("\0\0").as_bytes())))
    })();

    let removed = ancestors.remove(&canonical);
    debug_assert!(removed);
    result
}

fn collect_package_files(
    root: &Path,
    directory: &Path,
    matcher: Option<&Gitignore>,
    files: &mut Vec<PathBuf>,
) -> Result<(), HarborError> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if is_ignored(root, &path, matcher) {
            continue;
        }
        if file_type.is_dir() {
            collect_package_files(root, &path, matcher, files)?;
        } else if file_type.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn package_ignore_matcher(root: &Path) -> Result<Option<Gitignore>, HarborError> {
    let path = root.join(".gitignore");
    if !path.is_file() {
        return Ok(None);
    }
    let mut builder = GitignoreBuilder::new(root);
    builder.add(path);
    Ok(Some(builder.build()?))
}

fn is_ignored(root: &Path, path: &Path, matcher: Option<&Gitignore>) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    if let Some(matcher) = matcher {
        return matcher
            .matched_path_or_any_parents(relative, path.is_dir())
            .is_ignore();
    }

    relative.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        name == "__pycache__"
            || name == ".DS_Store"
            || name.ends_with(".pyc")
            || name.ends_with(".swp")
            || name.ends_with(".swo")
            || name.ends_with('~')
    })
}

fn relative_name(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{directory_hash, package_content_hash};

    #[test]
    fn matches_harbor_hashes_for_the_fixture() {
        let task = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tasks/write-greeting");

        assert_eq!(
            directory_hash(&task).unwrap(),
            "eaa13434b21464b5a55c6a61b660c89fee3364084233f48311d29c143722390f"
        );
        assert_eq!(
            package_content_hash(&task).unwrap(),
            "e1a05661b2068b6f93e0874941d1fc930604d5c58965eacbc5cc4b4a95882d59"
        );
    }

    #[cfg(unix)]
    #[test]
    fn removes_original_ancestor_when_directory_resolves_differently() {
        use std::{cell::Cell, collections::HashSet, fs, os::unix::fs::symlink};

        use tempfile::tempdir;

        use super::hash_directory;

        let workspace = tempdir().unwrap();
        let original = workspace.path().join("original");
        let replacement = workspace.path().join("replacement");
        let task = workspace.path().join("task");
        fs::create_dir(&original).unwrap();
        fs::create_dir(&replacement).unwrap();
        fs::write(replacement.join("selected.txt"), b"replacement").unwrap();
        symlink(&original, &task).unwrap();
        let expected_hash = directory_hash(&replacement).unwrap();

        let switched = Cell::new(false);
        let canonicalize = |path: &Path| {
            let canonical = fs::canonicalize(path)?;
            if path == task && !switched.replace(true) {
                fs::remove_file(&task)?;
                symlink(&replacement, &task)?;
            }
            Ok(canonical)
        };
        let mut ancestors = HashSet::new();

        let hash = hash_directory(&task, &mut ancestors, &canonicalize).unwrap();

        assert_eq!(hash.as_deref(), Some(expected_hash.as_str()));
        assert!(ancestors.is_empty());
    }
}
