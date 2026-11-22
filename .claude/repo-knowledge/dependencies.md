# Dependencies
Hubs: api (router+AppState) -> auth (grants, policy, manager), projects, deployer, runtime (process, limits, port), cluster (manager, ports, node_ips, shift, modules), databases, caddy, logs, monitor, vault, updater, maintenance.
maintenance -> db, cluster::ports (free_and_open_counts), pg_dump via control-pg conn.json, curl (webhooks); called by main.rs loop, api/accounts.rs (request notify), cluster/manager.rs (node offline/online).
Changing grants.rs powers affects: api/access.rs, api/users.rs, api/projects.rs, api/databases.rs, public/js/permissions.js, accounts.js.
External: GitHub (git clone, Releases), Caddy (TLS reverse proxy, `bind` per app IP), PostgreSQL binaries (pg_dump/pg_restore too), WireGuard (mesh), UFW/iptables (sudo -n), cgroups v2 (/sys/fs/cgroup/webdep), curl.
