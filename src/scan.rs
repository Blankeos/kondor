//! Parallel filesystem scan.
//!
//! Every folder is read exactly once. When a folder is a project, its
//! artifact folders are measured on the thread pool while the walk carries on
//! into the project's source folders (to find nested projects, e.g. monorepo
//! packages). Results stream to the UI as soon as each project is measured.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rayon::prelude::*;

use crate::project::{self, Kind};

pub struct Options {
    pub follow_links: bool,
    pub same_fs: bool,
    /// Folders never entered (user `--ignore` plus built-in system folders).
    pub skip: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Artifact {
    /// Path relative to the project root, e.g. `node_modules`.
    pub rel: &'static str,
    pub bytes: u64,
}

#[derive(Clone, Debug)]
pub struct Found {
    pub path: PathBuf,
    pub kinds: Vec<Kind>,
    pub artifacts: Vec<Artifact>,
    pub bytes: u64,
    /// Most recent modification anywhere inside the artifact folders:
    /// effectively "last built / installed".
    pub newest: Option<SystemTime>,
}

pub enum Msg {
    Found(Found),
    Done { elapsed: Duration },
}

#[derive(Default)]
pub struct Progress {
    pub dirs: AtomicU64,
}

struct Ctx<'a> {
    opts: &'a Options,
    tx: &'a Sender<Msg>,
    progress: &'a Progress,
    /// Only used with `follow_links`, to avoid symlink loops.
    seen: Mutex<HashSet<(u64, u64)>>,
}

/// Scan `roots` and stream results into `tx`. Blocks until done.
pub fn run(roots: &[PathBuf], opts: &Options, tx: &Sender<Msg>, progress: &Progress) {
    let start = Instant::now();
    let ctx = Ctx { opts, tx, progress, seen: Mutex::new(HashSet::new()) };
    let ctx = &ctx;
    rayon::scope(|s| {
        for root in roots {
            let dev = if opts.same_fs { fs::metadata(root).ok().and_then(|m| dev(&m)) } else { None };
            s.spawn(move |s| visit(s, ctx, root.clone(), dev, false));
        }
    });
    let _ = tx.send(Msg::Done { elapsed: start.elapsed() });
}

fn visit<'s>(s: &rayon::Scope<'s>, ctx: &'s Ctx<'s>, dir: PathBuf, root_dev: Option<u64>, in_project: bool) {
    ctx.progress.dirs.fetch_add(1, Relaxed);
    let Ok(read) = fs::read_dir(&dir) else { return };

    let mut files: Vec<OsString> = Vec::new();
    let mut dirs: Vec<OsString> = Vec::new();
    for entry in read.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            dirs.push(entry.file_name());
        } else if ft.is_file() {
            files.push(entry.file_name());
        } else if ft.is_symlink() && ctx.opts.follow_links {
            match fs::metadata(entry.path()) {
                Ok(m) if m.is_dir() => dirs.push(entry.file_name()),
                Ok(m) if m.is_file() => files.push(entry.file_name()),
                _ => {}
            }
        }
    }

    let mut kinds = project::detect(files.iter().filter_map(|f| f.to_str()), |d| dirs.iter().any(|x| x == d));
    if in_project {
        kinds.retain(|k| !k.is_weak());
    }
    let is_project = !kinds.is_empty();

    // Don't walk into the folders we're about to measure (or their parents,
    // like `android/` for `android/build`): they'd be double counted.
    let mut skip_names: Vec<&'static str> = Vec::new();
    if is_project {
        let artifacts = project::artifacts_for(&kinds);
        skip_names.extend(artifacts.iter().map(|a| a.split('/').next().unwrap_or(a)));
        let path = dir.clone();
        s.spawn(move |_| {
            if let Some(found) = measure(path, kinds, &artifacts) {
                let _ = ctx.tx.send(Msg::Found(found));
            }
        });
    }

    for name in dirs {
        let lossy = name.to_string_lossy();
        if lossy.starts_with('.') || ALWAYS_SKIP.contains(&lossy.as_ref()) || skip_names.contains(&lossy.as_ref()) {
            continue;
        }
        if cfg!(target_os = "macos") && lossy.ends_with(".app") {
            continue;
        }
        let child = dir.join(&name);
        if ctx.opts.skip.contains(&child) {
            continue;
        }
        if ctx.opts.same_fs || ctx.opts.follow_links {
            let Ok(meta) = fs::metadata(&child) else { continue };
            if root_dev.is_some() && dev(&meta) != root_dev {
                continue;
            }
            if ctx.opts.follow_links {
                if let Some(id) = file_id(&meta) {
                    if !ctx.seen.lock().unwrap().insert(id) {
                        continue;
                    }
                }
            }
        }
        s.spawn(move |s| visit(s, ctx, child, root_dev, in_project || is_project));
    }
}

