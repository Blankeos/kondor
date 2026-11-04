//! Persistent size cache, so a second launch can show sizes immediately and
//! re-measure in the background (stale-while-revalidate).
//!
//! Plain text, one artifact folder per line, atomic writes:
//!
//! ```text
//! kondor-cache 1
//! <bytes>\t<newest mtime secs or ->\t<scanned_at secs>\t<absolute path>
//! ```
//!
//! Anything unreadable is ignored: a broken cache is just an empty cache.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::scan::Found;

const HEADER: &str = "kondor-cache 1";
const FILE: &str = "sizes.txt";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub bytes: u64,
    /// Newest modification inside the folder (unix seconds).
    pub newest: Option<u64>,
    /// When it was measured (unix seconds).
    pub scanned_at: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Cache {
    map: HashMap<PathBuf, Entry>,
}

pub fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `$XDG_CACHE_HOME/kondor/`, else the platform default.
pub fn default_dir(home: Option<&Path>) -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(x).join("kondor"));
    }
    if cfg!(windows) {
        if let Some(l) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(l).join("kondor"));
        }
    }
    let home = home?;
    Some(if cfg!(target_os = "macos") { home.join("Library/Caches/kondor") } else { home.join(".cache/kondor") })
}

pub fn default_path(home: Option<&Path>) -> Option<PathBuf> {
    default_dir(home).map(|d| d.join(FILE))
}

impl Cache {
    /// Read the cache file. Missing, unreadable or corrupt gives an empty cache.
    pub fn load(path: &Path) -> Self {
        fs::read(path).map(|b| Self::parse(&String::from_utf8_lossy(&b))).unwrap_or_default()
    }

    /// Parse leniently: a wrong header drops everything, a bad line drops that line.
    pub fn parse(text: &str) -> Self {
        let mut lines = text.lines();
        let mut map = HashMap::new();
        if lines.next().map(str::trim) != Some(HEADER) {
            return Self { map };
        }
        for line in lines {
            let mut f = line.splitn(4, '\t');
            let (Some(bytes), Some(newest), Some(at), Some(path)) = (f.next(), f.next(), f.next(), f.next()) else {
                continue;
            };
            let (Ok(bytes), Ok(scanned_at)) = (bytes.parse(), at.parse()) else { continue };
            let newest = match newest {
                "-" => None,
                n => match n.parse() {
                    Ok(n) => Some(n),
                    Err(_) => continue,
                },
            };
            if path.is_empty() {
                continue;
            }
            map.insert(PathBuf::from(path), Entry { bytes, newest, scanned_at });
        }
        Self { map }
    }

    pub fn serialize(&self) -> String {
        let mut rows: Vec<(&PathBuf, &Entry)> = self.map.iter().collect();
        rows.sort_by(|a, b| a.0.cmp(b.0));
        let mut out = String::from(HEADER);
        out.push('\n');
        for (path, e) in rows {
            // Paths we can't represent on one line are simply not cached.
            let Some(p) = path.to_str().filter(|p| !p.contains('\n')) else { continue };
            let newest = e.newest.map(|n| n.to_string()).unwrap_or_else(|| "-".into());
            out.push_str(&format!("{}\t{}\t{}\t{}\n", e.bytes, newest, e.scanned_at, p));
        }
        out
    }

    /// Write to a temp file next to the target, then rename over it.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        let res = fs::File::create(&tmp)
            .and_then(|mut f| f.write_all(self.serialize().as_bytes()).and_then(|_| f.sync_all()))
            .and_then(|_| fs::rename(&tmp, path));
        if res.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        res
    }

    pub fn get(&self, path: &Path) -> Option<&Entry> {
        self.map.get(path)
    }

    pub fn insert(&mut self, path: PathBuf, entry: Entry) {
        self.map.insert(path, entry);
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Add everything from `fresh`, which wins over what's here.
    pub fn merge(&mut self, fresh: &Cache) {
        self.map.extend(fresh.map.iter().map(|(k, v)| (k.clone(), *v)));
    }

    /// Remember a fresh measurement of every artifact folder of `f`.
    pub fn record(&mut self, f: &Found, now: u64) {
        for a in &f.artifacts {
            let entry = Entry { bytes: a.bytes, newest: a.newest.map(unix_secs), scanned_at: now };
            self.insert(f.path.join(a.rel), entry);
        }
    }

    /// Write `self` + `fresh` (pruned of vanished folders under `roots`) to `path`.
    pub fn save_merged(&self, fresh: &Cache, roots: &[PathBuf], path: &Path) -> io::Result<()> {
        let mut all = self.clone();
        all.merge(fresh);
        all.prune(roots);
        all.save(path)
    }

    /// Drop entries under `roots` whose folder is gone.
    pub fn prune(&mut self, roots: &[PathBuf]) {
        self.map.retain(|p, _| !roots.iter().any(|r| p.starts_with(r)) || p.is_dir());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("kondor-cache-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn round_trip_on_disk() {
        let dir = tmp("rt");
        let file = dir.join("nested/sizes.txt");
        let mut c = Cache::default();
        let a = Entry { bytes: 1234, newest: Some(1_700_000_000), scanned_at: 1_700_000_100 };
        let b = Entry { bytes: 0, newest: None, scanned_at: 5 };
        c.insert(PathBuf::from("/p/a/node_modules"), a);
        c.insert(PathBuf::from("/p/with space/tab\tname/target"), b);
        c.save(&file).unwrap();

        let back = Cache::load(&file);
        assert_eq!(back.len(), 2);
        assert_eq!(back.get(Path::new("/p/a/node_modules")), Some(&a));
        assert_eq!(back.get(Path::new("/p/with space/tab\tname/target")), Some(&b));
        // No temp files left behind.
        assert_eq!(fs::read_dir(file.parent().unwrap()).unwrap().count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_cache_is_ignored() {
        assert_eq!(Cache::parse("").len(), 0);
        assert_eq!(Cache::parse("garbage\n1\t2\t3\t/x").len(), 0, "wrong header");
        let text = format!("{HEADER}\nnot a row\n10\tx\t1\t/bad-newest\n10\t-\tz\t/bad-time\n7\t-\t1\t/ok\n\t\t\t\n");
        let c = Cache::parse(&text);
        assert_eq!(c.len(), 1);
        assert_eq!(c.get(Path::new("/ok")).map(|e| e.bytes), Some(7));

        let dir = tmp("corrupt");
        let file = dir.join("sizes.txt");
        fs::write(&file, [0xff, 0xfe, 0x00, 0x01]).unwrap();
        assert_eq!(Cache::load(&file).len(), 0);
        assert_eq!(Cache::load(&dir.join("missing")).len(), 0);
        assert_eq!(Cache::load(&dir).len(), 0, "a directory is not a cache");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_drops_vanished_folders_under_roots_only() {
        let dir = tmp("prune");
        fs::create_dir_all(dir.join("proj/node_modules")).unwrap();
        let e = Entry { bytes: 1, newest: None, scanned_at: 1 };
        let mut c = Cache::default();
        c.insert(dir.join("proj/node_modules"), e);
        c.insert(dir.join("proj/gone"), e);
        c.insert(PathBuf::from("/elsewhere/gone"), e);
        c.prune(std::slice::from_ref(&dir));
        assert!(c.get(&dir.join("proj/node_modules")).is_some());
        assert!(c.get(&dir.join("proj/gone")).is_none());
        assert!(c.get(Path::new("/elsewhere/gone")).is_some(), "other roots are left alone");
        let _ = fs::remove_dir_all(&dir);
    }
}
