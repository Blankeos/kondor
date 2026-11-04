//! Application state and input handling. Rendering lives in `ui.rs`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use crate::cache::{self, Cache};
use crate::clean::{self, Job, Outcome};
use crate::cli::SortBy;
use crate::format::{self, plural};
use crate::project;
use crate::scan::{Artifact, Found, Msg, Progress};
use crate::theme::Theme;

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Ready,
    Cleaning,
    Cleaned,
    Failed(String),
}

/// How much to trust `Entry::bytes`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeState {
    /// Not measured yet; `bytes` is meaningless.
    Pending,
    /// From the cache of an earlier run; being re-measured.
    Stale,
    Fresh,
}

pub struct Entry {
    pub id: usize,
    pub path: PathBuf,
    pub name: String,
    /// Parent folder, `~`-shortened.
    pub location: String,
    pub kinds: String,
    pub artifacts: Vec<Artifact>,
    pub bytes: u64,
    /// Seconds since the artifacts were last written.
    pub age: Option<u64>,
    pub marked: bool,
    pub status: Status,
    pub size: SizeState,
    /// Newer than `--older` (known only once the age is).
    recent: bool,
    /// Its artifact folders turned out to be gone.
    pub gone: bool,
    haystack: String,
}

impl Entry {
    pub fn from_found(id: usize, f: Found, home: Option<&Path>, now: SystemTime, size: SizeState) -> Self {
        let name = f
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| f.path.display().to_string());
        let location = f.path.parent().map(|p| format::tilde(p, home)).unwrap_or_default();
        let kinds = project::label(&f.kinds);
        let age = f.newest.map(|t| now.duration_since(t).unwrap_or_default().as_secs());
        let haystack = format!("{name} {location} {kinds}").to_lowercase();
        Self {
            id,
            path: f.path,
            name,
            location,
            kinds,
            artifacts: f.artifacts,
            bytes: f.bytes,
            age,
            marked: false,
            status: Status::Ready,
            size,
            recent: false,
            gone: false,
            haystack,
        }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.status, Status::Ready | Status::Failed(_))
    }

    /// Can be marked and deleted: not busy, and measured (or at least known
    /// from the cache). A row that is still pending has no numbers to confirm.
    pub fn actionable(&self) -> bool {
        self.is_ready() && !self.gone && self.size != SizeState::Pending
    }

    /// Take a fresh measurement. Ignored while the project is being cleaned.
    fn apply(&mut self, f: Found, now: SystemTime) {
        if !self.is_ready() {
            return;
        }
        self.gone = f.artifacts.is_empty();
        if self.gone {
            self.marked = false;
        }
        self.age = f.newest.map(|t| now.duration_since(t).unwrap_or_default().as_secs());
        self.artifacts = f.artifacts;
        self.bytes = f.bytes;
        self.size = SizeState::Fresh;
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    Normal,
    Filter,
    Info,
    Help,
    /// Waiting for `y` to delete these entry ids.
    Confirm(Vec<usize>),
}

#[derive(Clone, Copy, PartialEq)]
pub enum Tone {
    Info,
    Good,
    Bad,
}

pub struct Toast {
    pub text: String,
    pub tone: Tone,
    at: Instant,
}

/// Screen regions from the last draw, used to map mouse clicks.
#[derive(Default)]
pub struct Hit {
    /// Table data rows (excluding the header).
    pub rows: Rect,
    /// Sortable column headers.
    pub sort_cols: Vec<(Rect, SortBy)>,
    pub scrollbar: Option<Rect>,
    /// Outer rect of the open popup, if any.
    pub modal: Option<Rect>,
}

/// Lines moved per wheel notch.
const WHEEL_STEP: isize = 3;
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

