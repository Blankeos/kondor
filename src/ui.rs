//! Rendering. Pure function of `App` (apart from scroll bookkeeping).

use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Padding, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Table, TableState, Wrap,
};
use ratatui::Frame;

use crate::app::{App, Entry, Mode, SizeState, Status, Tone};
use crate::cli::SortBy;
use crate::format::{self, plural, truncate, truncate_left};
use crate::theme::Theme;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const BAR_WIDTH: u16 = 10;
const SIZE_W: u16 = 9;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area().inner(ratatui::layout::Margin { horizontal: 1, vertical: 0 });
    let [header, _, body, footer] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Fill(1), Constraint::Length(3)])
            .areas(area);

    app.hit = Default::default();
    draw_header(f, app, header);
    draw_body(f, app, body);
    draw_footer(f, app, footer);

    app.hit.modal = match app.mode.clone() {
        Mode::Info => draw_info(f, app, area),
        Mode::Help => Some(draw_help(f, app, area)),
        Mode::Confirm(ids) => Some(draw_confirm(f, app, &ids, area)),
        Mode::Normal | Mode::Filter => None,
    };
}

fn spinner(app: &App) -> &'static str {
    SPINNER[app.spin % SPINNER.len()]
}

// ----- header -------------------------------------------------------------

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let roots = app.roots.iter().map(|r| format::tilde(r, app.home.as_deref())).collect::<Vec<_>>().join(", ");
    let mut left = vec![Span::styled(" kondor ", t.badge()), Span::raw("  "), Span::styled(roots, t.dim())];
    if app.dry_run {
        left.push(Span::raw("  "));
        left.push(Span::styled(" DRY RUN ", t.danger_badge()));
    }

    let dirs = format::count(app.dirs_scanned());
    let remaining = app.unsized_count();
    let pct = app.sized_percent();
    let right = if app.walking {
        Line::from(vec![
            Span::styled(spinner(app), t.accent()),
            Span::styled(format!(" Scanning · {dirs} folders"), t.dim()),
        ])
    } else if app.scanning {
        Line::from(vec![
            Span::styled(spinner(app), t.accent()),
            Span::styled(format!(" Sizing · {pct}% · {remaining} remaining · {dirs} folders"), t.dim()),
        ])
    } else {
        let secs = app.scan_time.map(|d| format!(" in {:.1}s", d.as_secs_f64())).unwrap_or_default();
        Line::from(vec![Span::styled("✓", t.good()), Span::styled(format!(" Scanned {dirs} folders{secs}"), t.dim())])
    };
    let right_w = right.width() as u16;
    let [l, r] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(right_w)]).spacing(2).areas(area);
    f.render_widget(Line::from(left), l);
    f.render_widget(right.alignment(Alignment::Right), r);
}

// ----- table --------------------------------------------------------------

