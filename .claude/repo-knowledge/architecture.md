# Architecture
- WebDep v2.0.12: self-hosted deploy platform (GitHub sync, releases, health-checked deploys, rollback, multi-node).
- Rust 2021, axum 0.7 + tokio; sqlx Postgres (control DB, auto-local in data/control-pg) + rusqlite (legacy import).
- Frontend: vanilla ES modules in public/ embedded via rust-embed (src/static_assets.rs) — binary is self-contained.
- Roles (src/main.rs CliOptions): default = Core+local Node; `--core` dashboard only; `--node [--join URL --token T]` worker; `--pair-local`, `--enroll-only`, `--version`, `--import-sqlite`.
- Install/run: start.sh (Linux; downloads release binary from GitHub Releases, falls back to cargo build; non-main branch => builds from source), start.ps1/start.bat (Windows).
- systemd units written by start.sh: webdep-core (`--core`), webdep-node (`--node`); legacy webdep.service migrated.
- CI: .github/workflows/release.yml builds musl x86_64/aarch64 `webdep3-linux-<arch>.tar.gz` + checksums.txt on push to main. Name differs from SQLite-era `webdep-linux-*` on purpose: old updaters skip it; bridge release (branch release/sqlite-bridge) tells them to reinstall.
