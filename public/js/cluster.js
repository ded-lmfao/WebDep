/**
 * WebDep Cluster Mesh & Web-Node Control Plane UI Subsystem
 * Manages cluster node inventory, live heartbeats, single-use enrollment tokens,
 * drain/remove operations, and the global cluster node filter.
 */

import {
  state,
  setHtmlIfChanged,
  matchesNodeFilter,
  getNodeDisplayName,
  escapeHtml,
  formatRelativeTime,
  openModal,
  closeModal,
  showConfirmDialog,
} from './state.js';
import { api, toast } from './api.js';
import { renderResourceBreakdown } from './overview.js';
import { renderServicesList } from './services.js';
import { renderDatabasesList } from './databases.js';

function formatBytesHuman(bytes) {
  const b = Number(bytes || 0);
  if (!Number.isFinite(b) || b <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let idx = 0;
  let val = b;
  while (val >= 1024 && idx < units.length - 1) {
    val /= 1024;
    idx += 1;
  }
  return `${val >= 10 || idx === 0 ? val.toFixed(0) : val.toFixed(1)} ${units[idx]}`;
}

function formatUptimeShort(secs) {
  const s = Number(secs || 0);
  if (s <= 0) return '--';
  const days = Math.floor(s / 86400);
  const hours = Math.floor((s % 86400) / 3600);
  const mins = Math.floor((s % 3600) / 60);
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${mins}m`;
  return `${mins}m`;
}

export { matchesNodeFilter, getNodeDisplayName };

export function syncGlobalNodeFilterOptions() {
  const select = document.getElementById('global-node-filter');
  if (!select) return;
  const nodes = Array.isArray(state.nodes) ? state.nodes : [];
  const current = String(state.selectedNodeFilter || 'all');

  // The filter only makes sense once a second node has joined.
  select.hidden = nodes.length <= 1;

  const options = ['<option value="all">All nodes</option>'];
  for (const n of nodes) {
    const status = String(n.status || '').toLowerCase();
    const label = `${escapeHtml(n.name)}${status && status !== 'online' ? ` · ${escapeHtml(status)}` : ''}`;
    options.push(`<option value="${n.id}" title="${escapeHtml(n.meshIp || '')}">${label}</option>`);
  }
  const html = options.join('');
  if (select._lastHtml !== html) {
    select._lastHtml = html;
    select.innerHTML = html;
  }
  if (current !== 'all' && !nodes.some((n) => String(n.id) === current)) {
    state.selectedNodeFilter = 'all';
    select.value = 'all';
  } else {
    select.value = current;
  }
}

export async function loadNodes(force = false) {
  try {
    const list = await api('/api/cluster/nodes', force ? { noCache: true } : {});
    state.nodes = Array.isArray(list) ? list : [];
    const badge = document.getElementById('badge-nodes-count');
    if (badge) badge.textContent = String(state.nodes.length);
    syncGlobalNodeFilterOptions();
  } catch (err) {
    console.warn('Failed to load cluster nodes:', err);
  }
}

export async function loadNodesView(force = false) {
  await loadNodes(force);
  let tokens = [];
  if (state.user?.isMainAdmin) {
    try {
      const res = await api('/api/cluster/enroll-tokens', { noCache: true });
      if (Array.isArray(res)) tokens = res;
    } catch (_) {}
  }
  renderNodesView(tokens);
}

export function renderNodesView(enrollTokens = null) {
  const container = document.getElementById('nodes-cards-container');
  if (!container) return;

  const nodes = Array.isArray(state.nodes) ? state.nodes : [];
  const isMainAdmin = Boolean(state.user?.isMainAdmin);

  const onlineCount = nodes.filter((n) => n.status === 'online').length;
  const drainingCount = nodes.filter((n) => n.status === 'draining').length;
  const totalWorkloads = nodes.reduce(
    (sum, n) => sum + Number(n.projectCount || 0) + Number(n.databaseCount || 0),
    0,
  );
  const totalRam = nodes.reduce((sum, n) => sum + Number(n.metrics?.memTotal || 0), 0);

  const statTotal = document.getElementById('nodes-stat-total');
  const statOnline = document.getElementById('nodes-stat-online');
  const statWorkloads = document.getElementById('nodes-stat-workloads');
  const statRam = document.getElementById('nodes-stat-ram');
  if (statTotal) statTotal.textContent = String(nodes.length);
  if (statOnline) statOnline.textContent = drainingCount > 0 ? `${onlineCount} (${drainingCount} draining)` : String(onlineCount);
  if (statWorkloads) statWorkloads.textContent = String(totalWorkloads);
  if (statRam) statRam.textContent = totalRam > 0 ? formatBytesHuman(totalRam) : '--';

  const addBtn = document.getElementById('add-node-header-btn');
  if (addBtn) addBtn.style.display = isMainAdmin ? '' : 'none';
  const genTokenBtn = document.getElementById('nodes-generate-token-btn');
  if (genTokenBtn) genTokenBtn.style.display = isMainAdmin ? '' : 'none';
  const fleetBtn = document.getElementById('fleet-update-header-btn');
  if (fleetBtn) fleetBtn.style.display = isMainAdmin ? '' : 'none';

  const barTone = (pct) => (pct >= 85 ? 'rose' : pct >= 65 ? 'amber' : 'green');

  if (!nodes.length) {
    setHtmlIfChanged(
      container,
      `
        <div style="grid-column: 1 / -1; text-align: center; padding: 48px; background: var(--bg-card); border-radius: var(--radius-lg); border: 1px dashed var(--border-medium);">
          <p style="font-size: 16px; font-weight: 600; margin-bottom: 8px;">No Web-Nodes enrolled yet</p>
          <p style="color: var(--text-muted); font-size: 13px; margin-bottom: 20px;">
            Start WebDep on a worker VPS with a single-use enrollment token to join the private WireGuard mesh.
          </p>
          ${isMainAdmin ? '<button class="btn btn-primary" onclick="openAddNodeModal()">+ Add Worker Node</button>' : ''}
        </div>
      `,
    );
  } else {
    const cardsHtml = nodes.map((n) => {
      const m = n.metrics || {};
      const caps = n.capabilities || {};
      const isOnline = n.status === 'online';
      const isDraining = n.status === 'draining';
      const statusDot = isOnline ? 'online' : isDraining ? 'maintenance' : 'offline';
      const statusBadge = isOnline
        ? '<span class="badge badge-success">Online</span>'
        : isDraining
          ? '<span class="badge badge-warning">Draining</span>'
          : '<span class="badge badge-danger">Offline</span>';

      const cpuPct = Math.max(0, Math.min(100, Number(m.cpuPct || 0)));
      const memUsed = Number(m.memUsed || 0);
      const memTotal = Number(m.memTotal || 0);
      const memPct = memTotal > 0 ? Math.min(100, Math.round((memUsed / memTotal) * 100)) : 0;
      const diskUsed = Number(m.diskUsed || 0);
      const diskTotal = Number(m.diskTotal || 0);
      const diskPct = diskTotal > 0 ? Math.min(100, Math.round((diskUsed / diskTotal) * 100)) : 0;

      const capBadges = [
        caps.cpuCores ? `${caps.cpuCores} vCPU` : '',
        caps.os ? `${caps.os}/${caps.arch || ''}` : '',
        caps.hasWireguard ? 'WireGuard' : '',
        caps.hasCaddy ? 'Caddy' : '',
        caps.hasPostgres ? 'PostgreSQL' : '',
      ]
        .filter(Boolean)
        .map((c) => `<span class="badge badge-neutral" style="font-size: 10.5px;">${escapeHtml(c)}</span>`)
        .join(' ');

      const isSelectedFilter = String(state.selectedNodeFilter || 'all') === String(n.id);

      return `
        <div class="node-card">
          <div class="node-card-head">
            <div style="min-width: 0;">
              <div style="display: flex; align-items: center; gap: 8px; flex-wrap: wrap;">
                <span class="status-dot ${statusDot}"></span>
                <strong style="font-size: 15px; color: var(--text-main);">${escapeHtml(n.name)}</strong>
                ${n.isLocal ? '<span class="badge badge-info" style="font-size: 10.5px;">Control Node</span>' : '<span class="badge badge-neutral" style="font-size: 10.5px;">Worker</span>'}
                ${statusBadge}
              </div>
              <div class="node-card-meta">
                <span class="node-meta-item">Region <strong style="color: var(--text-main);">${escapeHtml(n.region || 'default')}</strong></span>
                <span class="node-meta-item">Mesh <code>${escapeHtml(n.meshIp || '--')}</code></span>
                ${n.publicIp ? `<span class="node-meta-item">IP <code>${escapeHtml(n.publicIp)}</code></span>` : ''}
              </div>
            </div>
            <div style="text-align: right; font-size: 11.5px; color: var(--text-dim); white-space: nowrap;">
              <div style="color: var(--text-muted); font-family: var(--font-mono);">v${escapeHtml(n.nodeVersion || '--')}</div>
              <div>${n.lastSeenAt ? `Seen ${formatRelativeTime(n.lastSeenAt)}` : 'Never seen'}</div>
            </div>
          </div>

          <div class="node-metrics-grid">
            <div class="node-metric-box">
              <div class="node-metric-label"><span>CPU</span><span>${cpuPct.toFixed(1)}%</span></div>
              <div class="node-metric-val">${cpuPct.toFixed(1)}%</div>
              <div class="progress-bar"><div class="progress-fill ${barTone(cpuPct)}" style="width: ${cpuPct.toFixed(1)}%;"></div></div>
            </div>
            <div class="node-metric-box">
              <div class="node-metric-label"><span>Memory</span><span>${memTotal > 0 ? `${memPct}%` : '--'}</span></div>
              <div class="node-metric-val">${memTotal > 0 ? `${formatBytesHuman(memUsed)} / ${formatBytesHuman(memTotal)}` : '--'}</div>
              <div class="progress-bar"><div class="progress-fill ${barTone(memPct)}" style="width: ${memPct}%;"></div></div>
            </div>
            <div class="node-metric-box">
              <div class="node-metric-label"><span>Disk</span><span>Uptime ${formatUptimeShort(m.uptimeS)}</span></div>
              <div class="node-metric-val">${diskTotal > 0 ? `${diskPct}% (${formatBytesHuman(diskUsed)})` : '--'}</div>
              <div class="progress-bar"><div class="progress-fill ${barTone(diskPct)}" style="width: ${diskPct}%;"></div></div>
            </div>
          </div>

          <div style="display: flex; flex-wrap: wrap; gap: 6px; align-items: center;">
            ${capBadges}
          </div>

          <div class="node-card-footer">
            <div style="font-size: 12px; color: var(--text-muted);">
              <strong style="color: var(--text-main);">${Number(n.projectCount || 0)}</strong> apps &bull;
              <strong style="color: var(--text-main);">${Number(n.databaseCount || 0)}</strong> databases
            </div>
            <div style="display: flex; gap: 6px; flex-wrap: wrap;">
              <button class="btn btn-secondary btn-sm" onclick="setGlobalNodeFilter('${isSelectedFilter ? 'all' : n.id}')">
                ${isSelectedFilter ? 'Clear Filter' : 'Filter View'}
              </button>
              ${isMainAdmin ? `
                <button class="btn btn-secondary btn-sm" onclick="toggleNodeDrain(${n.id}, ${isDraining ? 'false' : 'true'})">
                  ${isDraining ? 'Resume' : 'Drain'}
                </button>
                ${Number(n.projectCount || 0) > 0 ? `
                  <button class="btn btn-secondary btn-sm" onclick="evacuateClusterNode(${n.id}, '${escapeHtml(n.name)}')">Evacuate</button>
                ` : ''}
              ` : ''}
              ${isMainAdmin && !n.isLocal ? `
                <button class="btn btn-danger btn-sm" onclick="removeClusterNode(${n.id}, '${escapeHtml(n.name)}')">Remove</button>
              ` : ''}
            </div>
          </div>
        </div>
      `;
    }).join('');
    setHtmlIfChanged(container, cardsHtml);
  }

  const tokensEl = document.getElementById('nodes-tokens-panel');
  if (tokensEl && Array.isArray(enrollTokens)) {
    const nowS = Math.floor(Date.now() / 1000);
    if (!enrollTokens.length) {
      setHtmlIfChanged(
        tokensEl,
        '<div style="padding: 20px; color: var(--text-dim); font-size: 13px;">No enrollment tokens generated yet. Click <strong>+ Add Node</strong> to create a single-use join command.</div>',
      );
    } else {
      const rows = enrollTokens.slice(0, 8).map((t) => {
        let status = '<span class="badge badge-success">Active</span>';
        if (t.usedAt) status = '<span class="badge badge-neutral">Used</span>';
        else if (t.expiresAt <= nowS) status = '<span class="badge badge-danger">Expired</span>';

        const resultCell = t.usedNodeId
          ? `<span class="badge badge-info">Enrolled &bull; Node #${t.usedNodeId}</span>`
          : (t.expiresAt > nowS ? `Expires ${formatRelativeTime(t.expiresAt)}` : 'Expired');

        return `
          <tr>
            <td><strong>${escapeHtml(t.label || '(unlabeled)')}</strong></td>
            <td><span class="badge badge-neutral">${escapeHtml(t.region || 'default')}</span></td>
            <td>${status}</td>
            <td style="color: var(--text-dim);">${formatRelativeTime(t.createdAt)}</td>
            <td style="text-align: right;">${resultCell}</td>
          </tr>
        `;
      }).join('');
      setHtmlIfChanged(
        tokensEl,
        `
          <div class="table-responsive" style="padding-bottom: 0;">
            <table class="table">
              <thead>
                <tr>
                  <th>Label</th>
                  <th>Region</th>
                  <th>Status</th>
                  <th>Created</th>
                  <th style="text-align: right;">Result / Expiry</th>
                </tr>
              </thead>
              <tbody>${rows}</tbody>
            </table>
          </div>
        `,
      );
    }
  }
}