fn draw_body(f: &mut Frame, app: &mut App, area: Rect) {
    if app.view.is_empty() {
        return draw_empty(f, app, area);
    }
    let t = app.theme;
    let w = area.width;
    let show_bar = w >= 96;
    let show_loc = w >= 72;
    let show_type = w >= 58;

    let rows_h = (area.height.saturating_sub(1) as usize).max(1);
    app.page = rows_h;
    app.cursor = app.cursor.min(app.view.len() - 1);
    if app.follow {
        if app.cursor < app.offset {
            app.offset = app.cursor;
        } else if app.cursor >= app.offset + rows_h {
            app.offset = app.cursor + 1 - rows_h;
        }
    }
    app.offset = app.offset.min(app.view.len().saturating_sub(rows_h));
    let window = &app.view[app.offset..(app.offset + rows_h).min(app.view.len())];

    let name_w = window.iter().map(|&i| app.entries[i].name.chars().count()).max().unwrap_or(8).clamp(8, 32) as u16;
    let type_w = window.iter().map(|&i| app.entries[i].kinds.chars().count()).max().unwrap_or(4).clamp(4, 18) as u16;

    let mut widths = vec![Constraint::Length(2), Constraint::Length(SIZE_W)];
    let mut head = vec![Cell::from(""), header_cell(&t, "SIZE", app.sort == SortBy::Size, true)];
    if show_bar {
        widths.push(Constraint::Length(BAR_WIDTH));
        head.push(Cell::from(""));
        widths.push(Constraint::Length(4));
        head.push(header_cell(&t, "%", false, true));
    }
    widths.push(if show_loc { Constraint::Length(name_w) } else { Constraint::Fill(1) });
    head.push(header_cell(&t, "PROJECT", app.sort == SortBy::Name, false));
    if show_loc {
        widths.push(Constraint::Fill(1));
        head.push(header_cell(&t, "LOCATION", false, false));
    }
    if show_type {
        widths.push(Constraint::Length(type_w));
        head.push(header_cell(&t, "TYPE", false, false));
    }
    widths.push(Constraint::Length(4));
    head.push(header_cell(&t, "AGE", app.sort == SortBy::Age, true));

    // Location column width, to truncate from the left (keep the end of paths).
    let fixed: u16 =
        widths.iter().map(|c| if let Constraint::Length(n) = c { *n } else { 0 }).sum::<u16>() + widths.len() as u16; // column spacing + scrollbar
    let loc_w = w.saturating_sub(fixed) as usize;

    // None when the wheel has scrolled the selection out of view.
    let selected = app.cursor.checked_sub(app.offset).filter(|&s| s < window.len());
    let rows: Vec<Row> = window
        .iter()
        .enumerate()
        .map(|(n, &i)| {
            let e = &app.entries[i];
            let mut cells = vec![Cell::from(mark_cell(app, e, Some(n) == selected)), Cell::from(size_cell(app, e))];
            if show_bar {
                cells.push(Cell::from(bar(&t, e, app.max_bytes)));
                let pct = if e.size == SizeState::Fresh { format::percent(e.bytes, app.total_bytes) } else { None };
                let pct_style = if pct.as_deref().is_some_and(|p| p != "<1%") { t.dim() } else { t.faint() };
                let pct = pct.unwrap_or_else(|| "–".into());
                cells.push(Cell::from(Line::from(Span::styled(pct, pct_style)).alignment(Alignment::Right)));
            }
            let name_style = if e.is_ready() { t.strong() } else { t.dim() };
            cells.push(Cell::from(Span::styled(truncate(&e.name, name_w as usize), name_style)));
            if show_loc {
                cells.push(Cell::from(Span::styled(truncate_left(&e.location, loc_w), t.faint())));
            }
            if show_type {
                cells.push(Cell::from(Span::styled(truncate(&e.kinds, type_w as usize), t.dim())));
            }
            let age = e.age.map(format::age_short).unwrap_or_else(|| "–".into());
            cells.push(Cell::from(Line::from(Span::styled(age, t.dim())).alignment(Alignment::Right)));
            Row::new(cells)
        })
        .collect();

    let table_area = Rect { width: area.width.saturating_sub(1), ..area };
    // Same layout the table uses internally, to map header clicks to columns.
    let cols = Layout::horizontal(widths.clone()).flex(Flex::Start).spacing(1).split(table_area);
    let project_col = if show_bar { 4 } else { 2 };
    let header_cell_rect = |i: usize| Rect { height: 1, ..cols[i] };
    app.hit.sort_cols = vec![
        (header_cell_rect(1), SortBy::Size),
        (header_cell_rect(project_col), SortBy::Name),
        (header_cell_rect(cols.len() - 1), SortBy::Age),
    ];
    app.hit.rows = Rect { y: area.y + 1, height: window.len() as u16, ..table_area };

    let table = Table::new(rows, widths)
        .header(Row::new(head))
        .column_spacing(1)
        .flex(Flex::Start)
        .row_highlight_style(t.selected());
    let mut state = TableState::default().with_selected(selected);
    f.render_stateful_widget(table, table_area, &mut state);

    if app.view.len() > rows_h {
        let mut sb = ScrollbarState::new(app.view.len().saturating_sub(rows_h)).position(app.offset);
        let sb_area = Rect { y: area.y + 1, height: area.height.saturating_sub(1), ..area };
        app.hit.scrollbar = Some(Rect { x: area.right().saturating_sub(1), width: 1, ..sb_area });
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .track_style(t.faint())
                .thumb_symbol("┃")
                .thumb_style(t.dim()),
            sb_area,
            &mut sb,
        );
    }
}

