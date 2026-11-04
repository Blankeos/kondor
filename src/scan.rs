//! Parallel filesystem scan in two phases.
//!
//! 1. **Walk**: level-by-level breadth-first, so top-level projects are found
//!    before anything deep. Every folder is read exactly once. As soon as a
//!    project with at least one artifact folder is detected it is announced
//!    (`Msg::Discovered`), carrying cached sizes if we have any.
//! 2. **Size**: each announced project is queued on a separate pool,
//!    shallowest folder first (FIFO among equal depths) and measured while the walk carries on. The result
//!    arrives as `Msg::Sized`.
//!
//! Both phases are I/O bound, so both pools are oversubscribed.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashSet};
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::sync::atomic::{
    AtomicU64, AtomicUsize,
    Ordering::{AcqRel, Relaxed},
};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rayon::prelude::*;
use rayon::ThreadPoolBuilder;

use crate::cache::Cache;
use crate::project::{self, Kind};

#[derive(Default)]
pub struct Options {
    pub follow_links: bool,
    pub same_fs: bool,
    /// Folders never entered (user `--ignore` plus built-in system folders).
    pub skip: Vec<PathBuf>,
    /// Sizes from a previous run, shown until the fresh measurement lands.
    pub cache: Option<Arc<Cache>>,
    /// Threads for the walk and for sizing. 0 picks a default.
    pub walk_threads: usize,
    pub size_threads: usize,
}

/// Default `(walk, size)` thread counts. Benchmarked on an M-series Mac (14
/// cores, APFS, ~30 GB / 1M files): oversubscribing (2-4x cores) was slower
/// than fewer threads because the kernel's directory locks, not I/O latency,
/// are the bottleneck. About half the cores walk and 3/4 size, and the two
/// overlap, which gave the best time-to-everything-sized and a fast walk.
pub fn default_threads() -> (usize, usize) {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    ((cores / 2).clamp(4, 16), (cores * 3 / 4).clamp(4, 24))
}

#[derive(Clone, Debug)]
pub struct Artifact {
    /// Path relative to the project root, e.g. `node_modules`.
    pub rel: &'static str,
    pub bytes: u64,
    pub newest: Option<SystemTime>,
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
    /// A project with at least one artifact folder (verified to exist). Sizes
    /// are zero unless `cached`, in which case they're last run's numbers.
    Discovered { found: Found, cached: bool },
    /// Fresh measurement. No artifacts means they vanished or can't be trusted.
    Sized(Found),
    /// The folder walk is over; sizing may still be running.
    WalkDone { elapsed: Duration },
    /// Everything is measured.
    Done { elapsed: Duration },
}

#[derive(Default)]
pub struct Progress {
    pub dirs: AtomicU64,
}

struct Ctx<'a> {
    opts: &'a Options,
    queue: &'a Queue,
    tx: &'a Sender<Msg>,
    progress: &'a Progress,
    /// Only used with `follow_links`, to avoid symlink loops.
    seen: Mutex<HashSet<(u64, u64)>>,
}

struct Item {
    dir: PathBuf,
    root_dev: Option<u64>,
    in_project: bool,
}

/// Sizing work, handed out shallowest path first (first-in first-out among
/// equal depths): shallow projects finish first and a huge folder's deep
/// subfolders wait behind every shallower one.
/// (Rayon's own FIFO spawning is only FIFO per worker and lets a busy worker
/// starve freshly injected jobs, which is exactly the wrong thing here.)
struct Queue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

struct QueueState {
    jobs: BinaryHeap<Queued>,
    /// Push counter; breaks depth ties first-in first-out.
    seq: u64,
    /// Tasks currently running; they may still enqueue more.
    active: usize,
    /// No more tasks will arrive from outside (the walk is over).
    closed: bool,
}

enum Task {
    Begin { path: PathBuf, kinds: Vec<Kind>, rels: Vec<&'static str> },
    Dir { job: Arc<Sizing>, idx: usize, dir: PathBuf },
}

impl Task {
    /// Number of path components of the folder this task works on.
    fn depth(&self) -> usize {
        match self {
            Task::Begin { path, .. } => path.components().count(),
            Task::Dir { dir, .. } => dir.components().count(),
        }
    }
}

/// A task plus its ordering key. Compares on the key only: the max-heap pops
/// the smallest depth, then the smallest sequence number.
struct Queued {
    depth: usize,
    seq: u64,
    task: Task,
}

impl Queued {
    fn key(&self) -> (Reverse<usize>, Reverse<u64>) {
        (Reverse(self.depth), Reverse(self.seq))
    }
}

impl PartialEq for Queued {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}
impl Eq for Queued {}
impl PartialOrd for Queued {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Queued {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key().cmp(&other.key())
    }
}

impl Queue {
    fn new() -> Self {
        Self {
            state: Mutex::new(QueueState { jobs: BinaryHeap::new(), seq: 0, active: 0, closed: false }),
            ready: Condvar::new(),
        }
    }