pub struct App {
    pub entries: Vec<Entry>,
    /// Indices into `entries`: filtered and sorted.
    pub view: Vec<usize>,
    /// Position in `view`.
    pub cursor: usize,
    pub offset: usize,
    /// Whether the viewport follows the cursor. Wheel scrolling turns this
    /// off so the selection stays put; any key turns it back on.
    pub follow: bool,
    pub hit: Hit,
    last_click: Option<(Instant, usize)>,
    dragging_scrollbar: bool,
    /// Rows that fit on screen, updated on every draw.
    pub page: usize,
    pub sort: SortBy,
    pub filter: String,
    pub mode: Mode,
    pub dry_run: bool,
    pub roots: Vec<PathBuf>,
    pub home: Option<PathBuf>,
    pub min_age: Option<u64>,
    /// Projects found but hidden because they're newer than `--older`.
    pub hidden_recent: usize,
    /// Until everything (walk and sizing) is finished.
    pub scanning: bool,
    /// The folder walk itself is still going.
    pub walking: bool,
    pub scan_time: Option<Duration>,
    pub progress: Arc<Progress>,
    pub max_bytes: u64,
    pub total_bytes: u64,
    pub pending: usize,
    pub freed: u64,
    pub freed_projects: usize,
    batch: (u64, usize, usize),
    index: HashMap<PathBuf, usize>,
    /// Measurements from this run, for the cache.
    fresh: Cache,
    old_cache: Cache,
    cache_path: Option<PathBuf>,
    cache_saved: bool,
    pub toast: Option<Toast>,
    pub spin: usize,
    pub theme: Theme,
    quit_armed: bool,
    pub should_quit: bool,
    scan_rx: Receiver<Msg>,
    clean_tx: Sender<Outcome>,
    clean_rx: Receiver<Outcome>,
}

pub struct Config {
    pub roots: Vec<PathBuf>,
    pub home: Option<PathBuf>,
    pub sort: SortBy,
    pub dry_run: bool,
    pub min_age: Option<u64>,
    /// Where to keep sizes between runs (`None`: no cache).
    pub cache_path: Option<PathBuf>,
    pub old_cache: Cache,
}

impl App {
    pub fn new(cfg: Config, scan_rx: Receiver<Msg>, progress: Arc<Progress>) -> Self {
        let (clean_tx, clean_rx) = mpsc::channel();
        Self {
            entries: Vec::new(),
            view: Vec::new(),
            cursor: 0,
            offset: 0,
            follow: true,
            hit: Hit::default(),
            last_click: None,
            dragging_scrollbar: false,
            page: 10,
            sort: cfg.sort,
            filter: String::new(),
            mode: Mode::Normal,
            dry_run: cfg.dry_run,
            roots: cfg.roots,
            home: cfg.home,
            min_age: cfg.min_age,
            hidden_recent: 0,
            scanning: true,
            walking: true,
            scan_time: None,
            progress,
            max_bytes: 0,
            total_bytes: 0,
            pending: 0,
            freed: 0,
            freed_projects: 0,
            batch: (0, 0, 0),
            index: HashMap::new(),
            fresh: Cache::default(),
            old_cache: cfg.old_cache,
            cache_saved: cfg.cache_path.is_none(),
            cache_path: cfg.cache_path,
            toast: None,
            spin: 0,
            theme: Theme::detect(),
            quit_armed: false,
            should_quit: false,
            scan_rx,
            clean_tx,
            clean_rx,
        }
    }

    /// Something on screen is moving and needs regular redraws.
    pub fn animating(&self) -> bool {
        self.scanning || self.pending > 0
    }

