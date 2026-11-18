# Flows
1. Install single VPS: start.sh --service -> install_release_binary -> webdep --pair-local -> write webdep-core + webdep-node units -> systemctl enable/restart.
2. Join worker: panel POST /api/cluster/enroll-tokens -> cluster/manager.rs create_enroll_token builds join/service/fresh_vps commands -> UI cluster.js shows freshVpsCommand -> on VPS2 start.sh --service --node --join --token -> webdep --enroll-only writes data/node-state.json -> webdep-node unit.
3. Deploy: /api/projects/* -> deployer (builder, worker, retention) -> runtime (process/container supervisor) -> caddy reload.
