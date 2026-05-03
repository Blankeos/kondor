use rayon::prelude::*;

use std::{
    env::current_dir,
    error::Error,
    fmt,
    io,
    num::ParseIntError,
    path::PathBuf,
    sync::mpsc::{Receiver, Sender},
    time::Duration,
};

use clap::{Command, CommandFactory, Parser};
use clap_complete::{generate, Generator, Shell};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    Terminal,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use kondo_lib::{path_canonicalise, pretty_size, print_elapsed, scan, Project, ScanOptions};

#[derive(Parser, Debug)]
#[command(name = "kondor")]
struct Opt {
    #[arg(name = "DIRS")]
    dirs: Vec<PathBuf>,

    #[arg(short = 'I', long)]
    ignored_dirs: Vec<PathBuf>,

    #[arg(short = 'L', long)]
    follow_symlinks: bool,

    #[arg(short, long)]
    same_filesystem: bool,

    #[arg(short, long, value_parser = parse_age_filter, default_value = "0d")]
    older: u64,

    #[arg(long = "completions", value_enum)]
    generator: Option<Shell>,

    #[arg(long, default_value = "size")]
    sort: SortMode,

    #[arg(short = 'n', long)]
    dry_run: bool,
}

#[derive(Clone, Debug, clap::ValueEnum)]
enum SortMode {
    Size,
    Date,
}

#[derive(Debug)]
pub enum ParseAgeFilterError {
    ParseIntError(ParseIntError),
    InvalidUnit,
}

impl fmt::Display for ParseAgeFilterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseAgeFilterError::ParseIntError(e) => e.fmt(f),
            ParseAgeFilterError::InvalidUnit => {
                "invalid age unit, must be one of m, h, d, w, M, y".fmt(f)
            }
        }
    }
}

impl From<ParseIntError> for ParseAgeFilterError {
    fn from(e: ParseIntError) -> Self {
        Self::ParseIntError(e)
    }
}

impl Error for ParseAgeFilterError {}

pub fn parse_age_filter(age_filter: &str) -> Result<u64, ParseAgeFilterError> {
    const MINUTE: u64 = 60;
    const HOUR: u64 = MINUTE * 60;
    const DAY: u64 = HOUR * 24;
    const WEEK: u64 = DAY * 7;
    const MONTH: u64 = WEEK * 4;
    const YEAR: u64 = DAY * 365;

    let (digit_end, unit) = age_filter
        .char_indices()
        .last()
        .ok_or(ParseAgeFilterError::InvalidUnit)?;

    let multiplier = match unit {
        'm' => MINUTE,
        'h' => HOUR,
        'd' => DAY,
        'w' => WEEK,
        'M' => MONTH,
        'y' => YEAR,
        _ => return Err(ParseAgeFilterError::InvalidUnit),
    };

    let count = age_filter[..digit_end].parse::<u64>()?;
    Ok(count * multiplier)
}

fn prepare_directories(dirs: Vec<PathBuf>) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let cd = current_dir()?;
    if dirs.is_empty() {
        return Ok(vec![cd]);
    }
    let dirs = dirs
        .into_iter()
        .filter_map(|path| {
            let exists = path.try_exists().unwrap_or(false);
            if !exists {
                eprintln!("error: directory {} does not exist", path.to_string_lossy());
                return None;
            }
            if let Ok(metadata) = path.metadata() {
                if metadata.is_file() {
                    eprintln!(
                        "error: file supplied but directory expected: {}",
                        path.to_string_lossy()
                    );
                    return None;
                }
            }
            path_canonicalise(&cd, path).ok()
        })
        .collect();
    Ok(dirs)
}

#[derive(Clone)]
struct ProjectEntry {
    project: Project,
    artifact_bytes: u64,
    last_modified_str: String,
    last_modified_secs: u64,
    marked: bool,
    cleaning: bool,
    cleaned: bool,
}

impl ProjectEntry {
    fn display_name(&self) -> String {
        self.project
            .path
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_else(|| self.project.name())
            .to_string()
    }
}

