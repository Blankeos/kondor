//! `kondor upgrade`: update to the latest release the same way kondor was installed.
//!
//! Kondor ships through Homebrew, npm, cargo and the shell installer, and each one owns
//! the files it put down. Replacing the binary behind its back would leave the package
//! manager's records wrong, so this finds out who installed it and asks that tool to update.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const REPO: &str = "Blankeos/kondor";

#[derive(Debug, PartialEq, Eq)]
enum Method {
    Homebrew,
    Npm,
    /// The cargo-dist shell installer (leaves a receipt behind).
    Installer,
    Cargo,
    Unknown,
}

pub fn run(args: &[String]) -> ExitCode {
    let mut check = false;
    for arg in args {
        match arg.as_str() {
            "--check" | "-c" => check = true,
            "-h" | "--help" => {
                println!("Update kondor to the latest release.\n\nUsage: kondor upgrade [--check]\n\n  -c, --check  Only say whether a newer version exists");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("kondor: unknown option '{other}' (try: kondor upgrade --help)");
                return ExitCode::FAILURE;
            }
        }
    }
    match upgrade(check) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("kondor: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn upgrade(check: bool) -> Result<(), String> {
    let current = env!("CARGO_PKG_VERSION");
    let latest = latest_version()?;
    if !is_newer(&latest, current) {
        println!("kondor {current} is up to date");
        return Ok(());
    }
    println!("kondor {current} -> {latest}");
    if check {
        println!("Run: kondor upgrade");
        return Ok(());
    }

    let exe = env::current_exe().map_err(|e| format!("cannot tell where kondor is installed: {e}"))?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let home = ["HOME", "USERPROFILE"].iter().find_map(|k| env::var_os(k).filter(|h| !h.is_empty())).map(PathBuf::from);
    let receipt = home.as_deref().is_some_and(|h| installer_receipt(h).is_file());

    match detect(&exe, receipt) {
        Method::Homebrew => run_tool("brew", &["upgrade", "blankeos/tap/kondor"]),
        Method::Npm => run_tool("npm", &["install", "-g", "kondor@latest"]),
        Method::Cargo => run_tool("cargo", &["install", "kondor-cli", "--locked"]),
        Method::Installer => {
            let url = format!("https://github.com/{REPO}/releases/latest/download/kondor-cli-installer.sh");
            run_tool("sh", &["-c", &format!("curl --proto '=https' --tlsv1.2 -LsSf {url} | sh")])
        }
        Method::Unknown => Err(format!(
            "can't tell how {} was installed. Update it the way you installed it:\n  brew upgrade blankeos/tap/kondor\n  npm install -g kondor@latest\n  cargo install kondor-cli",
            exe.display()
        )),
    }
}

fn installer_receipt(home: &Path) -> PathBuf {
    let config = env::var_os("XDG_CONFIG_HOME")
        .filter(|c| !c.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    config.join("kondor-cli").join("kondor-cli-receipt.json")
}

fn detect(exe: &Path, receipt: bool) -> Method {
    let path = exe.to_string_lossy().replace('\\', "/");
    if path.contains("/Cellar/") || path.contains("/homebrew/") || path.contains("/linuxbrew/") {
        Method::Homebrew
    } else if path.contains("/node_modules/") {
        Method::Npm
    } else if path.contains("/.cargo/bin/") {
        if receipt {
            Method::Installer
        } else {
            Method::Cargo
        }
    } else {
        Method::Unknown
    }
}

fn run_tool(program: &str, args: &[&str]) -> Result<(), String> {
    println!("$ {program} {}", args.join(" "));
    let status = Command::new(program).args(args).status().map_err(|e| format!("could not run {program}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with {status}"))
    }
}

fn latest_version() -> Result<String, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let out = Command::new("curl")
        .args(["-fsSL", "--max-time", "15", "-H", "Accept: application/vnd.github+json", &url])
        .output()
        .map_err(|e| format!("could not run curl to look up the latest release: {e}"))?;
    if !out.status.success() {
        return Err("could not reach GitHub to look up the latest release".into());
    }
    parse_tag(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| "unexpected answer from GitHub".to_string())
}

/// Pulls `tag_name` out of the release JSON without a JSON dependency.
fn parse_tag(body: &str) -> Option<String> {
    let rest = body.split("\"tag_name\"").nth(1)?;
    let tag = rest.split('"').nth(1)?;
    let version = tag.trim_start_matches('v');
    parse_version(version)?;
    Some(version.to_string())
}

fn parse_version(v: &str) -> Option<[u64; 3]> {
    let mut parts = v.split('.').map(|p| p.parse::<u64>().ok());
    let version = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(version)
}

fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse_version(latest), parse_version(current)), (Some(l), Some(c)) if l > c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_numbers_not_text() {
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(!is_newer("0.1.1", "0.1.1"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(!is_newer("garbage", "0.1.1"));
    }

    #[test]
    fn reads_the_tag_from_the_release_json() {
        assert_eq!(parse_tag(r#"{"url":"x","tag_name": "v0.2.0","name":"v0.2.0"}"#).as_deref(), Some("0.2.0"));
        assert_eq!(parse_tag(r#"{"message":"Not Found"}"#), None);
        assert_eq!(parse_tag(r#"{"tag_name":"nightly"}"#), None);
    }

    #[test]
    fn finds_who_installed_it() {
        let at = |p: &str, receipt| detect(Path::new(p), receipt);
        assert_eq!(at("/opt/homebrew/Cellar/kondor/0.1.1/bin/kondor", false), Method::Homebrew);
        assert_eq!(at("/home/a/.nvm/lib/node_modules/kondor/npm/bin/kondor", false), Method::Npm);
        assert_eq!(at("/home/a/.cargo/bin/kondor", true), Method::Installer);
        assert_eq!(at("/home/a/.cargo/bin/kondor", false), Method::Cargo);
        assert_eq!(at("/usr/local/bin/kondor", false), Method::Unknown);
    }
}
