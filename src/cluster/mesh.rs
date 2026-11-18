use base64::{engine::general_purpose::STANDARD, Engine as _};
use rand::RngCore;
use std::collections::HashSet;
use std::path::Path;

use super::models::{NodeCapabilities, NodeStateManifest, PeerConfig};

pub const DEFAULT_WG_PORT: u16 = 51820;

/// Allocates the next available `10.200.<hi>.<lo>` IPv4 inside the private mesh subnet.
pub fn next_available_mesh_ip(existing: &[String]) -> String {
    let used: HashSet<&str> = existing.iter().map(|s| s.trim()).collect();
    for hi in 0..=254u16 {
        let start_lo = if hi == 0 { 1 } else { 1 };
        for lo in start_lo..=254u16 {
            let candidate = format!("10.200.{hi}.{lo}");
            if !used.contains(candidate.as_str()) {
                return candidate;
            }
        }
    }
    "10.200.254.254".to_string()
}

/// Generates a Curve25519 (WireGuard) keypair `(private_b64, public_b64)`.
/// Uses `wg genkey`/`wg pubkey` when installed; falls back to pure-Rust RFC 7748 X25519.
pub fn generate_wireguard_keypair() -> (String, String) {
    #[cfg(unix)]
    if let Some(pair) = try_wg_cli_keypair() {
        return pair;
    }

    let mut sk = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut sk);
    sk[0] &= 248;
    sk[31] &= 127;
    sk[31] |= 64;

    let pk = x25519_basepoint(&sk);
    (STANDARD.encode(sk), STANDARD.encode(pk))
}

#[cfg(unix)]
fn try_wg_cli_keypair() -> Option<(String, String)> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let out = Command::new("wg").arg("genkey").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let priv_b64 = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let mut child = Command::new("wg")
        .arg("pubkey")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(priv_b64.as_bytes());
    }
    let pub_out = child.wait_with_output().ok()?;
    if !pub_out.status.success() {
        return None;
    }
    let pub_b64 = String::from_utf8_lossy(&pub_out.stdout).trim().to_string();
    if priv_b64.is_empty() || pub_b64.is_empty() {
        return None;
    }
    Some((priv_b64, pub_b64))
}

/// Pure-Rust RFC 7748 X25519 scalar multiplication over GF(2^255 - 19).
pub fn x25519_scalarmult(scalar: &[u8; 32], u_bytes: &[u8; 32]) -> [u8; 32] {
    let mut k = *scalar;
    k[0] &= 248;
    k[31] &= 127;
    k[31] |= 64;

    x25519_ladder_51(&k, u_bytes)
}

pub fn x25519_basepoint(scalar: &[u8; 32]) -> [u8; 32] {
    let mut base = [0u8; 32];
    base[0] = 9;
    x25519_scalarmult(scalar, &base)
}

#[derive(Clone, Copy)]
struct Fe([u64; 5]);

impl Fe {
    const ZERO: Self = Self([0, 0, 0, 0, 0]);
    const ONE: Self = Self([1, 0, 0, 0, 0]);

    fn from_bytes(b: &[u8; 32]) -> Self {
        let mask51 = (1u64 << 51) - 1;
        let load8 = |off: usize| -> u64 { u64::from_le_bytes(b[off..off + 8].try_into().unwrap()) };
        let t0 = load8(0);
        let t1 = load8(6) >> 3;
        let t2 = load8(12) >> 6;
        let t3 = load8(19) >> 1;
        let t4 = (load8(24) >> 12) & mask51;
        Self([t0 & mask51, t1 & mask51, t2 & mask51, t3 & mask51, t4])
    }