export function setGlobalNodeFilter(nodeId) {
  state.selectedNodeFilter = String(nodeId || 'all');
  const select = document.getElementById('global-node-filter');
  if (select) select.value = state.selectedNodeFilter;

  if (state.currentView === 'projects' && typeof window.renderProjectsList === 'function') {
    window.renderProjectsList();
  } else if (state.currentView === 'services') {
    renderServicesList();
  } else if (state.currentView === 'databases') {
    renderDatabasesList();
  } else if (state.currentView === 'overview') {
    renderResourceBreakdown();
  } else if (state.currentView === 'nodes') {
    renderNodesView();
  }
}

export function setAddNodeTargetIp(ip) {
  const el = document.getElementById('add-node-target-ip');
  if (el) el.value = ip;
  if (ip === '127.0.0.1' || ip === '::1') {
    const coreEl = document.getElementById('add-node-core-url');
    if (coreEl) coreEl.value = window.location.origin;
  }
}

export async function openAddNodeModal() {
  const labelEl = document.getElementById('add-node-label');
  const regionEl = document.getElementById('add-node-region');
  const targetIpEl = document.getElementById('add-node-target-ip');
  const coreUrlEl = document.getElementById('add-node-core-url');
  const sugEl = document.getElementById('add-node-ip-suggestions');
  const outEl = document.getElementById('add-node-result-box');

  const nextNum = Math.max(2, (state.nodes || []).length + 1);
  if (labelEl) labelEl.value = `VPS-${nextNum}`;
  if (regionEl && !regionEl.value.trim()) regionEl.value = 'mumbai-IND';
  if (targetIpEl) targetIpEl.value = '';
  if (coreUrlEl) coreUrlEl.value = window.location.origin;
  if (sugEl) sugEl.innerHTML = '<span>Detecting panel IP...</span>';
  if (outEl) outEl.style.display = 'none';
  openModal('modal-add-node');

  try {
    const info = await api('/api/cluster/panel-info', { noCache: true });
    // When the panel is opened through a domain (typically behind a reverse proxy with TLS),
    // that domain is what other servers can reach; the raw IP:port may not be exposed at all.
    const host = window.location.hostname;
    const openedViaDomain = host !== 'localhost' && !/^[\d.]+$/.test(host) && !host.includes(':');
    if (coreUrlEl) {
      coreUrlEl.value = openedViaDomain ? window.location.origin : (info.suggestedPanelUrl || window.location.origin);
    }
    if (sugEl) {
      sugEl.innerHTML = '';
      const addChip = (labelText, url) => {
        if (!url) return;
        const btn = document.createElement('button');
        btn.type = 'button';
        btn.className = 'btn btn-secondary btn-sm';
        btn.style.cssText = 'padding: 2px 8px; font-size: 10.5px;';
        btn.textContent = labelText;
        btn.addEventListener('click', () => {
          if (coreUrlEl) coreUrlEl.value = url;
        });
        sugEl.appendChild(btn);
      };

      const portSuffix = info.port ? `:${info.port}` : '';
      const proto = info.requestUrl && info.requestUrl.startsWith('https://') ? 'https' : 'http';
      if (info.publicIp) addChip(`Public IP (${info.publicIp})`, `${proto}://${info.publicIp}${portSuffix}`);
      if (info.lanIp && info.lanIp !== info.publicIp) addChip(`LAN IP (${info.lanIp})`, `${proto}://${info.lanIp}${portSuffix}`);
      if (info.requestUrl) addChip(`Current Browser (${info.requestUrl})`, info.requestUrl);
    }
  } catch (_) {
    if (sugEl) sugEl.innerHTML = '';
  }
}

