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