    fn to_bytes(mut self) -> [u8; 32] {
        let mask51 = (1u64 << 51) - 1;
        self.reduce();
        let mut t = self.0;
        // Canonical reduction mod (2^255 - 19)
        let mut c = (t[0] + 19) >> 51;
        c = (t[1] + c) >> 51;
        c = (t[2] + c) >> 51;
        c = (t[3] + c) >> 51;
        c = (t[4] + c) >> 51;
        t[0] += 19 * c;
        t[1] += t[0] >> 51;
        t[0] &= mask51;
        t[2] += t[1] >> 51;
        t[1] &= mask51;
        t[3] += t[2] >> 51;
        t[2] &= mask51;
        t[4] += t[3] >> 51;
        t[3] &= mask51;
        t[4] &= mask51;

        let mut out = [0u8; 32];
        let s0 = t[0] | (t[1] << 51);
        let s1 = (t[1] >> 13) | (t[2] << 38);
        let s2 = (t[2] >> 26) | (t[3] << 25);
        let s3 = (t[3] >> 39) | (t[4] << 12);
        out[0..8].copy_from_slice(&s0.to_le_bytes());
        out[8..16].copy_from_slice(&s1.to_le_bytes());
        out[16..24].copy_from_slice(&s2.to_le_bytes());
        out[24..32].copy_from_slice(&s3.to_le_bytes());
        out
    }

    fn reduce(&mut self) {
        let mask51 = (1u64 << 51) - 1;
        for _ in 0..2 {
            for i in 0..4 {
                let c = self.0[i] >> 51;
                self.0[i] &= mask51;
                self.0[i + 1] += c;
            }
            let c = self.0[4] >> 51;
            self.0[4] &= mask51;
            self.0[0] += c * 19;
        }
    }

    fn add(self, o: Self) -> Self {
        Self([
            self.0[0] + o.0[0],
            self.0[1] + o.0[1],
            self.0[2] + o.0[2],
            self.0[3] + o.0[3],
            self.0[4] + o.0[4],
        ])
    }

    fn sub(self, o: Self) -> Self {
        // Add 2 * p so unsigned limbs stay positive before multiply
        const P2_0: u64 = 0xFFFFFFFFFFFDA; // 2 * ((1<<51)-19)
        const P2_N: u64 = 0xFFFFFFFFFFFFE; // 2 * ((1<<51)-1)
        let mut r = Self([
            (self.0[0] + P2_0) - o.0[0],
            (self.0[1] + P2_N) - o.0[1],
            (self.0[2] + P2_N) - o.0[2],
            (self.0[3] + P2_N) - o.0[3],
            (self.0[4] + P2_N) - o.0[4],
        ]);
        r.reduce();
        r
    }

    fn mul(self, o: Self) -> Self {
        let a = self.0;
        let b = o.0;
        let (a0, a1, a2, a3, a4) = (a[0] as u128, a[1] as u128, a[2] as u128, a[3] as u128, a[4] as u128);
        let (b0, b1, b2, b3, b4) = (b[0] as u128, b[1] as u128, b[2] as u128, b[3] as u128, b[4] as u128);
        let (b1_19, b2_19, b3_19, b4_19) = (b1 * 19, b2 * 19, b3 * 19, b4 * 19);

        let mut r0 = a0 * b0 + a1 * b4_19 + a2 * b3_19 + a3 * b2_19 + a4 * b1_19;
        let mut r1 = a0 * b1 + a1 * b0 + a2 * b4_19 + a3 * b3_19 + a4 * b2_19;
        let mut r2 = a0 * b2 + a1 * b1 + a2 * b0 + a3 * b4_19 + a4 * b3_19;
        let mut r3 = a0 * b3 + a1 * b2 + a2 * b1 + a3 * b0 + a4 * b4_19;
        let mut r4 = a0 * b4 + a1 * b3 + a2 * b2 + a3 * b1 + a4 * b0;

        let mask51 = (1u128 << 51) - 1;
        r1 += r0 >> 51;
        r0 &= mask51;
        r2 += r1 >> 51;
        r1 &= mask51;
        r3 += r2 >> 51;
        r2 &= mask51;
        r4 += r3 >> 51;
        r3 &= mask51;
        r0 += (r4 >> 51) * 19;
        r4 &= mask51;
        r1 += r0 >> 51;
        r0 &= mask51;

        Self([r0 as u64, r1 as u64, r2 as u64, r3 as u64, r4 as u64])
    }

    fn sq(self) -> Self {
        self.mul(self)
    }

