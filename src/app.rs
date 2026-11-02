//! Application state and input handling. Rendering lives in `ui.rs`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

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
    haystack: String,
}

impl Entry {
    pub fn from_found(id: usize, f: Found, home: Option<&Path>, now: SystemTime) -> Self {
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
            haystack,
        }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.status, Status::Ready | Status::Failed(_))
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
    pub scanning: bool,
    pub scan_time: Option<Duration>,
    pub progress: Arc<Progress>,
    pub max_bytes: u64,
    pub pending: usize,
    pub freed: u64,
    pub freed_projects: usize,
    batch: (u64, usize, usize),
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
            scan_time: None,
            progress,
            max_bytes: 0,
            pending: 0,
            freed: 0,
            freed_projects: 0,
            batch: (0, 0, 0),
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
                Ok(Msg::Found(found)) => {
                    let entry = Entry::from_found(self.entries.len(), found, self.home.as_deref(), now);
                    if self.min_age.is_some_and(|min| entry.age.is_some_and(|a| a < min)) {
                        self.hidden_recent += 1;
                    } else {
                        self.entries.push(entry);
                    }
                    changed = true;
                }
                Ok(Msg::Done { elapsed }) => {
                    self.scanning = false;
                    self.scan_time = Some(elapsed);
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    changed |= self.scanning;
                    self.scanning = false;
                    break;
                }
            }
        }
        if changed {
            self.rebuild();
        }
        while let Ok(outcome) = self.clean_rx.try_recv() {
            self.finish_clean(outcome);
            changed = true;
        }
        if self.toast.as_ref().is_some_and(|t| t.at.elapsed() > Duration::from_secs(6)) {
            self.toast = None;
            changed = true;
        }
        changed
    }

    /// Re-filter and re-sort, keeping the same project under the cursor.
    pub fn rebuild(&mut self) {
        let keep = self.selected().map(|e| e.id);
        let terms: Vec<String> = self.filter.to_lowercase().split_whitespace().map(String::from).collect();
        let entries = &self.entries;
        self.view =
            (0..entries.len()).filter(|&i| terms.iter().all(|t| entries[i].haystack.contains(t.as_str()))).collect();
        match self.sort {
            SortBy::Size => self.view.sort_by(|&a, &b| entries[b].bytes.cmp(&entries[a].bytes)),
            SortBy::Age => self.view.sort_by(|&a, &b| entries[b].age.cmp(&entries[a].age)),
            SortBy::Name => {
                self.view.sort_by_cached_key(|&i| (entries[i].name.to_lowercase(), entries[i].location.clone()))
            }
        }
        self.max_bytes = entries.iter().map(|e| e.bytes).max().unwrap_or(0);
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
        self.entries.iter().filter(|e| e.marked && e.is_ready()).fold((0, 0), |(n, b), e| (n + 1, b + e.bytes))
    }

    pub fn reclaimable(&self) -> (usize, u64) {
        self.entries.iter().filter(|e| e.is_ready()).fold((0, 0), |(n, b), e| (n + 1, b + e.bytes))
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
            if e.is_ready() {
                e.marked = !e.marked;
            }
        }
        if advance {
            self.move_by(1);
        }
    }

    /// Mark every visible project; if they're all marked already, unmark them.
    fn toggle_all(&mut self) {
        let all = self.view.iter().all(|&i| !self.entries[i].is_ready() || self.entries[i].marked);
        for &i in &self.view {
            let e = &mut self.entries[i];
            if e.is_ready() {
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
        let mut ids: Vec<usize> = self.entries.iter().filter(|e| e.marked && e.is_ready()).map(|e| e.id).collect();
        if ids.is_empty() {
            match self.selected() {
                Some(e) if e.is_ready() => ids.push(e.id),
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
            if !e.is_ready() {
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
            e.status = if out.freed > 0 { Status::Cleaned } else { Status::Failed(out.errors.join("; ")) };
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
                artifacts: vec![Artifact { rel: "node_modules", bytes: 1000 * (n - i) as u64 }],
                bytes: 1000 * (n - i) as u64,
                newest: None,
            };
            tx.send(Msg::Found(found)).unwrap();
        }
        drop(tx);
        let cfg = Config { roots: vec![], home: None, sort: SortBy::Size, dry_run: true, min_age: None };
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
}
