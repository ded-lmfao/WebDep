# Data model (migrations/0001-0010, Postgres; registered in src/pg_control.rs build_migrator)
_meta settings projects project_env deployments releases logs users sessions backups audit project_keys user_passkeys databases access_grants(powers_json) nodes node_enroll_tokens migrations
0006 user_management: users.email/display_name/role/must_change_password, account_requests
0007 node_ports (node_id, port, is_open, manual, project_id for extra ports)
0008 port_exposure: node_ports.public; nodes.firewall_state/firewall_error/firewall_synced_at
0009 account_security: password_resets (token_hash); sessions.created_at/ip/user_agent
0010 app_placement: node_ips (node_id, ip, label); projects.bind_ip, cpu_limit (% of core), memory_limit_mb
Project row is read by column position: SELECT_PROJECT_FIELDS + pg_row_to_project (src/projects/models.rs) — new columns go at the end, indices must match. Project literals in tests: caddy.rs, cluster/node_exec.rs, cluster/shift.rs, runtime/tree.rs.
Settings keys added: allow_registration, registration_email_domains, require_2fa, audit_retention_days, db_backup_keep, notify_webhook_url, custom_modules.