export async function generateNodeEnrollToken() {
  const label = (document.getElementById('add-node-label')?.value || '').trim();
  const region = (document.getElementById('add-node-region')?.value || 'default').trim();
  const targetPublicIp = (document.getElementById('add-node-target-ip')?.value || '').trim();
  const coreUrl = (document.getElementById('add-node-core-url')?.value || '').trim();
  const ttlSeconds = Number(document.getElementById('add-node-ttl')?.value || 900);
  const btn = document.getElementById('generate-node-token-btn');

  if (!label) {
    toast('Please enter a VPS / Node Name (e.g. VPS-2)', 'warning');
    document.getElementById('add-node-label')?.focus();
    return;
  }

  if (btn) btn.disabled = true;
  try {
    const res = await api('/api/cluster/enroll-tokens', {
      method: 'POST',
      body: {
        label,
        region,
        ttlSeconds,
        coreUrl: coreUrl || undefined,
        targetPublicIp: targetPublicIp || undefined,
      },
    });
    const outEl = document.getElementById('add-node-result-box');
    const freshEl = document.getElementById('add-node-fresh-cmd');
    const svcEl = document.getElementById('add-node-service-cmd');
    const cmdEl = document.getElementById('add-node-join-cmd');
    const psEl = document.getElementById('add-node-ps-cmd');
    const tokEl = document.getElementById('add-node-raw-token');
    if (freshEl) freshEl.value = res.freshVpsCommand || '';
    if (svcEl) svcEl.value = res.serviceCommand || '';
    if (cmdEl) cmdEl.value = res.joinCommand || '';
    if (psEl) {
      const idx = Math.max(2, (state.nodes || []).length + 1);
      const safeDir = `C:\\tmp\\webdep-node${idx}`;
      const port = 4690 + idx;
      const flags = (res.joinCommand || '').replace(/^webdep\s+/, '');
      psEl.value = `mkdir ${safeDir} -Force | Out-Null; cd ${safeDir}; Set-Content .env @("DATA_DIR=${safeDir}\\data", "PORT=${port}"); $env:WEBDEP_NODE_P2P_BIND="127.0.0.1"; $env:WEBDEP_NODE_P2P_PORT="0"; webdep.exe ${flags}`;
    }
    if (tokEl) tokEl.textContent = res.token || '';
    if (outEl) outEl.style.display = 'block';
    toast('Complete VPS join command generated', 'success');
    await loadNodesView(true);
  } catch (err) {
    toast(`Failed generating join token: ${err.message}`, 'error');
  } finally {
    if (btn) btn.disabled = false;
  }
}