    /// Pull in background results. Returns true if anything changed.
    pub fn update(&mut self) -> bool {
        let mut changed = false;
        let now = SystemTime::now();
        loop {
            match self.scan_rx.try_recv() {
                Ok(Msg::Discovered { found, cached }) => {
                    let id = self.entries.len();
                    let size = if cached { SizeState::Stale } else { SizeState::Pending };
                    self.index.insert(found.path.clone(), id);
                    self.entries.push(Entry::from_found(id, found, self.home.as_deref(), now, size));
                    self.refresh_recent(id);
                    changed = true;
                }
                Ok(Msg::Sized(found)) => {
                    self.fresh.record(&found, cache::unix_secs(now));
                    if let Some(&id) = self.index.get(&found.path) {
                        self.entries[id].apply(found, now);
                        self.refresh_recent(id);
                    }
                    changed = true;
                }
                Ok(Msg::WalkDone { elapsed }) => {
                    self.walking = false;
                    self.scan_time = Some(elapsed);
                    changed = true;
                }
                Ok(Msg::Done { elapsed }) => {
                    self.walking = false;
                    self.scanning = false;
                    self.scan_time.get_or_insert(elapsed);
                    self.save_cache();
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    changed |= self.scanning;
                    self.scanning = false;
                    self.walking = false;
                    break;
                }
            }
        }
        if changed {
            self.rebuild();
        }
        let mut cleaned = false;
        while let Ok(outcome) = self.clean_rx.try_recv() {
            self.finish_clean(outcome);
            cleaned = true;
        }
        if cleaned {
            // Partial failures shrink a project's size, which can move it.
            self.rebuild();
            changed = true;
        }
        if self.toast.as_ref().is_some_and(|t| t.at.elapsed() > Duration::from_secs(6)) {
            self.toast = None;
            changed = true;
        }
        changed
    }

    /// Entries still waiting for a measurement.
    pub fn unsized_count(&self) -> usize {
        self.entries.iter().filter(|e| e.size != SizeState::Fresh && !e.gone).count()
    }

    /// Share of live entries already measured, 0..=100 (99 until all are).
    pub fn sized_percent(&self) -> u64 {
        let total = self.entries.iter().filter(|e| !e.gone).count() as u64;
        if total == 0 {
            return 0;
        }
        let remaining = self.unsized_count() as u64;
        let pct = total.saturating_sub(remaining) * 100 / total;
        if remaining > 0 {
            pct.min(99)
        } else {
            pct.min(100)
        }
    }

    /// Apply `--older`. Rows with an unknown age stay visible until measured.
    fn refresh_recent(&mut self, id: usize) {
        let min = self.min_age;
        let e = &mut self.entries[id];
        e.recent = match (min, e.age) {
            (Some(min), Some(age)) => age < min,
            _ => false,
        };
        if e.recent {
            e.marked = false;
        }
    }

    /// Write the cache (once): last run's numbers plus this run's.
    pub fn save_cache(&mut self) {
        if self.cache_saved {
            return;
        }
        self.cache_saved = true;
        if let Some(path) = &self.cache_path {
            // Best effort: a cache that can't be written is not an error.
            let _ = self.old_cache.save_merged(&self.fresh, &self.roots, path);
        }
    }

    /// Re-filter and re-sort, keeping the same project under the cursor.
    pub fn rebuild(&mut self) {
        let keep = self.selected().map(|e| e.id);
        let terms: Vec<String> = self.filter.to_lowercase().split_whitespace().map(String::from).collect();
        let entries = &self.entries;
        self.view = (0..entries.len())
            .filter(|&i| {
                let e = &entries[i];
                !e.gone && !e.recent && terms.iter().all(|t| e.haystack.contains(t.as_str()))
            })
            .collect();
        match self.sort {
            SortBy::Size => self.view.sort_by(|&a, &b| entries[b].bytes.cmp(&entries[a].bytes)),
            SortBy::Age => self.view.sort_by(|&a, &b| entries[b].age.cmp(&entries[a].age)),
            SortBy::Name => {
                self.view.sort_by_cached_key(|&i| (entries[i].name.to_lowercase(), entries[i].location.clone()))
            }
        }
        self.max_bytes = entries.iter().filter(|e| !e.gone && !e.recent).map(|e| e.bytes).max().unwrap_or(0);
        self.total_bytes = entries.iter().filter(|e| !e.gone).map(|e| e.bytes).sum();
        self.hidden_recent = entries.iter().filter(|e| e.recent && !e.gone).count();
        self.cursor = keep
            .and_then(|id| self.view.iter().position(|&i| i == id))
            .unwrap_or(self.cursor)
            .min(self.view.len().saturating_sub(1));
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.view.get(self.cursor).map(|&i| &self.entries[i])
    }

