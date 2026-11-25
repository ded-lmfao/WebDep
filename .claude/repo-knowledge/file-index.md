# File index (lines re-grepped at 5bb6ba7 v3.5.2)
- CLI parse: src/main.rs:48 CliOptions, :61 parse_cli_options
- Join commands: src/cluster/manager.rs (join_command, service_command, fresh_vps_command); model src/cluster/models.rs; UI public/js/cluster.js freshVpsCommand
- Static embed: src/static_assets.rs:13; SPA routes src/static_assets.rs:315 is_spa_route_path
- Installer: start.sh, install.sh, install.ps1
- AuthZ: src/auth/grants.rs:20 PANEL_ACCOUNT_POWERS, :26 create_power, :35 grant_owner, :169 parse_powers (no legacy powers any more); delegation src/api/access.rs:109 ensure_can_grant; 2FA policy src/auth/policy.rs:33 reload (test override is thread-local); gates src/api/common.rs:191 (two_factor_required), :356 resolve_client_ip
- Accounts: src/api/accounts.rs:93 registration_challenge_handler (PoW), :468 reset_password_handler
- App create: src/api/projects.rs:176 create_project_handler (create power + owner grant), :486 propagate_project_change; src/api/databases.rs:201 create_database_handler
- Ports/IPs: src/cluster/ports.rs:402 free_and_open_counts, :413 next_free_port, :644 apply_on_node; src/runtime/port.rs:36 plan_firewall_commands; src/api/ports.rs:550 set_app_ip_handler; src/cluster/node_ips.rs:116 check_assignable; src/caddy.rs:33 bind_line
- Limits: src/app_limits.rs:70 AppLimits; src/api/limits.rs:51 set_project_limits_handler, :92 set_database_limits_handler; src/runtime/limits.rs:49 prepare (cgroup v2), joined in child at src/runtime/process.rs:183, :160 apply_to_process (Windows job objects); watcher src/api/limit_watch.rs
- Maintenance: src/maintenance.rs:35 validate_setting, :133 backup_now, :183 daily_backup_if_due, :218 notify; hourly loop src/main.rs (log rows capped 20 000 at :285 prune_old_logs); backups API src/api/system/settings.rs:500; audit paging src/api/logs.rs:271 global_audit_rows; node offline notify src/cluster/manager.rs:652
- Upgrades: src/pg_control.rs:27 build_migrator (single migration v1 "schema", migrations/0001_schema.sql), :162 backup_before_migrate
- Project row mapping: src/projects/models.rs:130 SELECT_PROJECT_FIELDS, :139 pg_row_to_project
- Env vars: src/api/projects/env.rs:171 reveal_env_handler (only way to read secrets, audited), :194 log_env_read; list masks secrets without decrypting (src/projects.rs get_env_vars); UI public/js/env_vars.js:42 revealSecrets
- App runtime: src/runtime/process.rs start_project (:138 isolation::prepare, :185 setsid, stdout/stderr piped to the panel); restart on panel boot src/main.rs:351; external-PID check src/runtime/supervisor.rs:138; app auth key src/runtime/container.rs:337 (random, .webdep-auth-key); logs src/logs.rs:44 add (broadcast + INSERT logs)
- Log relay: src/runtime/relay.rs (Msg protocol, Queue, AppProcess, attach, unix::run/connect); spawn via relay in src/runtime/process.rs spawn_project_process_pure, reattach_app + Runtime::reattach_project; entry flag in src/main.rs main; e2e tests/log_relay.rs (unix)
- Isolation: src/runtime/isolation.rs:106 wrap_with (`unshare --pid --fork --kill-child ...`, databases without PID ns)
- Frontend: public/js/permissions.js:129 canCreate; public/js/cluster.js:1382 loadIpsView; public/js/app_limits.js:55 loadLimitsView; public/js/limits_fields.js; public/js/files_editor.js:128 syncEditorScroll (code layer moved with transform, not scrolled); syntax colours .shj-syn-* in public/css/files.css
- Backups domain: src/backups.rs:20 archives_root, :105 recover_interrupted, :227 stage_archive, :267 swap_in, :347 create_archive_file, :439 restore_files (shared by Core and nodes), :513 begin_restore; src/cluster/archive.rs:481 pack_backup_archive (sha256, ignore names)
- Backups API: src/api/projects/backups.rs:808 create_archive_backup, :986 start_restore, :1070 restore_in_place, :1187 undo_housekeeping, :1216 lock_backup_handler, :1247 download_backup_handler (also needs env power), :1335 run_remote_archive_backup; node ops src/cluster/node_exec.rs:1283 project.backup_archive, :1340 project.backup_restore
- DB backups: src/db_backups.rs (dump/restore engine), src/api/db_backups.rs (handlers); UI public/js/backups.js:150 loadBackupsTable (shared by websites, services, databases)
- Security: sessions hashed src/auth/manager.rs session_key; lockout keys login:<user>|<ip>, loginip:<ip>, loginuser:<user> in src/api/auth.rs; CSP in src/api/common.rs attach_security_headers; domain validation src/api/projects.rs normalize_domain/ensure_domains_free; git URL rules src/git.rs validate_git_url
Propagation (new Project column): migrations/0002+ -> models.rs SELECT/pg_row_to_project -> 4 test literals -> api handler -> UI.
Propagation (new panel power): grants.rs PANEL_ACCOUNT_POWERS -> permissions.js PANEL_ACCOUNT_POWERS + LABELS.