/// Folders that are never projects worth descending into.
const ALWAYS_SKIP: &[&str] = &["node_modules", "__pycache__"];

/// Measure a project's artifact folders. `None` if none of them exist.
pub fn measure(path: PathBuf, kinds: Vec<Kind>, artifact_names: &[&'static str]) -> Option<Found> {
    let mut artifacts = Vec::new();
    let mut newest = 0u64;
    for &rel in artifact_names {
        let dir = path.join(rel);
        // symlink_metadata: a symlinked `node_modules` is not ours to measure.
        let Ok(meta) = fs::symlink_metadata(&dir) else { continue };
        if !meta.is_dir() {
            continue;
        }
        let acc = Acc::default();
        acc.add(&[meta]);
        tree(&dir, &acc);
        newest = newest.max(acc.newest.load(Relaxed));
        artifacts.push(Artifact { rel, bytes: acc.bytes.load(Relaxed) });
    }
    if artifacts.is_empty() {
        return None;
    }
    artifacts.sort_by_key(|a| std::cmp::Reverse(a.bytes));
    let bytes = artifacts.iter().map(|a| a.bytes).sum();
    let newest = (newest > 0).then(|| UNIX_EPOCH + Duration::from_secs(newest));
    Some(Found { path, kinds, artifacts, bytes, newest })
}

#[derive(Default)]
struct Acc {
    bytes: AtomicU64,
    newest: AtomicU64,
    /// Hard-linked files (e.g. pnpm) are only counted once.
    links: Mutex<HashSet<(u64, u64)>>,
}

impl Acc {
    fn add(&self, metas: &[Metadata]) {
        let mut bytes = 0;
        let mut newest = 0;
        for m in metas {
            if !m.is_dir() && nlink(m) > 1 {
                if let Some(id) = file_id(m) {
                    if !self.links.lock().unwrap().insert(id) {
                        continue;
                    }
                }
            }
            bytes += disk_usage(m);
            if let Ok(t) = m.modified() {
                if let Ok(d) = t.duration_since(UNIX_EPOCH) {
                    newest = newest.max(d.as_secs());
                }
            }
        }
        self.bytes.fetch_add(bytes, Relaxed);
        self.newest.fetch_max(newest, Relaxed);
    }
}

/// Recursively total a folder. Never follows symlinks.
fn tree(dir: &Path, acc: &Acc) {
    let Ok(read) = fs::read_dir(dir) else { return };
    let mut metas = Vec::new();
    let mut subdirs = Vec::new();
    for entry in read.flatten() {
        // DirEntry::metadata does not traverse symlinks.
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            subdirs.push(entry.path());
        }
        metas.push(meta);
    }
    acc.add(&metas);
    match subdirs.len() {
        0 => {}
        1 => tree(&subdirs[0], acc),
        _ => subdirs.par_iter().for_each(|d| tree(d, acc)),
    }
}

#[cfg(unix)]
fn disk_usage(m: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    // Allocated blocks: what deleting actually frees.
    m.blocks() * 512
}
#[cfg(not(unix))]
fn disk_usage(m: &Metadata) -> u64 {
    m.len()
}

#[cfg(unix)]
fn file_id(m: &Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((m.dev(), m.ino()))
}
#[cfg(not(unix))]
fn file_id(_: &Metadata) -> Option<(u64, u64)> {
    None
}

#[cfg(unix)]
fn nlink(m: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.nlink()
}
#[cfg(not(unix))]
fn nlink(_: &Metadata) -> u64 {
    1
}

#[cfg(unix)]
fn dev(m: &Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(m.dev())
}
#[cfg(not(unix))]
fn dev(_: &Metadata) -> Option<u64> {
    None
}

/// System folders that are huge, never hold your projects, and can contain
/// app data that merely looks like a project. Skipped unless you scan inside
/// one explicitly.
pub fn builtin_skips(home: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = home {
        if cfg!(target_os = "macos") {
            out.push(home.join("Library"));
        }
        if cfg!(windows) {
            out.push(home.join("AppData"));
        }
    }
    if cfg!(target_os = "linux") {
        out.extend(["/proc", "/sys", "/dev", "/run"].map(PathBuf::from));
    }
    if cfg!(target_os = "macos") {
        out.extend(["/System", "/Volumes", "/private", "/dev"].map(PathBuf::from));
    }
    out
}