export async function copyNodeJoinCommand(targetInputId = 'add-node-fresh-cmd') {
  const cmdEl = document.getElementById(targetInputId);
  if (!cmdEl || !cmdEl.value) return;
  try {
    await navigator.clipboard.writeText(cmdEl.value);
    toast('Command copied to clipboard', 'success');
  } catch {
    cmdEl.select();
    document.execCommand('copy');
    toast('Command copied', 'success');
  }
}

export async function toggleNodeDrain(nodeId, drain) {
  try {
    await api(`/api/cluster/nodes/${nodeId}/drain`, {
      method: 'POST',
      body: { draining: Boolean(drain) },
    });
    toast(drain ? `Node #${nodeId} set to draining` : `Node #${nodeId} resumed`, 'success');
    await loadNodesView(true);
  } catch (err) {
    toast(`Failed updating node state: ${err.message}`, 'error');
  }
}

export async function removeClusterNode(nodeId, nodeName) {
  const ok = await showConfirmDialog({
    title: 'Remove Worker Node',
    message: `Remove "${nodeName}" from the cluster mesh? Workloads assigned to this node must be moved or deleted first.`,
    danger: true,
    confirmText: 'Remove Node',
  });
  if (!ok) return;

  try {
    await api(`/api/cluster/nodes/${nodeId}`, { method: 'DELETE' });
    if (String(state.selectedNodeFilter) === String(nodeId)) {
      state.selectedNodeFilter = 'all';
    }
    toast(`Removed node "${nodeName}"`, 'success');
    await loadNodesView(true);
  } catch (err) {
    toast(`Failed removing node: ${err.message}`, 'error');
  }
}