fn discover(
    dirs: Vec<PathBuf>,
    scan_options: &ScanOptions,
    project_min_age: u64,
    entry_sender: std::sync::mpsc::Sender<ProjectEntry>,
    ignored_dirs: &[PathBuf],
) {
    let projects: Vec<Project> = dirs
        .iter()
        .flat_map(|dir| scan(dir, scan_options))
        .filter_map(|p| p.ok())
        .filter(|p| ignored_dirs.iter().all(|i| !p.path.starts_with(i)))
        .collect();

    projects.into_par_iter().for_each(|project| {
        let artifact_bytes: u64 = project
            .artifact_dirs()
            .iter()
            .copied()
            .map(|d| kondo_lib::dir_size(&project.path.join(d), scan_options))
            .sum();

        if artifact_bytes == 0 {
            return;
        }

        let mut last_modified_str = String::new();
        let mut last_modified_secs: u64 = 0;

        if let Ok(last_modified) = project.artifact_last_modified() {
            if let Ok(elapsed) = last_modified.elapsed() {
                last_modified_secs = elapsed.as_secs();
                last_modified_str = print_elapsed(last_modified_secs);
            }
        }

        if last_modified_secs < project_min_age {
            return;
        }

        let entry = ProjectEntry {
            project,
            artifact_bytes,
            last_modified_str,
            last_modified_secs,
            marked: false,
            cleaning: false,
            cleaned: false,
        };

        let _ = entry_sender.send(entry);
    });
}

#[derive(PartialEq)]
enum AppMode {
    Normal,
    Search,
    Info,
}

struct App {
    entries: Vec<ProjectEntry>,
    visible_indices: Vec<usize>,
    list_state: ListState,
    sort_mode: SortMode,
    dry_run: bool,
    scanning: bool,
    total_bytes: u64,
    cleaned_bytes: u64,
    cleaned_count: usize,
    receiver: Receiver<ProjectEntry>,
    clean_done_send: Sender<(PathBuf, u64)>,
    clean_done_recv: Receiver<(PathBuf, u64)>,
    mode: AppMode,
    search_query: String,
}

impl App {
    fn new(
        receiver: Receiver<ProjectEntry>,
        sort_mode: SortMode,
        dry_run: bool,
    ) -> Self {
        let (clean_done_send, clean_done_recv) = std::sync::mpsc::channel();
        Self {
            entries: Vec::new(),
            visible_indices: Vec::new(),
            list_state: ListState::default(),
            sort_mode,
            dry_run,
            scanning: true,
            total_bytes: 0,
            cleaned_bytes: 0,
            cleaned_count: 0,
            receiver,
            clean_done_send,
            clean_done_recv,
            mode: AppMode::Normal,
            search_query: String::new(),
        }
    }

