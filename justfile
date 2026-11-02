default:
    just --list

dev *args:
    cargo run -- {{ args }}

fmt:
    cargo fmt

lint:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings

test:
    cargo test --locked

[doc('Check registry publishing workflow and npm archive compatibility')]
test-release:
    bun test scripts/release-workflow.test.ts npm/install.test.js

[doc('Release: bump versions, commit, and tag from master (just tag [patch|minor|major])')]
tag bump="":
    sh scripts/tag_and_release.sh {{ bump }}