document.getElementById('global-node-filter')?.addEventListener('change', (e) => {
  setGlobalNodeFilter(e.target.value);
});

let activeShiftMigration = null;

function renderShiftModalState(mig, historyList = []) {
  const setupEl = document.getElementById('shift-setup-panel');
  const activeEl = document.getElementById('shift-active-panel');
  const bannerEl = document.getElementById('shift-stage-banner');
  const dnsCard = document.getElementById('shift-dns-card');
  const dnsRecEl = document.getElementById('shift-dns-records');
  const startBtn = document.getElementById('shift-start-btn');
  const histEl = document.getElementById('shift-history-box');

  if (mig && ['running', 'pending', 'awaiting_dns'].includes(mig.status)) {
    activeShiftMigration = mig;
    if (setupEl) setupEl.style.display = 'none';
    if (activeEl) activeEl.style.display = 'flex';
    if (startBtn) startBtn.style.display = 'none';
    if (bannerEl) {
      bannerEl.innerHTML = `
        <div style="display:flex; justify-content:space-between; align-items:center; gap:8px;">
          <strong>Shift #${mig.id}: ${escapeHtml(mig.fromNodeName || 'Source')} &rarr; ${escapeHtml(mig.toNodeName || 'Target')}</strong>
          <span class="badge ${mig.status === 'awaiting_dns' ? 'badge-warning' : 'badge-info'}">${escapeHtml(mig.stage)}</span>
        </div>
        <div style="font-size:12px; color:var(--text-muted); margin-top:4px;">
          Target port: <code>${mig.targetPort || '--'}</code> &bull; Transferred: <strong>${formatBytesHuman(mig.transferBytes || 0)}</strong>
        </div>
      `;
    }
    if (dnsCard) {
      dnsCard.style.display = mig.status === 'awaiting_dns' ? 'block' : 'none';
    }
    if (dnsRecEl && mig.status === 'awaiting_dns') {
      const domRows = (mig.domains || []).map((d) =>
        `<div><code>A</code> &bull; <strong>${escapeHtml(d)}</strong> &bull; <code>${escapeHtml(mig.fromIp || '--')}</code> &rarr; <code>${escapeHtml(mig.toIp || '--')}</code></div>`
      ).join('');
      dnsRecEl.innerHTML = `
        ${domRows || '<div>Update DNS A record to point to target IP.</div>'}
        <div style="margin-top:6px; font-size:11.5px; color:var(--text-dim);">
          ${mig.dnsVerified ? '<span class="badge badge-success">DNS Verified</span>' : 'Update your DNS A record(s), click <strong>Check DNS</strong>, or use <strong>Complete Cutover Now</strong>.'}
        </div>
      `;
    }
  } else {
    activeShiftMigration = null;
    if (setupEl) setupEl.style.display = 'block';
    if (activeEl) activeEl.style.display = 'none';
    if (startBtn) startBtn.style.display = '';
    if (dnsCard) dnsCard.style.display = 'none';
  }

  if (histEl) {
    if (!historyList || !historyList.length) {
      histEl.textContent = 'No prior migrations recorded for this website.';
    } else {
      histEl.innerHTML = `<div style="margin-bottom:4px; font-weight:600; color:var(--text-muted);">Recent Shifts</div>` +
        historyList.slice(0, 4).map((h) =>
          `<div>#${h.id}: ${escapeHtml(h.fromNodeName || '?')} &rarr; ${escapeHtml(h.toNodeName || '?')} &bull; <span class="badge badge-neutral">${escapeHtml(h.status)}</span> (${formatBytesHuman(h.transferBytes || 0)})</div>`
        ).join('');
    }
  }
}

