# Data model (migrations/0001-0016, Postgres; registered in src/pg_control.rs build_migrator)
_meta settings projects project_env deployments releases logs users sessions backups audit project_keys user_passkeys databases access_grants(powers_json) nodes node_enroll_tokens migrations
0006 user_management: users.email/display_name/role/must_change_password, account_requests
0007 node_ports (node_id, port, is_open, manual, project_id for extra ports)
0008 port_exposure: node_ports.public; nodes.firewall_state/firewall_error/firewall_synced_at
0009 account_security: password_resets (token_hash); sessions.created_at/ip/user_agent
0010 app_placement: node_ips (node_id, ip, label); projects.bind_ip, cpu_limit (% of core), memory_limit_mb
0011 audit_cap: per-app audit trimmed to newest 50 (also enforced on insert: api/common.rs trim_app_audit)
0012 app_limits: projects + databases cpu_limit, memory_limit_mb, disk_limit_mb, backup_limit (0 = no limit)
0013 backup_archives: backups.status (running|complete|failed), error, checksum_sha256, file_count, locked, created_by, completed_at, archive_path ('' = older folder backup, 'pending' = being built on a node)
0014 database_backups: id, database_id (FK cascade), created_at, completed_at, status running|complete|failed, error, size_bytes, checksum_sha256, note, locked, created_by, archive_path (pg_dump -F c file)
0015 hash_session_ids: sessions.id now stores sha256(cookie value) (auth::manager::session_key); the cookie keeps the raw id
0016 limit_events: id, created_at, kind project|database, app_id, app_name, limit_kind cpu|ram|disk|ram_start, message, stopped (newest 200 kept); shown as pop-ups
Project row is read by column position: SELECT_PROJECT_FIELDS + pg_row_to_project (src/projects/models.rs) — new columns go at the end, indices must match. Project literals in tests: caddy.rs, cluster/node_exec.rs, cluster/shift.rs, runtime/tree.rs.
Settings keys added: allow_registration, registration_email_domains, require_2fa, audit_retention_days, db_backup_keep, notify_webhook_url, custom_modules.
