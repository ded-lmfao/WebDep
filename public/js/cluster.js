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
  nodeLabel,
  getNodeEffectivePublicIp,
  isPublicIp,
  renderPublicIpPill,
  escapeHtml,
  formatRelativeTime,
  openModal,
  closeModal,
  showConfirmDialog,
  showPromptDialog,
  getAppNodeLabel,
} from './state.js';
import { api, toast } from './api.js';
import { nodeNetChartHtml, syncNodeNetCharts, stopNodeNetCharts } from './node_net_charts.js';

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

export async function loadNodes(force = false) {
  try {
    const list = await api('/api/cluster/nodes', force ? { noCache: true } : {});
    state.nodes = Array.isArray(list) ? list : [];
    const current = String(state.selectedNodeFilter || 'all');
    if (current !== 'all' && !state.nodes.some((n) => String(n.id) === current)) {
      state.selectedNodeFilter = 'all';
    }
    const badge = document.getElementById('badge-nodes-count');
    if (badge) badge.textContent = String(state.nodes.length);
  } catch (err) {
    console.warn('Failed to load cluster nodes:', err);
  }
}

export async function loadNodesView(force = false) {
  await loadNodes(force);
  let tokens = [];
  if (state.user?.isAdmin) {
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
  const isMainAdmin = Boolean(state.user?.isAdmin);

  const onlineCount = nodes.filter((n) => n.status === 'online').length;
  const drainingCount = nodes.filter((n) => n.status === 'draining').length;
  const totalWorkloads = nodes.reduce(
    (sum, n) => sum + Number(n.projectCount || 0) + Number(n.databaseCount || 0),
    0,
  );
  const totalRam = nodes.reduce((sum, n) => sum + Number(n.metrics?.memTotal || 0), 0);
  const totalCores = nodes.reduce((sum, n) => sum + Number(n.capabilities?.cpuCores || 0), 0);
  const totalDisk = nodes.reduce((sum, n) => sum + Number(n.metrics?.diskTotal || 0), 0);
  const usedDisk = nodes.reduce((sum, n) => sum + Number(n.metrics?.diskUsed || 0), 0);

  const statTotal = document.getElementById('nodes-stat-total');
  const statOnline = document.getElementById('nodes-stat-online');
  const statWorkloads = document.getElementById('nodes-stat-workloads');
  const statRam = document.getElementById('nodes-stat-ram');
  if (statTotal) statTotal.textContent = String(nodes.length);
  if (statOnline) statOnline.textContent = drainingCount > 0 ? `${onlineCount} (${drainingCount} frozen)` : String(onlineCount);
  if (statWorkloads) statWorkloads.textContent = String(totalWorkloads);
  if (statRam) statRam.textContent = totalRam > 0 ? formatBytesHuman(totalRam) : '--';
  const statCores = document.getElementById('nodes-stat-cores');
  const statStorage = document.getElementById('nodes-stat-storage');
  const statStorageMeta = document.getElementById('nodes-stat-storage-meta');
  if (statCores) statCores.textContent = totalCores > 0 ? `${totalCores} core${totalCores === 1 ? '' : 's'}` : '--';
  if (statStorage) statStorage.textContent = totalDisk > 0 ? formatBytesHuman(totalDisk) : '--';
  if (statStorageMeta) {
    statStorageMeta.textContent = totalDisk > 0
      ? `${formatBytesHuman(Math.max(0, totalDisk - usedDisk))} free`
      : 'Total disk across nodes';
  }

  const addBtn = document.getElementById('add-node-header-btn');
  if (addBtn) addBtn.style.display = isMainAdmin ? '' : 'none';
  const genTokenBtn = document.getElementById('nodes-generate-token-btn');
  if (genTokenBtn) genTokenBtn.style.display = isMainAdmin ? '' : 'none';
  const fleetBtn = document.getElementById('fleet-update-header-btn');
  if (fleetBtn) fleetBtn.style.display = isMainAdmin ? '' : 'none';

  const barTone = (pct) => (pct >= 85 ? 'rose' : pct >= 65 ? 'amber' : 'green');

  if (!nodes.length) {
    stopNodeNetCharts();
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
    // Freeze (stop new deploys here) and Evacuate move work to other nodes, so they only make sense
    // when another node is online.
    const onlineNodeIds = nodes.filter((x) => x.status === 'online').map((x) => x.id);
    const cardsHtml = nodes.map((n) => {
      const m = n.metrics || {};
      const caps = n.capabilities || {};
      const isOnline = n.status === 'online';
      const isDraining = n.status === 'draining';
      const statusDot = isOnline ? 'online' : isDraining ? 'maintenance' : 'offline';
      const statusBadge = isOnline
        ? '<span class="badge badge-success">Online</span>'
        : isDraining
          ? '<span class="badge badge-warning" title="No new deployments are placed here; running apps keep running">Frozen</span>'
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

      const pubIp = getNodeEffectivePublicIp(n);
      const hasOtherOnlineNode = onlineNodeIds.some((id) => id !== n.id);
      // Unfreeze stays available so a frozen node can always be put back.
      const showDrain = isMainAdmin && (isDraining || hasOtherOnlineNode);
      const showEvacuate = isMainAdmin && hasOtherOnlineNode && Number(n.projectCount || 0) > 0;

      return `
        <div class="node-card">
          <div class="node-card-head">
            <div style="min-width: 0;">
              <div style="display: flex; align-items: center; gap: 8px; flex-wrap: wrap;">
                <span class="status-dot ${statusDot}"></span>
                <strong style="font-size: 15px; color: var(--text-main);">${escapeHtml(nodeLabel(n))}</strong>
                ${n.displayName ? `<span style="font-size: 11.5px; color: var(--text-dim); font-family: var(--font-mono);" title="Node's own name">${escapeHtml(n.name)}</span>` : ''}
                ${n.isLocal ? '<span class="badge badge-info" style="font-size: 10.5px;">Control Node</span>' : '<span class="badge badge-neutral" style="font-size: 10.5px;">Worker</span>'}
                ${statusBadge}
              </div>
              <div class="node-card-meta">
                <span class="node-meta-item">Region <strong style="color: var(--text-main);">${escapeHtml(n.region || 'default')}</strong></span>
                <span class="node-meta-item">Mesh <code>${escapeHtml(n.meshIp || '--')}</code></span>
                ${pubIp ? `<span class="node-meta-item">${renderPublicIpPill(pubIp, { label: 'Public IP' })}</span>` : ''}
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

          ${nodeNetChartHtml(n.id)}

          <div style="display: flex; flex-wrap: wrap; gap: 6px; align-items: center;">
            ${capBadges}
          </div>

          <div class="node-card-footer">
            <div style="font-size: 12px; color: var(--text-muted);">
              <strong style="color: var(--text-main);">${Number(n.projectCount || 0)}</strong> apps &bull;
              <strong style="color: var(--text-main);">${Number(n.databaseCount || 0)}</strong> databases
            </div>
            <div style="display: flex; gap: 6px; flex-wrap: wrap;">
              ${isMainAdmin ? `
                <button class="btn btn-secondary btn-sm" onclick="renameClusterNode(${n.id})" title="Set the name shown for this node in the panel">Rename</button>
              ` : ''}
              ${showDrain ? `
                <button class="btn btn-secondary btn-sm" onclick="toggleNodeDrain(${n.id}, ${isDraining ? 'false' : 'true'})" title="${isDraining ? 'Allow new deployments on this node again' : 'Stop new deployments landing on this node; running apps keep running'}">
                  ${isDraining ? 'Unfreeze' : 'Freeze'}
                </button>
              ` : ''}
              ${showEvacuate ? `
                <button class="btn btn-secondary btn-sm" onclick="evacuateClusterNode(${n.id}, '${escapeHtml(nodeLabel(n))}')" title="Freeze this node and move all its apps to other online nodes">Evacuate</button>
              ` : ''}
              ${isMainAdmin && !n.isLocal ? `
                <button class="btn btn-danger btn-sm" onclick="removeClusterNode(${n.id}, '${escapeHtml(nodeLabel(n))}')">Remove</button>
              ` : ''}
            </div>
          </div>
        </div>
      `;
    }).join('');
    setHtmlIfChanged(container, cardsHtml);
    syncNodeNetCharts(nodes.map((n) => n.id));
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
          ? `<span class="badge badge-info">Joined as ${escapeHtml(getNodeDisplayName(t.usedNodeId))}</span>`
          : (t.expiresAt > nowS ? `Expires ${formatRelativeTime(t.expiresAt)}` : 'Expired');
        const isActive = !t.usedAt && t.expiresAt > nowS;
        const actionCell = `<button class="btn btn-secondary btn-sm" onclick="deleteEnrollToken(${Number(t.id)}, ${isActive})">${isActive ? 'Revoke' : 'Delete'}</button>`;

        return `
          <tr>
            <td><strong>${escapeHtml(t.label || '(unlabeled)')}</strong></td>
            <td><span class="badge badge-neutral">${escapeHtml(t.region || 'default')}</span></td>
            <td>${status}</td>
            <td style="color: var(--text-dim);">${formatRelativeTime(t.createdAt)}</td>
            <td>${resultCell}</td>
            <td style="text-align: right;">${actionCell}</td>
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
                  <th>Result / Expiry</th>
                  <th></th>
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
  if (sugEl) sugEl.innerHTML = '';
  showAddNodeStep('form');
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
      const addChip = (labelHtml, url) => {
        if (!url) return;
        const btn = document.createElement('button');
        btn.type = 'button';
        btn.className = 'btn btn-secondary btn-sm';
        btn.style.cssText = 'padding: 2px 8px; font-size: 10.5px; display: inline-flex; align-items: center; gap: 4px;';
        btn.innerHTML = labelHtml;
        btn.addEventListener('click', () => {
          if (coreUrlEl) coreUrlEl.value = url;
        });
        sugEl.appendChild(btn);
      };

      const portSuffix = info.port ? `:${info.port}` : '';
      const proto = info.requestUrl && info.requestUrl.startsWith('https://') ? 'https' : 'http';
      if (info.publicIp) addChip(`Public IP (${escapeHtml(info.publicIp)})`, `${proto}://${info.publicIp}${portSuffix}`);
      if (info.lanIp && info.lanIp !== info.publicIp) addChip(`LAN IP (${escapeHtml(info.lanIp)})`, `${proto}://${info.lanIp}${portSuffix}`);
      if (info.requestUrl) addChip(`This browser (${escapeHtml(info.requestUrl)})`, info.requestUrl);
    }
  } catch (_) {
    if (sugEl) sugEl.innerHTML = '';
  }
}