    fn update_visible(&mut self) {
        let q = self.search_query.to_lowercase();
        let q = q.trim();
        self.visible_indices = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                if q.is_empty() {
                    return true;
                }
                let path = e.project.path.to_string_lossy().to_lowercase();
                let name = e.display_name().to_lowercase();
                let type_name = e.project.type_name().to_lowercase();
                path.contains(q) || name.contains(q) || type_name.contains(q)
            })
            .map(|(i, _)| i)
            .collect();

        if let Some(s) = self.list_state.selected() {
            if s >= self.visible_indices.len() {
                if self.visible_indices.is_empty() {
                    self.list_state.select(None);
                } else {
                    self.list_state.select(Some(self.visible_indices.len() - 1));
                }
            }
        } else if !self.visible_indices.is_empty() {
            self.list_state.select(Some(0));
        }
    }

    fn selected_entry_idx(&self) -> Option<usize> {
        self.visible_indices
            .get(self.list_state.selected()?)
            .copied()
    }

    fn tick(&mut self) {
        let mut got_new = false;
        while let Ok(entry) = self.receiver.try_recv() {
            self.total_bytes += entry.artifact_bytes;
            self.entries.push(entry);
            got_new = true;
        }
        if got_new {
            self.sort_entries();
        }

        while let Ok((path, bytes)) = self.clean_done_recv.try_recv() {
            if let Some(entry) = self.entries.iter_mut().find(|e| e.project.path == path) {
                entry.cleaned = true;
                entry.cleaning = false;
                entry.marked = false;
                entry.artifact_bytes = 0;
                self.cleaned_bytes += bytes;
                self.cleaned_count += 1;
            }
        }

        let all_done = self.entries.iter().all(|e| e.cleaned);
        if !self.entries.is_empty() && all_done {
            self.scanning = false;
        }
    }

    fn sort_entries(&mut self) {
        let selected_path = self.selected_entry_idx().and_then(|i| {
            self.entries.get(i).map(|e| e.project.path.clone())
        });

        match self.sort_mode {
            SortMode::Size => {
                self.entries.sort_by(|a, b| b.artifact_bytes.cmp(&a.artifact_bytes));
            }
            SortMode::Date => {
                self.entries.sort_by(|a, b| b.last_modified_secs.cmp(&a.last_modified_secs));
            }
        }

        self.update_visible();

        if let Some(path) = selected_path {
            let new_visible = self
                .visible_indices
                .iter()
                .position(|&i| self.entries.get(i).map(|e| &e.project.path) == Some(&path));
            if new_visible.is_some() {
                self.list_state.select(new_visible);
            }
        }
    }

    fn toggle_mark(&mut self) {
        if let Some(idx) = self.selected_entry_idx() {
            if let Some(entry) = self.entries.get_mut(idx) {
                if !entry.cleaned && !entry.cleaning {
                    entry.marked = !entry.marked;
                }
            }
        }
    }

    fn mark_all(&mut self) {
        for &idx in &self.visible_indices {
            if let Some(entry) = self.entries.get_mut(idx) {
                if !entry.cleaned && !entry.cleaning {
                    entry.marked = true;
                }
            }
        }
    }

    fn unmark_all(&mut self) {
        for &idx in &self.visible_indices {
            if let Some(entry) = self.entries.get_mut(idx) {
                if !entry.cleaned && !entry.cleaning {
                    entry.marked = false;
                }
            }
        }
    }

    fn spawn_clean(&mut self, idx: usize) {
        let entry = match self.entries.get_mut(idx) {
            Some(e) => e,
            None => return,
        };
        if entry.cleaned || entry.cleaning {
            return;
        }
        entry.cleaning = true;
        entry.marked = false;
        let project = entry.project.clone();
        let path = entry.project.path.clone();
        let bytes = entry.artifact_bytes;
        let send = self.clean_done_send.clone();
        std::thread::spawn(move || {
            project.clean();
            let _ = send.send((path, bytes));
        });
    }

    fn clean_marked(&mut self) {
        if self.dry_run {
            return;
        }
        let indices: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.marked && !e.cleaned && !e.cleaning)
            .map(|(i, _)| i)
            .collect();
        for i in indices {
            self.spawn_clean(i);
        }
    }

    fn clean_selected(&mut self) {
        if self.dry_run {
            return;
        }
        if let Some(idx) = self.selected_entry_idx() {
            self.spawn_clean(idx);
        }
    }

    fn open_selected(&self) {
        if let Some(idx) = self.selected_entry_idx() {
            if let Some(entry) = self.entries.get(idx) {
                open_in_os(entry.project.path.clone());
            }
        }
    }

    fn move_up(&mut self) {
        if let Some(i) = self.list_state.selected() {
            if i > 0 {
                self.list_state.select(Some(i - 1));
            }
        }
    }

    fn move_down(&mut self) {
        if let Some(i) = self.list_state.selected() {
            if i < self.visible_indices.len().saturating_sub(1) {
                self.list_state.select(Some(i + 1));
            }
        }
    }

    fn enter_search(&mut self) {
        self.mode = AppMode::Search;
    }

    fn append_search(&mut self, c: char) {
        self.search_query.push(c);
        self.update_visible();
        if !self.visible_indices.is_empty() {
            self.list_state.select(Some(0));
        }
    }

    fn backspace_search(&mut self) {
        self.search_query.pop();
        self.update_visible();
        if !self.visible_indices.is_empty() {
            self.list_state.select(Some(0));
        }
    }

    fn cancel_search(&mut self) {
        self.search_query.clear();
        self.update_visible();
        self.mode = AppMode::Normal;
        if !self.visible_indices.is_empty() {
            self.list_state.select(Some(0));
        }
    }

    fn confirm_search(&mut self) {
        self.mode = AppMode::Normal;
    }

    fn open_info(&mut self) {
        if self.selected_entry_idx().is_some() {
            self.mode = AppMode::Info;
        }
    }

    fn close_modal(&mut self) {
        self.mode = AppMode::Normal;
    }

    fn toggle_sort(&mut self) {
        self.sort_mode = match self.sort_mode {
            SortMode::Size => SortMode::Date,
            SortMode::Date => SortMode::Size,
        };
        self.sort_entries();
    }
}

