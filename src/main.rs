mod app;
mod clean;
mod cli;
mod format;
mod project;
mod scan;
mod theme;
mod ui;

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use clap::{CommandFactory, Parser};
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::app::{App, Config};
use crate::cli::Args;
use crate::scan::Progress;

fn main() -> ExitCode {
    let args = Args::parse();

    if let Some(shell) = args.completions {
        clap_complete::generate(shell, &mut Args::command(), "kondor", &mut io::stdout());
        return ExitCode::SUCCESS;
    }

    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("kondor: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<(), String> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    let roots = resolve_roots(&args.paths)?;

    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        return Err("needs an interactive terminal".into());
    }

    let mut skip: Vec<PathBuf> = if args.all { Vec::new() } else { scan::builtin_skips(home.as_deref()) };
    for p in &args.ignore {
        skip.push(p.canonicalize().unwrap_or_else(|_| p.clone()));
    }
    // Never skip something the user explicitly asked to scan.
    skip.retain(|s| !roots.iter().any(|r| r == s));

    let opts = scan::Options { follow_links: args.follow_links, same_fs: args.same_fs, skip };
    let progress = Arc::new(Progress::default());
    let (tx, rx) = mpsc::channel();
    {
        let roots = roots.clone();
        let progress = Arc::clone(&progress);
        std::thread::spawn(move || scan::run(&roots, &opts, &tx, &progress));
    }

    let cfg = Config { roots, home, sort: args.sort, dry_run: args.dry_run, min_age: args.older };
    let mut app = App::new(cfg, rx, progress);

    let mut terminal = ratatui::init();
    set_mouse(true);
    // ratatui's panic hook restores the screen; also release the mouse.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        set_mouse(false);
        prev_hook(info);
    }));
    let result = event_loop(&mut terminal, &mut app);
    set_mouse(false);
    ratatui::restore();
    result.map_err(|e| e.to_string())?;

    if app.freed > 0 {
        let verb = if app.dry_run { "Would have freed" } else { "Freed" };
        println!(
            "{verb} {} from {}.",
            format::size(app.freed),
            format::plural(app.freed_projects, "project", "projects")
        );
    }
    Ok(())
}

fn resolve_roots(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let given: Vec<PathBuf> = if paths.is_empty() { vec![PathBuf::from(".")] } else { paths.to_vec() };
    let mut roots: Vec<PathBuf> = Vec::with_capacity(given.len());
    for p in given {
        let abs = p.canonicalize().map_err(|e| format!("{}: {e}", p.display()))?;
        if !abs.is_dir() {
            return Err(format!("{}: not a directory", p.display()));
        }
        roots.push(abs);
    }
    // Drop roots nested inside other roots so nothing is scanned twice.
    roots.sort();
    roots.dedup();
    let all = roots.clone();
    roots.retain(|r| !all.iter().any(|o| o != r && is_within(r, o)));
    Ok(roots)
}

fn is_within(path: &Path, parent: &Path) -> bool {
    path.starts_with(parent)
}

/// Mouse reporting: clicks, wheel and button-held drags (modes 1000/1002 with
/// SGR encoding). Unlike crossterm's `EnableMouseCapture` this leaves out
/// mode 1003, so plain pointer movement doesn't flood the event loop.
#[cfg(not(windows))]
fn set_mouse(on: bool) {
    use std::io::Write;
    let seq: &[u8] = if on { b"\x1b[?1000h\x1b[?1002h\x1b[?1006h" } else { b"\x1b[?1006l\x1b[?1002l\x1b[?1000l" };
    let mut out = io::stdout();
    let _ = out.write_all(seq);
    let _ = out.flush();
}

#[cfg(windows)]
fn set_mouse(on: bool) {
    use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
    use ratatui::crossterm::execute;
    let _ = if on { execute!(io::stdout(), EnableMouseCapture) } else { execute!(io::stdout(), DisableMouseCapture) };
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> io::Result<()> {
    let mut dirty = true;
    while !app.should_quit {
        if app.update() {
            dirty = true;
        }
        if dirty {
            terminal.draw(|f| ui::draw(f, app))?;
            dirty = false;
        }
        let timeout = if app.animating() { Duration::from_millis(80) } else { Duration::from_millis(250) };
        if event::poll(timeout)? {
            // Drain everything queued so key repeat doesn't lag behind rendering.
            loop {
                dirty |= match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        app.on_key(key);
                        true
                    }
                    Event::Mouse(m) => app.on_mouse(m),
                    Event::Resize(..) => true,
                    _ => false,
                };
                if app.should_quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }
    Ok(())
}