// The dialog has two steps: the details form, then (after generating) only the join command.
function showAddNodeStep(step) {
  const isResult = step === 'result';
  const show = (id, visible) => {
    const el = document.getElementById(id);
    if (el) el.style.display = visible ? '' : 'none';
  };
  show('add-node-form', !isResult);
  show('add-node-result-box', isResult);
  show('generate-node-token-btn', !isResult);
  show('add-node-another-btn', isResult);
  setText('add-node-close-btn', isResult ? 'Done' : 'Cancel');
}

function setText(id, text) {
  const el = document.getElementById(id);
  if (el) el.textContent = text;
}

export function addAnotherNode() {
  void openAddNodeModal();
}

export async function deleteEnrollToken(tokenId, isActive) {
  const ok = await showConfirmDialog({
    title: isActive ? 'Revoke join token' : 'Delete join token',
    message: isActive
      ? 'Revoke this join token? Any join command made with it stops working right away.'
      : 'Remove this token from the list? Nodes that already joined are not affected.',
    confirmText: isActive ? 'Revoke' : 'Delete',
    danger: isActive,
  });
  if (!ok) return;
  try {
    await api(`/api/cluster/enroll-tokens/${tokenId}`, { method: 'DELETE' });
    toast(isActive ? 'Join token revoked' : 'Join token removed', 'success');
    await loadNodesView(true);
  } catch (err) {
    toast(`Could not remove token: ${err.message}`, 'error');
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
    const ttlMin = Math.round(ttlSeconds / 60);
    setText('add-node-result-title', `Join command for ${label}`);
    setText('add-node-result-hint', `Run it on the new VPS as a sudo user. It works once and expires in ${ttlMin} minutes.`);
    showAddNodeStep('result');
    toast('Join command generated', 'success');
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
    toast(drain ? `Node #${nodeId} frozen: no new deployments will be placed on it` : `Node #${nodeId} unfrozen: accepting new deployments`, 'success');
    await loadNodesView(true);
  } catch (err) {
    toast(`Failed updating node state: ${err.message}`, 'error');
  }
}

