//! Project detection: which kind of project a folder is, and which of its
//! folders are disposable build output.

/// A kind of project, recognised by a marker file in its root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Cargo,
    Node,
    ReactNative,
    Unity,
    Unreal,
    Godot,
    Dotnet,
    Gradle,
    Maven,
    Sbt,
    CMake,
    Swift,
    Cocoapods,
    Flutter,
    Elixir,
    Stack,
    Cabal,
    Zig,
    Composer,
    Pixi,
    Turborepo,
    Terraform,
    Python,
    Jupyter,
}

impl Kind {
    /// Human-friendly name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Cargo => "Rust",
            Kind::Node => "Node",
            Kind::ReactNative => "React Native",
            Kind::Unity => "Unity",
            Kind::Unreal => "Unreal",
            Kind::Godot => "Godot",
            Kind::Dotnet => ".NET",
            Kind::Gradle => "Gradle",
            Kind::Maven => "Maven",
            Kind::Sbt => "sbt",
            Kind::CMake => "CMake",
            Kind::Swift => "Swift",
            Kind::Cocoapods => "CocoaPods",
            Kind::Flutter => "Flutter",
            Kind::Elixir => "Elixir",
            Kind::Stack => "Stack",
            Kind::Cabal => "Cabal",
            Kind::Zig => "Zig",
            Kind::Composer => "PHP",
            Kind::Pixi => "Pixi",
            Kind::Turborepo => "Turborepo",
            Kind::Terraform => "Terraform",
            Kind::Python => "Python",
            Kind::Jupyter => "Jupyter",
        }
    }

    /// Folders (relative to the project root) that are safe to delete.
    pub fn artifacts(self) -> &'static [&'static str] {
        match self {
            Kind::Cargo => &["target", ".xwin-cache"],
            Kind::Node => &["node_modules", ".angular"],
            Kind::ReactNative => &[
                "node_modules",
                "android/build",
                "android/app/build",
                "android/.gradle",
                "ios/build",
                "ios/DerivedData",
                "ios/Pods",
                ".expo",
                ".metro",
            ],
            Kind::Unity => &["Library", "Temp", "Obj", "Logs", "MemoryCaptures", "Build", "Builds"],
            Kind::Unreal => &["Binaries", "Build", "Saved", "DerivedDataCache", "Intermediate"],
            Kind::Godot => &[".godot"],
            Kind::Dotnet => &["bin", "obj"],
            Kind::Gradle => &["build", ".gradle"],
            Kind::Maven => &["target"],
            Kind::Sbt => &["target", "project/target"],
            Kind::CMake => &["build", "cmake-build-debug", "cmake-build-release"],
            Kind::Swift => &[".build", ".swiftpm"],
            Kind::Cocoapods => &["Pods"],
            Kind::Flutter => &["build", ".dart_tool", "linux/flutter/ephemeral", "windows/flutter/ephemeral"],
            Kind::Elixir => &["_build", ".elixir-tools", ".elixir_ls", ".lexical"],
            Kind::Stack => &[".stack-work"],
            Kind::Cabal => &["dist-newstyle"],
            Kind::Zig => &["zig-cache", ".zig-cache", "zig-out"],
            Kind::Composer => &["vendor"],
            Kind::Pixi => &[".pixi"],
            Kind::Turborepo => &[".turbo"],
            Kind::Terraform => &[".terraform"],
            // Never .venv: virtualenvs hold hand-installed state.
            Kind::Python => {
                &["__pycache__", "__pypackages__", ".mypy_cache", ".pytest_cache", ".ruff_cache", ".tox", ".nox"]
            }
            Kind::Jupyter => &[".ipynb_checkpoints"],
        }
    }

    /// Weak kinds are guessed from loose source files rather than a manifest.
    /// Inside another project they only add noise, so they're ignored there.
    pub fn is_weak(self) -> bool {
        matches!(self, Kind::Python | Kind::Jupyter)
    }
}

