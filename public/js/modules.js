/**
 * WebDep VPS Modules & Language Runtime Readiness Subsystem
 * Displays cross-VPS installation readiness for required tools (unrar, git)
 * and language runners (Node.js, Python, Java, Go, Rust, PHP, Ruby, .NET),
 * and installs missing modules on individual VPSes or across the whole fleet.
 */

import {
  state,
  setHtmlIfChanged,
  escapeHtml,
  showConfirmDialog,
} from './state.js';
import { api, toast } from './api.js';

let modulesData = null;
let modulesFilter = 'all';
let modulesSearch = '';
const activeInstalls = new Set();
let lastInstallLogs = [];

function shortVersion(ver) {
  const s = String(ver || '').trim();
  if (!s) return 'Installed';
  // Tool banners vary ("git version 2.54.0 (Apple Git-157)", 'openjdk version "17.0.18" 2026-01-20',
  // "go version go1.26.0 darwin/arm64"): show just the version number, full text stays in the tooltip.
  const m = s.match(/\d+(?:\.\d+){1,3}/);
  if (m) return m[0];
  const cleaned = s.replace(/^"|"$/g, '');
  return cleaned.length > 16 ? `${cleaned.slice(0, 15)}…` : cleaned;
}

function setInstallBusy(key, busy) {
  if (busy) activeInstalls.add(key);
  else activeInstalls.delete(key);
}

function updateModulesSidebarBadge(data) {
  const badge = document.getElementById('badge-modules-missing');
  if (!badge) return;
  const nodes = Array.isArray(data?.nodes) ? data.nodes : [];
  const onlineNodes = nodes.filter((n) => n.isLocal || n.status === 'online' || n.status === 'draining');
  const missingTotal = onlineNodes.reduce((sum, n) => sum + Number(n.requiredMissingCount || 0), 0);
  if (missingTotal > 0) {
    badge.textContent = String(missingTotal);
    badge.style.display = '';
    badge.style.background = 'rgba(245, 158, 11, 0.22)';
    badge.style.color = 'var(--warning-light, #fbbf24)';
  } else {
    badge.style.display = 'none';
  }
}

export async function loadModulesBadge() {
  try {
    const res = await api('/api/cluster/modules');
    if (res && Array.isArray(res.nodes)) {
      modulesData = res;
      updateModulesSidebarBadge(res);
    }
  } catch (_) {}
}

export async function loadModulesView(force = false) {
  try {
    const res = await api('/api/cluster/modules', force ? { noCache: true } : {});
    if (res && Array.isArray(res.nodes)) {
      modulesData = res;
      updateModulesSidebarBadge(res);
      renderModulesView();
    }
  } catch (err) {
    console.warn('Failed loading VPS modules:', err);
  }
}

export function setModulesFilter(filter) {
  modulesFilter = filter || 'all';
  document.querySelectorAll('#modules-category-filters [data-mod-filter]').forEach((btn) => {
    btn.classList.toggle('active', btn.getAttribute('data-mod-filter') === modulesFilter);
  });
  renderModulesView();
}

export function onModulesSearchInput(val) {
  modulesSearch = String(val || '').trim().toLowerCase();
  renderModulesView();
}

function appendInstallLog(title, payload) {
  const ts = new Date().toLocaleTimeString();
  const lines = [`[${ts}] ${title}`];
  const nodeEntries = Array.isArray(payload?.nodes)
    ? payload.nodes
    : payload?.nodeId !== undefined
      ? [payload]
      : [];

  for (const n of nodeEntries) {
    const label = n.nodeName || `Node #${n.nodeId}`;
    if (n.error) {
      lines.push(`  ✗ ${label}: ${n.error}`);
      continue;
    }
    if (n.skipped && (!Array.isArray(n.results) || !n.results.length)) {
      lines.push(`  ✓ ${label}: All target modules already installed`);
      continue;
    }
    for (const r of (n.results || [])) {
      const mark = r.success ? '✓' : '✗';
      lines.push(`  ${mark} ${label} [${r.moduleId}]: ${r.message}`);
      if (r.output && String(r.output).trim()) {
        const snippet = String(r.output)
          .trim()
          .split(/\r?\n/)
          .slice(-8)
          .map((l) => `      ${l}`)
          .join('\n');
        lines.push(snippet);
      }
    }
  }
  lastInstallLogs = [lines.join('\n'), ...lastInstallLogs].slice(0, 8);
  renderModulesLogs();
}

