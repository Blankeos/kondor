//! Deleting artifact folders. This is the only module that removes anything.

use std::fs::{self, Metadata};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

pub struct Job {
    pub id: usize,
    pub root: PathBuf,
    /// `(relative path, bytes)` as measured during the scan.
    pub artifacts: Vec<(&'static str, u64)>,
    pub dry_run: bool,
}

pub struct Outcome {
    pub id: usize,
    pub freed: u64,
    /// Artifacts that are gone now (deleted, or already missing).
    pub removed: Vec<&'static str>,
    pub errors: Vec<String>,
}

/// Run the job on the thread pool and report back on `tx`.
pub fn spawn(job: Job, tx: Sender<Outcome>) {
    rayon::spawn(move || {
        let _ = tx.send(run(&job));
    });
}

/// Look up `root/rel`, requiring every component below `root` to be a real
/// folder. A symlink anywhere on the way (say `linux/` in
/// `linux/flutter/ephemeral`) is refused, so nothing outside the project can
/// be measured or deleted through it. `Ok(None)` means it doesn't exist.
pub fn artifact_dir(root: &Path, rel: &str) -> Result<Option<Metadata>, String> {
    // Artifact names are compile-time constants; this is belt and braces.
    if rel.is_empty() || rel.starts_with('/') || rel.split('/').any(|c| c.is_empty() || c == ".." || c == ".") {
        return Err("refused (unsafe path)".into());
    }
    let mut path = root.to_path_buf();
    let mut meta = None;
    for part in rel.split('/') {
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => return Err(format!("{part} is a symlink, left alone")),
            Ok(m) if m.is_dir() => meta = Some(m),
            Ok(_) => return Err(format!("{part} is not a folder, left alone")),
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(meta)
}

pub fn run(job: &Job) -> Outcome {
    let mut freed = 0;
    let mut removed = Vec::new();
    let mut errors = Vec::new();
    for &(rel, bytes) in &job.artifacts {
        match artifact_dir(&job.root, rel) {
            Ok(Some(_)) => {}
            Ok(None) => {
                removed.push(rel);
                continue;
            }
            Err(e) => {
                errors.push(format!("{rel}: {e}"));
                continue;
            }
        }
        if job.dry_run {
            freed += bytes;
            removed.push(rel);
            continue;
        }
        match fs::remove_dir_all(job.root.join(rel)) {
            Ok(()) => {
                freed += bytes;
                removed.push(rel);
            }
            Err(e) => errors.push(format!("{rel}: {e}")),
        }
    }
    Outcome { id: job.id, freed, removed, errors }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A fresh folder under the system temp dir, removed on drop.
    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("kondor-test-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn never_deletes_through_a_symlinked_parent() {
        let tmp = Tmp::new("symlink-parent");
        let outside = tmp.0.join("outside");
        fs::create_dir_all(outside.join("flutter/ephemeral")).unwrap();
        fs::write(outside.join("flutter/ephemeral/keep"), b"x").unwrap();
        let project = tmp.0.join("app");
        fs::create_dir_all(project.join("build")).unwrap();
        symlink(&outside, project.join("linux")).unwrap();

        assert!(artifact_dir(&project, "linux/flutter/ephemeral").is_err());
        let job = Job {
            id: 0,
            root: project.clone(),
            artifacts: vec![("linux/flutter/ephemeral", 1), ("build", 1)],
            dry_run: false,
        };
        let out = run(&job);
        assert!(outside.join("flutter/ephemeral/keep").exists(), "symlink target must survive");
        assert!(!project.join("build").exists());
        assert_eq!(out.removed, vec!["build"]);
        assert_eq!(out.errors.len(), 1);
    }

    #[test]
    fn dry_run_deletes_nothing() {
        let tmp = Tmp::new("dry-run");
        fs::create_dir_all(tmp.0.join("target/debug")).unwrap();
        let job = Job { id: 0, root: tmp.0.clone(), artifacts: vec![("target", 5)], dry_run: true };
        let out = run(&job);
        assert!(tmp.0.join("target/debug").exists());
        assert_eq!((out.freed, out.errors.len()), (5, 0));
    }
}
