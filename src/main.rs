mod app;
mod cache;
mod clean;
mod cli;
mod format;
mod project;
mod scan;
mod theme;
mod ui;
mod upgrade;

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use clap::{CommandFactory, Parser};
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::app::{App, Config};
use crate::cache::Cache;
use crate::cli::Args;
use crate::scan::{Msg, Progress};

fn main() -> ExitCode {
    // `kondor upgrade` is handled before the folder arguments, which would take it for a path.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("upgrade") {
        return upgrade::run(&argv[1..]);
    }
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
    let home =
        ["HOME", "USERPROFILE"].iter().find_map(|k| std::env::var_os(k).filter(|h| !h.is_empty())).map(PathBuf::from);
    let roots = resolve_roots(&args.paths)?;

    if !args.bench && (!io::stdout().is_terminal() || !io::stdin().is_terminal()) {
        return Err("needs an interactive terminal".into());
    }

    let mut skip: Vec<PathBuf> = if args.all { Vec::new() } else { scan::builtin_skips(home.as_deref()) };
    for p in &args.ignore {
        skip.push(p.canonicalize().unwrap_or_else(|_| p.clone()));
    }
    // Never skip something the user explicitly asked to scan.
    skip.retain(|s| !roots.iter().any(|r| r == s));

    let cache_path = if args.no_cache { None } else { cache::default_path(home.as_deref()) };
    let old_cache = cache_path.as_deref().map(Cache::load).unwrap_or_default();
    let opts = scan::Options {
        follow_links: args.follow_links,
        same_fs: args.same_fs,
        skip,
        cache: (!args.no_cache).then(|| Arc::new(old_cache.clone())),
        walk_threads: args.walk_threads,
        size_threads: args.size_threads,
    };
    if args.bench {
        return bench(&roots, opts, cache_path.as_deref(), &old_cache);
    }

    let progress = Arc::new(Progress::default());
    let (tx, rx) = mpsc::channel();
    {
        let roots = roots.clone();
        let progress = Arc::clone(&progress);
        std::thread::spawn(move || scan::run(&roots, &opts, &tx, &progress));
    }

    let cfg =
        Config { roots, home, sort: args.sort, dry_run: args.dry_run, min_age: args.older, cache_path, old_cache };
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
    // Quit mid-scan: keep what was measured so far.
    app.save_cache();
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

/// Headless scan for benchmarking: prints when things happened.
fn bench(roots: &[PathBuf], opts: scan::Options, cache_path: Option<&Path>, old: &Cache) -> Result<(), String> {
    let progress = Arc::new(Progress::default());
    let (tx, rx) = mpsc::channel();
    let t0 = std::time::Instant::now();
    let (r, p) = (roots.to_vec(), Arc::clone(&progress));
    let (walk_threads, size_threads) = (opts.walk_threads, opts.size_threads);
    let handle = std::thread::spawn(move || scan::run(&r, &opts, &tx, &p));
    let (mut first, mut walk_done, mut projects, mut bytes, mut sized) = (None, None, 0u64, 0u64, 0u64);
    let mut fresh = Cache::default();
    let now = cache::unix_secs(std::time::SystemTime::now());
    let all_done;
    let mut open: std::collections::HashSet<PathBuf> = Default::default();
    // Give up after a while and say what is still being measured.
    let limit = Duration::from_secs(120);
    loop {
        let Some(left) = limit.checked_sub(t0.elapsed()) else {
            eprintln!("gave up after {limit:?}; still sizing {} projects:", open.len());
            for p in open.iter().take(15) {
                eprintln!("  {}", p.display());
            }
            std::process::exit(2);
        };
        let Ok(msg) = rx.recv_timeout(left) else { continue };
        match msg {
            Msg::Discovered { found, .. } => {
                open.insert(found.path);
                if first.is_none() {
                    first = Some(t0.elapsed());
                    eprintln!("first project at {:.3}s", t0.elapsed().as_secs_f64());
                }
                projects += 1;
            }
            Msg::Sized(f) => {
                open.remove(&f.path);
                bytes += f.bytes;
                sized += 1;
                if sized % 25 == 0 {
                    eprintln!("  sized {sized}/{projects} at {:.1}s", t0.elapsed().as_secs_f64());
                }
                fresh.record(&f, now);
            }
            Msg::WalkDone { .. } => {
                walk_done = Some(t0.elapsed());
                eprintln!("walk done at {:.3}s ({projects} projects)", t0.elapsed().as_secs_f64());
            }
            Msg::Done { .. } => {
                all_done = Some(t0.elapsed());
                break;
            }
        }
    }
    let _ = handle.join();
    let fmt = |d: Option<Duration>| d.map(|d| format!("{:.3}s", d.as_secs_f64())).unwrap_or_else(|| "-".into());
    let (dw, ds) = scan::default_threads();
    println!(
        "threads (walk/size): {}/{}",
        if walk_threads > 0 { walk_threads } else { dw },
        if size_threads > 0 { size_threads } else { ds }
    );
    println!("cache entries loaded: {}", old.len());
    println!("first project:  {}", fmt(first));
    println!("walk done:      {}", fmt(walk_done));
    println!("all sized:      {}", fmt(all_done));
    println!("projects: {projects}");
    println!("dirs: {}", progress.dirs.load(std::sync::atomic::Ordering::Relaxed));
    println!("bytes: {bytes} ({})", format::size(bytes));
    if let Some(path) = cache_path {
        let _ = old.save_merged(&fresh, roots, path);
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
    const TICK: Duration = Duration::from_millis(80);
    let mut dirty = true;
    let mut last_tick = Instant::now();
    while !app.should_quit {
        if app.update() {
            dirty = true;
        }
        if dirty {
            terminal.draw(|f| ui::draw(f, app))?;
            dirty = false;
        }
        if app.animating() && last_tick.elapsed() >= TICK {
            app.spin = app.spin.wrapping_add(1);
            last_tick = Instant::now();
            dirty = true;
        }
        let timeout = if app.animating() {
            TICK.saturating_sub(last_tick.elapsed()).max(Duration::from_millis(1))
        } else {
            Duration::from_millis(250)
        };
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