    fn push(&self, task: Task) {
        let depth = task.depth();
        {
            let mut st = self.state.lock().unwrap();
            let seq = st.seq;
            st.seq += 1;
            st.jobs.push(Queued { depth, seq, task });
        }
        self.ready.notify_one();
    }

    fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.ready.notify_all();
    }

    /// Run tasks until the queue is closed and drained.
    fn work(&self, tx: &Sender<Msg>) {
        let mut st = self.state.lock().unwrap();
        loop {
            if let Some(Queued { task, .. }) = st.jobs.pop() {
                st.active += 1;
                drop(st);
                match task {
                    Task::Begin { path, kinds, rels } => begin_sizing(self, tx, path, kinds, rels),
                    Task::Dir { job, idx, dir } => size_dir(self, job, idx, dir),
                }
                st = self.state.lock().unwrap();
                st.active -= 1;
                if st.active == 0 && st.jobs.is_empty() && st.closed {
                    self.ready.notify_all();
                }
            } else if st.closed && st.active == 0 {
                return;
            } else {
                st = self.ready.wait(st).unwrap();
            }
        }
    }
}

/// Scan `roots` and stream results into `tx`. Blocks until done.
pub fn run(roots: &[PathBuf], opts: &Options, tx: &Sender<Msg>, progress: &Progress) {
    let start = Instant::now();
    let (walk_default, size_default) = default_threads();
    let walk_pool = ThreadPoolBuilder::new()
        .num_threads(if opts.walk_threads > 0 { opts.walk_threads } else { walk_default })
        .thread_name(|i| format!("kondor-walk-{i}"))
        .build()
        .expect("thread pool");
    let size_threads = if opts.size_threads > 0 { opts.size_threads } else { size_default };
    let queue = Queue::new();
    let ctx = Ctx { opts, queue: &queue, tx, progress, seen: Mutex::new(HashSet::new()) };
    let ctx = &ctx;
    let queue = &queue;

    // The scope returns once every queued sizing job has finished.
    std::thread::scope(|sc| {
        for i in 0..size_threads {
            let _ =
                std::thread::Builder::new().name(format!("kondor-size-{i}")).spawn_scoped(sc, move || queue.work(tx));
        }
        let frontier: Vec<Item> = roots
            .iter()
            .map(|r| {
                let root_dev = if opts.same_fs { fs::metadata(r).ok().and_then(|m| dev(&m)) } else { None };
                Item { dir: r.clone(), root_dev, in_project: false }
            })
            .collect();
        walk_pool.install(|| walk(ctx, frontier));
        let _ = tx.send(Msg::WalkDone { elapsed: start.elapsed() });
        queue.close();
    });
    let _ = tx.send(Msg::Done { elapsed: start.elapsed() });
}

/// Breadth-first: read a whole level in parallel, then move to the next.
fn walk(ctx: &Ctx, mut frontier: Vec<Item>) {
    while !frontier.is_empty() {
        frontier = frontier.par_iter().flat_map_iter(|item| visit(ctx, item)).collect();
    }
}

/// Read one folder; announce and queue it if it's a project. Returns the
/// subfolders to walk next.
fn visit(ctx: &Ctx, item: &Item) -> Vec<Item> {
    let dir = &item.dir;
    ctx.progress.dirs.fetch_add(1, Relaxed);
    let Ok(read) = fs::read_dir(dir) else { return Vec::new() };

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
    if item.in_project {
        kinds.retain(|k| !k.is_weak());
    }
    let is_project = !kinds.is_empty();

    // Don't walk into the folders we're about to measure (or their parents,
    // like `android/` for `android/build`): they'd be double counted.
    let mut skip_names: Vec<&'static str> = Vec::new();
    if is_project {
        let artifacts = project::artifacts_for(&kinds);
        skip_names.extend(artifacts.iter().map(|a| a.split('/').next().unwrap_or(a)));
        // Cheap existence check; symlinks and non-folders are refused here too.
        let existing: Vec<&'static str> = artifacts
            .iter()
            .copied()
            .filter(|rel| {
                let top = rel.split('/').next().unwrap_or(rel);
                dirs.iter().any(|d| d == top) && matches!(crate::clean::artifact_dir(dir, rel), Ok(Some(_)))
            })
            .collect();
        if !existing.is_empty() {
            announce(ctx, dir.clone(), kinds, existing);
        }
    }

    let mut next = Vec::new();
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
            if item.root_dev.is_some() && dev(&meta) != item.root_dev {
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
        next.push(Item { dir: child, root_dev: item.root_dev, in_project: item.in_project || is_project });
    }
    next
}