    fn inv(self) -> Self {
        // Compute self^(p - 2) where p - 2 = 2^255 - 21
        let z2 = self.sq();
        let z9 = z2.sq().sq().mul(self);
        let z11 = z9.mul(z2);
        let t0 = z11.sq().mul(z9); // 2^5 - 1
        let mut t1 = t0;
        for _ in 0..5 {
            t1 = t1.sq();
        }
        let z_10 = t1.mul(t0); // 2^10 - 1
        let mut t2 = z_10;
        for _ in 0..10 {
            t2 = t2.sq();
        }
        let z_20 = t2.mul(z_10); // 2^20 - 1
        let mut t3 = z_20;
        for _ in 0..20 {
            t3 = t3.sq();
        }
        let z_40 = t3.mul(z_20); // 2^40 - 1
        let mut t4 = z_40;
        for _ in 0..10 {
            t4 = t4.sq();
        }
        let z_50 = t4.mul(z_10); // 2^50 - 1
        let mut t5 = z_50;
        for _ in 0..50 {
            t5 = t5.sq();
        }
        let z_100 = t5.mul(z_50); // 2^100 - 1
        let mut t6 = z_100;
        for _ in 0..100 {
            t6 = t6.sq();
        }
        let z_200 = t6.mul(z_100); // 2^200 - 1
        let mut t7 = z_200;
        for _ in 0..50 {
            t7 = t7.sq();
        }
        let z_250 = t7.mul(z_50); // 2^250 - 1
        let mut t8 = z_250;
        for _ in 0..5 {
            t8 = t8.sq();
        }
        t8.mul(z11)
    }

    fn cswap(swap: u64, a: &mut Self, b: &mut Self) {
        let mask = 0u64.wrapping_sub(swap & 1);
        for i in 0..5 {
            let x = mask & (a.0[i] ^ b.0[i]);
            a.0[i] ^= x;
            b.0[i] ^= x;
        }
    }
}

fn x25519_ladder_51(k: &[u8; 32], u_bytes: &[u8; 32]) -> [u8; 32] {
    let mut u_clean = *u_bytes;
    u_clean[31] &= 0x7F;
    let x1 = Fe::from_bytes(&u_clean);
    let mut x2 = Fe::ONE;
    let mut z2 = Fe::ZERO;
    let mut x3 = x1;
    let mut z3 = Fe::ONE;
    let mut swap = 0u64;
    let a24 = Fe([121665, 0, 0, 0, 0]);

    for t in (0..255).rev() {
        let k_t = ((k[t / 8] >> (t % 8)) & 1) as u64;
        swap ^= k_t;
        Fe::cswap(swap, &mut x2, &mut x3);
        Fe::cswap(swap, &mut z2, &mut z3);
        swap = k_t;

        let a = x2.add(z2);
        let aa = a.sq();
        let b = x2.sub(z2);
        let bb = b.sq();
        let e = aa.sub(bb);
        let c = x3.add(z3);
        let d = x3.sub(z3);
        let da = d.mul(a);
        let cb = c.mul(b);
        x3 = da.add(cb).sq();
        z3 = x1.mul(da.sub(cb).sq());
        x2 = aa.mul(bb);
        z2 = e.mul(aa.add(a24.mul(e)));
    }
    Fe::cswap(swap, &mut x2, &mut x3);
    Fe::cswap(swap, &mut z2, &mut z3);
    x2.mul(z2.inv()).to_bytes()
}

fn extract_url_host(url: &str) -> Option<String> {
    let stripped = url
        .trim()
        .strip_prefix("https://")
        .or_else(|| url.trim().strip_prefix("http://"))?;
    let host_port = stripped.split('/').next()?.trim();
    if host_port.is_empty() {
        return None;
    }
    let host = if host_port.starts_with('[') {
        host_port.split(']').next()?.trim_start_matches('[')
    } else {
        host_port.split(':').next()?
    };
    if host.is_empty() || host == "127.0.0.1" || host == "localhost" || host == "::1" {
        return None;
    }
    Some(host.to_string())
}