export async function openMoveToNodeModal(projectId) {
  const pid = Number(projectId || state.selectedProjectId);
  const proj = (state.projects || []).find((x) => x.id === pid);
  if (!proj) {
    toast('Select a website first.', 'warning');
    return;
  }
  state.selectedProjectId = pid;
  await loadNodes(true);

  const titleEl = document.getElementById('shift-modal-title');
  if (titleEl) titleEl.textContent = `Move "${proj.name}" to Node`;

  const sel = document.getElementById('shift-target-node-select');
  const candidates = (state.nodes || []).filter((n) => n.status === 'online' && Number(n.id) !== Number(proj.node_id));
  if (sel) {
    sel.innerHTML = candidates.length
      ? candidates.map((n) => `<option value="${n.id}">${escapeHtml(n.name)} (${escapeHtml(n.region || 'default')} &bull; ${escapeHtml(n.meshIp || n.publicIp || '')})</option>`).join('')
      : '<option value="">No other healthy online nodes available</option>';
  }

  let history = [];
  try {
    history = await api(`/api/projects/${pid}/migrations`, { noCache: true });
  } catch (_) {}
  const active = Array.isArray(history)
    ? history.find((m) => ['running', 'pending', 'awaiting_dns'].includes(m.status))
    : null;
  renderShiftModalState(active, Array.isArray(history) ? history : []);
  openModal('modal-move-project-node');
}

