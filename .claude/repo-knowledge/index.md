# Repo knowledge — WebDep
Git HEAD at last analysis: e877cea
Schema version: 2
Index confidence: 3/3 validated (main.rs CliOptions, manager.rs fresh_vps_command, static_assets RustEmbed)
## Quick Reference
- What is it? Rust/axum self-hosted deploy panel, single embedded binary. See architecture.md
- How installed? install.sh (Linux, curl | sudo bash, systemd) / install.ps1 (Windows, irm | iex, logon task); both wrap start.sh/webdep.exe.
- Where is the VPS-2 join command built? src/cluster/manager.rs (fresh_vps_command).
## Invariants & Gotchas
- start.sh on a non-main branch compiles from source (slow, needs Rust) unless WEBDEP_FORCE_RELEASE=1.
- Workers must run the same branch as Core (manager.rs adds branch flag).
- Release tarball contains only the binary; .env generated from .env.example by start.sh.
Store status: COMPLETE
