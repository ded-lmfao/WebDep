# File index
- CLI parse: src/main.rs:55 CliOptions, :69 parse_cli_options
- Join commands: src/cluster/manager.rs:~170-200 (join_command, service_command, fresh_vps_command); model src/cluster/models.rs:321-323; test src/api/cluster.rs:~1308-1362; UI public/js/cluster.js:418-424 (freshVpsCommand)
- Static embed: src/static_assets.rs:13
- Installer: start.sh (install_release_binary, write_unit, core-only service), install.sh (Linux one-line bootstrap + menu), install.ps1 (Windows one-line bootstrap + menu; start-webdep.ps1 launcher, WebDep scheduled task)
Propagation (join command format): manager.rs -> api/cluster.rs tests -> README.md
