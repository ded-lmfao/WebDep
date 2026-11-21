# File index (lines validated at 99fe7b1)
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
Propagation (new Project column): migration -> models.rs SELECT/pg_row_to_project -> 4 test literals -> api handler -> UI.
Propagation (new panel power): grants.rs PANEL_ACCOUNT_POWERS -> permissions.js PANEL_ACCOUNT_POWERS + LABELS.