export async function renameClusterNode(nodeId) {
  const node = (state.nodes || []).find((n) => Number(n.id) === Number(nodeId));
  if (!node) return;
  const value = await showPromptDialog({
    title: 'Rename node',
    message: `Name shown for this node in the panel. Leave empty to use its own name (${node.name}). Letters, digits, spaces and - _ . ( ) # only, up to 48 characters.`,
    placeholder: 'Node name',
    defaultValue: node.displayName || '',
    confirmText: 'Save',
  });
  if (value === null) return;
  try {
    await api(`/api/cluster/nodes/${nodeId}`, { method: 'PATCH', body: { displayName: value.trim() } });
    toast(value.trim() ? `Node renamed to "${value.trim()}"` : `Node name reset to "${node.name}"`, 'success');
    await loadNodesView(true);
  } catch (err) {
    toast(`Rename failed: ${err.message}`, 'error');
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

let activeShiftMigration = null;
let shiftProgressTimer = null;
let shiftAutoCloseTimer = null;
let shiftInFlight = false;
let shiftMaxPct = 0;

const SHIFT_STEPS = [
  'Preflight',
  'Transfer',
  'Warm Start',
  'Cutover',
  'Cleanup',
];

const SHIFT_STAGE_META = {
  preflight: { pct: 12, step: 0, label: 'Checking nodes & reserving port', tone: 'active' },
  pending: { pct: 12, step: 0, label: 'Queued for live shift', tone: 'active' },
  packing: { pct: 28, step: 1, label: 'Packing app files & container data', tone: 'active' },
  transferring: { pct: 48, step: 1, label: 'Streaming P2P archive to target VPS', tone: 'active' },
  starting: { pct: 68, step: 2, label: 'Warm-starting app & health probing', tone: 'active' },
  awaiting_dns: { pct: 80, step: 3, label: 'Warmed on target — awaiting DNS cutover', tone: 'waiting' },
  cutting_over: { pct: 86, step: 3, label: 'Switching traffic & project ownership', tone: 'active' },
  cleaning_source: { pct: 94, step: 4, label: 'Stopping old instance & cleaning source VPS', tone: 'active' },
  finalizing: { pct: 97, step: 4, label: 'Refreshing cluster & project state', tone: 'active' },
  completed: { pct: 100, step: 5, label: 'App shifted completely!', tone: 'complete' },
  failed: { pct: 0, step: 0, label: 'Shift failed', tone: 'failed' },
  aborted: { pct: 0, step: 0, label: 'Shift aborted', tone: 'failed' },
};

function stopShiftProgressPoller() {
  if (shiftProgressTimer) {
    clearInterval(shiftProgressTimer);
    shiftProgressTimer = null;
  }
  if (shiftAutoCloseTimer) {
    clearTimeout(shiftAutoCloseTimer);
    shiftAutoCloseTimer = null;
  }
}

function startShiftProgressPoller(pid) {
  stopShiftProgressPoller();
  if (!pid) return;
  shiftProgressTimer = setInterval(async () => {
    const modal = document.getElementById('modal-move-project-node');
    if (!modal || !modal.classList.contains('active')) {
      stopShiftProgressPoller();
      return;
    }
    try {
      const history = await api(`/api/projects/${pid}/migrations`, { noCache: true });
      if (!Array.isArray(history) || !history.length) return;
      const latest = history[0];
      if (!latest) return;
      // Hold below 100% while local HTTP request/refresh is still finishing
      renderShiftModalState(latest, history, { allowComplete: !shiftInFlight });
      if (!shiftInFlight && !['running', 'pending'].includes(latest.status)) {
        stopShiftProgressPoller();
      }
    } catch (_) {}
  }, 450);
}

function renderShiftProgressHtml(mig, opts = {}) {
  const allowComplete = opts.allowComplete !== false;
  const stageKey = mig?.status === 'failed'
    ? 'failed'
    : mig?.status === 'aborted'
      ? 'aborted'
      : mig?.status === 'completed'
        ? (allowComplete ? 'completed' : 'finalizing')
        : (mig?.stage || 'preflight');
  const meta = SHIFT_STAGE_META[stageKey] || SHIFT_STAGE_META.preflight;

  let pct = meta.pct;
  if (stageKey === 'failed' || stageKey === 'aborted') {
    pct = Math.max(10, Math.min(94, shiftMaxPct || 25));
  } else if (stageKey === 'completed' && allowComplete) {
    pct = 100;
    shiftMaxPct = 100;
  } else {
    shiftMaxPct = Math.min(97, Math.max(shiftMaxPct, pct));
    pct = shiftMaxPct;
  }

  const toneCls = meta.tone === 'complete'
    ? 'is-complete'
    : meta.tone === 'waiting'
      ? 'is-waiting'
      : meta.tone === 'failed'
        ? 'is-failed'
        : 'is-active';

  const badgeCls = meta.tone === 'complete'
    ? 'badge-success'
    : meta.tone === 'waiting'
      ? 'badge-warning'
      : meta.tone === 'failed'
        ? 'badge-danger'
        : 'badge-info';

  const idLabel = mig?.id ? `Shift #${mig.id}` : 'Starting Live Shift';
  const fromLabel = escapeHtml(mig?.fromNodeName || 'Source');
  const toLabel = escapeHtml(mig?.toNodeName || 'Target');
  const bytesVal = Number(mig?.transferBytes || 0);
  const metaBits = [
    mig?.targetPort ? `Port <code>${mig.targetPort}</code>` : '',
    bytesVal > 0 ? `Transferred <strong>${formatBytesHuman(bytesVal)}</strong>` : '',
  ].filter(Boolean).join(' &bull; ');

  const stepsHtml = SHIFT_STEPS.map((label, idx) => {
    let cls = 'shift-step-pill';
    if (meta.tone === 'failed' && idx === (meta.step ?? 0)) cls += ' is-failed';
    else if (meta.step > idx) cls += ' is-done';
    else if (meta.step === idx && meta.tone !== 'complete') cls += ' is-current';
    return `<span class="${cls}">${escapeHtml(label)}</span>`;
  }).join('');

  return `
    <div class="shift-progress-wrap">
      <div class="shift-progress-head">
        <div>
          <strong style="font-size:13px; color:var(--text-main);">${idLabel}: ${fromLabel} &rarr; ${toLabel}</strong>
          <div style="font-size:11.5px; color:var(--text-muted); margin-top:2px;">${escapeHtml(meta.label)}</div>
        </div>
        <div style="display:flex; align-items:center; gap:8px;">
          <span class="shift-progress-pct">${pct}%</span>
          <span class="badge ${badgeCls}">${escapeHtml(stageKey === 'finalizing' ? 'finalizing' : (mig?.stage || 'preflight'))}</span>
          ${mig?.id && ['running', 'pending'].includes(mig.status) ? '<button type="button" class="btn btn-danger btn-sm" style="padding:2px 8px; font-size:11px;" onclick="abortShiftMigration()">Abort</button>' : ''}
        </div>
      </div>
      <div class="shift-progress-track" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${pct}">
        <div class="shift-progress-fill ${toneCls}" style="width: ${pct}%;"></div>
      </div>
      <div class="shift-steps-row">${stepsHtml}</div>
      ${metaBits ? `<div style="font-size:11.5px; color:var(--text-dim); margin-top:2px;">${metaBits}</div>` : ''}
      ${stageKey === 'failed' && mig?.error ? `<div style="font-size:11.5px; color:#fca5a5; margin-top:2px;">${escapeHtml(mig.error)}</div>` : ''}
    </div>
  `;
}

function renderShiftModalState(mig, historyList = [], opts = {}) {
  const setupEl = document.getElementById('shift-setup-panel');
  const activeEl = document.getElementById('shift-active-panel');
  const bannerEl = document.getElementById('shift-stage-banner');
  const dnsCard = document.getElementById('shift-dns-card');
  const dnsRecEl = document.getElementById('shift-dns-records');
  const startBtn = document.getElementById('shift-start-btn');
  const histEl = document.getElementById('shift-history-box');

  const allowComplete = opts.allowComplete !== false;
  const showActive = Boolean(
    mig && (
      ['running', 'pending', 'awaiting_dns'].includes(mig.status)
      || opts.forceShowProgress
      || (mig.status === 'completed' && opts.showCompleted)
    )
  );

  if (showActive) {
    activeShiftMigration = ['running', 'pending', 'awaiting_dns'].includes(mig.status) ? mig : null;
    if (setupEl) setupEl.style.display = opts.keepSetupVisible ? 'block' : 'none';
    if (activeEl) activeEl.style.display = 'flex';
    if (startBtn) startBtn.style.display = opts.keepSetupVisible ? '' : 'none';
    if (bannerEl) {
      setHtmlIfChanged(bannerEl, renderShiftProgressHtml(mig, { allowComplete }));
    }
    if (dnsCard) {
      dnsCard.style.display = mig.status === 'awaiting_dns' ? 'block' : 'none';
    }
    if (dnsRecEl && mig.status === 'awaiting_dns') {
      const fmtMigIp = (ip, label) => (isPublicIp(ip)
        ? renderPublicIpPill(ip, { label })
        : `<code>${escapeHtml(ip || '--')}</code>`);
      const domRows = (mig.domains || []).map((d) =>
        `<div style="display:flex; align-items:center; flex-wrap:wrap; gap:6px; margin-bottom:4px;"><code>A</code> &bull; <strong>${escapeHtml(d)}</strong> &bull; ${fmtMigIp(mig.fromIp, 'From')} &rarr; ${fmtMigIp(mig.toIp, 'To')}</div>`
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
      setHtmlIfChanged(histEl, 'No prior migrations recorded for this website.');
    } else {
      const histHtml = `<div style="margin-bottom:4px; font-weight:600; color:var(--text-muted);">Recent Shifts</div>` +
        historyList.slice(0, 4).map((h) =>
          `<div>#${h.id}: ${escapeHtml(h.fromNodeName || '?')} &rarr; ${escapeHtml(h.toNodeName || '?')} &bull; <span class="badge badge-neutral">${escapeHtml(h.status)}</span> (${formatBytesHuman(h.transferBytes || 0)})</div>`
        ).join('');
      setHtmlIfChanged(histEl, histHtml);
    }
  }
}

export async function openMoveToNodeModal(projectId, targetNodeId = null) {
  const pid = Number(projectId || state.selectedProjectId);
  const proj = (state.projects || []).find((x) => x.id === pid);
  if (!proj) {
    toast('Select a website first.', 'warning');
    return;
  }
  stopShiftProgressPoller();
  shiftInFlight = false;
  shiftMaxPct = 0;
  state.selectedProjectId = pid;
  await loadNodes(true);

  const titleEl = document.getElementById('shift-modal-title');
  if (titleEl) titleEl.textContent = `Move "${proj.name}" to Node`;

  const sel = document.getElementById('shift-target-node-select');
  const candidates = (state.nodes || []).filter((n) => n.status === 'online' && Number(n.id) !== Number(proj.node_id));
  if (sel) {
    sel.innerHTML = candidates.length
      ? candidates.map((n) => `<option value="${n.id}">${escapeHtml(nodeLabel(n))} (${escapeHtml(n.region || 'default')} &bull; ${escapeHtml(n.meshIp || 'Worker')})</option>`).join('')
      : '<option value="">No other healthy online nodes available</option>';
    if (targetNodeId && candidates.some((n) => Number(n.id) === Number(targetNodeId))) sel.value = String(targetNodeId);
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
  if (active && ['running', 'pending'].includes(active.status)) {
    startShiftProgressPoller(pid);
  }
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

  const proj = (state.projects || []).find((x) => Number(x.id) === pid);
  const fromNode = (state.nodes || []).find((n) => Number(n.id) === Number(proj?.node_id));
  const toNode = (state.nodes || []).find((n) => Number(n.id) === targetNodeId);

  shiftInFlight = true;
  shiftMaxPct = 10;
  renderShiftModalState(
    {
      id: null,
      projectId: pid,
      fromNodeName: fromNode?.name || 'Source',
      toNodeName: toNode?.name || 'Target',
      stage: 'preflight',
      status: 'running',
      targetPort: 0,
      transferBytes: 0,
    },
    [],
    { allowComplete: false, forceShowProgress: true },
  );
  startShiftProgressPoller(pid);

  try {
    const mig = await api(`/api/projects/${pid}/move`, {
      method: 'POST',
      body: { targetNodeId, autoCutoverWithoutDns: skipDns },
      timeoutMs: 600000,
    });
    stopShiftProgressPoller();
    const history = await api(`/api/projects/${pid}/migrations`, { noCache: true }).catch(() => [mig]);

    if (mig.status === 'completed') {
      // Keep progress below 100% until all cluster/project state refreshes finish
      renderShiftModalState(mig, Array.isArray(history) ? history : [mig], {
        allowComplete: false,
        showCompleted: true,
      });
      if (typeof window.refreshProjects === 'function') await window.refreshProjects();
      await loadNodes(true).catch(() => {});
      shiftInFlight = false;
      renderShiftModalState(mig, Array.isArray(history) ? history : [mig], {
        allowComplete: true,
        showCompleted: true,
      });
      toast(`Live shift #${mig.id} completed cleanly!`, 'success');
      shiftAutoCloseTimer = setTimeout(() => closeModal('modal-move-project-node'), 750);
    } else if (mig.status === 'awaiting_dns') {
      shiftInFlight = false;
      renderShiftModalState(mig, Array.isArray(history) ? history : [mig], { allowComplete: false });
      toast('Target warmed & staged on HTTP! Complete DNS update to finish HTTPS cutover.', 'info');
    } else {
      shiftInFlight = false;
      renderShiftModalState(mig, Array.isArray(history) ? history : [mig], { allowComplete: false });
    }
  } catch (err) {
    stopShiftProgressPoller();
    shiftInFlight = false;
    const history = await api(`/api/projects/${pid}/migrations`, { noCache: true }).catch(() => []);
    const failedMig = (Array.isArray(history) && history[0]) || {
      id: null,
      projectId: pid,
      fromNodeName: fromNode?.name || 'Source',
      toNodeName: toNode?.name || 'Target',
      stage: 'failed',
      status: 'failed',
      error: err.message,
      transferBytes: 0,
    };
    renderShiftModalState(failedMig, Array.isArray(history) ? history : [], {
      allowComplete: false,
      forceShowProgress: true,
      keepSetupVisible: true,
    });
    toast(`Shift failed: ${err.message}`, 'error');
  } finally {
    shiftInFlight = false;
    stopShiftProgressPoller();
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
        ? 'DNS verified pointing to target VPS! Ready to complete cutover.'
        : 'DNS does not point to target VPS yet.',
      res.dnsVerified ? 'success' : 'warning',
    );
  } catch (err) {
    toast(`DNS check failed: ${err.message}`, 'error');
  }
}

export async function completeShiftCutover() {
  if (!activeShiftMigration) return;
  const pid = activeShiftMigration.projectId;
  const cutBtn = document.getElementById('shift-complete-cutover-btn');
  if (cutBtn) {
    cutBtn.disabled = true;
    cutBtn.textContent = 'Cutting over...';
  }
  shiftInFlight = true;
  renderShiftModalState(
    { ...activeShiftMigration, stage: 'cutting_over', status: 'running' },
    [],
    { allowComplete: false, forceShowProgress: true },
  );
  startShiftProgressPoller(pid);
  try {
    const done = await api(`/api/migrations/${activeShiftMigration.id}/complete`, {
      method: 'POST',
      timeoutMs: 300000,
    });
    stopShiftProgressPoller();
    const history = await api(`/api/projects/${pid}/migrations`, { noCache: true }).catch(() => [done]);
    renderShiftModalState(done, Array.isArray(history) ? history : [done], {
      allowComplete: false,
      showCompleted: true,
    });
    if (typeof window.refreshProjects === 'function') await window.refreshProjects();
    await loadNodes(true).catch(() => {});
    shiftInFlight = false;
    renderShiftModalState(done, Array.isArray(history) ? history : [done], {
      allowComplete: true,
      showCompleted: true,
    });
    toast(`Shift #${done.id} cutover completed!`, 'success');
    activeShiftMigration = null;
    shiftAutoCloseTimer = setTimeout(() => closeModal('modal-move-project-node'), 750);
  } catch (err) {
    stopShiftProgressPoller();
    shiftInFlight = false;
    const history = await api(`/api/projects/${pid}/migrations`, { noCache: true }).catch(() => []);
    const latest = Array.isArray(history) ? history[0] : activeShiftMigration;
    renderShiftModalState(latest, Array.isArray(history) ? history : [], {
      allowComplete: false,
      forceShowProgress: true,
    });
    toast(`Cutover failed: ${err.message}`, 'error');
  } finally {
    shiftInFlight = false;
    if (cutBtn) {
      cutBtn.disabled = false;
      cutBtn.textContent = 'Complete Cutover Now';
    }
  }
}

export async function abortShiftMigration() {
  if (!activeShiftMigration) return;
  try {
    await api(`/api/migrations/${activeShiftMigration.id}/abort`, {
      method: 'POST',
      timeoutMs: 120000,
    });
    stopShiftProgressPoller();
    shiftInFlight = false;
    shiftMaxPct = 0;
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
    message: `Freeze "${nodeName}" and move all its apps to other healthy online nodes?`,
    confirmText: 'Freeze & Evacuate',
  });
  if (!ok) return;
  try {
    const res = await api(`/api/cluster/nodes/${nodeId}/evacuate`, {
      method: 'POST',
      body: {},
      timeoutMs: 600000,
    });
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
window.openAddNodeModal = openAddNodeModal;
window.generateNodeEnrollToken = generateNodeEnrollToken;
window.deleteEnrollToken = deleteEnrollToken;
window.addAnotherNode = addAnotherNode;
window.copyNodeJoinCommand = copyNodeJoinCommand;
window.toggleNodeDrain = toggleNodeDrain;
window.renameClusterNode = renameClusterNode;
window.evacuateClusterNode = evacuateClusterNode;
window.removeClusterNode = removeClusterNode;
window.triggerFleetUpdate = triggerFleetUpdate;
window.openMoveToNodeModal = openMoveToNodeModal;

window.confirmStartProjectShift = confirmStartProjectShift;
window.checkShiftDns = checkShiftDns;
window.completeShiftCutover = completeShiftCutover;
window.abortShiftMigration = abortShiftMigration;
window.closeAddNodeModal = () => closeModal('modal-add-node');

// ============================================================================
// Admin panel: IP addresses and Move apps
// ============================================================================
function localNodeId() {
  const local = (state.nodes || []).find((n) => n.isLocal);
  return local ? Number(local.id) : null;
}

/** Node an app runs on (apps without a node run on the local Core node). */
function appNodeId(item) {
  const raw = item.node_id ?? item.nodeId;
  return raw === null || raw === undefined || raw === '' ? localNodeId() : Number(raw);
}

function nodeIpLabel(n) {
  return getNodeEffectivePublicIp(n) || n.publicIp || 'No public IP';
}

async function refreshInfraData() {
  await Promise.all([
    loadNodes(true),
    window.loadProjects ? window.loadProjects().catch(() => {}) : Promise.resolve(),
    window.loadDatabases ? window.loadDatabases().catch(() => {}) : Promise.resolve(),
  ]);
}

function appPortText(p) {
  return p.port || p.assigned_port || '';
}

export async function loadIpsView() {
  const body = document.getElementById('ips-view-body');
  if (!body) return;
  await refreshInfraData();
  const nodes = state.nodes || [];
  if (!nodes.length) {
    body.innerHTML = '<p class="acct-muted">No nodes yet.</p>';
    return;
  }
  const online = nodes.filter((n) => n.status === 'online');
  const q = (document.getElementById('ips-search')?.value || '').trim().toLowerCase();

  body.innerHTML = nodes.map((n) => {
    const nid = Number(n.id);
    const ip = nodeIpLabel(n);
    const apps = (state.projects || []).filter((p) => appNodeId(p) === nid);
    const dbs = (state.databases || []).filter((d) => appNodeId(d) === nid);
    const matches = (name) => !q || name.toLowerCase().includes(q) || ip.toLowerCase().includes(q);
    const appRows = apps.filter((p) => matches(p.name)).map((p) => {
      const targets = online.filter((t) => Number(t.id) !== nid);
      const domains = Array.isArray(p.domains) && p.domains.length ? p.domains.join(', ') : '';
      return `<tr>
        <td><div class="acct-person-name">${escapeHtml(p.name)}</div><div class="acct-person-sub">${domains ? escapeHtml(domains) : 'No domain'}</div></td>
        <td><span class="badge badge-neutral">${p.project_kind === 'service' ? 'Service' : 'Website'}</span></td>
        <td><code>${escapeHtml(ip)}${appPortText(p) ? `:${appPortText(p)}` : ''}</code></td>
        <td>${targets.length ? `<div class="acct-actions" style="justify-content:flex-start">
            <select class="form-control ip-change-select" data-ip-app="${p.id}" style="height:30px; max-width:230px; font-size:12.5px;">
              <option value="">Change IP</option>
              ${targets.map((t) => `<option value="${t.id}">${escapeHtml(nodeIpLabel(t))} (${escapeHtml(nodeLabel(t))})</option>`).join('')}
            </select></div>` : '<span class="acct-muted">No other online IP</span>'}</td>
      </tr>`;
    }).join('');
    const dbRows = dbs.filter((d) => matches(d.name)).map((d) => `<tr>
        <td><div class="acct-person-name">${escapeHtml(d.name)}</div><div class="acct-person-sub">PostgreSQL</div></td>
        <td><span class="badge badge-neutral">Database</span></td>
        <td><code>${escapeHtml(ip)}${d.port ? `:${d.port}` : ''}</code></td>
        <td><span class="acct-muted" title="Databases stay on the node they were created on">Fixed to this node</span></td>
      </tr>`).join('');
    const rows = appRows + dbRows;
    if (q && !rows) return '';
    return `<div class="project-card ip-node-card">
      <div class="ip-node-head">
        <div>
          <div class="ip-node-ip"><code>${escapeHtml(ip)}</code> <span class="badge ${n.status === 'online' ? 'badge-success' : 'badge-danger'}">${n.status === 'online' ? 'Online' : escapeHtml(n.status || 'Offline')}</span></div>
          <div class="acct-muted">${escapeHtml(nodeLabel(n))}${n.isLocal ? ' (Core)' : ''} &middot; mesh ${escapeHtml(n.meshIp || '-')} &middot; ${apps.length} app${apps.length === 1 ? '' : 's'}, ${dbs.length} database${dbs.length === 1 ? '' : 's'}</div>
        </div>
      </div>
      ${rows ? `<div class="table-responsive"><table class="table acct-table"><thead><tr><th>App</th><th>Type</th><th>Address</th><th>Assign</th></tr></thead><tbody>${rows}</tbody></table></div>`
        : '<p class="acct-muted" style="margin:6px 0 0">Nothing is using this IP.</p>'}
    </div>`;
  }).join('') || '<p class="acct-muted">Nothing matches your search.</p>';
}

document.getElementById('ips-view-body')?.addEventListener('change', (e) => {
  const sel = e.target.closest('[data-ip-app]');
  if (!sel || !sel.value) return;
  const pid = Number(sel.getAttribute('data-ip-app'));
  const target = Number(sel.value);
  sel.value = '';
  void openMoveToNodeModal(pid, target);
});
document.getElementById('ips-search')?.addEventListener('input', () => void loadIpsView());

export async function loadMovesView() {
  const tbody = document.getElementById('moves-list-body');
  if (!tbody) return;
  await refreshInfraData();
  const q = (document.getElementById('moves-search')?.value || '').trim().toLowerCase();
  const nodesById = new Map((state.nodes || []).map((n) => [Number(n.id), n]));
  const list = (state.projects || []).filter((p) => !q
    || (p.name || '').toLowerCase().includes(q)
    || (p.slug || '').toLowerCase().includes(q)
    || nodeIpLabel(nodesById.get(appNodeId(p)) || {}).toLowerCase().includes(q));
  const otherOnline = (p) => (state.nodes || []).some((n) => n.status === 'online' && Number(n.id) !== appNodeId(p));
  tbody.innerHTML = list.length ? list.map((p) => {
    const node = nodesById.get(appNodeId(p));
    const running = p.app_status === 'running';
    return `<tr>
      <td><div class="acct-person-name">${escapeHtml(p.name)}</div><div class="acct-person-sub">${escapeHtml(p.slug || '')}</div></td>
      <td><span class="badge badge-neutral">${p.project_kind === 'service' ? 'Service' : 'Website'}</span></td>
      <td>${node ? escapeHtml(nodeLabel(node)) : '<span class="acct-muted">Unknown</span>'}</td>
      <td><code>${escapeHtml(node ? nodeIpLabel(node) : '-')}</code></td>
      <td>${running ? '<span class="badge badge-success">Running</span>' : '<span class="badge badge-neutral">Stopped</span>'}</td>
      <td><div class="acct-actions">${otherOnline(p)
        ? `<button class="btn btn-secondary btn-sm" onclick="openMoveToNodeModal(${p.id})">Move</button>`
        : '<span class="acct-muted" title="Add or bring another node online to move apps">No other node</span>'}</div></td>
    </tr>`;
  }).join('') : `<tr><td colspan="6" class="acct-empty">${q ? 'Nothing matches your search.' : 'No websites or services yet.'}</td></tr>`;
}
document.getElementById('moves-search')?.addEventListener('input', () => void loadMovesView());

window.loadIpsView = loadIpsView;
window.loadMovesView = loadMovesView;
