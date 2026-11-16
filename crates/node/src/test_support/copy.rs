//! A copy of a log directory a running node may still write to.
//!
//! A node's sequencer stops before its log-sync thread has always finished: a segment's staging
//! file (`<first seq>.jsonl.tmp`) is created at a rotation and linked and removed by that thread, so a
//! plain `read_dir` and `fs::copy` can meet a file that is gone or a segment that is still growing.
//! `copy_dir` copies only a state the directory held unchanged across the copy.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const ATTEMPTS: u32 = 100;
const PAUSE: Duration = Duration::from_millis(50);

/// Every file under a directory, by path relative to it: its length and, for the source, its
/// modification time.
type Files = Vec<(PathBuf, u64, Option<SystemTime>)>;

/// Copies the directory tree `from` into `to`, over what `to` holds, as `from` stood at one moment:
/// the copy is kept only if `from`'s files, their lengths and modification times are the same before
/// and after it and `to` holds each file at that length, so a file that vanished or a segment that
/// grew while it ran retries the whole copy (the files that try wrote are removed first), and a torn
/// copy is never returned.
/// Panics if `from` does not hold still for `ATTEMPTS` tries.
pub fn copy_dir(from: &Path, to: &Path) {
    for _ in 0..ATTEMPTS {
        if let Ok(before) = snapshot(from, from) {
            let (written, copied) = copy_files(from, to, &before);
            let held = snapshot(from, from).is_ok_and(|after| after == before);
            let whole = before.iter().all(|(path, len, _)| fs::metadata(to.join(path)).is_ok_and(|m| m.len() == *len));
            if copied.is_ok() && held && whole {
                return;
            }
            for path in written {
                let _ = fs::remove_file(path);
            }
        }
        std::thread::sleep(PAUSE);
    }
    panic!("{} did not hold still long enough to copy", from.display());
}

/// Copies `files`, listed under `from`, into `to`; the paths it wrote, and whether all were copied.
fn copy_files(from: &Path, to: &Path, files: &Files) -> (Vec<PathBuf>, io::Result<()>) {
    let mut written = Vec::new();
    let copied = (|| {
        fs::create_dir_all(to)?;
        for (path, _, _) in files {
            let target = to.join(path);
            fs::create_dir_all(target.parent().expect("under `to`"))?;
            written.push(target.clone());
            fs::copy(from.join(path), target)?;
        }
        Ok(())
    })();
    (written, copied)
}

fn snapshot(root: &Path, dir: &Path) -> io::Result<Files> {
    let mut files = Files::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            files.extend(snapshot(root, &entry.path())?);
        } else {
            let meta = entry.metadata()?;
            let relative = entry.path().strip_prefix(root).expect("under the root").to_owned();
            files.push((relative, meta.len(), meta.modified().ok()));
        }
    }
    files.sort();
    Ok(files)
}