fn resolve_peer_endpoint(manifest: &NodeStateManifest, peer: &PeerConfig) -> Option<String> {
    let ep = peer.wg_endpoint.trim();
    if !ep.is_empty() && !ep.starts_with("127.0.0.1:") && !ep.starts_with("localhost:") {
        return Some(ep.to_string());
    }
    let pub_ip = peer.public_ip.trim();
    if !pub_ip.is_empty() && pub_ip != "127.0.0.1" && pub_ip != "::1" && pub_ip != "0.0.0.0" {
        return Some(format!("{pub_ip}:{DEFAULT_WG_PORT}"));
    }
    if peer.node_id == 1 {
        if let Some(core_host) = extract_url_host(&manifest.core_url) {
            return Some(format!("{core_host}:{DEFAULT_WG_PORT}"));
        }
    }
    if !ep.is_empty() {
        Some(ep.to_string())
    } else {
        None
    }
}

pub fn render_wg_conf(manifest: &NodeStateManifest) -> String {
    let mut out = String::new();
    out.push_str("[Interface]\n");
    out.push_str(&format!("# WebDep Web-Node: {} (node #{})\n", manifest.name, manifest.node_id));
    out.push_str(&format!("PrivateKey = {}\n", manifest.wg_private_key));
    out.push_str(&format!("Address = {}/16\n", manifest.mesh_ip));
    out.push_str(&format!("ListenPort = {}\n", manifest.wg_listen_port));
    // Clamp MTU to 1280 so WireGuard avoids Path-MTU blackholes across cloud overlays (e.g. Oracle OCI)
    out.push_str("MTU = 1280\n");

    for peer in &manifest.peers {
        if peer.node_id == manifest.node_id || peer.wg_pubkey.trim().is_empty() || peer.mesh_ip.trim().is_empty() {
            continue;
        }
        out.push_str("\n[Peer]\n");
        out.push_str(&format!("# {} (node #{})\n", peer.name, peer.node_id));
        out.push_str(&format!("PublicKey = {}\n", peer.wg_pubkey));
        out.push_str(&format!("AllowedIPs = {}/32\n", peer.mesh_ip));
        if let Some(ep) = resolve_peer_endpoint(manifest, peer) {
            out.push_str(&format!("Endpoint = {ep}\n"));
        }
        out.push_str("PersistentKeepalive = 25\n");
    }
    out
}

pub fn detect_capabilities() -> NodeCapabilities {
    detect_capabilities_with_p2p(None)
}

pub fn detect_capabilities_with_p2p(p2p_port: Option<u16>) -> NodeCapabilities {
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_all();
    let cores = sys.cpus().len().max(1);

    let has_wireguard = command_exists("wg");
    let has_caddy = command_exists("caddy");
    let has_postgres = crate::databases::binaries::PgBinaries::detect().is_ok();
    let modules_snap = super::modules::detect_node_modules_sync();

    NodeCapabilities {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        cpu_cores: cores,
        has_wireguard,
        has_caddy,
        has_postgres,
        p2p_port,
        package_manager: modules_snap.package_manager,
        modules: modules_snap.modules,
    }
}

fn command_exists(bin: &str) -> bool {
    #[cfg(windows)]
    let check = std::process::Command::new("where").arg(bin).output();
    #[cfg(not(windows))]
    let check = std::process::Command::new("which").arg(bin).output();
    check.map(|o| o.status.success()).unwrap_or(false)
}

