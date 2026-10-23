---
name: gh-cli
description: Using the GitHub CLI for the xdm repository (amajust/xdm) — PR workflow rules, token scopes and their limits, CI layout, and binary-swap deploys. Use whenever working with GitHub in this project (branches, PRs, merges, workflow files, releases) or when a gh push/PR fails.
---

# gh-cli (xdm)

## Environment

- `gh` is **not on PATH**: it lives at `~/.local/bin/gh`. Always invoke the
  full path. Authenticated as `amajust`, HTTPS remote.
- Repo: <https://github.com/amajust/xdm>.
- Token scopes: `gist`, `read:org`, `repo` — **no `workflow` scope**.
  Consequence: pushing commits that create or edit `.github/workflows/*`
  is rejected with *"refusing to allow an OAuth App to create or update
  workflow"*. Fixes, in order of preference:
  1. One-time scope refresh: `~/.local/bin/gh auth refresh -h github.com -s workflow`
     (device flow, ~15-minute code expiry). Generate the code **only when the
     user is actually ready** at <https://github.com/login/device>; codes
     left on an idle chat expire. After authorization the push just works.
  2. Split the branch: push everything **except** the workflow file, then the
     user adds the yml via GitHub web UI (Add file → paste on the task branch,
     so it joins the open PR).
- `npx` is also not on PATH: prepend
  `$HOME/.nvm/versions/node/v24.22.1/bin`.
- `gh` prints file lists reliably even for repos whose checkout breaks on
  Linux (paths > 255 bytes, e.g. `github/awesome-copilot`):
  `~/.local/bin/gh api repos/OWNER/REPO/git/trees/BRANCH?recursive=1`.

## Workflow rules (AGENTS.md — binding)

- PRs **always target `rust-rewrite`**, never `master`.
- One PR per finished task; branch named after the task (`ts-*`).
- Only open once verified: `cargo test --workspace` green, clippy 0 warnings.
- **Never merge automatically.** Present the PR URL and wait for an explicit
  user confirmation (e.g. "merge it"); then
  `~/.local/bin/gh pr merge N --merge`. After a merge: `git fetch origin
  rust-rewrite`, fast-forward the local branch, delete the task branch.

## CI landscape

- Legacy Java pipelines live in `binaries.yml` (tag `v*` / dispatch-release)
  and `build.yml` — they predate the rewrite; **leave them untouched**.
- Rust CI: `.github/workflows/rust.yml` — clippy+tests on Linux; release
  builds for `linux-x86_64`, `macos-aarch64`, `macos-x86_64`,
  `windows-x86_64`, each uploading its binary as an artifact.
- Cross-platform memory: `ksni` (the tray) is a Linux-only dependency;
  macOS/Windows close = quit. `ring` cannot be cross-compiled from Linux
  (`cargo check --target x86_64-pc-windows-msvc` dies in its build script),
  so validate off-platform via the native CI runners.

## Deploying the GUI binary (this machine)

The running GUI locks `target/release/xdm-app` (ETXTBSY). Loop:

1. Build elsewhere: `CARGO_TARGET_DIR=/tmp/xdm-gui-build cargo build --release -p xdm-app`
2. Kill the running `xdm-app` (find via `pgrep -af xdm-app`)
3. `cp /tmp/xdm-gui-build/release/xdm-app target/release/xdm-app`
4. `(setsid ./target/release/xdm-app >/tmp/xdm-gui.log 2>&1 &)`
5. Sanity: `curl -m 5 http://127.0.0.1:8597/sync` (extension monitor) and
   `tail /tmp/xdm-gui.log` for the tray/monitor listen lines.
