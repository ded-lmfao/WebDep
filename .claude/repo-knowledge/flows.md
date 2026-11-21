# Flows
1. Install single VPS: start.sh --service -> install_release_binary -> webdep --pair-local -> write webdep-core + webdep-node units -> systemctl enable/restart.
2. Join worker: panel POST /api/cluster/enroll-tokens -> cluster/manager.rs create_enroll_token builds join/service/fresh_vps commands -> UI cluster.js shows freshVpsCommand -> on VPS2 start.sh --service --node --join --token -> webdep --enroll-only writes data/node-state.json -> webdep-node unit.
3. Deploy: /api/projects/* -> deployer (builder, worker, retention) -> runtime (process/container supervisor) -> caddy reload.
4. Create app: POST /api/projects or /api/databases -> needs is_admin or panel create_<kind> (or legacy *_manage) -> port from node pool (none free => 409, no app row) -> non-admin gets owner grant (grants::grant_owner) -> access::after_change.
5. Port/IP/limit change (admin): api/ports.rs -> UPDATE projects -> propagate_project_change (process env, Caddy write+reload or remote project.ensure + sync_remote_caddy, firewall) -> restart if running (limits/port).
6. Firewall: pool change -> ports::firewall_targets -> apply_on_node (local sync_firewall_ports or RPC firewall.ports) -> nodes.firewall_state ok/pending/failed; resync on node Register and Core start.
7. Upgrade: boot -> pending migrations? -> backup_before_migrate (pg_dump pre-upgrade-*.dump, keep 5) -> run_migrations (ignore_missing for rollback).
8. Hourly: main.rs loop -> maintenance::prune_audit, daily_backup_if_due, check_port_pools -> notify (webhook via curl).
