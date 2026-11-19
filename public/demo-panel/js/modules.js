/**
 * WebDep VPS Modules & Language Runtime Readiness Subsystem
 * Displays cross-VPS installation readiness for required tools (unrar, git)
 * and language runners (Node.js, Python, Java, Go, Rust, PHP, Ruby, .NET),
 * and installs or removes modules on each VPS. Nothing is shown as "missing": operators install what they need.
 */

import {
  state,
  setHtmlIfChanged,
  escapeHtml,
  showConfirmDialog,
  nodeLabel,
} from './state.js';
import { api, toast } from './api.js';

let modulesData = null;
const activeInstalls = new Set();
let lastInstallLogs = [];

const isDigit = (c) => c >= '0' && c <= '9';

// First "<digits>.<digit>..." run in the text, cut to at most 4 parts. A plain scan instead of a
// regex keeps it linear-time on any input.
function findVersionNumber(s) {
  for (let i = 0; i < s.length; i++) {
    if (!isDigit(s[i])) continue;
    let end = i;
    while (end < s.length && (isDigit(s[end]) || s[end] === '.')) end++;
    const run = s.slice(i, end);
    const dot = run.indexOf('.');
    if (dot > 0 && isDigit(run[dot + 1] || '')) {
      return run.split('.').filter(Boolean).slice(0, 4).join('.');
    }
  }
  return null;
}

// Modules API rows carry the node's own name; show the operator's display name when the node is known.
function moduleNodeLabel(n) {
  const node = (state.nodes || []).find((x) => Number(x.id) === Number(n.nodeId));
  return node ? nodeLabel(node) : (n.name || `Node #${n.nodeId}`);
}

function shortVersion(ver) {
  const s = String(ver || '').trim();
  if (!s) return 'Installed';
  // Tool banners vary ("git version 2.54.0 (Apple Git-157)", 'openjdk version "17.0.18" 2026-01-20',
  // "go version go1.26.0 darwin/arm64"): show just the version number, full text stays in the tooltip.
  const version = findVersionNumber(s);
  if (version) return version;
  const cleaned = s.replace(/^"|"$/g, '');
  return cleaned.length > 16 ? `${cleaned.slice(0, 15)}…` : cleaned;
}

function setInstallBusy(key, busy) {
  if (busy) activeInstalls.add(key);
  else activeInstalls.delete(key);
}

export async function loadModulesView(force = false) {
  try {
    const res = await api('/api/cluster/modules', force ? { noCache: true } : {});
    if (res && Array.isArray(res.nodes)) {
      modulesData = res;
      renderModulesView();
    }
  } catch (err) {
    console.warn('Failed loading VPS modules:', err);
  }
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

export async function uninstallNodeModules(nodeId, moduleId, moduleName = '', nodeName = '') {
  const key = `node:${nodeId}:${moduleId}`;
  if (activeInstalls.has(key)) return;
  const targetLabel = moduleName || moduleId;
  const hostLabel = nodeName || `Node #${nodeId}`;
  const ok = await showConfirmDialog({
    title: `Remove ${targetLabel}?`,
    message: `Remove ${targetLabel} from ${hostLabel}? Apps on this VPS that use it will stop working until it is installed again.`,
    confirmText: 'Remove',
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

export function renderModulesView() {
  const data = modulesData;
  const wrap = document.getElementById('modules-matrix-container');
  if (!wrap) return;

  if (!data) {
    setHtmlIfChanged(wrap, '<div class="card" style="padding:24px; text-align:center; color:var(--text-dim);">Scanning VPS modules...</div>');
    return;
  }

  const catalog = Array.isArray(data.catalog) ? data.catalog : [];
  const nodes = Array.isArray(data.nodes) ? data.nodes : [];
  const isMainAdmin = Boolean(state.user?.isMainAdmin);

  if (!nodes.length) {
    setHtmlIfChanged(wrap, '<div class="card" style="padding:24px; text-align:center; color:var(--text-dim);">No VPS nodes registered yet.</div>');
    renderModulesLogs();
    return;
  }

  const nodeCardsHtml = nodes
    .map((n) => {
      const isReachable = Boolean(n.isLocal || n.status === 'online' || n.status === 'draining');
      const label = moduleNodeLabel(n);
      const scanBusy = activeInstalls.has(`refresh:${n.nodeId}`);

      const tiles = catalog
        .map((entry) => {
          const st = (n.modules || []).find((m) => m.id === entry.id) || { installed: false, version: '' };
          const busy = activeInstalls.has(`node:${n.nodeId}:${entry.id}`);
          let action = '';
          if (st.installed) {
            action = `
              <span class="badge badge-success module-version" title="${escapeHtml(st.version || '')}">${escapeHtml(shortVersion(st.version))}</span>
              ${isMainAdmin && isReachable ? `
                <button type="button" class="btn btn-secondary btn-sm modules-uninstall-btn" ${busy ? 'disabled' : ''}
                  onclick="window.uninstallNodeModules(${n.nodeId}, '${entry.id}', '${escapeHtml(entry.name)}', '${escapeHtml(label)}')">
                  ${busy ? 'Removing…' : 'Remove'}
                </button>` : ''}
            `;
          } else if (isMainAdmin && isReachable) {
            action = `
              <button type="button" class="btn btn-secondary btn-sm" ${busy ? 'disabled' : ''}
                onclick="window.installNodeModules(${n.nodeId}, ['${entry.id}'], 'Install ${escapeHtml(entry.name)} on ${escapeHtml(label)}')">
                ${busy ? 'Installing…' : 'Install'}
              </button>`;
          } else {
            action = '<span class="modules-not-installed">Not installed</span>';
          }
          return `
            <div class="modules-tile${st.installed ? ' is-installed' : ''}" title="${escapeHtml(entry.description || entry.name)}">
              <div class="modules-tile-top">
                <span class="modules-tile-name">${escapeHtml(entry.name.split(' ')[0])}</span>
              </div>
              <div class="modules-tile-status">${action}</div>
            </div>
          `;
        })
        .join('');

      return `
        <div class="card modules-node-card">
          <div class="modules-node-header">
            <div style="display:flex; align-items:center; gap:8px; flex-wrap:wrap;">
              <span class="status-dot ${isReachable ? 'online' : 'offline'}"></span>
              <strong style="color:var(--text-main); font-size:14px;">${escapeHtml(label)}</strong>
              <span class="modules-node-meta" style="font-family:var(--font-mono);">${escapeHtml(n.os || '?')}/${escapeHtml(n.arch || '?')}</span>
            </div>
            ${isReachable ? `
              <button type="button" class="btn btn-secondary btn-sm" ${scanBusy ? 'disabled' : ''}
                onclick="window.refreshNodeModules(${n.nodeId}, '${escapeHtml(label)}')">${scanBusy ? '...' : 'Rescan'}</button>
            ` : '<span class="badge badge-danger" style="font-size:10px;">Offline</span>'}
          </div>
          <div class="modules-node-grid">${tiles}</div>
        </div>
      `;
    })
    .join('');

  setHtmlIfChanged(wrap, `<div class="modules-fleet-list">${nodeCardsHtml}</div>`);
  renderModulesLogs();
}

function renderModulesLogs() {
  const card = document.getElementById('modules-install-log-card');
  const el = document.getElementById('modules-install-console');
  if (card) card.style.display = lastInstallLogs.length ? '' : 'none';
  if (el) el.textContent = lastInstallLogs.join('\n\n');
}

window.loadModulesView = loadModulesView;
window.refreshNodeModules = refreshNodeModules;
window.installNodeModules = installNodeModules;
window.uninstallNodeModules = uninstallNodeModules;