/// Phase one for a project: show it now (with cached sizes if any), and queue
/// the real measurement.
fn announce(ctx: &Ctx, path: PathBuf, kinds: Vec<Kind>, rels: Vec<&'static str>) {
    let (found, cached) = discovered(ctx.opts.cache.as_deref(), &path, &kinds, &rels);
    let _ = ctx.tx.send(Msg::Discovered { found, cached });
    ctx.queue.push(Task::Begin { path, kinds, rels });
}

/// The not-yet-measured view of a project. Cached only if every artifact folder
/// has a cache entry; otherwise sizes are left at zero.
fn discovered(cache: Option<&Cache>, path: &Path, kinds: &[Kind], rels: &[&'static str]) -> (Found, bool) {
    let hits: Option<Vec<Artifact>> = cache.and_then(|c| {
        rels.iter()
            .map(|&rel| {
                let e = c.get(&path.join(rel))?;
                Some(Artifact { rel, bytes: e.bytes, newest: e.newest.map(|n| UNIX_EPOCH + Duration::from_secs(n)) })
            })
            .collect()
    });
    let cached = hits.is_some();
    let mut artifacts =
        hits.unwrap_or_else(|| rels.iter().map(|&rel| Artifact { rel, bytes: 0, newest: None }).collect());
    artifacts.sort_by_key(|a| std::cmp::Reverse(a.bytes));
    let bytes = artifacts.iter().map(|a| a.bytes).sum();
    let newest = artifacts.iter().filter_map(|a| a.newest).max();
    (Found { path: path.to_path_buf(), kinds: kinds.to_vec(), artifacts, bytes, newest }, cached)
}

/// Folders that are never projects worth descending into.
const ALWAYS_SKIP: &[&str] = &["node_modules", "__pycache__"];

/// One project being measured. Sizing is a swarm of per-folder tasks on a FIFO
/// priority queue (shallowest first), so a monster `node_modules` queues its
/// deep subfolders behind every shallower folder instead of starving them.
struct Sizing {
    tx: Sender<Msg>,
    path: PathBuf,
    kinds: Vec<Kind>,
    /// One accumulator per artifact folder that passed the safety check.
    accs: Vec<(&'static str, Acc)>,
    /// Outstanding tasks; the one that drops it to zero reports.
    pending: AtomicUsize,
}

fn begin_sizing(q: &Queue, tx: &Sender<Msg>, path: PathBuf, kinds: Vec<Kind>, artifact_names: Vec<&'static str>) {
    let mut accs = Vec::new();
    for rel in artifact_names {
        // A symlinked `node_modules` (or parent, like `ios/`) is not ours to measure.
        let Ok(Some(meta)) = crate::clean::artifact_dir(&path, rel) else { continue };
        let acc = Acc::default();
        acc.add(&[meta]);
        accs.push((rel, acc));
    }
    let roots: Vec<PathBuf> = accs.iter().map(|(rel, _)| path.join(rel)).collect();
    // The extra 1 is this task itself.
    let job = Arc::new(Sizing { tx: tx.clone(), path, kinds, accs, pending: AtomicUsize::new(roots.len() + 1) });
    for (i, dir) in roots.into_iter().enumerate() {
        q.push(Task::Dir { job: Arc::clone(&job), idx: i, dir });
    }
    finish_task(&job);
}

/// Total one folder; each subfolder becomes its own queued task (no inline
/// descent, which would dive depth-first and defeat the ordering). Never
/// follows symlinks.
fn size_dir(q: &Queue, job: Arc<Sizing>, idx: usize, dir: PathBuf) {
    {
        let mut metas = Vec::new();
        let mut subdirs = Vec::new();
        if let Ok(read) = fs::read_dir(&dir) {
            for entry in read.flatten() {
                // DirEntry::metadata does not traverse symlinks.
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_dir() {
                    subdirs.push(entry.path());
                }
                metas.push(meta);
            }
        }
        job.accs[idx].1.add(&metas);
        for sub in subdirs {
            job.pending.fetch_add(1, Relaxed);
            q.push(Task::Dir { job: Arc::clone(&job), idx, dir: sub });
        }
    }
    finish_task(&job);
}

fn finish_task(job: &Sizing) {
    if job.pending.fetch_sub(1, AcqRel) != 1 {
        return;
    }
    let mut artifacts: Vec<Artifact> = job
        .accs
        .iter()
        .map(|(rel, acc)| {
            let newest = acc.newest.load(Relaxed);
            Artifact {
                rel,
                bytes: acc.bytes.load(Relaxed),
                newest: (newest > 0).then(|| UNIX_EPOCH + Duration::from_secs(newest)),
            }
        })
        .collect();
    artifacts.sort_by_key(|a| std::cmp::Reverse(a.bytes));
    let bytes = artifacts.iter().map(|a| a.bytes).sum();
    let newest = artifacts.iter().filter_map(|a| a.newest).max();
    let found = Found { path: job.path.clone(), kinds: job.kinds.clone(), artifacts, bytes, newest };
    let _ = job.tx.send(Msg::Sized(found));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn queue_pops_shallowest_first_fifo_on_ties() {
        let q = Queue::new();
        for p in ["/a/b/c", "/x/y", "/a/b/d", "/p/q", "/z"] {
            q.push(Task::Begin { path: PathBuf::from(p), kinds: vec![], rels: vec![] });
        }
        let mut st = q.state.lock().unwrap();
        let mut order = Vec::new();
        while let Some(Queued { task: Task::Begin { path, .. }, .. }) = st.jobs.pop() {
            order.push(path.to_string_lossy().into_owned());
        }
        assert_eq!(order, ["/z", "/x/y", "/p/q", "/a/b/c", "/a/b/d"]);
    }

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("kondor-scan-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Tmp(p.canonicalize().unwrap())
        }
        fn node_project(&self, rel: &str, payload: usize) -> PathBuf {
            let dir = self.0.join(rel);
            fs::create_dir_all(dir.join("node_modules/dep")).unwrap();
            fs::write(dir.join("package.json"), b"{}").unwrap();
            fs::write(dir.join("node_modules/dep/index.js"), vec![b'x'; payload]).unwrap();
            dir
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn scan(root: &Path, opts: &Options) -> Vec<Msg> {
        let (tx, rx) = mpsc::channel();
        run(&[root.to_path_buf()], opts, &tx, &Progress::default());
        drop(tx);
        rx.into_iter().collect()
    }

    #[test]
    fn shallow_projects_are_reported_before_deep_ones() {
        let tmp = Tmp::new("bfs");
        // Created deep-first, so any accidental depth-first order would show.
        let deep = tmp.node_project("a/b/c/d/deep", 10);
        let mid = tmp.node_project("a/b/mid", 10);
        let top = tmp.node_project("top", 10);

        let msgs = scan(&tmp.0, &Options::default());
        let found: Vec<&PathBuf> = msgs
            .iter()
            .filter_map(|m| match m {
                Msg::Discovered { found, .. } => Some(&found.path),
                _ => None,
            })
            .collect();
        assert_eq!(found, [&top, &mid, &deep]);

        // Everything announced is also sized, and the phases finish in order.
        let sized = msgs.iter().filter(|m| matches!(m, Msg::Sized(f) if !f.artifacts.is_empty())).count();
        assert_eq!(sized, 3);
        let walk_done = msgs.iter().position(|m| matches!(m, Msg::WalkDone { .. })).unwrap();
        let done = msgs.iter().position(|m| matches!(m, Msg::Done { .. })).unwrap();
        assert!(walk_done < done && done == msgs.len() - 1);
        assert!(msgs[..walk_done].iter().all(|m| !matches!(m, Msg::Done { .. })));
    }

    #[test]
    fn discovery_carries_cached_sizes_and_skips_missing_artifacts() {
        let tmp = Tmp::new("cached");
        let app = tmp.node_project("app", 10);
        // A node project that was never installed: nothing to show.
        fs::create_dir_all(tmp.0.join("bare")).unwrap();
        fs::write(tmp.0.join("bare/package.json"), b"{}").unwrap();

        let mut cache = Cache::default();
        cache.insert(
            app.join("node_modules"),
            crate::cache::Entry { bytes: 4242, newest: Some(1_700_000_000), scanned_at: 1 },
        );
        let opts = Options { cache: Some(Arc::new(cache)), ..Options::default() };
        let msgs = scan(&tmp.0, &opts);

        let discovered: Vec<_> = msgs
            .iter()
            .filter_map(|m| match m {
                Msg::Discovered { found, cached } => Some((found, *cached)),
                _ => None,
            })
            .collect();
        assert_eq!(discovered.len(), 1, "bare project has no artifact folder");
        let (found, cached) = discovered[0];
        assert!(cached);
        assert_eq!((found.path.as_path(), found.bytes), (app.as_path(), 4242));

        // The fresh measurement replaces the stale number.
        let Some(Msg::Sized(fresh)) = msgs.iter().find(|m| matches!(m, Msg::Sized(_))) else { panic!("no Sized") };
        assert_ne!(fresh.bytes, 4242);
        assert!(fresh.bytes > 0 && fresh.newest.is_some());
    }
}
