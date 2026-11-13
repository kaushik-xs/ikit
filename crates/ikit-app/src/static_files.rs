//! Packing a static route's folder for the host: a tar whose bytes depend only on the files'
//! paths and contents (sorted entries, fixed modes, owners and times), so its hash says
//! whether the host already has these files.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{AppError, Result};

/// A folder packed for upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Packed {
    /// Hex SHA-256 of `tar`, shortened to 16 characters: the upload's folder name.
    pub hash: String,
    pub tar: Vec<u8>,
    pub files: usize,
}

/// Packs at most this much; larger sites belong in a container or object storage.
const MAX_BYTES: u64 = 1024 * 1024 * 1024;

/// Packs every file under `dir`, following symlinks. Fails if `dir` is missing, holds no
/// files, or is larger than 1 GiB.
pub(crate) fn pack(dir: &Path) -> Result<Packed> {
    if !dir.is_dir() {
        return Err(AppError::Invalid(format!(
            "the static folder {} does not exist on this machine",
            dir.display()
        )));
    }
    let mut entries = Vec::new();
    walk(dir, PathBuf::new(), &mut entries)?;
    entries.sort();
    let mut builder = tar::Builder::new(Vec::new());
    let mut total = 0u64;
    let mut files = 0usize;
    for (rel, is_dir) in &entries {
        let mut header = tar::Header::new_gnu();
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        let io = |e: std::io::Error| AppError::Io(e);
        if *is_dir {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            builder
                .append_data(&mut header, rel, std::io::empty())
                .map_err(io)?;
        } else {
            let data = std::fs::read(dir.join(rel)).map_err(io)?;
            total += data.len() as u64;
            if total > MAX_BYTES {
                return Err(AppError::Invalid(format!(
                    "the static folder {} is larger than 1 GiB",
                    dir.display()
                )));
            }
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_size(data.len() as u64);
            builder
                .append_data(&mut header, rel, data.as_slice())
                .map_err(io)?;
            files += 1;
        }
    }
    if files == 0 {
        return Err(AppError::Invalid(format!(
            "the static folder {} has no files",
            dir.display()
        )));
    }
    let tar = builder.into_inner().map_err(AppError::Io)?;
    let digest = Sha256::digest(&tar);
    let hash = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    Ok(Packed { hash, tar, files })
}

/// Collects `(relative path, is folder)` under `root/rel`.
fn walk(root: &Path, rel: PathBuf, out: &mut Vec<(PathBuf, bool)>) -> Result<()> {
    let read = std::fs::read_dir(root.join(&rel)).map_err(AppError::Io)?;
    for entry in read {
        let entry = entry.map_err(AppError::Io)?;
        let path = rel.join(entry.file_name());
        // Follows symlinks; a dangling one is skipped.
        let Ok(meta) = std::fs::metadata(entry.path()) else {
            continue;
        };
        if meta.is_dir() {
            out.push((path.clone(), true));
            walk(root, path, out)?;
        } else if meta.is_file() {
            out.push((path, false));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("index.html"), "hi").unwrap();
        std::fs::write(dir.path().join("assets/a.css"), "x").unwrap();
        let a = pack(dir.path()).unwrap();
        assert_eq!(a.files, 2);
        assert_eq!(a.hash.len(), 16);
        // Touching a file does not change the hash; changing it does.
        std::fs::write(dir.path().join("index.html"), "hi").unwrap();
        assert_eq!(pack(dir.path()).unwrap().hash, a.hash);
        std::fs::write(dir.path().join("index.html"), "hello").unwrap();
        assert_ne!(pack(dir.path()).unwrap().hash, a.hash);

        let mut archive = tar::Archive::new(a.tar.as_slice());
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().display().to_string())
            .collect();
        assert_eq!(names, ["assets", "assets/a.css", "index.html"]);
    }

    #[test]
    fn refuses_missing_and_empty_folders() {
        let dir = tempfile::tempdir().unwrap();
        assert!(pack(&dir.path().join("nope")).is_err());
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        assert!(pack(&dir.path().join("empty")).is_err());
    }
}