export async function confirmStartProjectShift() {
  const pid = Number(state.selectedProjectId);
  const targetNodeId = Number(document.getElementById('shift-target-node-select')?.value || 0);
  const skipDns = Boolean(document.getElementById('shift-skip-dns-checkbox')?.checked);
  if (!pid || !targetNodeId) {
    toast('Select an online destination Web-Node first.', 'warning');
    return;
  }
  const btn = document.getElementById('shift-start-btn');
  if (btn) {
    btn.disabled = true;
    btn.textContent = 'Shifting...';
  }
  try {
    const mig = await api(`/api/projects/${pid}/move`, {
      method: 'POST',
      body: { targetNodeId, autoCutoverWithoutDns: skipDns },
    });
    const history = await api(`/api/projects/${pid}/migrations`, { noCache: true }).catch(() => [mig]);
    renderShiftModalState(mig, Array.isArray(history) ? history : [mig]);
    if (mig.status === 'completed') {
      toast(`Live shift #${mig.id} completed cleanly!`, 'success');
      if (typeof window.refreshProjects === 'function') await window.refreshProjects();
      closeModal('modal-move-project-node');
    } else if (mig.status === 'awaiting_dns') {
      toast('Target warmed & staged on HTTP! Complete DNS update to finish HTTPS cutover.', 'info');
    }
  } catch (err) {
    toast(`Shift failed: ${err.message}`, 'error');
  } finally {
    if (btn) {
      btn.disabled = false;
      btn.textContent = 'Start Live P2P Shift';
    }
  }
}

