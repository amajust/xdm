# Workflow rules

## Git / PR workflow

- **Never open PRs against `master`.** In this project, PRs always target the
  `rust-rewrite` branch.
- **One PR per meaningful finished task.** As soon as a task is complete and
  verified (tests + clippy clean), create a task branch off `rust-rewrite`,
  push it, and open the PR with base `rust-rewrite`.
- **Never merge PRs automatically.** Always present the PR to the user and ask
  them to review it. Only merge when the user explicitly says so.
- Task branches are named after the task, e.g. `ts-mp4-remuxer`,
  `hls-aes-decrypt`.

## Branch / upstream policy

- **`master` is the upstream mirror.** It tracks the original Java XDM
  (`subhra74/xdm`) and stays untouched unless the user explicitly asks for a
  sync. Never commit rewrite work to `master`.
- **Never merge `master` into `rust-rewrite`.** Upstream changes are
  Java-side; the rewrite shares almost no code with them. If something from
  upstream is genuinely needed (docs assets, translations), cherry-pick
  individual files only, and never anything under `browser-extension/`.
- **`browser-extension/` is off-limits.** Do not modify it, and do not pull
  upstream changes into it, unless the user explicitly asks.
- The default branch is `rust-rewrite`: new clones, the repo landing page,
  and PR targets all point at the rewrite.
