//! Deleting artifact folders. This is the only module that removes anything.

use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
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
    pub errors: Vec<String>,
}

/// Run the job on the thread pool and report back on `tx`.
pub fn spawn(job: Job, tx: Sender<Outcome>) {
    rayon::spawn(move || {
        let _ = tx.send(run(&job));
    });
}

pub fn run(job: &Job) -> Outcome {
    let mut freed = 0;
    let mut errors = Vec::new();
    for &(rel, bytes) in &job.artifacts {
        // Artifact names are compile-time constants; this is belt and braces.
        if rel.is_empty() || rel.starts_with('/') || rel.split('/').any(|c| c == ".." || c == ".") {
            errors.push(format!("{rel}: refused (unsafe path)"));
            continue;
        }
        let path = job.root.join(rel);
        match fs::symlink_metadata(&path) {
            // Only real folders. A symlink is left alone, never followed.
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                errors.push(format!("{rel}: no longer a folder, left alone"));
                continue;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => {
                errors.push(format!("{rel}: {e}"));
                continue;
            }
        }
        if job.dry_run {
            freed += bytes;
            continue;
        }
        match fs::remove_dir_all(&path) {
            Ok(()) => freed += bytes,
            Err(e) => errors.push(format!("{rel}: {e}")),
        }
    }
    Outcome { id: job.id, freed, errors }
}