fn header_cell<'a>(t: &Theme, label: &'a str, sorted: bool, right: bool) -> Cell<'a> {
    let style = if sorted { t.accent().add_modifier(Modifier::BOLD) } else { t.faint() };
    let line = Line::from(Span::styled(label, style));
    Cell::from(if right { line.alignment(Alignment::Right) } else { line })
}

fn mark_cell<'a>(app: &App, e: &Entry, selected: bool) -> Line<'a> {
    let t = &app.theme;
    let gutter = if selected { Span::styled("▌", t.accent()) } else { Span::raw(" ") };
    let mark = match &e.status {
        Status::Cleaning => Span::styled(spinner(app), t.accent()),
        Status::Cleaned => Span::styled("✓", t.good()),
        Status::Failed(_) if !e.marked => Span::styled("!", t.bad()),
        _ if e.marked => Span::styled("●", t.accent()),
        _ => Span::styled("○", t.faint()),
    };
    Line::from(vec![gutter, mark])
}

fn size_cell<'a>(app: &App, e: &Entry) -> Line<'a> {
    let t = &app.theme;
    let sizing = app.scanning && !e.gone && e.size != SizeState::Fresh && e.status == Status::Ready;
    if e.size == SizeState::Pending {
        let span = if sizing { Span::styled(spinner(app), t.accent()) } else { Span::styled("…", t.faint()) };
        return Line::from(span).alignment(Alignment::Right);
    }
    let text = format::size(e.bytes);
    let style = match e.status {
        Status::Cleaning => t.dim(),
        Status::Cleaned => t.good().add_modifier(Modifier::CROSSED_OUT),
        Status::Failed(_) => t.bad(),
        Status::Ready if e.bytes >= 1_000_000_000 => t.accent().add_modifier(Modifier::BOLD),
        Status::Ready if e.bytes >= 100_000_000 => t.text(),
        Status::Ready => t.dim(),
    };
    // Last run's number: shown, but quieter until it's re-measured.
    let style = if e.size == SizeState::Stale && e.status == Status::Ready { t.faint() } else { style };
    // Re-measuring: spinner ahead of the last known size (when it fits the column).
    if sizing && text.chars().count() + 2 <= SIZE_W as usize {
        let line = Line::from(vec![Span::styled(spinner(app), t.accent()), Span::styled(format!(" {text}"), style)]);
        return line.alignment(Alignment::Right);
    }
    Line::from(Span::styled(text, style)).alignment(Alignment::Right)
}

/// A size for display; `…` while unmeasured.
fn shown(e: &Entry, bytes: u64) -> String {
    if e.size == SizeState::Pending {
        "…".into()
    } else {
        format::size(bytes)
    }
}

fn bar<'a>(t: &Theme, e: &Entry, max: u64) -> Span<'a> {
    let bytes = if e.size == SizeState::Pending { 0 } else { e.bytes };
    let live = e.is_ready() && e.size == SizeState::Fresh;
    Span::styled(bar_text(bytes, max, BAR_WIDTH as usize), if live { t.accent() } else { t.faint() })
}

fn bar_text(bytes: u64, max: u64, width: usize) -> String {
    const EIGHTHS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    if max == 0 || bytes == 0 {
        return String::new();
    }
    let units = ((bytes as f64 / max as f64) * (width * 8) as f64).round().max(1.0) as usize;
    let units = units.min(width * 8);
    format!("{}{}", "█".repeat(units / 8), EIGHTHS[units % 8])
}