export async function checkShiftDns() {
  if (!activeShiftMigration) return;
  try {
    const res = await api(`/api/migrations/${activeShiftMigration.id}/check-dns`, { method: 'POST' });
    const history = await api(`/api/projects/${res.migration.projectId}/migrations`, { noCache: true }).catch(() => []);
    renderShiftModalState(res.migration, Array.isArray(history) ? history : []);
    toast(
      res.dnsVerified
        ? `DNS verified pointing to ${res.expectedIp}! Ready to complete cutover.`
        : `DNS resolved [${(res.resolvedIps || []).join(', ') || 'none'}] (expected ${res.expectedIp})`,
      res.dnsVerified ? 'success' : 'warning',
    );
  } catch (err) {
    toast(`DNS check failed: ${err.message}`, 'error');
  }
}

export async function completeShiftCutover() {
  if (!activeShiftMigration) return;
  try {
    const done = await api(`/api/migrations/${activeShiftMigration.id}/complete`, { method: 'POST' });
    toast(`Shift #${done.id} cutover completed!`, 'success');
    activeShiftMigration = null;
    if (typeof window.refreshProjects === 'function') await window.refreshProjects();
    closeModal('modal-move-project-node');
  } catch (err) {
    toast(`Cutover failed: ${err.message}`, 'error');
  }
}

export async function abortShiftMigration() {
  if (!activeShiftMigration) return;
  try {
    await api(`/api/migrations/${activeShiftMigration.id}/abort`, { method: 'POST' });
    toast('Shift aborted and target cleaned up; source untouched.', 'info');
    const pid = activeShiftMigration.projectId;
    activeShiftMigration = null;
    const history = await api(`/api/projects/${pid}/migrations`, { noCache: true }).catch(() => []);
    renderShiftModalState(null, Array.isArray(history) ? history : []);
  } catch (err) {
    toast(`Abort failed: ${err.message}`, 'error');
  }
}

export async function evacuateClusterNode(nodeId, nodeName) {
  const ok = await showConfirmDialog({
    title: `Evacuate "${nodeName}"`,
    message: `Drain "${nodeName}" and automatically shift all hosted applications to healthy online nodes?`,
    confirmText: 'Drain & Evacuate',
  });
  if (!ok) return;
  try {
    const res = await api(`/api/cluster/nodes/${nodeId}/evacuate`, { method: 'POST', body: {} });
    toast(
      `Evacuated ${nodeName}: ${res.shifted.length} shifted, ${res.awaitingDns.length} awaiting DNS, ${res.failed.length} failed.`,
      res.failed.length ? 'warning' : 'success',
    );
    await loadNodesView(true);
    if (typeof window.refreshProjects === 'function') await window.refreshProjects();
  } catch (err) {
    toast(`Evacuation failed: ${err.message}`, 'error');
  }
}

export async function triggerFleetUpdate() {
  if (typeof window.startPlatformUpdate === 'function') {
    return window.startPlatformUpdate();
  }
  const ok = await showConfirmDialog({
    title: 'Coordinated Fleet Update',
    message: 'Upgrade all connected remote Web-Nodes first, then upgrade and restart Web-Core?',
    confirmText: 'Update Fleet',
  });
  if (!ok) return;
  try {
    const res = await api('/api/cluster/fleet-update', { method: 'POST', body: {} });
    toast(res.message || 'Coordinated fleet update started', 'success');
  } catch (err) {
    toast(`Fleet update failed: ${err.message}`, 'error');
  }
}

window.loadNodes = loadNodes;
window.loadNodesView = loadNodesView;
window.renderNodesView = renderNodesView;
window.setGlobalNodeFilter = setGlobalNodeFilter;
window.openAddNodeModal = openAddNodeModal;
window.setAddNodeTargetIp = setAddNodeTargetIp;
window.generateNodeEnrollToken = generateNodeEnrollToken;
window.copyNodeJoinCommand = copyNodeJoinCommand;
window.toggleNodeDrain = toggleNodeDrain;
window.evacuateClusterNode = evacuateClusterNode;
window.removeClusterNode = removeClusterNode;
window.triggerFleetUpdate = triggerFleetUpdate;
window.openMoveToNodeModal = openMoveToNodeModal;
window.confirmStartProjectShift = confirmStartProjectShift;
window.checkShiftDns = checkShiftDns;
window.completeShiftCutover = completeShiftCutover;
window.abortShiftMigration = abortShiftMigration;
window.closeAddNodeModal = () => closeModal('modal-add-node');
