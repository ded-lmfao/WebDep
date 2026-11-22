# File index (lines validated at 43a6cd9 + phase 4 backups)
- CLI parse: src/main.rs CliOptions / parse_cli_options (re-grep; main.rs grew)
- Join commands: src/cluster/manager.rs (join_command, service_command, fresh_vps_command); model src/cluster/models.rs; UI public/js/cluster.js freshVpsCommand
- Static embed: src/static_assets.rs:13; SPA routes src/static_assets.rs:315 is_spa_route_path
- Installer: start.sh, install.sh, install.ps1
- AuthZ: src/auth/grants.rs:22 PANEL_ACCOUNT_POWERS, :28 create_power, :37 grant_owner, :435 grant_legacy_delete_once; delegation src/api/access.rs:109 ensure_can_grant; 2FA policy src/auth/policy.rs:33 reload; gates src/api/common.rs:191 (two_factor_required), :356 resolve_client_ip
- Accounts: src/api/accounts.rs:93 registration_challenge_handler (PoW), :468 reset_password_handler
- App create: src/api/projects.rs:176 create_project_handler (create power + owner grant), :436 propagate_project_change; src/api/databases.rs:201 create_database_handler
- Ports/IPs/limits: src/cluster/ports.rs:385 free_and_open_counts, :396 next_free_port, :541 apply_on_node; src/runtime/port.rs:36 plan_firewall_commands; src/api/ports.rs:423 set_app_ip_handler, :465 set_app_limits_handler; src/cluster/node_ips.rs:116 check_assignable; src/caddy.rs:29 bind_line
- Limits: src/runtime/limits.rs:46 prepare (cgroup v2), joined in child at src/runtime/process.rs:152
- Maintenance: src/maintenance.rs:35 validate_setting, :133 backup_now, :183 daily_backup_if_due, :218 notify; hourly loop in src/main.rs; backups API src/api/system/settings.rs:485; audit paging src/api/logs.rs:271 global_audit_rows; node offline notify src/cluster/manager.rs:759
- Upgrades: src/pg_control.rs:16 build_migrator, :216 backup_before_migrate
- Project row mapping: src/projects/models.rs:126 SELECT_PROJECT_FIELDS, :135 pg_row_to_project
- Frontend: public/js/permissions.js:134 canCreate; public/js/cluster.js:1415 loadIpsView (ports, node IPs, limits modal); public/js/accounts.js:1433 loadMaintenanceSettings; public/js/touch_labels.js
- Backups domain: src/backups.rs:20 archives_root, :105 recover_interrupted, :227 stage_archive, :267 swap_in, :347 create_archive_file, :439 restore_files (shared by Core and nodes), :512 begin_restore; src/cluster/archive.rs:481 pack_backup_archive (sha256, ignore names)
- Backups API: src/api/projects/backups.rs:835 create_archive_backup, :1013 start_restore, :1097 restore_in_place, :1214 undo_housekeeping, :1243 lock_backup_handler, :1274 download_backup_handler, :1360 run_remote_archive_backup; node ops src/cluster/node_exec.rs:1278 project.backup_archive, :1335 project.backup_restore, project.backup_delete
- Limits: src/app_limits.rs:19 AppLimits; src/api/limits.rs:161 enforce_disk_limits (every 60 s from main.rs); src/runtime/limits.rs:135 apply_to_process (Windows job objects; Linux cgroups in prepare)
- Frontend: public/js/releases.js:165 loadProjectBackupsTable (status, icons, restore dialog with typed name); public/js/app_limits.js:54 loadLimitsView; public/js/limits_fields.js
- DB backups: src/db_backups.rs (dump/restore engine), src/api/db_backups.rs (handlers), table in migrations/0020_baseline.sql; UI public/js/backups.js (shared by websites, services, databases; tab 'backups' in state.js WEBSITE/DATABASE_WORKSPACE_TABS)
- Security: sessions hashed src/auth/manager.rs session_key; lockout keys login:<user>|<ip>, loginip:<ip>, loginuser:<user> in src/api/auth.rs; CSP in src/api/common.rs attach_security_headers; domain validation src/api/projects.rs normalize_domain/ensure_domains_free; git URL rules src/git.rs validate_git_url; isolation src/runtime/isolation.rs
Propagation (new Project column): migration -> models.rs SELECT/pg_row_to_project -> 4 test literals -> api handler -> UI.
Propagation (new panel power): grants.rs PANEL_ACCOUNT_POWERS -> permissions.js PANEL_ACCOUNT_POWERS + LABELS.
