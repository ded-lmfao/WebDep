/**
 * WebDep live demo: multi-node cluster data (one control VPS + one worker VPS).
 * Loaded before demo_mock.js; demo_mock.js reads window.__WEBDEP_DEMO_CLUSTER__ to answer the
 * /api/cluster/* routes, place workloads on nodes and feed each node card its own live chart.
 */
(() => {
  if (window.__WEBDEP_DEMO_CLUSTER__) return;

  const GB = 1073741824;

  const MODULE_CATALOG = [
    ['unrar', 'UnRAR Extractor', 'archive', true, 'Required on every VPS to extract .rar transfer and backup archives.', ['Archives', 'App Shift', 'Backups']],
    ['git', 'Git CLI', 'system', true, 'Required to clone, fetch, and deploy GitHub repositories on the VPS.', ['Git Deployments']],
    ['node', 'Node.js & npm', 'language', true, 'JavaScript & TypeScript runtime plus npm for web apps, APIs, and bots.', ['Node.js', 'Next.js', 'React/Vite', 'NestJS']],
    ['python', 'Python 3 + pip & venv', 'language', true, 'Python 3 runtime, pip package manager, and venv support.', ['Python', 'FastAPI', 'Django', 'Flask']],
    ['java', 'Java JDK / JRE', 'language', false, 'OpenJDK runtime for Java backends, Lavalink, and JVM services.', ['Java', 'Spring Boot', 'Lavalink']],
    ['go', 'Go Toolchain', 'language', false, 'Go compiler and module toolchain.', ['Go', 'Gin', 'Fiber']],
    ['rust', 'Rust (rustc & cargo)', 'language', false, 'Rust compiler and Cargo package manager.', ['Rust', 'Axum', 'Actix']],
    ['php', 'PHP & Composer', 'language', false, 'PHP CLI/FPM runtime for PHP sites and Laravel apps.', ['PHP', 'Laravel']],
    ['ruby', 'Ruby Runtime', 'language', false, 'Ruby interpreter and Bundler tooling.', ['Ruby', 'Rails']],
    ['dotnet', '.NET SDK / Runtime', 'language', false, '.NET CLI for C#, F#, and ASP.NET Core apps.', ['C#', '.NET', 'ASP.NET']],
  ].map(([id, name, category, required, description, languages]) => ({ id, name, category, required, description, languages }));

  const MODULE_VERSIONS = {
    unrar: 'UNRAR 7.00 freeware',
    git: 'git version 2.43.0',
    node: 'v22.23.4',
    python: 'Python 3.12.3',
    java: 'openjdk 21.0.5',
    go: 'go1.23.4',
    rust: 'rustc 1.99.1',
    php: 'PHP 8.3.6',
    ruby: 'ruby 3.2.3',
    dotnet: '8.0.404',
  };

  const nodes = [
    {
      id: 1,
      name: 'vps-mumbai-01',
      displayName: 'Mumbai Control',
      region: 'mumbai-IND',
      meshIp: '10.200.0.1',
      publicIp: '203.0.113.10',
      status: 'online',
      isLocal: true,
      nodeVersion: '3.0.4',
      capabilities: { cpuCores: 4, os: 'linux', arch: 'x86_64', hasWireguard: true, hasCaddy: true, hasPostgres: true, packageManager: 'apt-get' },
      metrics: { cpuPct: 14.2, memUsed: 3.1 * GB, memTotal: 8 * GB, diskUsed: 41 * GB, diskTotal: 160 * GB, uptimeS: 9 * 86400 + 4 * 3600 },
      installed: new Set(['unrar', 'git', 'node', 'python', 'java', 'rust']),
      net: { scale: 1, phase: 0, rx: 7.2 * GB, tx: 4.9 * GB, history: [] },
    },
    {
      id: 2,
      name: 'vps-frankfurt-02',
      displayName: 'Frankfurt Worker',
      region: 'eu-central',
      meshIp: '10.200.0.2',
      publicIp: '198.51.100.24',
      status: 'online',
      isLocal: false,
      nodeVersion: '3.0.4',
      capabilities: { cpuCores: 2, os: 'linux', arch: 'aarch64', hasWireguard: true, hasCaddy: true, hasPostgres: true, packageManager: 'apt-get' },
      metrics: { cpuPct: 8.6, memUsed: 1.4 * GB, memTotal: 4 * GB, diskUsed: 18 * GB, diskTotal: 80 * GB, uptimeS: 3 * 86400 + 11 * 3600 },
      installed: new Set(['unrar', 'git', 'node', 'python', 'go']),
      net: { scale: 0.55, phase: 1.7, rx: 3.4 * GB, tx: 1.9 * GB, history: [] },
    },
  ];

  // Which node each demo workload runs on (project id / database id -> node id).
  const placement = { projects: { 1: 1, 2: 2, 3: 1, 4: 2, 5: 1 }, databases: { 1: 2, 2: 1 } };

  function sampleNet(n) {
    const st = n.net;
    st.phase += 0.45;
    const live = n.status !== 'offline';
    const inbound = live ? Math.round(Math.max(4000, (52000 + Math.sin(st.phase) * 24000 + Math.random() * 16000) * st.scale)) : 0;
    const outbound = live ? Math.round(Math.max(7000, (120000 + Math.cos(st.phase * 0.8) * 52000 + Math.random() * 30000) * st.scale)) : 0;
    st.rx += inbound;
    st.tx += outbound;
    st.history.push({ ts: Date.now(), inboundBps: inbound, outboundBps: outbound });
    if (st.history.length > 75) st.history.shift();
    n.metrics.cpuPct = Math.max(1, Math.min(92, n.metrics.cpuPct + (Math.random() - 0.5) * 3));
    return {
      inboundBps: inbound,
      outboundBps: outbound,
      interfaceCount: n.isLocal ? 2 : 1,
      totalRxBytes: Math.round(st.rx),
      totalTxBytes: Math.round(st.tx),
      history: [...st.history],
    };
  }

  // Seed 48 seconds of history so charts are drawn immediately.
  for (const n of nodes) {
    for (let i = 48; i >= 0; i--) {
      sampleNet(n);
      n.net.history[n.net.history.length - 1].ts = Date.now() - i * 1000;
    }
  }

  function modulesView(n) {
    return {
      nodeId: n.id,
      name: n.name,
      displayName: n.displayName,
      isLocal: n.isLocal,
      status: n.status,
      os: n.capabilities.os,
      arch: n.capabilities.arch,
      packageManager: n.capabilities.packageManager,
      modules: MODULE_CATALOG.map((m) => {
        const on = n.installed.has(m.id);
        return { id: m.id, installed: on, version: on ? MODULE_VERSIONS[m.id] : '', binaryPath: on ? `/usr/bin/${m.id}` : '', details: '' };
      }),
    };
  }

  window.__WEBDEP_DEMO_CLUSTER__ = {
    nodes,
    placement,
    catalog: MODULE_CATALOG,
    versions: MODULE_VERSIONS,
    modulesView,
    sampleAll() {
      const out = {};
      for (const n of nodes) out[n.id] = sampleNet(n);
      return out;
    },
  };
})();