fn draw_empty(f: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let mut lines: Vec<Line> = Vec::new();
    if !app.filter.is_empty() && !app.entries.is_empty() {
        lines.push(Line::styled(format!("No projects match “{}”", app.filter), t.strong()));
        lines.push(Line::styled("Press esc to clear the filter.", t.dim()));
    } else if app.scanning {
        lines.push(Line::from(vec![
            Span::styled(spinner(app), t.accent()),
            Span::styled(" Looking for build leftovers…", t.strong()),
        ]));
        lines.push(Line::styled("Projects show up here as soon as they're found.", t.dim()));
    } else {
        lines.push(Line::styled("✓ All clean", t.good().add_modifier(Modifier::BOLD)));
        lines.push(Line::styled("No build artifacts found here.", t.dim()));
        if app.hidden_recent > 0 {
            lines.push(Line::styled(
                format!(
                    "{} newer than your --older limit {} hidden.",
                    plural(app.hidden_recent, "project", "projects"),
                    if app.hidden_recent == 1 { "is" } else { "are" }
                ),
                t.faint(),
            ));
        }
        lines.push(Line::default());
        lines.push(Line::styled("Press q to quit.", t.faint()));
    }
    let h = lines.len() as u16;
    let [mid] = Layout::vertical([Constraint::Length(h)]).flex(Flex::Center).areas(area);
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), mid);
}

// ----- footer -------------------------------------------------------------

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let block = Block::new().borders(Borders::TOP).border_style(t.faint());
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [status, keys] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);

    // Status line: toast, or totals. Marked summary on the right.
    let left = if let Some(toast) = &app.toast {
        let (icon, style) = match toast.tone {
            Tone::Good => ("✓ ", t.good()),
            Tone::Bad => ("! ", t.bad()),
            Tone::Info => ("· ", t.dim()),
        };
        Line::from(vec![Span::styled(icon, style), Span::styled(toast.text.clone(), style)])
    } else {
        let (n, bytes) = app.reclaimable();
        let mut spans = Vec::new();
        if !app.filter.is_empty() {
            spans.push(Span::styled(format!("{} of {} shown", app.view.len(), app.entries.len()), t.dim()));
            spans.push(Span::styled(" · ", t.faint()));
        }
        if n > 0 || app.freed == 0 {
            spans.push(Span::styled(format::size(bytes), t.accent().add_modifier(Modifier::BOLD)));
            spans.push(Span::styled(format!(" reclaimable in {}", plural(n, "project", "projects")), t.dim()));
        }
        if app.freed > 0 {
            if n > 0 {
                spans.push(Span::styled(" · ", t.faint()));
            }
            let verb = if app.dry_run { "would free" } else { "freed" };
            spans.push(Span::styled(format!("{verb} {}", format::size(app.freed)), t.good()));
        }
        Line::from(spans)
    };

    let (marked, marked_bytes) = app.marked();
    let right = if marked > 0 {
        Line::from(vec![
            Span::styled(
                format!("{marked} marked · {}", format::size(marked_bytes)),
                t.accent().add_modifier(Modifier::BOLD),
            ),
            Span::styled("  d", t.accent()),
            Span::styled(" delete", t.dim()),
        ])
    } else {
        Line::default()
    };
    let rw = right.width() as u16;
    let [l, r] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(rw)]).spacing(2).areas(status);
    f.render_widget(left, l);
    f.render_widget(right.alignment(Alignment::Right), r);

    // Key hints.
    if app.mode == Mode::Filter {
        let line = Line::from(vec![
            Span::styled("/ ", t.accent()),
            Span::raw(app.filter.clone()),
            Span::styled("▏", t.accent()),
            Span::styled("   enter", t.accent()),
            Span::styled(" done  ", t.dim()),
            Span::styled("esc", t.accent()),
            Span::styled(" clear", t.dim()),
        ]);
        f.render_widget(line, keys);
        return;
    }
    let sort = match app.sort {
        SortBy::Size => "sort: size",
        SortBy::Age => "sort: age",
        SortBy::Name => "sort: name",
    };
    let filter_hint = if app.filter.is_empty() { "filter" } else { "edit filter" };
    let hints: &[(&str, &str)] = match app.mode {
        Mode::Info => {
            &[("↑↓", "prev/next"), ("space", "mark"), ("d", "delete"), ("o", "open folder"), ("esc", "close")]
        }
        Mode::Help => &[("esc", "close")],
        Mode::Confirm(_) => &[("y", "delete"), ("n", "cancel")],
        _ => &[
            ("↑↓", "move"),
            ("space", "mark"),
            ("a", "mark all"),
            ("d", "delete"),
            ("/", filter_hint),
            ("s", sort),
            ("i", "details"),
            ("?", "help"),
            ("q", "quit"),
        ],
    };
    let mut spans = Vec::new();
    let mut used = 0usize;
    for (key, label) in hints {
        let w = key.chars().count() + label.chars().count() + 3;
        if used + w > keys.width as usize {
            break;
        }
        used += w;
        spans.push(Span::styled(*key, t.accent()));
        spans.push(Span::styled(format!(" {label}   "), t.dim()));
    }
    f.render_widget(Line::from(spans), keys);
}

