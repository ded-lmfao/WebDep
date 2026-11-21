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
    toast('Enter a node name.', 'warning');
    document.getElementById('add-node-label')?.focus();
    return;
  }
  const portMinRaw = (document.getElementById('add-node-port-min')?.value || '').trim();
  const portMaxRaw = (document.getElementById('add-node-port-max')?.value || '').trim();
  if (Boolean(portMinRaw) !== Boolean(portMaxRaw)) {
    toast('Enter both the lowest and the highest port, or leave both empty.', 'warning');
    return;
  }
  const portMin = portMinRaw ? Number(portMinRaw) : undefined;
  const portMax = portMaxRaw ? Number(portMaxRaw) : undefined;
  if (portMin !== undefined && (portMin < 1024 || portMax > 65535 || portMax < portMin)) {
    toast('Use ports between 1024 and 65535, lowest first.', 'warning');
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
        portMin,
        portMax,
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

const ipsPools = new Map();
const ipsUi = {
  nodeId: null,
  selected: new Set(),
  selectMode: false,
  stateFilter: 'all',
  settingsOpen: false,
};

async function loadNodePool(nodeId) {
  try {
    const res = await api(`/api/cluster/nodes/${nodeId}/ports`, { noCache: true });
    ipsPools.set(Number(nodeId), res);
    if (res.firewallNote) toast(res.firewallNote, 'warning');
  } catch (err) {
    ipsPools.set(Number(nodeId), { error: err.message });
  }
}

/** in-use | extra | free | closed */
function portState(p) {
  if (p.usedBy) return p.usedBy.role === 'extra' ? 'extra' : 'used';
  return p.open ? 'free' : 'closed';
}

function poolCounts(ports) {
  const c = { used: 0, extra: 0, free: 0, closed: 0, total: ports.length };
  ports.forEach((p) => { c[portState(p)] += 1; });
  return c;
}

function kindLabel(kind) {
  return kind === 'database' ? 'Database' : kind === 'service' ? 'Service' : 'Website';
}

function currentIpsNode() {
  const nodes = state.nodes || [];
  if (!nodes.some((n) => Number(n.id) === Number(ipsUi.nodeId))) {
    ipsUi.nodeId = nodes.length ? Number((nodes.find((n) => n.isLocal) || nodes[0]).id) : null;
  }
  return nodes.find((n) => Number(n.id) === Number(ipsUi.nodeId)) || null;
}

function renderNodeChips(nodes) {
  return `<div class="ipx-chips" role="tablist" aria-label="Nodes">${nodes.map((n) => {
    const data = ipsPools.get(Number(n.id));
    const ports = data?.pool?.ports || [];
    const c = poolCounts(ports);
    const active = Number(n.id) === Number(ipsUi.nodeId);
    return `<button type="button" role="tab" aria-selected="${active}" class="ipx-chip ${active ? 'is-active' : ''}" data-ipx-node="${n.id}">
      <span class="status-dot ${n.status === 'online' ? 'online' : 'offline'}"></span>
      <span class="ipx-chip-name">${escapeHtml(nodeLabel(n))}</span>
      <span class="ipx-chip-meta">${ports.length ? `${c.used + c.extra}/${c.total}` : 'No range'}</span>
    </button>`;
  }).join('')}</div>`;
}

function renderUsageBar(c) {
  if (!c.total) return '';
  const seg = (n, cls, label) => (n ? `<span class="ipx-bar-seg ${cls}" style="flex:${n}" title="${label}: ${n}"></span>` : '');
  return `<div class="ipx-bar" aria-hidden="true">${seg(c.used, 'is-used', 'In use')}${seg(c.extra, 'is-extra', 'Extra')}${seg(c.free, 'is-free', 'Free')}${seg(c.closed, 'is-closed', 'Closed')}</div>`;
}

function renderPortMap(ports) {
  const q = (document.getElementById('ips-search')?.value || '').trim().toLowerCase();
  const f = ipsUi.stateFilter;
  if (!ports.length) {
    return '<div class="ipx-empty">No ports yet. Open <strong>Port settings</strong> to set a range for this node.</div>';
  }
  return `<div class="ipx-grid" role="grid" aria-label="Ports">${ports.map((p) => {
    const st = portState(p);
    const matchesState = f === 'all' || f === st || (f === 'used' && st === 'extra');
    const matchesQuery = !q || String(p.port).includes(q) || (p.usedBy && p.usedBy.name.toLowerCase().includes(q));
    const dim = !(matchesState && matchesQuery);
    const sel = ipsUi.selected.has(p.port);
    const tip = p.usedBy
      ? `${p.port}: ${p.usedBy.name}${p.usedBy.role === 'extra' ? ' (extra)' : ''}`
      : `${p.port}: ${st === 'free' ? 'free' : 'closed'}${p.inPool ? '' : ', outside range'}`;
    return `<button type="button" class="ipx-tile is-${st} ${sel ? 'is-selected' : ''} ${dim ? 'is-dim' : ''} ${p.inPool ? '' : 'is-outside'}"
      data-ipx-port="${p.port}" title="${escapeHtml(tip)}" aria-pressed="${sel}">
      <span class="ipx-tile-port">${p.port}</span>
      ${p.usedBy ? `<span class="ipx-tile-app">${escapeHtml(p.usedBy.name)}</span>` : ''}
    </button>`;
  }).join('')}</div>`;
}

function appActionButtons(appId, running, others) {
  return `<button class="btn btn-secondary btn-sm" data-ipx-act="app-port" data-app="${appId}">Change port</button>
    <button class="btn btn-secondary btn-sm" data-ipx-act="app-extra" data-app="${appId}">Extra ports</button>
    <button class="btn btn-sm ${running ? 'ipx-btn-stop' : 'ipx-btn-start'}" data-ipx-act="${running ? 'app-stop' : 'app-start'}" data-app="${appId}">${running ? 'Stop' : 'Start'}</button>
    ${others.length ? `<select class="form-control ipx-move" data-app="${appId}" aria-label="Move to another node">
      <option value="">Move to node</option>${others.map((t) => `<option value="${t.id}">${escapeHtml(nodeLabel(t))}</option>`).join('')}
    </select>` : ''}`;
}

function renderDetail(ports, others) {
  const picked = ports.filter((p) => ipsUi.selected.has(p.port));
  if (!picked.length) {
    return `<div class="ipx-detail-empty">
      <div class="ipx-detail-title">Select a port</div>
      <p>Click a port to see who uses it and manage it. Hold <kbd>Shift</kbd> or <kbd>Ctrl</kbd>, or turn on <strong>Select</strong>, to pick several and open or close them together.</p>
    </div>`;
  }
  if (picked.length > 1) {
    const closable = picked.filter((p) => !p.usedBy && p.open).length;
    const openable = picked.filter((p) => !p.open).length;
    const busy = picked.filter((p) => p.usedBy).length;
    return `<div class="ipx-detail-title">${picked.length} ports selected</div>
      <p class="acct-muted">${busy ? `${busy} in use (left unchanged). ` : ''}${closable} can be closed, ${openable} can be opened.</p>
      <div class="ipx-detail-actions">
        <button class="btn btn-secondary btn-sm" data-ipx-act="bulk-open" ${openable ? '' : 'disabled'}>Open ${openable || ''}</button>
        <button class="btn btn-secondary btn-sm" data-ipx-act="bulk-close" ${closable ? '' : 'disabled'}>Close ${closable || ''}</button>
        <button class="btn btn-secondary btn-sm" data-ipx-act="clear">Clear selection</button>
      </div>`;
  }
  const p = picked[0];
  const st = portState(p);
  const badge = { used: ['badge-info', 'In use'], extra: ['badge-info', 'Extra port'], free: ['badge-success', 'Open, free'], closed: ['badge-danger', 'Closed'] }[st];
  let body = '';
  if (p.usedBy) {
    const u = p.usedBy;
    body = `<div class="ipx-detail-app">
        <div class="acct-person-name">${escapeHtml(u.name)}</div>
        <div class="acct-person-sub">${kindLabel(u.kind)} &middot; ${u.running ? 'Running' : 'Stopped'}${u.role === 'extra' ? ' &middot; extra port' : ''}</div>
      </div>
      ${u.role === 'main' && u.kind !== 'database'
        ? `<div class="ipx-detail-actions">${appActionButtons(u.id, u.running, others)}</div>`
        : `<p class="acct-muted">${u.kind === 'database' ? 'Databases keep their port.' : 'Change extra ports from the app\'s main port.'}</p>`}`;
  } else {
    body = `<div class="ipx-detail-actions">
        ${p.open ? '<button class="btn btn-secondary btn-sm" data-ipx-act="close-one">Close port</button>' : '<button class="btn btn-secondary btn-sm" data-ipx-act="open-one">Open port</button>'}
        ${p.manual && !p.open ? '<button class="btn btn-secondary btn-sm" data-ipx-act="remove-one">Remove port</button>' : ''}
      </div>
      <p class="acct-muted">${p.open ? 'The next new or moved app on this node takes the lowest free port.' : 'Closed ports are blocked in the firewall and never handed out.'}</p>`;
  }
  return `<div class="ipx-detail-head"><span class="ipx-detail-port">${p.port}</span><span class="badge ${badge[0]}">${badge[1]}</span></div>
    ${p.manual ? '<p class="acct-muted" style="margin:2px 0 8px">Added by hand</p>' : ''}${!p.inPool ? '<p class="acct-muted" style="margin:2px 0 8px">Outside the port range</p>' : ''}
    ${body}`;
}

function renderAppsTable(ports, others) {
  const apps = new Map();
  ports.forEach((p) => {
    if (!p.usedBy) return;
    const key = `${p.usedBy.kind}:${p.usedBy.id}`;
    const row = apps.get(key) || { ...p.usedBy, main: null, extra: [] };
    if (p.usedBy.role === 'extra') row.extra.push(p.port); else row.main = p.port;
    row.running = row.running || p.usedBy.running;
    apps.set(key, row);
  });
  const q = (document.getElementById('ips-search')?.value || '').trim().toLowerCase();
  const list = [...apps.values()]
    .filter((a) => !q || a.name.toLowerCase().includes(q) || String(a.main).includes(q))
    .sort((a, b) => (a.main || 0) - (b.main || 0));
  if (!list.length) return '';
  return `<div class="ipx-apps">
    <div class="ipx-section-title">Apps on this node <span class="acct-muted">${list.length}</span></div>
    <div class="table-responsive"><table class="table acct-table">
      <thead><tr><th>App</th><th>Port</th><th>Extra ports</th><th>Status</th><th style="text-align:right">Actions</th></tr></thead>
      <tbody>${list.map((a) => `<tr>
        <td><div class="acct-person-name">${escapeHtml(a.name)}</div><div class="acct-person-sub">${kindLabel(a.kind)}</div></td>
        <td>${a.main ? `<button type="button" class="ipx-port-link" data-ipx-jump="${a.main}">${a.main}</button>` : '-'}</td>
        <td>${a.extra.length ? a.extra.map((x) => `<button type="button" class="ipx-port-link" data-ipx-jump="${x}">${x}</button>`).join(' ') : '<span class="acct-muted">None</span>'}</td>
        <td>${a.running ? '<span class="badge badge-success">Running</span>' : '<span class="badge badge-neutral">Stopped</span>'}</td>
        <td><div class="acct-actions">${a.kind !== 'database' ? appActionButtons(a.id, a.running, others) : '<span class="acct-muted">Fixed port</span>'}</div></td>
      </tr>`).join('')}</tbody>
    </table></div>
  </div>`;
}

function renderPortSettings(nid, pool, data) {
  return `<details class="ipx-settings" ${ipsUi.settingsOpen ? 'open' : ''} data-ipx-settings>
    <summary>Port settings</summary>
    <div class="ipx-settings-body">
      <div class="ipx-field">
        <label class="form-label">Port range</label>
        <div class="ipx-inline">
          <input type="number" class="form-control" id="pool-min-${nid}" min="1024" max="65535" value="${pool.portMin ?? ''}" placeholder="Lowest port">
          <span class="acct-muted">to</span>
          <input type="number" class="form-control" id="pool-max-${nid}" min="1024" max="65535" value="${pool.portMax ?? ''}" placeholder="Highest port">
          <button class="btn btn-primary btn-sm" data-ipx-act="range">Save range</button>
        </div>
        <p class="acct-muted">New ports open in the firewall. Ports dropped from the range are removed unless an app uses them.</p>
      </div>
      <div class="ipx-field">
        <label class="form-label">Add ports outside the range</label>
        <div class="ipx-inline">
          <input type="text" class="form-control" id="pool-add-${nid}" placeholder="Ports or ranges to add">
          <button class="btn btn-secondary btn-sm" data-ipx-act="add">Add ports</button>
        </div>
      </div>
      <div class="ipx-field">
        <label class="form-label">Firewall</label>
        <div class="ipx-inline">${data.firewallManaged
          ? '<button class="btn btn-secondary btn-sm" data-ipx-act="sync">Sync firewall</button><span class="acct-muted">Apply open and closed ports to this VPS again.</span>'
          : '<span class="acct-muted">Firewall changes are turned off on this panel (WEBDEP_MANAGE_FIREWALL).</span>'}</div>
      </div>
    </div>
  </details>`;
}

export async function loadIpsView(reloadPools = true) {
  const body = document.getElementById('ips-view-body');
  if (!body) return;
  if (reloadPools) {
    await refreshInfraData();
    await Promise.all((state.nodes || []).map((n) => loadNodePool(n.id)));
  }
  const nodes = state.nodes || [];
  if (!nodes.length) {
    body.innerHTML = '<p class="acct-muted">No nodes yet.</p>';
    return;
  }
  const n = currentIpsNode();
  const nid = Number(n.id);
  const data = ipsPools.get(nid) || {};
  const pool = data.pool || { ports: [] };
  const ports = pool.ports || [];
  const others = nodes.filter((t) => t.status === 'online' && Number(t.id) !== nid);
  const c = poolCounts(ports);
  ipsUi.selected = new Set([...ipsUi.selected].filter((p) => ports.some((x) => x.port === p)));
  const pill = (key, label, count) => `<button type="button" class="pill ${ipsUi.stateFilter === key ? 'active' : ''}" data-ipx-filter="${key}">${label} <span class="ipx-pill-count">${count}</span></button>`;

  body.innerHTML = `
    ${nodes.length > 1 ? renderNodeChips(nodes) : ''}
    ${data.error ? `<p class="acct-muted">${escapeHtml(data.error)}</p>` : `
    <div class="project-card ipx-node">
      <div class="ipx-head">
        <div>
          <div class="ipx-node-name">${escapeHtml(nodeLabel(n))}${n.isLocal ? ' <span class="acct-muted">(Core)</span>' : ''}
            <span class="badge ${n.status === 'online' ? 'badge-success' : 'badge-danger'}">${n.status === 'online' ? 'Online' : escapeHtml(n.status || 'Offline')}</span></div>
          <div class="acct-muted">IP ${escapeHtml(nodeIpLabel(n))}${pool.portMin ? ` &middot; Range ${pool.portMin}&ndash;${pool.portMax}` : ' &middot; No port range'}</div>
        </div>
        <div class="ipx-stats">
          <div><strong>${c.used}</strong><span>In use</span></div>
          <div><strong>${c.extra}</strong><span>Extra</span></div>
          <div><strong>${c.free}</strong><span>Free</span></div>
          <div><strong>${c.closed}</strong><span>Closed</span></div>
        </div>
      </div>
      ${renderUsageBar(c)}
      ${renderPortSettings(nid, pool, data)}
      <div class="ipx-toolbar">
        <div class="filter-pills">${pill('all', 'All', c.total)}${pill('used', 'In use', c.used + c.extra)}${pill('free', 'Free', c.free)}${pill('closed', 'Closed', c.closed)}</div>
        <div class="ipx-toolbar-right">
          <div class="ipx-legend" aria-hidden="true"><span class="is-used"></span>In use<span class="is-extra"></span>Extra<span class="is-free"></span>Free<span class="is-closed"></span>Closed</div>
          <button type="button" class="btn btn-sm ${ipsUi.selectMode ? 'btn-primary' : 'btn-secondary'}" data-ipx-act="toggle-select" aria-pressed="${ipsUi.selectMode}">${ipsUi.selectMode ? 'Selecting' : 'Select'}</button>
        </div>
      </div>
      <div class="ipx-main">
        <div class="ipx-map">${renderPortMap(ports)}</div>
        <aside class="ipx-detail" aria-live="polite">${renderDetail(ports, others)}</aside>
      </div>
      ${renderAppsTable(ports, others)}
    </div>`}`;
}

async function portRequest(nodeId, method, path, body) {
  const res = await api(`/api/cluster/nodes/${nodeId}${path}`, { method, body });
  ipsPools.set(Number(nodeId), res);
  if (res.firewallNote) toast(res.firewallNote, 'warning');
  await loadIpsView(false);
  return res;
}

async function runIpxAction(act, el) {
  const nid = Number(ipsUi.nodeId);
  const ports = ipsPools.get(nid)?.pool?.ports || [];
  const picked = ports.filter((p) => ipsUi.selected.has(p.port));
  const one = picked[0];
  const appId = Number(el.dataset.app || 0);
  switch (act) {
    case 'toggle-select':
      ipsUi.selectMode = !ipsUi.selectMode;
      if (!ipsUi.selectMode && ipsUi.selected.size > 1) ipsUi.selected.clear();
      return loadIpsView(false);
    case 'clear':
      ipsUi.selected.clear();
      return loadIpsView(false);
    case 'range': {
      const min = Number(document.getElementById(`pool-min-${nid}`)?.value || 0);
      const max = Number(document.getElementById(`pool-max-${nid}`)?.value || 0);
      if (!min || !max) return toast('Enter the lowest and highest port.', 'warning');
      await portRequest(nid, 'PUT', '/port-range', { min, max });
      return toast(`Port range ${min}-${max} saved.`, 'success');
    }
    case 'add': {
      const list = (document.getElementById(`pool-add-${nid}`)?.value || '').trim();
      if (!list) return toast('Enter ports to add.', 'warning');
      await portRequest(nid, 'POST', '/ports', { ports: list });
      return toast('Ports added and opened.', 'success');
    }
    case 'sync':
      await portRequest(nid, 'POST', '/firewall-sync');
      return toast('Firewall updated.', 'success');
    case 'open-one':
    case 'close-one':
      await portRequest(nid, 'PATCH', '/ports', { ports: String(one.port), open: act === 'open-one' });
      return toast(`Port ${one.port} ${act === 'open-one' ? 'opened' : 'closed'}.`, 'success');
    case 'remove-one':
      await portRequest(nid, 'DELETE', `/ports/${one.port}`);
      ipsUi.selected.delete(one.port);
      return toast(`Port ${one.port} removed.`, 'success');
    case 'bulk-open':
    case 'bulk-close': {
      const open = act === 'bulk-open';
      const targets = picked.filter((p) => (open ? !p.open : !p.usedBy && p.open)).map((p) => p.port);
      if (!targets.length) return undefined;
      await portRequest(nid, 'PATCH', '/ports', { ports: targets.join(','), open });
      return toast(`${targets.length} port${targets.length === 1 ? '' : 's'} ${open ? 'opened' : 'closed'}.`, 'success');
    }
    case 'app-port': {
      const typed = await showPromptDialog({
        title: 'Change port',
        message: 'Enter a new port from this node, or leave it empty to use the next free port. Caddy and the firewall update automatically and a running app restarts.',
        placeholder: 'New port',
        confirmText: 'Change port',
      });
      if (typed === null) return undefined;
      const value = String(typed).trim();
      const updated = await api(`/api/projects/${appId}/port`, { method: 'PUT', body: { port: value ? Number(value) : null } });
      ipsUi.selected = new Set([updated.assigned_port || updated.port].filter(Boolean));
      toast(`Now on port ${updated.assigned_port || updated.port}.`, 'success');
      return loadIpsView(true);
    }
    case 'app-extra': {
      const current = ports.filter((p) => p.usedBy && p.usedBy.role === 'extra' && p.usedBy.id === appId).map((p) => p.port);
      const typed = await showPromptDialog({
        title: 'Extra ports',
        message: 'Ports this app may also listen on, from this node\'s open ports. The app gets them in EXTRA_PORTS after a restart. Leave empty to remove them.',
        placeholder: 'Ports or ranges',
        defaultValue: current.join(', '),
        confirmText: 'Save',
      });
      if (typed === null) return undefined;
      const res = await api(`/api/projects/${appId}/extra-ports`, { method: 'PUT', body: { ports: String(typed).trim() } });
      if (res.firewallNote) toast(res.firewallNote, 'warning');
      toast('Extra ports saved. Restart the app to use them.', 'success');
      return loadIpsView(true);
    }
    case 'app-stop':
    case 'app-start':
      await api(`/api/projects/${appId}/${act === 'app-stop' ? 'stop' : 'start'}`, { method: 'POST' });
      toast(act === 'app-stop' ? 'App stopped.' : 'App started.', 'success');
      return loadIpsView(true);
    default:
      return undefined;
  }
}

document.getElementById('ips-view-body')?.addEventListener('click', async (e) => {
  const chip = e.target.closest('[data-ipx-node]');
  if (chip) {
    ipsUi.nodeId = Number(chip.dataset.ipxNode);
    ipsUi.selected.clear();
    return loadIpsView(false);
  }
  const filterBtn = e.target.closest('[data-ipx-filter]');
  if (filterBtn) {
    ipsUi.stateFilter = filterBtn.dataset.ipxFilter;
    return loadIpsView(false);
  }
  const jump = e.target.closest('[data-ipx-jump]');
  if (jump) {
    ipsUi.selected = new Set([Number(jump.dataset.ipxJump)]);
    await loadIpsView(false);
    document.querySelector(`[data-ipx-port="${jump.dataset.ipxJump}"]`)?.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
    return undefined;
  }
  const tile = e.target.closest('[data-ipx-port]');
  if (tile) {
    const port = Number(tile.dataset.ipxPort);
    const multi = ipsUi.selectMode || e.shiftKey || e.ctrlKey || e.metaKey;
    if (multi) {
      if (ipsUi.selected.has(port)) ipsUi.selected.delete(port); else ipsUi.selected.add(port);
    } else {
      ipsUi.selected = ipsUi.selected.size === 1 && ipsUi.selected.has(port) ? new Set() : new Set([port]);
    }
    return loadIpsView(false);
  }
  const btn = e.target.closest('[data-ipx-act]');
  if (!btn) return undefined;
  btn.disabled = true;
  try {
    await runIpxAction(btn.dataset.ipxAct, btn);
  } catch (err) {
    toast(err.message, 'error');
  } finally {
    btn.disabled = false;
  }
  return undefined;
});

document.getElementById('ips-view-body')?.addEventListener('toggle', (e) => {
  if (e.target.matches?.('[data-ipx-settings]')) ipsUi.settingsOpen = e.target.open;
}, true);

document.getElementById('ips-view-body')?.addEventListener('change', (e) => {
  const sel = e.target.closest('.ipx-move');
  if (!sel || !sel.value) return;
  const target = Number(sel.value);
  const appId = Number(sel.dataset.app);
  sel.value = '';
  void openMoveToNodeModal(appId, target);
});
document.getElementById('ips-search')?.addEventListener('input', () => void loadIpsView(false));

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
      <td><code>${escapeHtml(String(p.assigned_port || p.port || '-'))}</code></td>
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
