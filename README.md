# WebDep

**A modern, self-hosted multi-VPS deployment platform and process orchestrator written in Rust.**  
*Turn one or more Linux or Windows servers into your own private PaaS with zero container overhead.*

---

## Overview

**WebDep** is a self-contained hosting, deployment, and multi-server fleet orchestrator compiled into a **single static binary** with embedded frontend assets (`rust-embed`). It combines an async control plane (**Web-Core**), lightweight worker agents (**Web-Node**), Git deployment pipelines, automated Caddy HTTPS, live SSE telemetry, an AES-256-GCM encrypted `.env` vault, managed PostgreSQL clusters, and direct node-to-node live app shifting—without Docker or external orchestration clutter.

On first boot, **Web-Core** automatically initializes a zero-config control PostgreSQL cluster (`data/control-pg/` on port `5431`, or the next free port below it; or your external `DATABASE_URL`). Upgrading from WebDep 2.x? The old `data/webdep.sqlite` is imported once, automatically — see [Upgrading from 2.x](#upgrading-from-2x-sqlite--postgresql). **Web-Node** workers require **zero PostgreSQL**—they connect outbound to Web-Core over WebSocket and store only their local manifest in `data/node-state.json`.

---

## Understanding `Web-Core` vs `Web-Node`

Every WebDep server runs one of two roles using the **same binary**:

| Component | What it is | Needs PostgreSQL? | Where you run it |
| :--- | :--- | :--- | :--- |
| **Web-Core** (`--core`) | **The Control Plane & Dashboard.** Serves the web UI (`:4600`), stores users/RBAC/projects/deployments, holds `.env` Vault secrets & off-node snapshots, and dispatches work. | Yes (auto-started on port `5431`) | **Only on VPS 1** (your main server) |
| **Web-Node** (`--node`) | **A Worker Agent.** Connects outbound to Web-Core, builds & supervises apps locally, configures Caddy, streams logs/files, and receives direct P2P `.tar.gz` app shifts. | **No** | Automatically paired on VPS 1 + installed on **VPS 2, 3, ...** |

### Quick Answer: *"Do I need to run `--node` on VPS 1?"*
**No — not manually.**
- When you install on **VPS 1** with `sudo ./start.sh --service` (or run `webdep` with no flags), **both `Web-Core` and `Web-Node` start automatically on VPS 1**. That gives you the dashboard **and** lets VPS 1 host websites, services, and databases right away.
- You only run `--node --join ...` on **VPS 2, VPS 3, ...** so extra servers join your dashboard without running duplicate control panels.
- **When would you use `--core` alone?** Only if you want VPS 1 to act as a **dashboard-only control server** hosting zero user apps (for example, in a 3+ VPS fleet), or when testing separate worker processes locally on one machine.

---

## Setup Guides

### Quick Install — One Line (Linux VPS)

SSH into your server and paste **one** line. No coding needed: it installs git/curl, downloads WebDep into `/opt/webdep`, sets up the 24/7 system service, and opens the firewall ports.

**Both — dashboard + host websites on this VPS** *(recommended for a single VPS)*

```bash
curl -fsSL https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.sh | sudo bash -s -- both
```

**Core — dashboard only**

```bash
curl -fsSL https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.sh | sudo bash -s -- core
```

**Node — extra VPS joining your dashboard** *(copy the ready-made line from the panel: **Nodes → + Add Node**)*

```bash
curl -fsSL https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.sh | sudo bash -s -- node --join http://<VPS1_IP>:4600 --token <WDJOIN_TOKEN>
```

Not sure which one? Run it without a role and pick from a menu (1 Both · 2 Core · 3 Node · 4 Update · 5 Status · 6 Uninstall). It asks for the install folder and, for a Node, the dashboard URL and token:

```bash
curl -fsSL https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.sh | sudo bash
```

**Windows (PowerShell, no admin needed):** same menu, installs to `%LOCALAPPDATA%\WebDep`, starts automatically at logon:

```powershell
irm https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.ps1 | iex
```

Direct, without the menu: `& ([scriptblock]::Create((irm https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.ps1))) both` (or `core`, or `node -Join http://<VPS1_IP>:4600 -Token <TOKEN>`). Until a release includes the Windows download, it installs Rust + Visual C++ Build Tools and compiles (10–20 min, first time only). `both`/`core` also need PostgreSQL, which it installs via `winget` if missing.

Then open `http://YOUR_SERVER_IP:4600`. Your first login (`ADMIN_USERNAME` / `ADMIN_PASSWORD`) is in `/opt/webdep/.env` and is logged once on first start (`sudo journalctl -u webdep-core | grep -i password`).

| Option | Meaning |
|--------|---------|
| `--dir /path` | Install somewhere other than `/opt/webdep` (e.g. `--dir /root/webdep`) |
| `--branch <name>` | Install a different Git branch (default `main`) |
| `--name`, `--region`, `--public-ip` | Node display name, region label, public IP |
| `--uninstall` | Stop and remove the WebDep services (your files stay in the install folder) |

**Update later:** run the same line again. Your `data/` and `.env` are kept.

---

### 1. Single-VPS Setup (Linux) — Manual

```bash
git clone https://github.com/ded-lmfao/WebDep.git webdep
cd webdep

# Install & run Web-Core + local Web-Node 24/7 via systemd
sudo ./start.sh --service
```

Open `http://YOUR_SERVER_IP:4600` in your browser. Your initial `ADMIN_USERNAME` and `ADMIN_PASSWORD` are written to `.env` (and logged once on first startup).

---

### 2. Multi-VPS Setup (2+ Servers: Control VPS + Worker VPS)

#### Step 1 — Install on **VPS 1** (Control Plane + Node 1)

```bash
curl -fsSL https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.sh | sudo bash -s -- both
```

This installs `webdep-core.service` + `webdep-node.service`, configures UFW and WireGuard (`wireguard-tools`), and opens ports `80`, `443`, `4600` TCP, and `51820` UDP.

#### Step 2 — Generate the Complete VPS 2 Command in the Panel

1. Open `http://<VPS1_IP>:4600` and sign in as **Main Admin**.
2. In the left sidebar, open **Nodes** → click **`+ Add Node`**.
3. Fill in:
   - **VPS / Node Name** (e.g. `VPS-2`)
   - **Region** (e.g. `mumbai-IND`)
   - **Target VPS 2 Public IP** (`--public-ip`, e.g. `203.0.113.50`)
   - **Panel Reachable URL**: auto-detected by the panel with 1-click chips (`Public IP`, `LAN IP`, `Current Browser`). Pick the URL that VPS 2 can reach.
4. Click **`Generate Join Command`** and copy the generated one-line **Fresh Linux VPS 2** command.

#### Step 3 — Join **VPS 2** (One Copy & Paste)

Paste the generated command directly on VPS 2 (it installs WebDep, sets up the service, enrolls, and connects automatically):

```bash
curl -fsSL https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.sh | sudo bash -s -- \
  --node --join http://<VPS1_IP>:4600 --token <WDJOIN_TOKEN> \
  --name "VPS-2" --region "mumbai-IND" --public-ip "<VPS2_IP>"
```

What this does automatically:
- Enrolls VPS 2 once (`data/node-state.json`) and consumes the single-use join token.
- Installs and enables **`webdep-node.service`** so VPS 2 reconnects automatically across reboots.
- Configures WireGuard P2P mesh keys and peer IPs (`10.200.0.x`).

Refresh **Nodes** in your browser — both **VPS 1** and **VPS 2** appear **Online** with live CPU, RAM, disk, mesh IP, and P2P port status.

> **Cloud Firewall Reminder:** If your VPS provider uses an external security group (Hetzner, AWS, DigitalOcean, Oracle, etc.), allow:
> - **VPS 1:** TCP `80`, `443`, `4600` (inbound), UDP `51820` (WireGuard), and your node P2P port (`WEBDEP_NODE_P2P_PORT` / mesh)
> - **VPS 2+:** TCP `80`, `443` (if serving public traffic), UDP `51820` (WireGuard), and node P2P TCP between VPSes

#### Step 4 — Dashboard-Only Core *(Optional, for Dedicated Control Plane)*

If you want VPS 1 to run **only Web-Core** without hosting apps on VPS 1:

```bash
curl -fsSL https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.sh | sudo bash -s -- core
# or, inside an existing checkout:
sudo ./start.sh --service --core
```

#### Upgrading Your Fleet Later

- **From the panel:** Go to **Nodes** and click **`Update Fleet`** to upgrade connected remote Web-Nodes first, then Web-Core.
- **From SSH:** Run `./start.sh --update` (legacy single `webdep.service` installs are automatically migrated to `webdep-core` + `webdep-node`).

---

### 3. Local Multi-Node Practice on Windows (Single PC)

Want to test **Web-Core + 2 separate Web-Node workers** on one Windows computer before deploying to VPSes? Give each process its own folder and `.env` so `PORT` and `DATA_DIR` stay isolated.

First build once:

```powershell
cargo build --locked
```

1. **PowerShell Window 1 — Start `Web-Core` (panel only, `http://127.0.0.1:4600`):**
   ```powershell
   mkdir C:\tmp\webdep-core -Force | Out-Null; cd C:\tmp\webdep-core; Set-Content .env "ADMIN_USERNAME=admin`nADMIN_PASSWORD=AdminPass123`nHOST=127.0.0.1`nPORT=4600`nDATA_DIR=C:\tmp\webdep-core\data"; C:\Users\User\Documents\codes\Web-Dep\target\debug\webdep.exe --core
   ```
   Open `http://127.0.0.1:4600` → login (`admin` / `AdminPass123`) → **Nodes → `+ Add Node`** → click **`Use 127.0.0.1 (Local)`** for Target IP, select the **`Current Browser`** URL chip if needed, and generate **Token 1**.

2. **PowerShell Window 2 — Enroll & start `VPS-1`:** *(paste Token 1 into line 1)*
   ```powershell
   $TOKEN1 = "PASTE_TOKEN_1_HERE"
   mkdir C:\tmp\webdep-node1 -Force | Out-Null; cd C:\tmp\webdep-node1; Set-Content .env "DATA_DIR=C:\tmp\webdep-node1\data`nPORT=4691"; $env:WEBDEP_NODE_P2P_BIND="127.0.0.1"; $env:WEBDEP_NODE_P2P_PORT="0"; C:\Users\User\Documents\codes\Web-Dep\target\debug\webdep.exe --node --join http://127.0.0.1:4600 --token $TOKEN1 --name "VPS-1" --region "mumbai-IND" --public-ip 127.0.0.1
   ```

3. **PowerShell Window 3 — Enroll & start `VPS-2`:** *(generate Token 2 in `+ Add Node` and paste into line 1)*
   ```powershell
   $TOKEN2 = "PASTE_TOKEN_2_HERE"
   mkdir C:\tmp\webdep-node2 -Force | Out-Null; cd C:\tmp\webdep-node2; Set-Content .env "DATA_DIR=C:\tmp\webdep-node2\data`nPORT=4692"; $env:WEBDEP_NODE_P2P_BIND="127.0.0.1"; $env:WEBDEP_NODE_P2P_PORT="0"; C:\Users\User\Documents\codes\Web-Dep\target\debug\webdep.exe --node --join http://127.0.0.1:4600 --token $TOKEN2 --name "VPS-2" --region "eu-central" --public-ip 127.0.0.1
   ```

4. **Restarting an already-enrolled worker later** *(no token needed once `node-state.json` exists)*:
   ```powershell
   cd C:\tmp\webdep-node1; $env:WEBDEP_NODE_P2P_BIND="127.0.0.1"; $env:WEBDEP_NODE_P2P_PORT="0"; C:\Users\User\Documents\codes\Web-Dep\target\debug\webdep.exe --node
   ```

---

## Multi-Node Architecture & Capabilities

```text
                       Inbound Internet Traffic
                                  │
                                  ▼
                    ┌───────────────────────────┐
                    │       Caddy Proxy         │  (Port 80 / 443)
                    │  (Auto HTTPS / Let's Enc) │
                    └─────────────┬─────────────┘
                                  │
        ┌─────────────────────────┴─────────────────────────┐
        │ (panel.domain.com)                                │ (*.yourdomain.com)
        ▼                                                   ▼
┌────────────────────────────────────┐            ┌───────────────────────────────┐
│        Web-Core Control Plane      │            │     Web-Node Worker Fleet     │
│      (Axum + Tokio + PostgreSQL)   │◄═══ WS ═══►│  VPS 1 (local)  │  VPS 2..N   │
│                                    │  RPC/Logs  │  • Processes    │  • Caddy    │
│ • Passkey, TOTP 2FA & Access RBAC  │            │  • Git Builds   │  • Backups  │
│ • AES-256-GCM Vault & Off-Node DR  │            └────────▲──────────────▲───────┘
│ • Shift Orchestrator & Fleet Ops   │                     │  Direct P2P  │
└────────────────────────────────────┘                     └──────────────┘
                                                      (.tar.gz + WireGuard Mesh)
```

### Live App Shifting (`Move Node`) & 2-Stage Caddy TLS
Move a website or backend service from one VPS to another directly from its workspace toolbar (**`Move Node`**) or evacuate an entire server from **Nodes**:
1. **Preflight & Free Port Allocation**: Target node allocates an unused local port and validates host readiness.
2. **Direct P2P Streaming Transfer**: Source packs active files (`current`, `releases`, `app`, `containers`, `backups`) into a hardened `.tar.gz` stream (skipping `node_modules`/`.git`/symlinks and rejecting path traversal) and pushes directly to the Target node's token-authenticated P2P HTTP receiver.
3. **Warm-Start & Health Probe**: Target boots the app with decrypted `.env` secrets (rewriting linked `DATABASE_URL` / `PGHOST` to the database node's mesh IP automatically) and verifies health before switching live traffic. Any failure rolls back Target cleanly while leaving Source untouched.
4. **2-Stage Caddy + DNS Update Card**: If domains are attached, Target binds HTTP first and presents a **DNS Update Card** (`A` record old IP → new IP with live DNS verification and manual *Complete Cutover Now*), then upgrades Target to automatic Let's Encrypt HTTPS, stops Source, cleans Source's Caddy config, and increments `placement_version` in PostgreSQL to prevent split-brain on reconnect.

### Off-Node Backups & Cross-Node Disaster Recovery
- Under a workload's **Releases** tab, use **Save Off-Node on Web-Core** to pull a verified `.tar.gz` snapshot to Web-Core (`data/offnode-backups/project-<id>/`).
- Restore any off-node snapshot onto **any healthy Web-Node** using **Restore to Node**, automatically reallocating a free port, restoring `.env` Vault secrets, and rewriting linked PostgreSQL mesh routes.

---

## Panel Sections & Workspace Structure

### Sidebar Navigation

| Section | Purpose |
| :--- | :--- |
| **Overview** | Real-time fleet telemetry (network throughput, host CPU/RAM/disk, active workloads, node filter). |
| **Websites** | Multi-framework web apps placed on any node with auto-allocated ports, custom domains, Caddy HTTPS, and maintenance mode. |
| **Services** | 24/7 background daemons (Lavalink/Java, Python workers, Node bots, Go/Rust binaries) with live PID tracking and log streaming. |
| **Databases** | Isolated native PostgreSQL clusters on any node with SCRAM-SHA-256 auth, local/mesh/CIDR/public rules, and 1-click `DATABASE_URL` linking. |
| **Nodes** | Cluster Nodes & Mesh Topology view with per-node network charts, display names, `+ Add Node` token/command generator, drain/evacuate, and `Update Fleet`. |
| **Modules** | Per-VPS runtime readiness (Git, Node.js, Python, Java, Go, Rust, PHP, Ruby, .NET, UnRAR) with one-click install/remove through the server's package manager (Main Admin only). |
| **Platform Settings** | WebAuthn Passkeys, TOTP 2FA, users with simple access levels (Viewer / Developer / Full access / Custom) and per-website scope, GitHub tokens, SHA-256 verified Core/Fleet updates, server cache/restart controls, and 15 themes. |

The panel is fully responsive: on phones and tablets the sidebar becomes a drawer with a bottom tab bar, and data tables turn into compact cards.

### Website & Service Workspace Tabs
Opening any website or service exposes its workspace toolbar (**Start / Stop / Restart**, **Deploy**, **Move Node**) and tab bar:
- **Overview** — Live 7-stage deployment pipeline (`Pending -> Fetching -> Pulling -> Installing -> Building -> Deploying -> Health Checking`), URL/domain routing, and status.
- **Commits** — Branch and commit history for Git-connected workloads.
- **Logs** — Real-time stdout/stderr and build log streaming over SSE/WebSocket.
- **Files** — In-browser remote file explorer, upload/download, and syntax-highlighted editor scoped strictly to the project root.
- **Environment** — AES-256-GCM encrypted `.env` Vault secrets and linked database bindings.
- **Releases** — Build archives, local snapshots, **Save Off-Node on Web-Core**, and **Restore to Node**.
- **Activity** — Workload audit trail and placement/shift events.

### Data Layout

Everything WebDep stores lives under `DATA_DIR` (default `./data` inside the install folder; the service log prints `Data directory: ...` on start):

| Path | Contents |
| :--- | :--- |
| `control-pg/pgdata/` | Web-Core's own PostgreSQL database (users, projects, deployments, settings, nodes). `control-pg/conn.json` holds its port and credentials. |
| `master.key` | Key for the encrypted `.env` vault. **Back it up together with `control-pg/`** — without it stored secrets cannot be decrypted. |
| `databases/db-<id>/data/` | Each managed PostgreSQL database created in the panel. |
| `containers/<app>/`, `sites/`, `services/` | App runtime data and deployed code. |
| `caddy/`, `mesh/`, `keys/` | Reverse-proxy config and certificates, WireGuard mesh, deploy keys. |

Do not copy these folders while PostgreSQL is running; use panel backups or stop WebDep first.

### Upgrading from 2.x (SQLite → PostgreSQL)

On the first 3.x start, Web-Core imports `data/webdep.sqlite` into PostgreSQL in a single transaction (the SQLite file is opened read-only). If the import fails, WebDep stops without changing anything; fix the cause and restart, or retry with `webdep --import-sqlite`. It never imports twice.

To confirm the import and retire the old file, compare row counts on the server (from the install folder):

```bash
C=data/control-pg/conn.json
PGPASSWORD=$(python3 -c "import json;print(json.load(open('$C'))['password'])") psql -h 127.0.0.1   -p $(python3 -c "import json;print(json.load(open('$C'))['port'])") -U webdep_core -d webdep_core   -c "SELECT (SELECT count(*) FROM users) users, (SELECT count(*) FROM projects) projects, (SELECT count(*) FROM databases) dbs, (SELECT count(*) FROM deployments) deployments;"
python3 -c "import sqlite3; c=sqlite3.connect('file:data/webdep.sqlite?mode=ro', uri=True); print({t: c.execute(f'select count(*) from {t}').fetchone()[0] for t in ['users','projects','databases','deployments']})"
```

When PostgreSQL shows at least the SQLite numbers, move `data/webdep.sqlite*` out of `data/`, restart and check the panel, then delete the old files.

### Running as a Non-Root User (Ubuntu)

If the service runs as a regular user (for example `ubuntu`), systemd-logind deletes that user's shared memory when their last SSH session ends (`RemoveIPC=yes`). The control PostgreSQL then refuses new connections (`could not open shared memory segment`) and WebDep fails on its next restart. Disable it once per server:

```bash
sudo sed -i 's/^#\?RemoveIPC=.*/RemoveIPC=no/' /etc/systemd/logind.conf && sudo systemctl restart systemd-logind
```

If it already happened: `sudo systemctl stop webdep-core`, stop the stale server with `pg_ctl -D data/control-pg/pgdata -m fast stop` (as the service user), then start WebDep again. Services running as `root` are not affected.

---

## CLI & Configuration Reference

### CLI Flags

| Flag | Description |
| :--- | :--- |
| *(none)* | Default mode on Core host: starts **Web-Core** and auto-pairs/runs a local **Web-Node**. |
| `--core` | Run **Web-Core only** (panel + PostgreSQL, no local Web-Node agent). |
| `--node` | Run **Web-Node worker only** (no PostgreSQL; loads `data/node-state.json`). |
| `--join <URL>` | Enroll a Web-Node into Core (requires `--node`, `--token <WDJOIN_TOKEN>`). |
| `--name <LABEL>` | Node label used during enrollment (or local pairing). |
| `--region <REGION>` | Node region metadata (e.g. `mumbai-IND`, `eu-central`). |
| `--public-ip <IP>` | Public IP/hostname advertised for DNS cutover cards and P2P fallback. |
| `--wg-port <PORT>` | WireGuard UDP listen port (default `51820`). |
| `--enroll-only` | Enroll, write `data/node-state.json`, and exit (used by `start.sh --service`). |
| `--pair-local` | Pair the local Web-Node against Core PostgreSQL without starting the server. |
| `--version` / `-V` | Print binary version and exit (used by fleet self-update checks). |

### Environment Variables (`.env`)

WebDep loads configuration from `.env` in the process's current working directory or environment variables:

| Variable | Default | Description |
| :--- | :--- | :--- |
| `HOST` | `0.0.0.0` | Network interface for Web-Core panel binding. |
| `PORT` | `4600` | HTTP port for Web-Core panel and API. |
| `DATA_DIR` | `./data` | Data root for `control-pg/`, `node-state.json`, `sites/`, `services/`, `offnode-backups/`, and vault key. |
| `DATABASE_URL` | *(auto zero-config PG on `5431`)* | Optional external PostgreSQL connection URL for Web-Core (`--node` never reads this). |
| `ADMIN_USERNAME` | `admin` | Main administrator login username. |
| `ADMIN_PASSWORD` | *(auto-generated on 1st boot)* | Main administrator password (random 20-char secret generated at startup if empty/default). |
| `ADMIN_RESET_2FA` | `false` | Set to `true` temporarily to reset Main Admin TOTP 2FA on startup. |
| `SESSION_TTL_S` | `604800` | Absolute session lifetime in seconds (7 days default). |
| `SESSION_IDLE_TIMEOUT_S` | `1800` | Idle inactivity session expiration in seconds (`0` disables). |
| `FORCE_SECURE_COOKIES` | `false` | Unconditionally set `Secure` cookies (auto-enabled on `X-Forwarded-Proto: https`). |
| `PUBLIC_IP` | *(auto-detected)* | Optional override for Core panel IP returned by `/api/cluster/panel-info`. |
| `WEBDEP_NODE_P2P_BIND` | `0.0.0.0` | Bind address for Web-Node's direct P2P archive receiver. |
| `WEBDEP_NODE_P2P_PORT` | `0` *(ephemeral)* | TCP port for Web-Node's direct P2P HTTP receiver (`0` picks a free port). |
| `WEBDEP_MANAGE_FIREWALL` | `true` | Set to `false` to stop WebDep from editing UFW/iptables (panel, app, database and WireGuard mesh rules). Use it when the host firewall is managed elsewhere. |
| `RUST_LOG` | `webdep=info` | Tracing log filter (`debug`, `info`, `warn`, `error`). |

---

## Supported Runtimes & Frameworks

| Runtime | Example Install Command | Example Start Command | Use Cases |
| :--- | :--- | :--- | :--- |
| **Node.js** | `npm ci` / `pnpm install` | `npm start` / `node server.js` | Next.js, Express, Fastify, APIs, Node services |
| **Python** | `pip install -r requirements.txt` | `python -m uvicorn main:app --port {{PORT}}` | FastAPI, Flask, Django, Python workers |
| **Go** | `go build -o app .` | `./app` | Gin, Echo, Fiber, backend daemons |
| **Rust** | `cargo build --release` | `./target/release/server` | Axum, Actix-web, high-throughput services |
| **Java** | Pre-built JAR / Gradle | `java -jar Lavalink.jar` | Lavalink audio servers, Spring Boot |
| **Static / SPA** | `npm run build` | Static build output | React, Vite, Vue, Svelte, Astro |

---

## Automated Releases (CI/CD)

Pushes to `main` run the Rust test suite, a frontend syntax check and the SonarCloud quality gate, then build static `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` binaries plus a Windows `x86_64` binary via [`.github/workflows/release.yml`](.github/workflows/release.yml) and publish SemVer releases (`vX.Y.Z`). A release is only cut when the quality gate passes:

- **Default commit** → bumps **`patch`** (`v2.0.0` → `v2.0.1`)
- **`[minor]` / `minor:`** in commit message → bumps **`minor`** (`v2.0.1` → `v2.1.0`)
- **`[major]` / `major:` / `BREAKING CHANGE:` / `feat!:`** → bumps **`major`** (`v2.1.0` → `v3.0.0`)
- **`docs:` / `[docs]` / `[skip release]` / `[skip ci]`** → skips cutting a release

---

## Testing & Verification

```bash
# Run all Rust unit and integration tests
cargo test --locked

# Lints (keep this clean; CI's SonarCloud gate blocks releases on new issues)
cargo clippy --tests

# Verify every frontend module's syntax (same check as CI)
for f in public/app.js public/js/*.js; do node --check "$f"; done
```

PostgreSQL-backed tests start a throwaway cluster automatically when PostgreSQL binaries are installed, or use `WEBDEP_TEST_DATABASE_URL` if set.

---

## License

This project is licensed under the [MIT License](LICENSE).
