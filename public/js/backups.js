/**
 * WebDep Backups tab
 * Manual backups for websites, services and databases: create, restore, download, lock, delete.
 */

import {
  state,
  getSelectedProject,
  getSelectedDatabase,
  escapeHtml,
  formatDate,
  formatRelativeTime,
  setHtmlIfChanged,
  openModal,
  closeModal,
  showConfirmDialog,
  showPromptDialog,
  nodeLabel,
} from './state.js';
import { api, toast } from './api.js';

/** The app whose Backups tab is open: a website or service (project) or a database. */
function currentTarget() {
  if (state.selectedEntityKind === 'database') {
    const d = getSelectedDatabase();
    return d ? { kind: 'database', id: d.id, name: d.name, base: `/api/databases/${d.id}`, app: d } : null;
  }
  const p = getSelectedProject();
  return p ? { kind: 'project', id: p.id, name: p.name, base: `/api/projects/${p.id}`, app: p } : null;
}

function targetFor(kind, id) {
  if (kind === 'database') {
    const d = (state.databases || []).find((x) => Number(x.id) === Number(id));
    return d ? { kind, id: d.id, name: d.name, base: `/api/databases/${d.id}`, app: d } : { kind, id, name: 'this database', base: `/api/databases/${id}`, app: {} };
  }
  const p = (state.projects || []).find((x) => Number(x.id) === Number(id));
  return p ? { kind, id: p.id, name: p.name, base: `/api/projects/${p.id}`, app: p } : { kind, id, name: 'this app', base: `/api/projects/${id}`, app: {} };
}

const keyOf = (t) => `${t.kind}:${t.id}`;

function canManageBackups(t) {
  if (t.kind === 'database') return typeof window.hasAnyPower === 'function' && window.hasAnyPower(['backups', 'databases_manage']);
  return window.hasPower('backups');
}

function formatBackupSize(bytes) {
  const b = Number(bytes || 0);
  if (b < 1024) return `${b} B`;
  if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)} KB`;
  return `${(b / (1024 * 1024)).toFixed(2)} MB`;
}

let backupPollTimer = null;
const lastBackupStatus = new Map();
const lastRestoreState = new Map();
const backupsById = new Map();

const BK_ICONS = {
  restore: '<path d="M3 12a9 9 0 1 0 3-6.7L3 8"/><path d="M3 3v5h5"/><path d="M12 7v5l3 3"/>',
  download: '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="m7 10 5 5 5-5"/><path d="M12 15V3"/>',
  lock: '<rect x="3" y="11" width="18" height="11" rx="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/>',
  unlock: '<rect x="3" y="11" width="18" height="11" rx="2"/><path d="M7 11V7a5 5 0 0 1 9.9-1"/>',
  trash: '<path d="M3 6h18"/><path d="M19 6v14c0 1-1 2-2 2H7c-1 0-2-1-2-2V6"/><path d="M8 6V4c0-1 1-2 2-2h4c1 0 2 1 2 2v2"/><line x1="10" x2="10" y1="11" y2="17"/><line x1="14" x2="14" y1="11" y2="17"/>',
};

function bkButton(icon, cls, label, onclick, disabled = false) {
  const svg = `<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${BK_ICONS[icon]}</svg>`;
  return `<button type="button" class="btn btn-sm btn-icon bk-btn ${cls}" title="${escapeHtml(label)}" aria-label="${escapeHtml(label)}" onclick="${onclick}" ${disabled ? 'disabled' : ''}>${svg}</button>`;
}

function appIsOnCoreServer(p) {
  const local = (state.nodes || []).find((n) => n.isLocal);
  return !p.node_id || (local && Number(p.node_id) === Number(local.id));
}

function backupStatusBadge(b) {
  if (b.status === 'running') return '<span class="badge badge-warning"><span class="btn-spinner"></span> Creating</span>';
  if (b.status === 'failed') return `<span class="badge badge-danger" title="${escapeHtml(b.error || 'The backup failed')}">Failed</span>${b.error ? `<div class="bk-error">${escapeHtml(b.error)}</div>` : ''}`;
  return '<span class="badge badge-success">Complete</span>';
}

/** "3 of 5 backups, 190 MB" above the table, and the Create button when the limit is reached. */
function updateBackupsHeader(t, backups, limit) {
  const held = backups.filter((b) => b.status !== 'failed');
  const bytes = held.filter((b) => b.status === 'complete').reduce((n, b) => n + (b.size || 0), 0);
  const count = limit > 0 ? `${held.length} of ${limit} backups` : `${held.length} backup${held.length === 1 ? '' : 's'}`;
  const usage = document.getElementById('backups-usage');
  if (usage) usage.textContent = `${count}${bytes ? ` · ${formatBackupSize(bytes)}` : ''}`;
  const btn = document.getElementById('create-backup-snapshot-btn');
  if (btn) {
    const atLimit = limit > 0 && held.length >= limit;
    btn.hidden = !canManageBackups(t);
    btn.disabled = atLimit;
    btn.title = atLimit ? 'This app holds its limit of backups. Delete one first or ask an admin to raise the limit.' : '';
  }
}

/** Shows "Restoring..." while a restore runs; reports how it ended. Returns true while running. */
async function refreshRestoreBanner(t) {
  const banner = document.getElementById('backups-restore-banner');
  let status = null;
  try {
    status = await api(`${t.base}/backups/restore-status`, { noCache: true });
  } catch (_) { /* leave as is */ }
  const running = status?.state === 'running';
  if (banner) {
    banner.hidden = !running;
    if (running) {
      banner.textContent = t.kind === 'database'
        ? `Restoring backup #${status.backupId}. The database's current contents are being replaced.`
        : `Restoring backup #${status.backupId}. The app is stopped until it finishes.`;
    }
  }
  const k = keyOf(t);
  if (lastRestoreState.get(k) === 'running' && status && status.state !== 'running') {
    if (status.state === 'done') toast(status.message || 'Restore finished.', 'success');
    else toast(`Restore failed: ${status.message}`, 'error');
    if (t.kind === 'project' && typeof window.refreshProjects === 'function') void window.refreshProjects();
  }
  lastRestoreState.set(k, status?.state ?? null);
  return running;
}

