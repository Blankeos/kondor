<p align="center">
  <img src="https://raw.githubusercontent.com/Blankeos/kondor/main/assets/logo.png" alt="kondor" width="480">
</p>

<p align="center">Find and clear build leftovers across all your projects.</p>

`kondor` scans your folders for `node_modules`, `target`, `build`, `Pods`, caches and other output your tools can regenerate. It shows how big and how old each one is, and deletes only what you mark.

## Install

```sh
brew install blankeos/tap/kondor   # Homebrew (macOS/Linux)
npm install -g kondor              # or npm / bun
cargo binstall kondor-cli          # or prebuilt via cargo-binstall
cargo install kondor-cli           # or build from source
```

Update with `kondor upgrade`. It runs whichever of the above you installed with.

## Use

```sh
kondor                 # scan the current folder
kondor ~/code ~/work   # scan several
kondor --older 30d     # only output untouched for 30+ days
kondor --dry-run       # try it without deleting anything
```

| Key | |
| --- | --- |
| `↑ ↓` `j k` | move |
| `space` / click `○` | mark |
| `a` | mark everything shown |
| `d` | delete marked (asks first) |
| `/` | filter by name, folder or type |
| `s` / click a header | sort by size, age or name |
| `i` / double-click | details |
| `?` | all keys |

The mouse wheel scrolls the list without moving your selection.

## What gets deleted

Only known build-output folders inside a recognised project, e.g. `target` next to a `Cargo.toml` or `node_modules` next to a `package.json`. Source code is never touched, and neither are Python virtualenvs. Symlinks are never followed when deleting. Deletion is permanent (no Trash), so use `--dry-run` if you're unsure.

Recognised: Rust, Node, React Native, Unity, Unreal, Godot, .NET, Gradle, Maven, sbt, CMake, Swift, CocoaPods, Flutter, Elixir, Haskell (Stack, Cabal), Zig, PHP (Composer), Pixi, Turborepo, Terraform, Python and Jupyter.

`~/Library` on macOS, `AppData` on Windows and system folders are skipped unless you pass `--all`. Run `kondor --help` for every option.

## Credits

Started as a fork of [kondo](https://github.com/tbillington/kondo) by Trent Billington. MIT licensed.