function renderModulesLogs() {
  const el = document.getElementById('modules-install-console');
  if (!el) return;
  if (!lastInstallLogs.length) {
    el.textContent = 'No module installations run in this session yet.';
    return;
  }
  el.textContent = lastInstallLogs.join('\n\n');
}

export async function refreshNodeModules(nodeId, nodeName = '') {
  const busyKey = `refresh:${nodeId}`;
  if (activeInstalls.has(busyKey)) return;
  setInstallBusy(busyKey, true);
  renderModulesView();
  try {
    await api(`/api/cluster/nodes/${nodeId}/modules/refresh`, { method: 'POST', timeoutMs: 60000 });
    await loadModulesView(true);
    toast(`Refreshed modules for ${nodeName || `node #${nodeId}`}`, 'success');
  } catch (err) {
    toast(`Refresh failed: ${err.message}`, 'error');
  } finally {
    setInstallBusy(busyKey, false);
    renderModulesView();
  }
}

export async function installNodeModules(nodeId, moduleIds = [], label = '') {
  const key = `node:${nodeId}:${moduleIds.join(',') || 'required'}`;
  if (activeInstalls.has(key)) return;
  setInstallBusy(key, true);
  renderModulesView();
  try {
    const res = await api(`/api/cluster/nodes/${nodeId}/modules/install`, {
      method: 'POST',
      body: { moduleIds },
      timeoutMs: 600000,
    });
    appendInstallLog(label || `Install on Node #${nodeId}`, res);
    await loadModulesView(true);
    if (window.loadNodes) await window.loadNodes(true);
    if (res?.ok) {
      toast(res.skipped ? 'All target modules already installed' : `Modules installed on ${res.nodeName || 'VPS'}`, 'success');
    } else {
      toast(`Some modules on ${res?.nodeName || 'VPS'} needed attention — check install log`, 'warning');
    }
  } catch (err) {
    appendInstallLog(`Install error on Node #${nodeId}`, { nodes: [{ nodeId, error: err.message }] });
    toast(`Install failed: ${err.message}`, 'error');
  } finally {
    setInstallBusy(key, false);
    renderModulesView();
  }
}

export async function uninstallNodeModules(nodeId, moduleId, moduleName = '', nodeName = '', isRequired = false) {
  const key = `node:${nodeId}:${moduleId}`;
  if (activeInstalls.has(key)) return;
  const targetLabel = moduleName || moduleId;
  const hostLabel = nodeName || `Node #${nodeId}`;
  const ok = await showConfirmDialog({
    title: `Uninstall ${targetLabel}?`,
    message: isRequired
      ? `${targetLabel} is marked as REQUIRED on ${hostLabel}. Uninstalling it may break deployments or archive extraction on this VPS.`
      : `Remove ${targetLabel} from ${hostLabel} using the system package manager?`,
    confirmText: 'Uninstall',
    danger: true,
  });
  if (!ok) return;

  setInstallBusy(key, true);
  renderModulesView();
  try {
    const res = await api(`/api/cluster/nodes/${nodeId}/modules/uninstall`, {
      method: 'POST',
      body: { moduleIds: [moduleId] },
      timeoutMs: 600000,
    });
    appendInstallLog(`Uninstall ${targetLabel} on ${hostLabel}`, res);
    await loadModulesView(true);
    if (window.loadNodes) await window.loadNodes(true);
    if (res?.ok) {
      toast(`${targetLabel} uninstalled from ${res.nodeName || hostLabel}`, 'success');
    } else {
      toast(`Could not uninstall ${targetLabel} on ${res?.nodeName || hostLabel} — check output log`, 'warning');
    }
  } catch (err) {
    appendInstallLog(`Uninstall error on ${hostLabel}`, { nodes: [{ nodeId, error: err.message }] });
    toast(`Uninstall failed: ${err.message}`, 'error');
  } finally {
    setInstallBusy(key, false);
    renderModulesView();
  }
}

export async function installFleetModules(moduleIds = [], title = '') {
  const key = `fleet:${moduleIds.join(',') || 'required'}`;
  if (activeInstalls.has(key)) return;
  setInstallBusy(key, true);
  renderModulesView();
  try {
    toast(
      moduleIds.length ? `Installing [${moduleIds.join(', ')}] across online VPS...` : 'Installing all missing required modules across fleet...',
      'info',
    );
    const res = await api('/api/cluster/modules/install-fleet', {
      method: 'POST',
      body: { moduleIds },
      timeoutMs: 900000,
    });
    appendInstallLog(title || `Fleet Install [${moduleIds.join(', ') || 'missing required'}]`, res);
    await loadModulesView(true);
    if (window.loadNodes) await window.loadNodes(true);
    if (res?.ok) {
      toast('Fleet module installation completed!', 'success');
    } else {
      toast('Fleet installation finished with warnings — inspect log output below', 'warning');
    }
  } catch (err) {
    appendInstallLog(`Fleet Install Error [${moduleIds.join(', ') || 'missing required'}]`, {
      nodes: [{ nodeId: 0, nodeName: 'Fleet', error: err.message }],
    });
    toast(`Fleet install failed: ${err.message}`, 'error');
  } finally {
    setInstallBusy(key, false);
    renderModulesView();
  }
}

export function renderModulesView() {
  const data = modulesData;
  const matrixWrap = document.getElementById('modules-matrix-container');
  const cardsWrap = document.getElementById('modules-catalog-grid');
  if (!matrixWrap || !cardsWrap) return;

  if (!data) {
    setHtmlIfChanged(matrixWrap, '<div style="padding:24px; text-align:center; color:var(--text-dim);">Scanning cluster VPS modules...</div>');
    return;
  }

  const catalog = Array.isArray(data.catalog) ? data.catalog : [];
  const nodes = Array.isArray(data.nodes) ? data.nodes : [];
  const isMainAdmin = Boolean(state.user?.isMainAdmin);
  const onlineNodes = nodes.filter((n) => n.isLocal || n.status === 'online' || n.status === 'draining');
  const readyVpsCount = onlineNodes.filter((n) => n.allRequiredReady).length;
  const totalMissingReq = onlineNodes.reduce((sum, n) => sum + Number(n.requiredMissingCount || 0), 0);

  // Update summary cards
  const elFleet = document.getElementById('modules-stat-fleet');
  const elReady = document.getElementById('modules-stat-ready');
  const elMissing = document.getElementById('modules-stat-missing');
  const elCatalog = document.getElementById('modules-stat-catalog');
  const fleetReqBtn = document.getElementById('modules-install-all-req-btn');
  if (elFleet) elFleet.textContent = `${onlineNodes.length} / ${nodes.length}`;
  if (elReady) elReady.textContent = `${readyVpsCount} / ${onlineNodes.length}`;
  if (elMissing) {
    elMissing.textContent = String(totalMissingReq);
    elMissing.style.color = totalMissingReq > 0 ? 'var(--warning-light, #fbbf24)' : 'var(--success-light, #34d399)';
  }
  if (elCatalog) elCatalog.textContent = String(catalog.length);
  if (fleetReqBtn) {
    const busy = activeInstalls.has('fleet:required');
    fleetReqBtn.style.display = isMainAdmin && totalMissingReq > 0 ? '' : 'none';
    fleetReqBtn.disabled = busy;
    fleetReqBtn.textContent = busy
      ? 'Installing Missing Required...'
      : `Install All Missing Required (${totalMissingReq})`;
  }

  // Filter catalog entries
  const filteredCatalog = catalog.filter((entry) => {
    if (modulesFilter === 'required' && !entry.required) return false;
    if (modulesFilter === 'language' && entry.category !== 'language') return false;
    if (modulesFilter === 'archive' && entry.category === 'language') return false;
    if (modulesSearch) {
      const hay = `${entry.id} ${entry.name} ${entry.description} ${(entry.languages || []).join(' ')}`.toLowerCase();
      if (!hay.includes(modulesSearch)) return false;
    }
    return true;
  });

  // 1. VPS × Module Readiness Matrix (responsive wrapping grid per node)
  if (!nodes.length) {
    setHtmlIfChanged(matrixWrap, '<div style="padding:24px; text-align:center; color:var(--text-dim);">No VPS nodes registered yet.</div>');
  } else {
    const nodeCardsHtml = nodes
      .map((n) => {
        const isReachable = Boolean(n.isLocal || n.status === 'online' || n.status === 'draining');
        const statusBadge = isReachable
          ? '<span class="badge badge-success" style="font-size:10px;">Online</span>'
          : `<span class="badge badge-danger" style="font-size:10px;">${escapeHtml(n.status || 'Offline')}</span>`;
        const reqMissing = Number(n.requiredMissingCount || 0);
        const rowBusyReq = activeInstalls.has(`node:${n.nodeId}:required`);
        const rowBusyRef = activeInstalls.has(`refresh:${n.nodeId}`);

        const modTiles = filteredCatalog.length
          ? filteredCatalog
              .map((entry) => {
                const st = (n.modules || []).find((m) => m.id === entry.id) || {
                  id: entry.id,
                  installed: false,
                  version: '',
                  details: 'Not scanned',
                };
                const cellBusy = activeInstalls.has(`node:${n.nodeId}:${entry.id}`);
                const tileState = st.installed
                  ? 'is-installed'
                  : (entry.required ? 'is-missing-req' : 'is-missing-opt');

                let statusHtml = '';
                if (st.installed) {
                  const uninstallBtn = isMainAdmin && isReachable
                    ? `
                      <button
                        type="button"
                        class="btn btn-secondary btn-sm modules-uninstall-btn"
                        title="Uninstall ${escapeHtml(entry.name)} from ${escapeHtml(n.name)}"
                        ${cellBusy ? 'disabled' : ''}
                        onclick="window.uninstallNodeModules(${n.nodeId}, '${entry.id}', '${escapeHtml(entry.name)}', '${escapeHtml(n.name)}', ${Boolean(entry.required)})"
                      >
                        ${cellBusy ? 'Removing…' : 'Uninstall'}
                      </button>
                    `
                    : '';
                  statusHtml = `
                    <span class="badge badge-success module-version" title="${escapeHtml(`${st.version}${st.details ? ` — ${st.details}` : ''}`)}">
                      ✓ ${escapeHtml(shortVersion(st.version))}
                    </span>
                    ${uninstallBtn}
                  `;
                } else if (isMainAdmin && isReachable) {
                  const toneClass = entry.required ? 'btn-primary' : 'btn-secondary';
                  statusHtml = `
                    <button
                      type="button"
                      class="btn ${toneClass} btn-sm"
                      title="${escapeHtml(st.details || `Install ${entry.name} on ${n.name}`)}"
                      ${cellBusy ? 'disabled' : ''}
                      onclick="window.installNodeModules(${n.nodeId}, ['${entry.id}'], 'Install ${escapeHtml(entry.name)} on ${escapeHtml(n.name)}')"
                    >
                      ${cellBusy ? 'Installing…' : 'Install'}
                    </button>
                  `;
                } else {
                  statusHtml = `
                    <span class="badge ${entry.required ? 'badge-warning' : 'badge-neutral'}" style="font-size:10.5px;" title="${escapeHtml(st.details || 'Missing')}">
                      Missing
                    </span>
                  `;
                }

                return `
                  <div class="modules-tile ${tileState}">
                    <div class="modules-tile-top">
                      <span class="modules-tile-name" title="${escapeHtml(entry.name)}">${escapeHtml(entry.name.split(' ')[0])}</span>
                      ${entry.required ? '<span class="badge badge-info" style="font-size:9.5px; padding:1px 5px; flex-shrink:0;">REQ</span>' : ''}
                    </div>
                    <div class="modules-tile-status">${statusHtml}</div>
                  </div>
                `;
              })
              .join('')
          : '<div style="grid-column:1 / -1; padding:14px; text-align:center; color:var(--text-dim); font-size:12px;">No modules match this filter.</div>';

        return `
          <div class="modules-node-card">
            <div class="modules-node-header">
              <div>
                <div style="display:flex; align-items:center; gap:6px; flex-wrap:wrap;">
                  <strong style="color:var(--text-main); font-size:14px;">${escapeHtml(n.name)}</strong>
                  ${n.isLocal ? '<span class="badge badge-info" style="font-size:9.5px;">Core</span>' : ''}
                  ${statusBadge}
                </div>
                <div class="modules-node-meta">
                  <span style="font-family:var(--font-mono);">${escapeHtml(n.os || '?')}/${escapeHtml(n.arch || '?')} &bull; pkg: ${escapeHtml(n.packageManager || 'none')}</span>
                  <span style="color:${reqMissing > 0 ? 'var(--warning-light, #fbbf24)' : 'var(--success-light, #34d399)'}; font-weight:500;">
                    ${reqMissing > 0 ? `${reqMissing} required missing` : 'All required ready'} (${n.installedCount}/${n.totalCount})
                  </span>
                </div>
              </div>
              <div class="modules-node-actions">
                ${isMainAdmin && isReachable && reqMissing > 0 ? `
                  <button
                    type="button"
                    class="btn btn-primary btn-sm"
                    ${rowBusyReq ? 'disabled' : ''}
                    onclick="window.installNodeModules(${n.nodeId}, [], 'Install missing required on ${escapeHtml(n.name)}')"
                  >
                    ${rowBusyReq ? 'Installing...' : `Fix Required (${reqMissing})`}
                  </button>
                ` : ''}
                ${isReachable ? `
                  <button
                    type="button"
                    class="btn btn-secondary btn-sm"
                    ${rowBusyRef ? 'disabled' : ''}
                    onclick="window.refreshNodeModules(${n.nodeId}, '${escapeHtml(n.name)}')"
                  >
                    ${rowBusyRef ? '...' : 'Scan'}
                  </button>
                ` : ''}
              </div>
            </div>
            <div class="modules-node-grid">${modTiles}</div>
          </div>
        `;
      })
      .join('');

    setHtmlIfChanged(matrixWrap, `<div class="modules-fleet-list">${nodeCardsHtml}</div>`);
  }

  // 2. Module & Language Cards Grid
  if (!filteredCatalog.length) {
    setHtmlIfChanged(cardsWrap, '<div style="grid-column:1 / -1; padding:32px; text-align:center; color:var(--text-dim);">No modules match this filter.</div>');
  } else {
    const cardsHtml = filteredCatalog
      .map((entry) => {
        const installedOn = onlineNodes.filter((n) => (n.modules || []).some((m) => m.id === entry.id && m.installed));
        const missingOn = onlineNodes.filter((n) => !(n.modules || []).some((m) => m.id === entry.id && m.installed));
        const covPct = onlineNodes.length > 0 ? Math.round((installedOn.length / onlineNodes.length) * 100) : 0;
        const barTone = missingOn.length === 0 ? 'green' : (entry.required ? 'rose' : 'amber');
        const fleetKey = `fleet:${entry.id}`;
        const fleetBusy = activeInstalls.has(fleetKey);

        const langTags = (entry.languages || [])
          .map((l) => `<span class="badge badge-neutral" style="font-size:10px;">${escapeHtml(l)}</span>`)
          .join(' ');

        const perNodePills = nodes
          .map((n) => {
            const st = (n.modules || []).find((m) => m.id === entry.id);
            const ok = Boolean(st?.installed);
            const reachable = Boolean(n.isLocal || n.status === 'online' || n.status === 'draining');
            if (ok) {
              return `<span class="badge badge-success" style="font-size:10.5px;" title="${escapeHtml(st?.version || '')}">✓ ${escapeHtml(n.name)}</span>`;
            }
            if (isMainAdmin && reachable) {
              const btnBusy = activeInstalls.has(`node:${n.nodeId}:${entry.id}`);
              return `
                <button
                  type="button"
                  class="btn btn-secondary btn-sm"
                  style="padding: 2px 7px; font-size: 10.5px; border-color: rgba(245, 158, 11, 0.45);"
                  ${btnBusy ? 'disabled' : ''}
                  onclick="window.installNodeModules(${n.nodeId}, ['${entry.id}'], 'Install ${escapeHtml(entry.name)} on ${escapeHtml(n.name)}')"
                >
                  ${btnBusy ? '...' : `+ ${escapeHtml(n.name)}`}
                </button>
              `;
            }
            return `<span class="badge badge-warning" style="font-size:10.5px;">✗ ${escapeHtml(n.name)}</span>`;
          })
          .join(' ');

        return `
          <div class="card" style="margin:0; display:flex; flex-direction:column;">
            <div class="card-body" style="display:flex; flex-direction:column; gap:10px; flex:1;">
              <div style="display:flex; justify-content:space-between; align-items:flex-start; gap:8px;">
                <div>
                  <div style="display:flex; align-items:center; gap:6px; flex-wrap:wrap;">
                    <strong style="font-size:14.5px; color:var(--text-main);">${escapeHtml(entry.name)}</strong>
                    ${entry.required
                      ? '<span class="badge badge-info" style="font-size:10px;">Required on All VPS</span>'
                      : `<span class="badge badge-neutral" style="font-size:10px;">${escapeHtml(entry.category)}</span>`}
                  </div>
                  <div style="font-size:12px; color:var(--text-muted); margin-top:4px; line-height:1.45;">
                    ${escapeHtml(entry.description)}
                  </div>
                </div>
              </div>

              <div style="display:flex; flex-wrap:wrap; gap:4px;">${langTags}</div>

              <div>
                <div style="display:flex; justify-content:space-between; font-size:11.5px; color:var(--text-dim); margin-bottom:4px;">
                  <span>Fleet Coverage</span>
                  <strong style="color:var(--text-main);">${installedOn.length} / ${onlineNodes.length} online VPS</strong>
                </div>
                <div class="progress-bar"><div class="progress-fill ${barTone}" style="width:${covPct}%;"></div></div>
              </div>

              <div style="display:flex; flex-wrap:wrap; gap:5px; align-items:center;">
                ${perNodePills}
              </div>

              ${isMainAdmin && missingOn.length > 0 ? `
                <div style="margin-top:auto; padding-top:6px;">
                  <button
                    type="button"
                    class="btn ${entry.required ? 'btn-primary' : 'btn-secondary'} btn-sm"
                    style="width:100%;"
                    ${fleetBusy ? 'disabled' : ''}
                    onclick="window.installFleetModules(['${entry.id}'], 'Install ${escapeHtml(entry.name)} on all missing VPS')"
                  >
                    ${fleetBusy ? 'Installing Across Fleet...' : `Install on All Missing VPS (${missingOn.length})`}
                  </button>
                </div>
              ` : ''}
            </div>
          </div>
        `;
      })
      .join('');

    setHtmlIfChanged(cardsWrap, cardsHtml);
  }

  renderModulesLogs();
}

window.loadModulesView = loadModulesView;
window.loadModulesBadge = loadModulesBadge;
window.setModulesFilter = setModulesFilter;
window.onModulesSearchInput = onModulesSearchInput;
window.refreshNodeModules = refreshNodeModules;
window.installNodeModules = installNodeModules;
window.uninstallNodeModules = uninstallNodeModules;
window.installFleetModules = installFleetModules;