pub async fn reconcile_mesh_network(manifest: &NodeStateManifest, data_dir: &Path) {
    let mesh_dir = data_dir.join("mesh");
    let _ = tokio::fs::create_dir_all(&mesh_dir).await;
    let local_conf_path = mesh_dir.join("webdep0.conf");
    let rendered = render_wg_conf(manifest);

    let changed = tokio::fs::read_to_string(&local_conf_path)
        .await
        .map(|existing| existing != rendered)
        .unwrap_or(true);
    if changed {
        let _ = tokio::fs::write(&local_conf_path, &rendered).await;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(&local_conf_path, std::fs::Permissions::from_mode(0o600)).await;
        }
    }

    #[cfg(unix)]
    {
        if command_exists("wg") && command_exists("wg-quick") {
            let sys_conf = Path::new("/etc/wireguard/webdep0.conf");
            let _ = tokio::process::Command::new("sudo")
                .args(["mkdir", "-p", "/etc/wireguard"])
                .output()
                .await;
            let _ = tokio::process::Command::new("sudo")
                .args([
                    "cp",
                    &local_conf_path.to_string_lossy(),
                    &sys_conf.to_string_lossy(),
                ])
                .output()
                .await;
            let _ = tokio::process::Command::new("sudo")
                .args(["chmod", "600", &sys_conf.to_string_lossy()])
                .output()
                .await;
            let up = tokio::process::Command::new("sudo")
                .args(["wg", "show", "webdep0"])
                .output()
                .await
                .map(|o| o.status.success())
                .unwrap_or(false);
            if up && changed {
                let _ = tokio::process::Command::new("sudo")
                    .args(["bash", "-c", "wg syncconf webdep0 <(wg-quick strip webdep0)"])
                    .output()
                    .await;
            } else if !up {
                let _ = tokio::process::Command::new("sudo")
                    .args(["wg-quick", "up", "webdep0"])
                    .output()
                    .await;
            }
            // Enforce safe MTU on live interface (`wg syncconf` preserves existing interface MTU)
            let _ = tokio::process::Command::new("sudo")
                .args(["ip", "link", "set", "dev", "webdep0", "mtu", "1280"])
                .output()
                .await;
        }
        if crate::runtime::port::firewall_management_enabled() {
            if command_exists("ufw") {
                let _ = tokio::process::Command::new("sudo")
                    .args(["ufw", "allow", &format!("{}/udp", manifest.wg_listen_port)])
                    .output()
                    .await;
                let _ = tokio::process::Command::new("sudo")
                    .args(["ufw", "allow", "from", "10.200.0.0/16"])
                    .output()
                    .await;
            }
            // Insert iptables ACCEPT rules ahead of cloud vendor REJECT rules (e.g. Oracle OCI)
            let wg_port = manifest.wg_listen_port;
            let ipt_cmd = format!(
                "(sudo -n iptables -C INPUT -p udp --dport {wg_port} -j ACCEPT >/dev/null 2>&1 || \
                  sudo -n iptables -I INPUT 1 -p udp --dport {wg_port} -j ACCEPT >/dev/null 2>&1 || true) && \
                 (sudo -n iptables -C INPUT -i webdep0 -j ACCEPT >/dev/null 2>&1 || \
                  sudo -n iptables -I INPUT 1 -i webdep0 -j ACCEPT >/dev/null 2>&1 || true) && \
                 (sudo -n iptables -C INPUT -s 10.200.0.0/16 -j ACCEPT >/dev/null 2>&1 || \
                  sudo -n iptables -I INPUT 1 -s 10.200.0.0/16 -j ACCEPT >/dev/null 2>&1 || true)"
            );
            let _ = tokio::process::Command::new("sh")
                .args(["-c", &ipt_cmd])
                .output()
                .await;
        }
    }

    let _ = update_managed_pg_hba_files(&data_dir.join("databases"), &manifest.peers);
}