// ----- modals -------------------------------------------------------------

/// Draw a centred popup frame. Returns `(outer, inner)`.
fn modal(f: &mut Frame, area: Rect, width: u16, height: u16, title: Line, border: Style) -> (Rect, Rect) {
    let w = width.min(area.width.saturating_sub(2));
    let h = (height + 2).min(area.height.saturating_sub(2));
    let [v] = Layout::vertical([Constraint::Length(h)]).flex(Flex::Center).areas(area);
    let [rect] = Layout::horizontal([Constraint::Length(w)]).flex(Flex::Center).areas(v);
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(title)
        .padding(Padding::horizontal(1));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    (rect, inner)
}

fn draw_info(f: &mut Frame, app: &App, area: Rect) -> Option<Rect> {
    let e = app.selected()?;
    let t = &app.theme;
    let label = |s: &'static str| Span::styled(format!("{s:<12}"), t.faint());
    let status = match &e.status {
        Status::Ready if e.size == SizeState::Pending => Span::styled("Measuring…", t.dim()),
        Status::Ready if e.size == SizeState::Stale => Span::styled("Ready to clean (size from last run)", t.text()),
        Status::Ready if e.marked => Span::styled("Marked for deletion", t.accent()),
        Status::Ready => Span::styled("Ready to clean", t.text()),
        Status::Cleaning => Span::styled("Deleting…", t.accent()),
        Status::Cleaned if app.dry_run => Span::styled("Cleaned (dry run, nothing deleted)", t.good()),
        Status::Cleaned => Span::styled("Cleaned", t.good()),
        Status::Failed(msg) => Span::styled(format!("Failed: {msg}"), t.bad()),
    };
    let built = e.age.map(format::age_long).unwrap_or_else(|| "unknown".into());

    let width = 66u16;
    let inner_w = (width - 4) as usize;
    let mut lines = vec![
        Line::from(Span::styled(format::tilde(&e.path, app.home.as_deref()), t.dim())),
        Line::default(),
        Line::from(vec![label("Type"), Span::raw(e.kinds.clone())]),
        Line::from(vec![label("Last build"), Span::raw(built)]),
        Line::from(vec![label("Status"), status]),
        Line::default(),
        Line::from(Span::styled("WILL DELETE", t.faint())),
    ];
    let max = e.artifacts.first().map(|a| a.bytes).unwrap_or(0);
    let name_w = inner_w.saturating_sub(9 + 2 + 12);
    for a in &e.artifacts {
        lines.push(Line::from(vec![
            Span::raw(format!("{:<name_w$}", truncate(a.rel, name_w))),
            Span::styled(format!("{:>9}", shown(e, a.bytes)), t.strong()),
            Span::raw("  "),
            Span::styled(bar_text(a.bytes, max, 12), t.accent()),
        ]));
    }
    lines.push(Line::from(Span::styled("─".repeat(name_w + 9), t.faint())));
    lines.push(Line::from(vec![
        Span::styled(format!("{:<name_w$}", "Total"), t.dim()),
        Span::styled(format!("{:>9}", shown(e, e.bytes)), t.accent().add_modifier(Modifier::BOLD)),
    ]));

    let title = Line::from(Span::styled(format!(" {} ", e.name), t.strong()));
    let (outer, inner) = modal(f, area, width, lines.len() as u16, title, t.faint());
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    Some(outer)
}