fn open_in_os(path: PathBuf) {
    std::thread::spawn(move || {
        let cmd = if cfg!(target_os = "macos") {
            "open"
        } else if cfg!(target_os = "windows") {
            "explorer"
        } else {
            "xdg-open"
        };
        let _ = std::process::Command::new(cmd).arg(&path).spawn();
    });
}

fn run_app(terminal: &mut Terminal<impl ratatui::backend::Backend>, mut app: App) -> io::Result<()> {
    loop {
        app.tick();

        terminal.draw(|f| {
            let size = f.area();

            let show_search_bar = app.mode == AppMode::Search || !app.search_query.is_empty();

            let constraints = if show_search_bar {
                vec![
                    Constraint::Length(1),
                    Constraint::Min(1),
                    Constraint::Length(1),
                    Constraint::Length(2),
                ]
            } else {
                vec![
                    Constraint::Length(1),
                    Constraint::Min(1),
                    Constraint::Length(2),
                ]
            };

            let chunks = Layout::vertical(constraints).split(size);

            render_header(f, &app, chunks[0]);
            render_list(
                f,
                &app.entries,
                &app.visible_indices,
                chunks[1],
                &mut app.list_state,
            );
            if show_search_bar {
                render_search_bar(f, &app, chunks[2]);
                render_footer(f, &app, chunks[3]);
            } else {
                render_footer(f, &app, chunks[2]);
            }

            if app.mode == AppMode::Info {
                render_info_popup(f, &app, size);
            }
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match app.mode {
                    AppMode::Search => match key.code {
                        KeyCode::Esc => app.cancel_search(),
                        KeyCode::Enter => app.confirm_search(),
                        KeyCode::Backspace => app.backspace_search(),
                        KeyCode::Char(c) => app.append_search(c),
                        _ => {}
                    },
                    AppMode::Info => match key.code {
                        KeyCode::Esc | KeyCode::Char('i') | KeyCode::Char('q') => {
                            app.close_modal()
                        }
                        _ => {}
                    },
                    AppMode::Normal => match key.code {
                        KeyCode::Char('q') => return Ok(()),
                        KeyCode::Esc => {
                            if !app.search_query.is_empty() {
                                app.cancel_search();
                            }
                        }
                        KeyCode::Char('/') => app.enter_search(),
                        KeyCode::Char('i') => app.open_info(),
                        KeyCode::Char('o') => app.open_selected(),
                        KeyCode::Char('j') | KeyCode::Down => app.move_down(),
                        KeyCode::Char('k') | KeyCode::Up => app.move_up(),
                        KeyCode::Char(' ') => app.toggle_mark(),
                        KeyCode::Char('a') => app.mark_all(),
                        KeyCode::Char('A') => app.unmark_all(),
                        KeyCode::Char('d') => app.clean_selected(),
                        KeyCode::Char('c') => app.clean_marked(),
                        KeyCode::Char('s') => app.toggle_sort(),
                        KeyCode::Char('G') => {
                            if !app.visible_indices.is_empty() {
                                app.list_state
                                    .select(Some(app.visible_indices.len() - 1));
                            }
                        }
                        KeyCode::Char('g') => {
                            if !app.visible_indices.is_empty() {
                                app.list_state.select(Some(0));
                            }
                        }
                        _ => {}
                    },
                }
            }
        }
    }
}

