# Repo knowledge — WebDep
Git HEAD at last analysis: 861d0bd
Schema version: 2
Last incremental update: 2026-11-19 (v3.0.2–v3.0.4 cleanup; file-index line numbers for src/cluster/modules.rs, src/api/cluster.rs, src/api/files.rs, src/api/projects/{git_ops,backups}.rs, src/cluster/node_exec.rs, src/projects.rs shifted — re-grep before editing)
Index confidence: 3/3 validated (main.rs CliOptions, manager.rs fresh_vps_command, static_assets RustEmbed)
## Quick Reference
- What is it? Rust/axum self-hosted deploy panel, single embedded binary. See architecture.md
- How installed? install.sh (Linux, curl | sudo bash, systemd) / install.ps1 (Windows, irm | iex, logon task); both wrap start.sh/webdep.exe.
- Where is the VPS-2 join command built? src/cluster/manager.rs (fresh_vps_command).
- Control DB: zero-config PostgreSQL in data/control-pg (pg_control.rs), migrations/ applied on boot; data/webdep.sqlite imported once (sqlite_import.rs, `_meta.sqlite_imported_at`).
- Module install/uninstall: api/cluster.rs apply_modules_action (local or RPC) -> cluster/modules.rs run_pkg_manager (sudo -n when not root).
- Remote-node proxying for files/git: api/files.rs proxy_file_op, api/projects/git_ops.rs proxy_git_op.
## Invariants & Gotchas
- start.sh on a non-main branch compiles from source (slow, needs Rust) unless WEBDEP_FORCE_RELEASE=1.
- Workers must run the same branch as Core (manager.rs adds branch flag).
- Release tarball contains only the binary; .env generated from .env.example by start.sh.
- systemd unit uses KillMode=process: restarting webdep keeps Postgres/apps alive and reconnects to them.
- Non-root installs need RemoveIPC=no (logind), else control Postgres loses shared memory on SSH logout.
- settings.rs test parses public/js/themes.js rows (`['<id>', `): keep THEME_ROWS format in sync with ALLOWED_THEME_IDS.
- View entrance animation: animateViewEnter on navigation; per-view stagger only on first fill (hadCards/hadRows) to avoid double animation.
- Release gate: CI runs cargo test, node --check on public/js, SonarCloud gate (project key ded-lmfao_WebDeployer) before builds.
- AuthZ (branch feat/user-management): source of truth = access_grants rows loaded into User.grants (src/auth/grants.rs) on get_session/get_user_by_id; scopes global (panel powers) / project (websites+services, scope_id NULL = all) / database. User.has_power = panel; can_access_project/has_project_power/has_database_power = per-app. Users from list_users carry NO grants (display only). Legacy users.permissions/allowed_projects still written by users API and mirrored via grants::replace_from_legacy_json; one-time boot conversion guarded by _meta legacy_grants_migrated_at (cleared by sqlite import). Migration 0006 adds users.email/display_name/role/must_change_password/last_login_at, registration_requests, audit.database_id. No SMTP.
## Changelog
- 2026-11-20: 861d0bd v3.0.4 — settings_users.js permission presets compared as sets; version bump only.
- 2026-11-19: d75b8e9 dead-code/duplication cleanup, user dialog redesign, animation fix, README refresh.
Store status: COMPLETE