fn draw_help(f: &mut Frame, app: &App, area: Rect) -> Rect {
    let t = &app.theme;
    let key = |k: &'static str, what: &'static str| {
        Line::from(vec![Span::styled(format!("{k:<14}"), t.accent()), Span::raw(what)])
    };
    let lines = vec![
        key("↑ ↓  j k", "Move"),
        key("PgUp PgDn", "Move a page"),
        key("g G", "Jump to top / bottom"),
        key("space", "Mark or unmark"),
        key("a", "Mark everything shown (again to unmark)"),
        key("d", "Delete marked (or the selected project)"),
        key("/", "Filter by name, folder or type"),
        key("esc", "Clear the filter, then the marks"),
        key("s", "Sort by size → age → name"),
        key("i  enter", "Details"),
        key("o", "Open the project folder"),
        key("q", "Quit"),
        key("wheel", "Scroll without moving the selection"),
        key("click", "Select (on ○ to mark), twice for details"),
        key("headers", "Click SIZE, PROJECT or AGE to sort"),
        Line::default(),
        Line::from(Span::styled("WHAT GETS DELETED", t.faint())),
        Line::raw("Only known build-output folders such as node_modules,"),
        Line::raw("target, build or Pods: things your tools regenerate."),
        Line::raw("Source code and virtualenvs are never touched."),
        Line::from(Span::styled("Deleting is permanent; nothing goes to the Trash.", t.dim())),
        Line::default(),
        Line::from(Span::styled("AGE is the time since the build output last changed.", t.dim())),
    ];
    let title = Line::from(Span::styled(" Help ", t.strong()));
    let (outer, inner) = modal(f, area, 62, lines.len() as u16, title, t.faint());
    f.render_widget(Paragraph::new(lines), inner);
    outer
}

fn draw_confirm(f: &mut Frame, app: &App, ids: &[usize], area: Rect) -> Rect {
    let t = &app.theme;
    let total: u64 = ids.iter().map(|&i| app.entries[i].bytes).sum();
    let width = 64u16;
    let inner_w = (width - 4) as usize;

    let mut lines = vec![Line::from(vec![
        Span::raw(if app.dry_run { "Pretend to delete " } else { "Permanently delete " }),
        Span::styled(format::size(total), t.accent().add_modifier(Modifier::BOLD)),
        Span::raw(" of build output from "),
        Span::styled(plural(ids.len(), "project", "projects"), t.strong()),
        Span::raw("?"),
    ])];
    lines.push(Line::default());
    const SHOW: usize = 6;
    let name_w = 18usize;
    let dirs_w = inner_w.saturating_sub(name_w + 9 + 2);
    for &i in ids.iter().take(if ids.len() > SHOW { SHOW - 1 } else { SHOW }) {
        let e = &app.entries[i];
        let dirs = e.artifacts.iter().map(|a| a.rel).collect::<Vec<_>>().join(", ");
        lines.push(Line::from(vec![
            Span::styled(format!("{:<name_w$} ", truncate(&e.name, name_w)), t.strong()),
            Span::styled(format!("{:<dirs_w$}", truncate(&dirs, dirs_w)), t.dim()),
            Span::raw(format!(" {:>9}", format::size(e.bytes))),
        ]));
    }
    if ids.len() > SHOW {
        lines.push(Line::styled(format!("…and {} more", ids.len() - (SHOW - 1)), t.dim()));
    }
    lines.push(Line::default());
    if app.dry_run {
        lines.push(Line::styled("Dry run: nothing will actually be deleted.", t.good()));
    } else {
        lines.push(Line::styled("Your tools can rebuild these (cargo build, npm install…).", t.dim()));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled(" y ", t.danger_badge()),
        Span::styled(if app.dry_run { " Run" } else { " Delete" }, t.strong()),
        Span::raw("     "),
        Span::styled("n", t.accent()),
        Span::styled(" Cancel", t.dim()),
    ]));

    let title = Line::from(Span::styled(
        if app.dry_run { " Clean up (dry run) " } else { " Delete build output " },
        t.bad().add_modifier(Modifier::BOLD),
    ));
    let (outer, inner) = modal(f, area, width, lines.len() as u16, title, t.bad());
    f.render_widget(Paragraph::new(lines), inner);
    outer
}

#[cfg(test)]
mod tests {
    use super::bar_text;

    #[test]
    fn bars_scale_and_never_vanish() {
        assert_eq!(bar_text(100, 100, 4), "████");
        assert_eq!(bar_text(50, 100, 4), "██");
        assert_eq!(bar_text(1, 1_000_000, 4), "▏");
        assert_eq!(bar_text(0, 100, 4), "");
    }
}