fn render_header(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let sort_label = match app.sort_mode {
        SortMode::Size => "size",
        SortMode::Date => "date",
    };
    let total = app.entries.len();
    let visible = app.visible_indices.len();
    let total_size = pretty_size(app.total_bytes - app.cleaned_bytes);

    let marked_bytes: u64 = app
        .entries
        .iter()
        .filter(|e| e.marked && !e.cleaned)
        .map(|e| e.artifact_bytes)
        .sum();

    let marked_count = app.entries.iter().filter(|e| e.marked && !e.cleaned).count();
    let cleaning_count = app.entries.iter().filter(|e| e.cleaning).count();

    let count_text = if visible != total {
        format!(" {visible}/{total} project(s) ")
    } else {
        format!(" {total} project(s) ")
    };

    let mut spans = vec![
        Span::styled(
            count_text,
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {} ", total_size),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
    ];

    if marked_count > 0 {
        spans.push(Span::styled(
            format!(" → {marked_count} marked: {} ", pretty_size(marked_bytes)),
            Style::default().fg(Color::LightRed).add_modifier(Modifier::BOLD),
        ));
    }

    if cleaning_count > 0 {
        spans.push(Span::styled(
            format!(" ⏳ {cleaning_count} cleaning… "),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
    }

    spans.push(Span::styled(
        format!(" sort:{} ", sort_label),
        Style::default().fg(Color::DarkGray),
    ));

    if app.dry_run {
        spans.push(Span::styled(" dry-run ", Style::default().fg(Color::Red)));
    }

    let line = Line::from(spans);

    f.render_widget(Paragraph::new(line), area);
}

fn render_list(
    f: &mut ratatui::Frame,
    entries: &[ProjectEntry],
    visible_indices: &[usize],
    area: Rect,
    list_state: &mut ListState,
) {
    let total = visible_indices.len();

    let items: Vec<ListItem> = visible_indices
        .iter()
        .enumerate()
        .filter_map(|(i, &entry_idx)| {
            let entry = entries.get(entry_idx)?;
            let idx = i + 1;
            let name = entry.display_name();
            let type_name = entry.project.type_name();
            let size_str = if entry.cleaned {
                "cleaned".to_string()
            } else if entry.cleaning {
                "cleaning…".to_string()
            } else {
                pretty_size(entry.artifact_bytes)
            };
            let age = &entry.last_modified_str;

            let marker = if entry.cleaned {
                "✓"
            } else if entry.cleaning {
                "…"
            } else if entry.marked {
                "x"
            } else {
                " "
            };

            let marker_style = if entry.cleaned {
                Style::default().fg(Color::Green)
            } else if entry.cleaning {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else if entry.marked {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let size_color = if entry.cleaned {
                Color::DarkGray
            } else {
                Color::Yellow
            };

            let count_style = Style::default().fg(Color::DarkGray);
            let name_style = if entry.cleaned {
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::CROSSED_OUT)
            } else {
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
            };
            let type_style = Style::default().fg(Color::Cyan);
            let age_style = Style::default().fg(Color::DarkGray);

            let line = Line::from(vec![
                Span::styled(format!("[{marker}] "), marker_style),
                Span::styled(format!("[{idx}/{total}] "), count_style),
                Span::styled(format!("{name} "), name_style),
                Span::styled(format!("{type_name} "), type_style),
                Span::styled(format!("({age}) "), age_style),
                Span::styled(size_str, Style::default().fg(size_color)),
            ]);

            Some(ListItem::new(line))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::NONE))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("");

    f.render_stateful_widget(list, area, list_state);
}

fn render_footer(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let key = |k: &'static str| Span::styled(format!(" {k} "), Style::default().fg(Color::Cyan));
    let shortcuts = vec![
        key("j/k"),
        Span::raw("nav  "),
        key("space"),
        Span::raw("mark  "),
        key("d"),
        Span::raw("delete  "),
        key("c"),
        Span::raw("clean marked  "),
        key("/"),
        Span::raw("search  "),
        key("i"),
        Span::raw("info  "),
        key("o"),
        Span::raw("open  "),
        key("s"),
        Span::raw("sort  "),
        key("q"),
        Span::raw("quit"),
    ];

    let line1 = Line::from(shortcuts);

    let summary = if app.cleaned_count > 0 {
        format!(
            " cleaned: {} · freed: {}",
            app.cleaned_count,
            pretty_size(app.cleaned_bytes),
        )
    } else {
        String::new()
    };

    let line2 = Line::from(vec![Span::styled(
        summary,
        Style::default().fg(Color::Green),
    )]);

    f.render_widget(Paragraph::new(vec![line1, line2]), area);
}

fn render_search_bar(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let (label, label_style) = if app.mode == AppMode::Search {
        (" search: ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD))
    } else {
        (" filter: ", Style::default().fg(Color::Black).bg(Color::DarkGray).add_modifier(Modifier::BOLD))
    };

    let cursor = if app.mode == AppMode::Search { "_" } else { "" };

    let line = Line::from(vec![
        Span::styled(label, label_style),
        Span::raw(" "),
        Span::styled(
            app.search_query.clone(),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::styled(cursor, Style::default().fg(Color::White).add_modifier(Modifier::SLOW_BLINK)),
        Span::raw("  "),
        if app.mode == AppMode::Search {
            Span::styled(
                "[Enter] confirm  [Esc] cancel",
                Style::default().fg(Color::DarkGray),
            )
        } else {
            Span::styled(
                "[Esc] clear filter  [/] edit",
                Style::default().fg(Color::DarkGray),
            )
        },
    ]);

    f.render_widget(Paragraph::new(line), area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(r);

    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(popup_layout[1])[1]
}

fn render_info_popup(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let entry = match app.selected_entry_idx().and_then(|i| app.entries.get(i)) {
        Some(e) => e,
        None => return,
    };

    let path = &entry.project.path;
    let name = entry.display_name();
    let type_name = entry.project.type_name();
    let parents: Vec<String> = path
        .ancestors()
        .skip(1)
        .filter_map(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .collect();
    let breadcrumb = if parents.is_empty() {
        path.to_string_lossy().to_string()
    } else {
        let mut crumbs: Vec<String> = parents.into_iter().rev().collect();
        crumbs.push(name.clone());
        crumbs.join(" › ")
    };

    let status = if entry.cleaned {
        ("cleaned", Color::Green)
    } else if entry.cleaning {
        ("cleaning…", Color::Yellow)
    } else if entry.marked {
        ("marked", Color::LightRed)
    } else {
        ("pending", Color::DarkGray)
    };

    let label = |s: &'static str| Span::styled(s, Style::default().fg(Color::DarkGray));
    let val = |s: String| Span::styled(s, Style::default().fg(Color::White).add_modifier(Modifier::BOLD));

    let mut lines: Vec<Line> = vec![
        Line::from(vec![label("Project   "), val(name.clone())]),
        Line::from(vec![label("Type      "), Span::styled(type_name.to_string(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))]),
        Line::from(vec![label("Path      "), val(path.to_string_lossy().to_string())]),
        Line::from(vec![label("Location  "), Span::styled(breadcrumb, Style::default().fg(Color::Yellow))]),
        Line::from(""),
        Line::from(vec![
            label("Last mod  "),
            val(if entry.last_modified_str.is_empty() { "unknown".to_string() } else { entry.last_modified_str.clone() }),
        ]),
        Line::from(vec![
            label("Size      "),
            Span::styled(
                pretty_size(entry.artifact_bytes),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![label("Status    "), Span::styled(status.0.to_string(), Style::default().fg(status.1).add_modifier(Modifier::BOLD))]),
        Line::from(""),
        Line::from(vec![label("Artifact dirs:")]),
    ];

    for d in entry.project.artifact_dirs() {
        let p = path.join(d);
        let exists = p.exists();
        let marker = if exists { "•" } else { "·" };
        let style = if exists {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{marker} {d}"), style),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Press [Esc] or [i] to close ",
        Style::default().fg(Color::DarkGray),
    )));

    let popup = centered_rect(70, 70, area);
    f.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " Project Info ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(Color::Cyan));

    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });

    f.render_widget(para, popup);
}

fn print_completions<G: Generator>(gen: G, cmd: &mut Command) {
    generate(gen, cmd, cmd.get_name().to_string(), &mut io::stdout());
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut opt = Opt::parse();

    if let Some(generator) = opt.generator {
        let mut cmd = Opt::command();
        eprintln!("Generating completion file for {generator:?}...");
        print_completions(generator, &mut cmd);
        return Ok(());
    }

    let dirs = prepare_directories(opt.dirs)?;

    let scan_options = ScanOptions {
        follow_symlinks: opt.follow_symlinks,
        same_file_system: opt.same_filesystem,
    };

    let project_min_age = opt.older;
    let ignored_dirs = {
        let cd = current_dir()?;
        std::mem::take(&mut opt.ignored_dirs)
            .into_iter()
            .map(|dir| path_canonicalise(&cd, dir))
            .collect::<Result<Vec<_>, _>>()?
    };

    let (entry_send, entry_recv) = std::sync::mpsc::channel::<ProjectEntry>();

    std::thread::spawn(move || {
        discover(dirs, &scan_options, project_min_age, entry_send, &ignored_dirs);
    });

    let sort_mode = opt.sort;
    let dry_run = opt.dry_run;
    let app = App::new(entry_recv, sort_mode, dry_run);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, app);

    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result?;

    Ok(())
}