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

  const options = ['<option value="all">All Nodes</option>'];
  for (const n of nodes) {
    const label = `${escapeHtml(n.name)} (${escapeHtml(n.meshIp || 'mesh')} · ${escapeHtml(n.status)})`;
    options.push(`<option value="${n.id}">${label}</option>`);
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

      const cpuPct = Number(m.cpuPct || 0);
      const memUsed = Number(m.memUsed || 0);
      const memTotal = Number(m.memTotal || 0);
      const memPct = memTotal > 0 ? Math.round((memUsed / memTotal) * 100) : 0;
      const diskUsed = Number(m.diskUsed || 0);
      const diskTotal = Number(m.diskTotal || 0);
      const diskPct = diskTotal > 0 ? Math.round((diskUsed / diskTotal) * 100) : 0;

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
        <div class="project-card" style="display: flex; flex-direction: column; gap: 12px;">
          <div style="display: flex; justify-content: space-between; align-items: flex-start; gap: 10px;">
            <div style="min-width: 0;">
              <div style="display: flex; align-items: center; gap: 8px; flex-wrap: wrap;">
                <span class="status-dot ${statusDot}"></span>
                <strong style="font-size: 15px; color: var(--text-main);">${escapeHtml(n.name)}</strong>
                ${n.isLocal ? '<span class="badge badge-info" style="font-size: 10.5px;">Control Node</span>' : '<span class="badge badge-neutral" style="font-size: 10.5px;">Worker</span>'}
                ${statusBadge}
              </div>
              <div style="font-size: 12px; color: var(--text-muted); margin-top: 4px;">
                Region: <strong style="color: var(--text-main);">${escapeHtml(n.region || 'default')}</strong>
                &bull; Mesh <code style="font-size: 11.5px;">${escapeHtml(n.meshIp || '--')}</code>
                ${n.publicIp ? `&bull; IP <code>${escapeHtml(n.publicIp)}</code>` : ''}
              </div>
            </div>
            <div style="text-align: right; font-size: 11.5px; color: var(--text-dim); white-space: nowrap;">
              <div>v${escapeHtml(n.nodeVersion || '--')}</div>
              <div>${n.lastSeenAt ? `Seen ${formatRelativeTime(n.lastSeenAt)}` : 'Never seen'}</div>
            </div>
          </div>

          <div style="display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 10px; padding: 10px; background: var(--bg-input); border-radius: var(--radius-md); border: 1px solid var(--border-subtle);">
            <div>
              <div style="font-size: 11px; color: var(--text-dim);">CPU</div>
              <div style="font-size: 13.5px; font-weight: 600; color: var(--text-main);">${cpuPct.toFixed(1)}%</div>
            </div>
            <div>
              <div style="font-size: 11px; color: var(--text-dim);">Memory</div>
              <div style="font-size: 13.5px; font-weight: 600; color: var(--text-main);">${memTotal > 0 ? `${memPct}% (${formatBytesHuman(memUsed)})` : '--'}</div>
            </div>
            <div>
              <div style="font-size: 11px; color: var(--text-dim);">Disk / Uptime</div>
              <div style="font-size: 13.5px; font-weight: 600; color: var(--text-main);">${diskTotal > 0 ? `${diskPct}%` : '--'} &bull; ${formatUptimeShort(m.uptimeS)}</div>
            </div>
          </div>

          <div style="display: flex; flex-wrap: wrap; gap: 6px; align-items: center;">
            ${capBadges}
          </div>

          <div style="display: flex; justify-content: space-between; align-items: center; gap: 8px; padding-top: 6px; border-top: 1px solid var(--border-subtle); flex-wrap: wrap;">
            <div style="font-size: 12px; color: var(--text-muted);">
              <strong>${Number(n.projectCount || 0)}</strong> apps &bull; <strong>${Number(n.databaseCount || 0)}</strong> databases
            </div>
            <div style="display: flex; gap: 6px; flex-wrap: wrap;">
              <button class="btn btn-secondary btn-sm" onclick="setGlobalNodeFilter('${isSelectedFilter ? 'all' : n.id}')">
                ${isSelectedFilter ? 'Clear Filter' : 'Filter View'}
              </button>
              ${isMainAdmin ? `
                <button class="btn btn-secondary btn-sm" onclick="toggleNodeDrain(${n.id}, ${isDraining ? 'false' : 'true'})">
                  ${isDraining ? 'Resume' : 'Drain'}
                </button>
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
      setHtmlIfChanged(tokensEl, '<div style="color: var(--text-dim); font-size: 12.5px;">No enrollment tokens generated yet.</div>');
    } else {
      const rows = enrollTokens.slice(0, 8).map((t) => {
        let status = '<span class="badge badge-success">Active</span>';
        if (t.usedAt) status = '<span class="badge badge-neutral">Used</span>';
        else if (t.expiresAt <= nowS) status = '<span class="badge badge-danger">Expired</span>';
        return `
          <tr>
            <td><strong>${escapeHtml(t.label || '(unlabeled)')}</strong></td>
            <td>${escapeHtml(t.region || 'default')}</td>
            <td>${status}</td>
            <td>${formatRelativeTime(t.createdAt)}</td>
            <td>${t.usedNodeId ? `Node #${t.usedNodeId}` : (t.expiresAt > nowS ? `Expires ${formatRelativeTime(t.expiresAt)}` : 'Expired')}</td>
          </tr>
        `;
      }).join('');
      setHtmlIfChanged(
        tokensEl,
        `<table class="data-table"><thead><tr><th>Label</th><th>Region</th><th>Status</th><th>Created</th><th>Result / Expiry</th></tr></thead><tbody>${rows}</tbody></table>`,
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

export function openAddNodeModal() {
  const labelEl = document.getElementById('add-node-label');
  const regionEl = document.getElementById('add-node-region');
  const outEl = document.getElementById('add-node-result-box');
  if (labelEl) labelEl.value = '';
  if (regionEl) regionEl.value = 'eu-central';
  if (outEl) outEl.style.display = 'none';
  openModal('modal-add-node');
}

export async function generateNodeEnrollToken() {
  const label = (document.getElementById('add-node-label')?.value || '').trim();
  const region = (document.getElementById('add-node-region')?.value || 'default').trim();
  const ttlSeconds = Number(document.getElementById('add-node-ttl')?.value || 900);
  const btn = document.getElementById('generate-node-token-btn');

  if (btn) btn.disabled = true;
  try {
    const res = await api('/api/cluster/enroll-tokens', {
      method: 'POST',
      body: { label, region, ttlSeconds },
    });
    const outEl = document.getElementById('add-node-result-box');
    const cmdEl = document.getElementById('add-node-join-cmd');
    const tokEl = document.getElementById('add-node-raw-token');
    if (cmdEl) cmdEl.value = res.joinCommand || '';
    if (tokEl) tokEl.textContent = res.token || '';
    if (outEl) outEl.style.display = 'block';
    toast('Single-use node enrollment token generated', 'success');
    await loadNodesView(true);
  } catch (err) {
    toast(`Failed generating join token: ${err.message}`, 'error');
  } finally {
    if (btn) btn.disabled = false;
  }
}

export async function copyNodeJoinCommand() {
  const cmdEl = document.getElementById('add-node-join-cmd');
  if (!cmdEl || !cmdEl.value) return;
  try {
    await navigator.clipboard.writeText(cmdEl.value);
    toast('Join command copied to clipboard', 'success');
  } catch {
    cmdEl.select();
    document.execCommand('copy');
    toast('Join command copied', 'success');
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
  const ok = await showConfirmDialog(
    'Remove Worker Node',
    `Remove "${nodeName}" from the cluster mesh? Workloads assigned to this node must be moved or deleted first.`,
    { danger: true, confirmText: 'Remove Node' },
  );
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

window.loadNodes = loadNodes;
window.loadNodesView = loadNodesView;
window.renderNodesView = renderNodesView;
window.setGlobalNodeFilter = setGlobalNodeFilter;
window.openAddNodeModal = openAddNodeModal;
window.generateNodeEnrollToken = generateNodeEnrollToken;
window.copyNodeJoinCommand = copyNodeJoinCommand;
window.toggleNodeDrain = toggleNodeDrain;
window.removeClusterNode = removeClusterNode;
window.closeAddNodeModal = () => closeModal('modal-add-node');
