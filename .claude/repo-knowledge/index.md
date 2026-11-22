# Repo knowledge — WebDep
Git HEAD at last analysis: 43a6cd9+backups-phase4
Schema version: 2
Last incremental update: 2026-11-21 (owner-facing hardening phases 1-6: upgrades, firewall, account security, maintenance, create perms, IPs, limits)
Index confidence: file-index re-grepped at 99fe7b1 (38 symbols)
Stale risk: LOW
## Quick Reference
- What is it? Rust/axum self-hosted deploy panel, single embedded binary. See architecture.md
- How installed? install.sh (Linux, curl | sudo bash, systemd) / install.ps1 (Windows, irm | iex, logon task); both wrap start.sh/webdep.exe.
- Where is the VPS-2 join command built? src/cluster/manager.rs (fresh_vps_command).
- Control DB: zero-config PostgreSQL in data/control-pg (pg_control.rs), migrations/ applied on boot; data/webdep.sqlite imported once (sqlite_import.rs, `_meta.sqlite_imported_at`).
- Module install/uninstall: api/cluster.rs apply_modules_action (local or RPC) -> cluster/modules.rs run_pkg_manager (sudo -n when not root).
- Remote-node proxying for files/git: api/files.rs proxy_file_op, api/projects/git_ops.rs proxy_git_op.
- Who can create apps? Admins, or panel powers create_websites/create_services/create_databases (legacy *_manage still works); creator gets owner grant. See flows.md #4.
- Backups of apps (archives, restore, lock, download): src/backups.rs + src/api/projects/backups.rs; control-DB backups/audit/webhooks: src/maintenance.rs (hourly loop in main.rs). Upgrade notes: docs/UPGRADING.md.
- Per-app IP + CPU/memory limits: api/ports.rs, cluster/node_ips.rs, runtime/limits.rs (Linux cgroups v2 only).
## Invariants & Gotchas
- start.sh on a non-main branch compiles from source (slow, needs Rust) unless WEBDEP_FORCE_RELEASE=1.
- Workers must run the same branch as Core (manager.rs adds branch flag).
- Release tarball contains only the binary; .env generated from .env.example by start.sh.
- systemd unit uses KillMode=process: restarting webdep keeps Postgres/apps alive and reconnects to them.
- Non-root installs need RemoveIPC=no (logind), else control Postgres loses shared memory on SSH logout.
- settings.rs test parses public/js/themes.js rows (`['<id>', `): keep THEME_ROWS format in sync with ALLOWED_THEME_IDS.
- View entrance animation: animateViewEnter on navigation; per-view stagger only on first fill (hadCards/hadRows) to avoid double animation.
- Release gate: CI runs cargo test, node --check on public/js, SonarCloud gate (project key ded-lmfao_WebDeployer) before builds.
- AuthZ (branch feat/user-management): access_grants rows loaded into User.grants (src/auth/grants.rs) on get_session/get_user_by_id/list_users; scopes global (panel) / project (websites+services, NULL = all) / database. Admin = main admin (id 1/"admin") or users.role='admin'; is_admin bypasses checks. Per-app API src/api/access.rs (/api/access/:scope/:id[/:user]) with delegation: ensure_can_grant (only powers you hold, needs access_manage). Accounts src/api/accounts.rs: /api/auth/register (public, setting allow_registration), /api/requests approve/reject, /api/auth/profile. users.must_change_password blocks all APIs except me/password/logout in security_middleware. Frontend: public/js/permissions.js (scope-aware hasPower), public/js/accounts.js (account/users/requests/admin-audit/access views, admin sidebar mode). New SPA routes must be added to static_assets.rs is_spa_route_path.
- Ports (branch feat/user-management): node port pools in node_ports (migration 0007, src/cluster/ports.rs); creation/move take ports::next_free_port when a node has a pool, else db::allocate_global_unique_port. Admin API src/api/ports.rs; firewall via runtime::port::sync_firewall_ports (local) or node RPC "firewall.ports". Custom VPS modules: settings.custom_modules, ids prefixed "x-", defs sent in modules.* RPC params (cluster/modules.rs). Moving apps between nodes is admin-only (shift::verify_cross_node_shift_rbac). Delete app needs per-app power app_delete.
- Workers older than Core answer "Unknown remote RPC op": call_node maps it to a 409 "update this node"; UI shows "Update needed" by comparing node versions with /healthz X-WebDep-Version.
- Firewall: pool ports private by default; an app's main port follows its access_mode; nodes without a pool keep old behaviour. Never edit iptables directly — go through ports::apply_on_node.
- Client IP comes from the TCP peer (into_make_service_with_connect_info); X-Forwarded-For is trusted only from a loopback peer.
- require_2fa covering the acting admin is refused unless they have TOTP (lockout guard).
- Limits apply on process start only; Linux code paths (limits.rs, port.rs firewall) are not compiled on Windows dev — release CI (musl) is the first Linux build.
- Moving an app to another node clears bind_ip (shift.rs). An IP in use can't be removed.
- Restore never merges: it replaces the app's Files root (api/files.rs service_root; a `current` symlink is swapped through its target). env-only backups must never wipe files (restore_files handles it).
- CPU/memory limits: Linux cgroups v2 (root) and Windows job objects; disk limit is a soft stop checked each minute on this server only.
## Changelog
- 2026-11-22: backups rebuilt (archives, background build, status, checksum, lock, download, safe restore, remote VPS support, older backups use the safe restore); App Configs tab; Windows CPU/memory limits; icon busy fix.
- 2026-11-21: 99fe7b1 — phases 1-6: pre-upgrade backup + rollback, private ports + firewall state, reset links/sessions/2FA policy/PoW sign-up, backups/audit paging/webhooks, create perms + owner grant, node IPs + Caddy bind, cgroup limits; migrations 0008-0010.
- 2026-11-21: feat/user-management at 9031ed4 — accounts/requests/admin panel, per-app grants, port pools, custom modules, icon UI.
- 2026-11-20: 861d0bd v3.0.4 — settings_users.js permission presets compared as sets; version bump only.
- 2026-11-19: d75b8e9 dead-code/duplication cleanup, user dialog redesign, animation fix, README refresh.
Store status: COMPLETE