    fn selected_mut(&mut self) -> Option<&mut Entry> {
        let i = *self.view.get(self.cursor)?;
        self.entries.get_mut(i)
    }

    pub fn marked(&self) -> (usize, u64) {
        self.entries.iter().filter(|e| e.marked && e.actionable()).fold((0, 0), |(n, b), e| (n + 1, b + e.bytes))
    }

    pub fn reclaimable(&self) -> (usize, u64) {
        self.entries.iter().filter(|e| e.actionable() && !e.recent).fold((0, 0), |(n, b), e| (n + 1, b + e.bytes))
    }

    pub fn notify(&mut self, text: impl Into<String>, tone: Tone) {
        self.toast = Some(Toast { text: text.into(), tone, at: Instant::now() });
    }

    // ----- input --------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) {
        self.follow = true;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c')) {
            return self.quit();
        }
        if !matches!(key.code, KeyCode::Char('q')) {
            self.quit_armed = false;
        }
        match self.mode.clone() {
            Mode::Confirm(ids) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.delete(ids),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
                _ => {}
            },
            Mode::Help => match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter => self.mode = Mode::Normal,
                _ => {}
            },
            Mode::Info => match key.code {
                KeyCode::Esc | KeyCode::Char('i') | KeyCode::Char('q') | KeyCode::Enter | KeyCode::Left => {
                    self.mode = Mode::Normal
                }
                KeyCode::Char(' ') => self.toggle_mark(false),
                KeyCode::Char('o') => self.open_selected(),
                KeyCode::Char('d') | KeyCode::Delete => self.ask_delete(),
                KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
                _ => {}
            },
            Mode::Filter => match key.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.mode = Mode::Normal;
                    self.rebuild();
                }
                KeyCode::Enter => self.mode = Mode::Normal,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.rebuild();
                }
                KeyCode::Char('u') if ctrl => {
                    self.filter.clear();
                    self.rebuild();
                }
                KeyCode::Up => self.move_by(-1),
                KeyCode::Down => self.move_by(1),
                KeyCode::Char(c) if !ctrl => {
                    self.filter.push(c);
                    self.cursor = 0;
                    self.rebuild();
                }
                _ => {}
            },
            Mode::Normal => self.on_normal_key(key, ctrl),
        }
    }

    fn on_normal_key(&mut self, key: KeyEvent, ctrl: bool) {
        let page = self.page.max(1) as isize;
        match key.code {
            KeyCode::Char('q') => self.quit(),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-page),
            KeyCode::PageDown => self.move_by(page),
            KeyCode::Char('u') if ctrl => self.move_by(-page / 2),
            KeyCode::Char('d') if ctrl => self.move_by(page / 2),
            KeyCode::Home | KeyCode::Char('g') => self.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.cursor = self.view.len().saturating_sub(1),
            KeyCode::Char(' ') => self.toggle_mark(true),
            KeyCode::Char('a') => self.toggle_all(),
            KeyCode::Char('d') | KeyCode::Delete => self.ask_delete(),
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('s') => self.cycle_sort(),
            KeyCode::Char('i') | KeyCode::Enter | KeyCode::Right => {
                if self.selected().is_some() {
                    self.mode = Mode::Info;
                }
            }
            KeyCode::Char('o') => self.open_selected(),
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.rebuild();
                } else if self.marked().0 > 0 {
                    self.entries.iter_mut().for_each(|e| e.marked = false);
                    self.notify("Cleared all marks", Tone::Info);
                }
            }
            _ => {}
        }
    }

    fn quit(&mut self) {
        if self.pending > 0 && !self.quit_armed {
            self.quit_armed = true;
            self.notify("Still deleting. Press q again to quit anyway.", Tone::Bad);
        } else {
            self.should_quit = true;
        }
    }

    // ----- mouse --------------------------------------------------------

    /// Handle a mouse event. Returns true if the screen needs a redraw.
    pub fn on_mouse(&mut self, m: MouseEvent) -> bool {
        let pos = Position::new(m.column, m.row);
        match m.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                if !matches!(self.mode, Mode::Normal | Mode::Filter) {
                    return false;
                }
                let step = if m.kind == MouseEventKind::ScrollDown { WHEEL_STEP } else { -WHEEL_STEP };
                self.scroll_by(step)
            }
            MouseEventKind::Down(MouseButton::Left) => self.on_click(pos),
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_scrollbar => self.scroll_to(pos.y),
            MouseEventKind::Up(_) => {
                self.dragging_scrollbar = false;
                false
            }
            _ => false,
        }
    }

    /// Move the viewport without touching the selection.
    fn scroll_by(&mut self, delta: isize) -> bool {
        let max = self.view.len().saturating_sub(self.page) as isize;
        let next = (self.offset as isize + delta).clamp(0, max) as usize;
        if next == self.offset {
            return false;
        }
        self.offset = next;
        self.follow = false;
        true
    }

    /// Jump the viewport to where `y` falls on the scrollbar track.
    fn scroll_to(&mut self, y: u16) -> bool {
        let Some(sb) = self.hit.scrollbar else { return false };
        let max = self.view.len().saturating_sub(self.page);
        let track = sb.height.saturating_sub(1).max(1) as usize;
        let rel = (y.saturating_sub(sb.y) as usize).min(track);
        self.offset = (rel * max + track / 2) / track;
        self.follow = false;
        true
    }

    fn on_click(&mut self, pos: Position) -> bool {
        self.quit_armed = false;
        match self.mode {
            Mode::Help => {
                self.mode = Mode::Normal;
                return true;
            }
            // Clicking outside a popup dismisses it (never confirms a delete).
            Mode::Info | Mode::Confirm(_) => {
                if self.hit.modal.is_some_and(|r| r.contains(pos)) {
                    return false;
                }
                self.mode = Mode::Normal;
                return true;
            }
            Mode::Filter => self.mode = Mode::Normal,
            Mode::Normal => {}
        }

        if self.hit.scrollbar.is_some_and(|r| r.contains(pos)) {
            self.dragging_scrollbar = true;
            return self.scroll_to(pos.y);
        }
        if let Some(&(_, sort)) = self.hit.sort_cols.iter().find(|(r, _)| r.contains(pos)) {
            if self.sort != sort {
                self.sort = sort;
                self.follow = true;
                self.rebuild();
            }
            return true;
        }
        let rows = self.hit.rows;
        if !rows.contains(pos) {
            return true;
        }
        let idx = self.offset + (pos.y - rows.y) as usize;
        let Some(&id) = self.view.get(idx) else { return true };
        self.cursor = idx;
        let now = Instant::now();
        let double = self.last_click.is_some_and(|(at, last)| last == id && now.duration_since(at) < DOUBLE_CLICK);
        if pos.x < rows.x + 2 {
            // The ○ / ● gutter toggles the mark.
            self.toggle_mark(false);
            self.last_click = None;
        } else if double {
            self.mode = Mode::Info;
            self.last_click = None;
        } else {
            self.last_click = Some((now, id));
        }
        true
    }

    fn move_by(&mut self, delta: isize) {
        if self.view.is_empty() {
            return;
        }
        let last = self.view.len() as isize - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, last) as usize;
    }

    fn toggle_mark(&mut self, advance: bool) {
        if let Some(e) = self.selected_mut() {
            if e.actionable() {
                e.marked = !e.marked;
            }
        }
        if advance {
            self.move_by(1);
        }
    }

    /// Mark every visible project; if they're all marked already, unmark them.
    fn toggle_all(&mut self) {
        let all = self.view.iter().all(|&i| !self.entries[i].actionable() || self.entries[i].marked);
        for &i in &self.view {
            let e = &mut self.entries[i];
            if e.actionable() {
                e.marked = !all;
            }
        }
    }

    fn cycle_sort(&mut self) {
        self.sort = match self.sort {
            SortBy::Size => SortBy::Age,
            SortBy::Age => SortBy::Name,
            SortBy::Name => SortBy::Size,
        };
        self.rebuild();
    }

    fn open_selected(&mut self) {
        let Some(path) = self.selected().map(|e| e.path.clone()) else { return };
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else if cfg!(windows) {
            "explorer"
        } else {
            "xdg-open"
        };
        let spawned =
            Command::new(opener).arg(&path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        if spawned.is_err() {
            self.notify(format!("Couldn't open the folder ({opener} not found)"), Tone::Bad);
        }
    }

    // ----- deleting -----------------------------------------------------

    /// Delete the marked projects, or the selected one if nothing is marked.
    fn ask_delete(&mut self) {
        let mut ids: Vec<usize> = self.entries.iter().filter(|e| e.marked && e.actionable()).map(|e| e.id).collect();
        if ids.is_empty() {
            match self.selected() {
                Some(e) if e.actionable() => ids.push(e.id),
                Some(e) if e.is_ready() => return self.notify("Still measuring, try again in a moment", Tone::Info),
                Some(_) => return self.notify("Already cleaned", Tone::Info),
                None => return,
            }
        }
        // Biggest first in the confirmation list.
        ids.sort_by(|&a, &b| self.entries[b].bytes.cmp(&self.entries[a].bytes));
        self.mode = Mode::Confirm(ids);
    }

    fn delete(&mut self, ids: Vec<usize>) {
        self.mode = Mode::Normal;
        for id in ids {
            let e = &mut self.entries[id];
            if !e.actionable() {
                continue;
            }
            e.marked = false;
            e.status = Status::Cleaning;
            self.pending += 1;
            let job = Job {
                id,
                root: e.path.clone(),
                artifacts: e.artifacts.iter().map(|a| (a.rel, a.bytes)).collect(),
                dry_run: self.dry_run,
            };
            clean::spawn(job, self.clean_tx.clone());
        }
    }

    fn finish_clean(&mut self, out: Outcome) {
        self.pending = self.pending.saturating_sub(1);
        let e = &mut self.entries[out.id];
        let name = e.name.clone();
        if out.errors.is_empty() {
            e.status = Status::Cleaned;
            self.batch.1 += 1;
        } else {
            // Keep what's left so it shows up as failed and can be retried.
            e.artifacts.retain(|a| !out.removed.contains(&a.rel));
            e.bytes = e.artifacts.iter().map(|a| a.bytes).sum();
            e.status = Status::Failed(out.errors.join("; "));
            self.batch.2 += 1;
            self.notify(format!("Couldn't fully clean {name}: {}", out.errors[0]), Tone::Bad);
        }
        self.batch.0 += out.freed;
        self.freed += out.freed;
        if out.freed > 0 {
            self.freed_projects += 1;
        }
        if self.pending == 0 {
            let (bytes, ok, failed) = std::mem::take(&mut self.batch);
            if failed == 0 {
                let what = plural(ok, "project", "projects");
                if self.dry_run {
                    self.notify(
                        format!("Dry run: would free {} from {what}. Nothing was deleted.", format::size(bytes)),
                        Tone::Good,
                    );
                } else {
                    self.notify(format!("Freed {} from {what}", format::size(bytes)), Tone::Good);
                }
            } else if ok + failed > 1 {
                self.notify(
                    format!(
                        "Freed {}. {} couldn't be fully cleaned (press i for details)",
                        format::size(bytes),
                        plural(failed, "project", "projects")
                    ),
                    Tone::Bad,
                );
            }
        }
    }

    pub fn dirs_scanned(&self) -> u64 {
        self.progress.dirs.load(Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Kind;

    fn app_with(n: usize, page: usize) -> App {
        let (tx, rx) = mpsc::channel();
        for i in 0..n {
            let found = Found {
                path: PathBuf::from(format!("/p/proj{i:02}")),
                kinds: vec![Kind::Node],
                artifacts: vec![Artifact { rel: "node_modules", bytes: 1000 * (n - i) as u64, newest: None }],
                bytes: 1000 * (n - i) as u64,
                newest: None,
            };
            tx.send(Msg::Discovered { found: found.clone(), cached: false }).unwrap();
            tx.send(Msg::Sized(found)).unwrap();
        }
        drop(tx);
        let cfg = Config {
            roots: vec![],
            home: None,
            sort: SortBy::Size,
            dry_run: true,
            min_age: None,
            cache_path: None,
            old_cache: Cache::default(),
        };
        let mut app = App::new(cfg, rx, Arc::new(Progress::default()));
        app.update();
        app.page = page;
        app.hit.rows = Rect::new(0, 3, 80, page as u16);
        app
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
        MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::NONE }
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn wheel_scrolls_viewport_not_selection() {
        let mut app = app_with(30, 10);
        app.on_mouse(mouse(MouseEventKind::ScrollDown, 20, 5));
        app.on_mouse(mouse(MouseEventKind::ScrollDown, 20, 5));
        assert_eq!((app.cursor, app.offset, app.follow), (0, 6, false));
        // Clamped at the bottom.
        for _ in 0..20 {
            app.on_mouse(mouse(MouseEventKind::ScrollDown, 20, 5));
        }
        assert_eq!(app.offset, 20);
        // Any key hands the viewport back to the selection.
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!((app.cursor, app.follow), (1, true));
    }

    #[test]
    fn click_selects_marks_and_double_click_opens() {
        let mut app = app_with(30, 10);
        app.offset = 5;
        app.follow = false;
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 20, 5));
        assert_eq!((app.cursor, app.offset), (7, 5));
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 20, 5));
        assert_eq!(app.mode, Mode::Info);

        let mut app = app_with(30, 10);
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 4));
        assert!(app.selected().unwrap().marked);
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn partial_failure_stays_retryable() {
        let mut app = app_with(1, 10);
        let e = &mut app.entries[0];
        e.artifacts = vec![
            Artifact { rel: "node_modules", bytes: 700, newest: None },
            Artifact { rel: ".angular", bytes: 300, newest: None },
        ];
        e.bytes = 1000;
        e.status = Status::Cleaning;
        app.pending = 1;
        app.finish_clean(Outcome {
            id: 0,
            freed: 700,
            removed: vec!["node_modules"],
            errors: vec![".angular: permission denied".into()],
        });
        let e = &app.entries[0];
        assert!(matches!(e.status, Status::Failed(_)));
        assert!(e.is_ready());
        assert_eq!((e.bytes, e.artifacts.len(), app.freed), (300, 1, 700));
    }

    #[test]
    fn click_outside_confirm_cancels_and_never_deletes() {
        let mut app = app_with(3, 10);
        app.on_key(key(KeyCode::Char('d')));
        assert!(matches!(app.mode, Mode::Confirm(_)));
        app.hit.modal = Some(Rect::new(10, 2, 40, 8));
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 20, 5));
        assert!(matches!(app.mode, Mode::Confirm(_)), "click inside does nothing");
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 70, 20));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.entries.iter().all(|e| e.status == Status::Ready));
    }

    fn found(path: &str, bytes: u64, age_secs: Option<u64>) -> Found {
        let newest = age_secs.map(|a| SystemTime::now() - Duration::from_secs(a));
        Found {
            path: PathBuf::from(path),
            kinds: vec![Kind::Node],
            artifacts: if bytes == 0 { vec![] } else { vec![Artifact { rel: "node_modules", bytes, newest }] },
            bytes,
            newest,
        }
    }

    fn live_app(min_age: Option<u64>) -> (App, Sender<Msg>) {
        let (tx, rx) = mpsc::channel();
        let cfg = Config {
            roots: vec![],
            home: None,
            sort: SortBy::Size,
            dry_run: true,
            min_age,
            cache_path: None,
            old_cache: Cache::default(),
        };
        (App::new(cfg, rx, Arc::new(Progress::default())), tx)
    }

    #[test]
    fn pending_row_becomes_sized_and_resorts() {
        let (mut app, tx) = live_app(None);
        tx.send(Msg::Discovered { found: found("/p/a", 0, None), cached: false }).unwrap();
        tx.send(Msg::Discovered { found: found("/p/b", 0, None), cached: false }).unwrap();
        app.update();
        assert_eq!(app.view.len(), 2);
        assert!(app.entries.iter().all(|e| e.size == SizeState::Pending));
        assert_eq!(app.unsized_count(), 2);

        // Pending rows can't be marked or deleted.
        app.on_key(key(KeyCode::Char(' ')));
        assert!(!app.entries[0].marked);
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.mode, Mode::Normal);

        tx.send(Msg::Sized(found("/p/b", 5000, Some(100)))).unwrap();
        tx.send(Msg::Sized(found("/p/a", 100, Some(100)))).unwrap();
        app.update();
        assert_eq!(app.unsized_count(), 0);
        assert_eq!(app.entries[app.view[0]].name, "b", "biggest first once sizes land");
        assert_eq!((app.entries[0].bytes, app.entries[0].size), (100, SizeState::Fresh));
        app.on_key(key(KeyCode::Char('d')));
        assert!(matches!(app.mode, Mode::Confirm(_)));
    }

    #[test]
    fn cached_rows_are_stale_until_measured() {
        let (mut app, tx) = live_app(None);
        tx.send(Msg::Discovered { found: found("/p/a", 777, Some(10)), cached: true }).unwrap();
        app.update();
        assert_eq!((app.entries[0].bytes, app.entries[0].size), (777, SizeState::Stale));
        tx.send(Msg::Sized(found("/p/a", 888, Some(10)))).unwrap();
        app.update();
        assert_eq!((app.entries[0].bytes, app.entries[0].size), (888, SizeState::Fresh));
    }

    #[test]
    fn min_age_applies_once_age_is_known() {
        let (mut app, tx) = live_app(Some(1000));
        tx.send(Msg::Discovered { found: found("/p/new", 0, None), cached: false }).unwrap();
        tx.send(Msg::Discovered { found: found("/p/old", 0, None), cached: false }).unwrap();
        app.update();
        assert_eq!(app.view.len(), 2, "unknown age stays visible");
        tx.send(Msg::Sized(found("/p/new", 10, Some(5)))).unwrap();
        tx.send(Msg::Sized(found("/p/old", 10, Some(5000)))).unwrap();
        app.update();
        assert_eq!(app.view.len(), 1);
        assert_eq!(app.entries[app.view[0]].name, "old");
        assert_eq!(app.hidden_recent, 1);
    }

    #[test]
    fn vanished_artifacts_remove_the_row() {
        let (mut app, tx) = live_app(None);
        tx.send(Msg::Discovered { found: found("/p/a", 0, None), cached: false }).unwrap();
        tx.send(Msg::Sized(found("/p/a", 0, None))).unwrap();
        app.update();
        assert!(app.view.is_empty());
        assert_eq!(app.hidden_recent, 0);
    }
}