/** Toasts when a backup this page was waiting on finishes, and keeps refreshing while anything runs. */
function trackBackupProgress(t, backups, restoring) {
  clearTimeout(backupPollTimer);
  for (const b of backups) {
    const key = `${keyOf(t)}:${b.id}`;
    const was = lastBackupStatus.get(key);
    if (was === 'running' && b.status === 'complete') toast(`Backup #${b.id} is complete.`, 'success');
    if (was === 'running' && b.status === 'failed') toast(`Backup #${b.id} failed: ${b.error || 'unknown error'}`, 'error');
    lastBackupStatus.set(key, b.status);
  }
  if (restoring || backups.some((b) => b.status === 'running')) {
    backupPollTimer = setTimeout(() => {
      const now = currentTarget();
      if (state.currentView === 'backups' && now && keyOf(now) === keyOf(t)) void loadBackupsTable(t);
    }, 2000);
  }
}

async function fetchBackups(t) {
  const res = await api(`${t.base}/backups`, { noCache: true });
  if (t.kind === 'database') return { backups: res.items || [], limit: Number(res.limit) || 0 };
  return { backups: Array.isArray(res) ? res : [], limit: Number(t.app.backup_limit) || 0 };
}

async function loadBackupsTable(t) {
  const tbody = document.getElementById('backups-table-body');
  if (!tbody || !t) return;
  try {
    const restoring = await refreshRestoreBanner(t);
    const { backups, limit } = await fetchBackups(t);
    backupsById.clear();
    backups.forEach((b) => backupsById.set(`${keyOf(t)}:${b.id}`, b));
    updateBackupsHeader(t, backups, limit);
    if (!backups.length) {
      setHtmlIfChanged(tbody, '<tr><td colspan="6" style="text-align: center; padding: 20px;">No backups yet. Create one to be able to restore this app later.</td></tr>');
      return;
    }
    trackBackupProgress(t, backups, restoring);
    const canManage = canManageBackups(t);
    const isProject = t.kind === 'project';
    const onlineNodes = (state.nodes || []).filter((n) => n.status === 'online');
    const html = backups.map((b) => {
      const isOffNode = isProject && b.storageMode === 'off-node';
      const done = b.status === 'complete';
      const downloadable = done && (isProject ? b.format === 'archive' && appIsOnCoreServer(t.app) : true);
      const storageBadge = isOffNode
        ? '<span class="badge badge-info">Off-Node (Core)</span>'
        : '<span class="badge badge-neutral">Local</span>';
      const nodeOptions = onlineNodes.length
        ? onlineNodes.map((n) =>
            `<option value="${n.id}" ${Number(n.id) === Number(t.app.node_id) ? 'selected' : ''}>${escapeHtml(nodeLabel(n))}</option>`
          ).join('')
        : '<option value="">Current Node</option>';
      const ref = `'${t.kind}', ${t.id}, ${b.id}`;
      const actions = canManage ? [
        isOffNode && onlineNodes.length && state.user?.isAdmin
          ? `<select id="backup-restore-node-${b.id}" class="form-control bk-node" aria-label="Restore onto node">${nodeOptions}</select>` : '',
        done ? bkButton('restore', 'bk-restore', `Restore backup #${b.id}`, `restoreBackup(${ref}, ${isOffNode})`, restoring) : '',
        downloadable ? bkButton('download', 'bk-download', `Download backup #${b.id}`, `downloadBackup(${ref})`) : '',
        bkButton(b.locked ? 'lock' : 'unlock', `bk-lock ${b.locked ? 'is-locked' : ''}`, b.locked ? 'Unlock: allow deleting' : 'Lock: prevent deleting', `toggleBackupLock(${ref}, ${!b.locked})`, b.status === 'running'),
        bkButton('trash', 'bk-delete', b.locked ? 'Locked: unlock it to delete' : `Delete backup #${b.id}`, `deleteBackup(${ref})`, b.status === 'running' || b.locked),
      ].join('') : '';
      return `
        <tr>
          <td class="bk-name"><strong>${escapeHtml(b.note || `Backup #${b.id}`)}</strong>
            <small>#${b.id} &middot; ${escapeHtml(b.kind || '')}${b.createdBy ? ` &middot; by ${escapeHtml(b.createdBy)}` : ''}${b.locked ? ' &middot; locked' : ''}</small></td>
          <td>${backupStatusBadge(b)}</td>
          <td>${storageBadge}</td>
          <td>${b.status === 'running' ? '-' : `${formatBackupSize(b.size)}${b.fileCount ? `<small class="bk-sub">${b.fileCount} files</small>` : ''}`}</td>
          <td class="bk-created" title="${escapeHtml(formatDate(b.ts))}">${escapeHtml(formatDate(b.ts))}<small class="bk-sub">${escapeHtml(formatRelativeTime(b.ts))}</small></td>
          <td><div class="bk-actions">${actions}</div></td>
        </tr>
      `;
    }).join('');
    setHtmlIfChanged(tbody, html);
  } catch (err) {
    setHtmlIfChanged(tbody, `<tr><td colspan="6" style="text-align: center; color: var(--danger); padding: 20px;">${escapeHtml(err.message)}</td></tr>`);
  }
}

/** Backups tab entry point for websites, services and databases. */
async function loadBackupsView() {
  const t = currentTarget();
  const sub = document.getElementById('backups-view-subtitle');
  if (sub) {
    sub.textContent = t?.kind === 'database'
      ? 'Manual backups of this database. Restoring replaces all of its data with the backup.'
      : 'Manual backups of this app. Restoring replaces its files and settings with the backup.';
  }
  if (t) await loadBackupsTable(t);
}

function openCreateBackupModal() {
  const t = currentTarget();
  if (!t) {
    toast('Select an app first.', 'warning');
    return;
  }
  const isProject = t.kind === 'project';
  document.getElementById('backup-note').value = '';
  document.getElementById('backup-ignore').value = '';
  const ignoreWrap = document.getElementById('backup-ignore-wrap');
  if (ignoreWrap) ignoreWrap.hidden = !isProject;
  const sub = document.getElementById('backup-modal-subtitle');
  if (sub) {
    sub.textContent = isProject
      ? "Saves every file in the app's Files folder and its environment variables."
      : 'Saves all tables and data of this database. The database keeps running while it is saved.';
  }
  const wrap = document.getElementById('backup-offnode-wrap');
  if (wrap) wrap.hidden = !isProject || appIsOnCoreServer(t.app);
  openModal('modal-create-backup');
}

async function submitCreateBackup() {
  const t = currentTarget();
  if (!t) return;
  const isProject = t.kind === 'project';
  const btn = document.getElementById('backup-create-confirm');
  const note = document.getElementById('backup-note').value.trim();
  const ignore = document.getElementById('backup-ignore').value.split(/[\n,]/).map((x) => x.trim()).filter(Boolean);
  const offNode = isProject && !appIsOnCoreServer(t.app) && Boolean(document.getElementById('backup-offnode-checkbox')?.checked);
  if (btn) btn.disabled = true;
  try {
    const body = isProject
      ? { kind: 'full-snapshot', note: note || (offNode ? 'Off-node archive snapshot' : 'Manual backup'), offNode, ignore }
      : { note: note || 'Manual backup' };
    const res = await api(`${t.base}/backup`, { method: 'POST', body });
    closeModal('modal-create-backup');
    toast(res.status === 'running' ? `Backup #${res.backupId} started. It keeps running in the background.` : `Created ${res.storageMode || 'local'} backup #${res.backupId}`, 'success');
    lastBackupStatus.set(`${keyOf(t)}:${res.backupId}`, res.status || 'complete');
    await loadBackupsTable(t);
  } catch (err) {
    toast(`Backup failed: ${err.message}`, 'error');
  } finally {
    if (btn) btn.disabled = false;
  }
}

async function restoreBackup(kind, id, backupId, isOffNode) {
  const t = targetFor(kind, id);
  const sel = document.getElementById(`backup-restore-node-${backupId}`);
  const targetNodeId = isOffNode && sel && sel.value ? Number(sel.value) : null;
  const b = backupsById.get(`${keyOf(t)}:${backupId}`);

  if (!targetNodeId && (kind === 'database' || b?.format === 'archive')) {
    const what = kind === 'database'
      ? `will replace ALL data in "${t.name}". Tables and rows added since then are removed. If anything goes wrong during the restore, nothing is changed.`
      : `will replace ALL current files of "${t.name}". Files added or changed since then are removed; the current files are kept for 24 hours in case you need them back.`;
    const typed = await showPromptDialog({
      title: `Restore backup #${backupId}`,
      message: `Backup #${backupId} (${formatDate(b?.ts)}${b?.note ? `, "${b.note}"` : ''}) ${what}`,
      placeholder: kind === 'database' ? 'Type the database name to confirm' : 'Type the app name to confirm',
      confirmText: 'Restore backup',
      danger: true,
    });
    if (typed === null) return;
    if (typed.trim() !== t.name) {
      toast("The name didn't match, so nothing was restored.", 'warning');
      return;
    }
  } else {
    const ok = await showConfirmDialog({
      title: `Restore Backup #${backupId}`,
      message: targetNodeId
        ? `Restore off-node backup #${backupId} onto Node #${targetNodeId}?`
        : `Restore backup #${backupId} on the active node?`,
      confirmText: 'Restore Backup',
      danger: true,
    });
    if (!ok) return;
  }
  try {
    const body = targetNodeId ? { backupId, targetNodeId } : { backupId };
    const res = await api(`${t.base}/backups/restore`, { method: 'POST', body });
    if (res && res.status === 'running') {
      toast(kind === 'database' ? `Restoring backup #${backupId}.` : `Restoring backup #${backupId}. The app is stopped until it finishes.`, 'info');
      lastRestoreState.set(keyOf(t), 'running');
      await loadBackupsTable(t);
      return;
    }
    toast(`Restored backup #${backupId}`, 'success');
    if (typeof window.refreshProjects === 'function') await window.refreshProjects();
    await loadBackupsTable(t);
  } catch (err) {
    toast(`Restore failed: ${err.message}`, 'error');
  }
}

function downloadBackup(kind, id, backupId) {
  // A normal navigation, so the browser streams the file to disk instead of holding it in memory.
  const a = document.createElement('a');
  a.href = `${targetFor(kind, id).base}/backups/${backupId}/download`;
  a.download = '';
  document.body.appendChild(a);
  a.click();
  a.remove();
}

async function toggleBackupLock(kind, id, backupId, locked) {
  const t = targetFor(kind, id);
  try {
    await api(`${t.base}/backups/${backupId}/lock`, { method: 'PUT', body: { locked } });
    toast(locked ? `Backup #${backupId} locked.` : `Backup #${backupId} unlocked.`, 'info');
    await loadBackupsTable(t);
  } catch (err) {
    toast(err.message, 'error');
  }
}

async function deleteBackup(kind, id, backupId) {
  const t = targetFor(kind, id);
  const ok = await showConfirmDialog({
    title: `Delete Backup #${backupId}`,
    message: `Remove backup #${backupId} permanently? This also deletes its file.`,
    confirmText: 'Delete Backup',
    danger: true,
  });
  if (!ok) return;
  try {
    await api(`${t.base}/backups/${backupId}`, { method: 'DELETE' });
    toast(`Deleted backup #${backupId}`, 'info');
    await loadBackupsTable(t);
  } catch (err) {
    toast(`Delete failed: ${err.message}`, 'error');
  }
}

window.loadBackupsView = loadBackupsView;
window.openCreateBackupModal = openCreateBackupModal;
window.submitCreateBackup = submitCreateBackup;
window.restoreBackup = restoreBackup;
window.downloadBackup = downloadBackup;
window.toggleBackupLock = toggleBackupLock;
window.deleteBackup = deleteBackup;

export { loadBackupsView };
