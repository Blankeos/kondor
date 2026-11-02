//! Command-line interface.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use clap_complete::Shell;

use crate::format;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SortBy {
    /// Largest first
    Size,
    /// Least recently built first
    Age,
    /// Alphabetical by project name
    Name,
}

/// Find and clear build leftovers (node_modules, target, caches) across all your projects.
///
/// Nothing is deleted until you mark projects and confirm.
#[derive(Debug, Parser)]
#[command(name = "kondor", version, about, long_about = None)]
pub struct Args {
    /// Folders to scan [default: current directory]
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// Only show projects whose build output is older than this (e.g. 30d, 3M, 1y)
    #[arg(short, long, value_name = "AGE", value_parser = format::parse_age)]
    pub older: Option<u64>,

    /// Initial sort order
    #[arg(short, long, value_enum, default_value_t = SortBy::Size)]
    pub sort: SortBy,

    /// Skip these folders (repeatable)
    #[arg(short, long, value_name = "PATH")]
    pub ignore: Vec<PathBuf>,

    /// Go through everything, including system and app-data folders skipped by default
    #[arg(long)]
    pub all: bool,

    /// Follow symbolic links while scanning
    #[arg(short = 'L', long)]
    pub follow_links: bool,

    /// Stay on the filesystem of each PATH
    #[arg(short = 'x', long)]
    pub same_fs: bool,

    /// Go through the motions without deleting anything
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Print a shell completion script and exit
    #[arg(long, value_name = "SHELL", exclusive = true)]
    pub completions: Option<Shell>,
}
