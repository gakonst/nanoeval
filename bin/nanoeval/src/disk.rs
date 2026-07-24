use std::{fs, io, path::Path};

#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};

pub(crate) fn reflink_or_sparse_copy(source: &Path, destination: &Path) -> io::Result<u64> {
    match reflink_copy::reflink(source, destination) {
        Ok(()) => return Ok(fs::metadata(destination)?.len()),
        Err(_) => remove_partial_copy(destination)?,
    }

    #[cfg(target_os = "linux")]
    {
        let status = Command::new("cp")
            .args(["--reflink=never", "--sparse=always", "--"])
            .arg(source)
            .arg(destination)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if status.success() {
            return Ok(fs::metadata(destination)?.len());
        }
        remove_partial_copy(destination)?;
        return Err(io::Error::other(format!(
            "sparse disk copy failed with {status}"
        )));
    }

    #[cfg(not(target_os = "linux"))]
    fs::copy(source, destination)
}

fn remove_partial_copy(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{
        fs::{self, File},
        io::{Seek, SeekFrom, Write},
        os::unix::fs::MetadataExt,
    };

    use super::reflink_or_sparse_copy;

    #[test]
    fn fallback_preserves_sparse_disk_allocation() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.ext4");
        let destination = directory.path().join("destination.ext4");
        let mut file = File::create(&source).unwrap();
        file.seek(SeekFrom::Start(64 * 1024 * 1024)).unwrap();
        file.write_all(b"disk-tail").unwrap();
        drop(file);

        reflink_or_sparse_copy(&source, &destination).unwrap();

        let source_metadata = fs::metadata(&source).unwrap();
        let destination_metadata = fs::metadata(&destination).unwrap();
        assert_eq!(destination_metadata.len(), source_metadata.len());
        assert!(destination_metadata.blocks().saturating_mul(512) < destination_metadata.len() / 4);
    }
}