fn update_managed_pg_hba_files(dbs_dir: &Path, peers: &[PeerConfig]) {
    let Ok(entries) = std::fs::read_dir(dbs_dir) else {
        return;
    };
    let mut peer_lines = String::new();
    for p in peers {
        if !p.mesh_ip.trim().is_empty() {
            peer_lines.push_str(&format!(
                "host    all             all             {:<23} scram-sha-256 # peer:{}\n",
                format!("{}/32", p.mesh_ip),
                p.name
            ));
        }
    }
    for entry in entries.flatten() {
        let hba = entry.path().join("data").join("pg_hba.conf");
        if hba.is_file() {
            if let Ok(existing) = std::fs::read_to_string(&hba) {
                let marker = "# WebDep Peer Mesh Entries";
                let base = existing.split(marker).next().unwrap_or(&existing).trim_end();
                let updated = if peer_lines.is_empty() {
                    format!("{base}\n")
                } else {
                    format!("{base}\n{marker}\n{peer_lines}")
                };
                if updated != existing {
                    let _ = std::fs::write(&hba, updated);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mesh_ip_allocation_avoids_collisions() {
        let used = vec!["10.200.0.1".to_string(), "10.200.0.2".to_string()];
        assert_eq!(next_available_mesh_ip(&used), "10.200.0.3");
        assert_eq!(next_available_mesh_ip(&[]), "10.200.0.1");
    }

    #[test]
    fn test_rfc7748_x25519_test_vector() {
        // RFC 7748 Section 6.1 vector: Alice private scalar & basepoint 9
        let alice_sk = hex::decode("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a").unwrap();
        let expected_pk = hex::decode("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a").unwrap();
        let sk_arr: [u8; 32] = alice_sk.try_into().unwrap();
        let pk = x25519_basepoint(&sk_arr);
        assert_eq!(pk.as_slice(), expected_pk.as_slice());

        let (sk_b64, pk_b64) = generate_wireguard_keypair();
        assert_eq!(STANDARD.decode(sk_b64).unwrap().len(), 32);
        assert_eq!(STANDARD.decode(pk_b64).unwrap().len(), 32);
    }

    #[test]
    fn test_render_wg_conf_skips_self_and_includes_peers() {
        let manifest = NodeStateManifest {
            node_id: 1,
            name: "vps-1".to_string(),
            region: "eu-west".to_string(),
            core_url: "https://core.example.com".to_string(),
            core_ws_url: "wss://core.example.com/api/cluster/node-ws".to_string(),
            mesh_ip: "10.200.0.1".to_string(),
            public_ip: "1.1.1.1".to_string(),
            wg_private_key: "priv1==".to_string(),
            wg_pubkey: "pub1==".to_string(),
            wg_listen_port: 51820,
            node_token: "tok".to_string(),
            peers: vec![
                PeerConfig {
                    node_id: 1,
                    name: "vps-1".to_string(),
                    mesh_ip: "10.200.0.1".to_string(),
                    public_ip: "1.1.1.1".to_string(),
                    wg_pubkey: "pub1==".to_string(),
                    wg_endpoint: "1.1.1.1:51820".to_string(),
                    status: "online".to_string(),
                },
                PeerConfig {
                    node_id: 2,
                    name: "vps-2".to_string(),
                    mesh_ip: "10.200.0.2".to_string(),
                    public_ip: "2.2.2.2".to_string(),
                    wg_pubkey: "pub2==".to_string(),
                    wg_endpoint: "2.2.2.2:51820".to_string(),
                    status: "online".to_string(),
                },
            ],
            workloads: vec![],
            updated_at: 100,
        };
        let conf = render_wg_conf(&manifest);
        assert!(conf.contains("Address = 10.200.0.1/16"));
        assert!(conf.contains("MTU = 1280"));
        assert!(!conf.contains("PublicKey = pub1=="));
        assert!(conf.contains("PublicKey = pub2=="));
        assert!(conf.contains("AllowedIPs = 10.200.0.2/32"));

        // Verify loopback Core endpoint on worker resolves to core_url host
        let worker_manifest = NodeStateManifest {
            node_id: 3,
            name: "oracle".to_string(),
            region: "ap".to_string(),
            core_url: "http://145.79.134.188:4600".to_string(),
            core_ws_url: "ws://145.79.134.188:4600/api/cluster/node-ws".to_string(),
            mesh_ip: "10.200.0.3".to_string(),
            public_ip: "130.210.62.154".to_string(),
            wg_private_key: "priv3==".to_string(),
            wg_pubkey: "pub3==".to_string(),
            wg_listen_port: 51820,
            node_token: "tok3".to_string(),
            peers: vec![PeerConfig {
                node_id: 1,
                name: "core".to_string(),
                mesh_ip: "10.200.0.1".to_string(),
                public_ip: "127.0.0.1".to_string(),
                wg_pubkey: "pub1==".to_string(),
                wg_endpoint: "127.0.0.1:51820".to_string(),
                status: "online".to_string(),
            }],
            workloads: vec![],
            updated_at: 100,
        };
        let worker_conf = render_wg_conf(&worker_manifest);
        assert!(worker_conf.contains("Endpoint = 145.79.134.188:51820"));
    }
}