/// Detect every project kind present in a folder, given its file names and a
/// way to ask whether it has a given sub-folder. Result is sorted and unique.
pub fn detect<'a>(files: impl IntoIterator<Item = &'a str>, has_dir: impl Fn(&str) -> bool) -> Vec<Kind> {
    let mut kinds = Vec::new();
    let mut csproj = false;
    for name in files {
        let kind = match name {
            "Cargo.toml" => Kind::Cargo,
            "package.json" => Kind::Node,
            "Assembly-CSharp.csproj" => Kind::Unity,
            "project.godot" => Kind::Godot,
            "build.gradle" | "build.gradle.kts" => Kind::Gradle,
            "pom.xml" => Kind::Maven,
            "build.sbt" => Kind::Sbt,
            "CMakeLists.txt" => Kind::CMake,
            "Package.swift" => Kind::Swift,
            "Podfile" => Kind::Cocoapods,
            "pubspec.yaml" => Kind::Flutter,
            "mix.exs" => Kind::Elixir,
            "stack.yaml" => Kind::Stack,
            "cabal.project" => Kind::Cabal,
            "build.zig" => Kind::Zig,
            "composer.json" => Kind::Composer,
            "pixi.toml" => Kind::Pixi,
            "turbo.json" => Kind::Turborepo,
            ".terraform.lock.hcl" => Kind::Terraform,
            "pyproject.toml" => Kind::Python,
            n if n.ends_with(".uproject") => Kind::Unreal,
            n if n.ends_with(".csproj") || n.ends_with(".fsproj") => {
                csproj = true;
                continue;
            }
            n if n.ends_with(".py") => Kind::Python,
            n if n.ends_with(".ipynb") => Kind::Jupyter,
            _ => continue,
        };
        kinds.push(kind);
    }
    // A .csproj inside a Godot or Unity project belongs to the engine.
    if csproj && !kinds.contains(&Kind::Godot) && !kinds.contains(&Kind::Unity) {
        kinds.push(Kind::Dotnet);
    }
    if kinds.contains(&Kind::Node) && (has_dir("ios") || has_dir("android")) {
        kinds.retain(|k| *k != Kind::Node);
        kinds.push(Kind::ReactNative);
    }
    kinds.sort_unstable();
    kinds.dedup();
    kinds
}

/// Union of the artifact folders of several kinds, without duplicates.
pub fn artifacts_for(kinds: &[Kind]) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for kind in kinds {
        for dir in kind.artifacts() {
            if !out.contains(dir) {
                out.push(dir);
            }
        }
    }
    out
}

/// "Rust · Node" style label for a set of kinds.
pub fn label(kinds: &[Kind]) -> String {
    kinds.iter().map(|k| k.label()).collect::<Vec<_>>().join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_dirs(_: &str) -> bool {
        false
    }

    #[test]
    fn detects_manifests() {
        assert_eq!(detect(["Cargo.toml", "README.md"], no_dirs), vec![Kind::Cargo]);
        assert_eq!(detect(["main.go", "README.md"], no_dirs), vec![]);
    }

    #[test]
    fn mixed_projects_get_every_kind() {
        let kinds = detect(["package.json", "Cargo.toml"], no_dirs);
        assert_eq!(kinds, vec![Kind::Cargo, Kind::Node]);
        assert_eq!(artifacts_for(&kinds), vec!["target", ".xwin-cache", "node_modules", ".angular"]);
    }

    #[test]
    fn csproj_belongs_to_engine() {
        assert_eq!(detect(["Game.csproj", "project.godot"], no_dirs), vec![Kind::Godot]);
        assert_eq!(detect(["Assembly-CSharp.csproj"], no_dirs), vec![Kind::Unity]);
        assert_eq!(detect(["App.fsproj"], no_dirs), vec![Kind::Dotnet]);
    }

    #[test]
    fn react_native_replaces_node() {
        assert_eq!(detect(["package.json"], |d| d == "ios"), vec![Kind::ReactNative]);
    }

    #[test]
    fn never_targets_virtualenvs_or_traversal() {
        let all = [
            Kind::Cargo,
            Kind::Node,
            Kind::ReactNative,
            Kind::Unity,
            Kind::Unreal,
            Kind::Godot,
            Kind::Dotnet,
            Kind::Gradle,
            Kind::Maven,
            Kind::Sbt,
            Kind::CMake,
            Kind::Swift,
            Kind::Cocoapods,
            Kind::Flutter,
            Kind::Elixir,
            Kind::Stack,
            Kind::Cabal,
            Kind::Zig,
            Kind::Composer,
            Kind::Pixi,
            Kind::Turborepo,
            Kind::Terraform,
            Kind::Python,
            Kind::Jupyter,
        ];
        for dir in artifacts_for(&all) {
            assert!(!dir.contains("..") && !dir.starts_with('/') && !dir.is_empty(), "{dir}");
            assert!(!dir.contains("venv"), "{dir}");
        }
    }
}
