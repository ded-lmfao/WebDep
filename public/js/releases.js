/**
 * WebDep Releases & Rollbacks Cockpit
 * Historical release points, atomic rollback execution, and release snapshot creation
 */

import {
  state,
  getSelectedProject,
  paginateAndRender,
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

export function syncReleasesHeaderControls() {
  const createBtn = document.getElementById('create-release-btn');
  if (createBtn) createBtn.style.display = hasPower('deploy') ? '' : 'none';
}

async function loadReleasesView() {
  syncReleasesHeaderControls();
  const p = getSelectedProject();
  const tbody = document.getElementById('releases-table-body');
  if (!p) return;

  try {
    const releases = await api(`/api/projects/${p.id}/releases`, { noCache: true });
    if (!releases.length) {
      tbody.innerHTML = '<tr><td colspan="9" style="text-align: center; padding: 24px;">No releases created yet.</td></tr>';
      const bar = document.getElementById('releases-pagination');
      if (bar) bar.innerHTML = '';
      // Backups don't depend on releases: still load them.
      await loadProjectBackupsTable(p);
      return;
    }

    paginateAndRender(releases, 'releases', 'releases-pagination', (slice) => {
      setHtmlIfChanged(tbody, slice.map((r) => {
        const isLive = r.live;
        return `
          <tr>
            <td><strong>Release #${r.id}</strong></td>
            <td><span class="sha-pill">${r.short_sha || shortSha(r.sha)}</span></td>
            <td style="max-width: 240px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis;">${escapeHtml(r.message || '')}</td>
            <td>${escapeHtml(r.author || 'Unknown')}</td>
            <td>${formatDate(r.deployed_at || r.created_at)}</td>
            <td><span class="badge ${r.build_status === 'success' ? 'badge-success' : 'badge-neutral'}">${escapeHtml(r.build_status ? r.build_status.charAt(0).toUpperCase() + r.build_status.slice(1) : '')}</span></td>
            <td><span class="badge ${r.health_status === 'ok' ? 'badge-success' : 'badge-neutral'}">${escapeHtml(r.health_status ? r.health_status.charAt(0).toUpperCase() + r.health_status.slice(1) : '')}</span></td>
            <td>${isLive ? '<span class="badge badge-success">Live</span>' : '<span class="badge badge-neutral">Retained</span>'}</td>
            <td>
              <div style="display: flex; gap: 6px;">
                ${hasPower('deploy') && !isLive && r.status === 'succeeded' ? `<button class="btn btn-primary btn-sm" onclick="rollbackToRelease(${p.id}, ${r.id})">Rollback</button>` : ''}
                ${hasPower('logs') && r.deployment_id ? `<button class="btn btn-secondary btn-sm" onclick="openDeploymentDetails(${r.deployment_id})">Logs</button>` : ''}
                ${hasPower('deploy') && !isLive ? `<button class="btn btn-danger btn-sm" onclick="deleteRelease(${p.id}, ${r.id})">Delete</button>` : ''}
              </div>
            </td>
          </tr>
        `;
      }).join(''));
    });
  } catch (err) {
    tbody.innerHTML = `<tr><td colspan="9" style="text-align: center; color: var(--danger); padding: 24px;">${escapeHtml(err.message)}</td></tr>`;
  }

  await loadProjectBackupsTable(p);
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
function updateBackupsHeader(p, backups) {
  const held = backups.filter((b) => b.status !== 'failed');
  const bytes = held.filter((b) => b.status === 'complete').reduce((n, b) => n + (b.size || 0), 0);
  const limit = Number(p.backup_limit) || 0;
  const count = limit > 0 ? `${held.length} of ${limit} backups` : `${held.length} backup${held.length === 1 ? '' : 's'}`;
  const usage = document.getElementById('backups-usage');
  if (usage) usage.textContent = `${count}${bytes ? ` · ${formatBackupSize(bytes)}` : ''}`;
  const btn = document.getElementById('create-backup-snapshot-btn');
  if (btn) {
    const atLimit = limit > 0 && held.length >= limit;
    btn.hidden = !hasPower('backups');
    btn.disabled = atLimit;
    btn.title = atLimit ? 'This app holds its limit of backups. Delete one first or ask an admin to raise the limit.' : '';
  }
}

/** Shows "Restoring..." while a restore runs; reports how it ended. Returns true while running. */
async function refreshRestoreBanner(p) {
  const banner = document.getElementById('backups-restore-banner');
  let status = null;
  try {
    status = await api(`/api/projects/${p.id}/backups/restore-status`, { noCache: true });
  } catch (_) { /* leave as is */ }
  const running = status?.state === 'running';
  if (banner) {
    banner.hidden = !running;
    if (running) banner.textContent = `Restoring backup #${status.backupId}. The app is stopped until it finishes.`;
  }
  if (lastRestoreState.get(p.id) === 'running' && status && status.state !== 'running') {
    if (status.state === 'done') toast(status.message || 'Restore finished.', 'success');
    else toast(`Restore failed: ${status.message}`, 'error');
    if (typeof window.refreshProjects === 'function') void window.refreshProjects();
  }
  lastRestoreState.set(p.id, status?.state ?? null);
  return running;
}

/** Toasts when a backup this page was waiting on finishes, and keeps refreshing while anything runs. */
function trackBackupProgress(p, backups, restoring) {
  clearTimeout(backupPollTimer);
  for (const b of backups) {
    const was = lastBackupStatus.get(b.id);
    if (was === 'running' && b.status === 'complete') toast(`Backup #${b.id} is complete.`, 'success');
    if (was === 'running' && b.status === 'failed') toast(`Backup #${b.id} failed: ${b.error || 'unknown error'}`, 'error');
    lastBackupStatus.set(b.id, b.status);
  }
  if (restoring || backups.some((b) => b.status === 'running')) {
    backupPollTimer = setTimeout(() => {
      if (state.currentView === 'releases' && getSelectedProject()?.id === p.id) void loadProjectBackupsTable(p);
    }, 2000);
  }
}

async function loadProjectBackupsTable(p) {
  const tbody = document.getElementById('backups-table-body');
  if (!tbody || !p) return;
  try {
    const restoring = await refreshRestoreBanner(p);
    const backups = await api(`/api/projects/${p.id}/backups`, { noCache: true });
    backupsById.clear();
    (Array.isArray(backups) ? backups : []).forEach((b) => backupsById.set(b.id, b));
    updateBackupsHeader(p, Array.isArray(backups) ? backups : []);
    if (!Array.isArray(backups) || !backups.length) {
      setHtmlIfChanged(tbody, '<tr><td colspan="6" style="text-align: center; padding: 20px;">No backups yet. Create one to be able to restore this app later.</td></tr>');
      return;
    }
    trackBackupProgress(p, backups, restoring);
    const canManage = hasPower('backups');
    const onlineNodes = (state.nodes || []).filter((n) => n.status === 'online');
    const html = backups.map((b) => {
      const isOffNode = b.storageMode === 'off-node';
      const done = b.status === 'complete';
      const archive = b.format === 'archive';
      const storageBadge = isOffNode
        ? '<span class="badge badge-info">Off-Node (Core)</span>'
        : '<span class="badge badge-neutral">Local</span>';
      const nodeOptions = onlineNodes.length
        ? onlineNodes.map((n) =>
            `<option value="${n.id}" ${Number(n.id) === Number(p.node_id) ? 'selected' : ''}>${escapeHtml(nodeLabel(n))}</option>`
          ).join('')
        : '<option value="">Current Node</option>';
      const actions = canManage ? [
        isOffNode && onlineNodes.length && state.user?.isAdmin
          ? `<select id="backup-restore-node-${b.id}" class="form-control bk-node" aria-label="Restore onto node">${nodeOptions}</select>` : '',
        done ? bkButton('restore', 'bk-restore', `Restore backup #${b.id}`, `restoreProjectBackup(${p.id}, ${b.id}, ${isOffNode})`, restoring) : '',
        done && archive && appIsOnCoreServer(p) ? bkButton('download', 'bk-download', `Download backup #${b.id}`, `downloadProjectBackup(${p.id}, ${b.id})`) : '',
        bkButton(b.locked ? 'lock' : 'unlock', `bk-lock ${b.locked ? 'is-locked' : ''}`, b.locked ? 'Unlock: allow deleting' : 'Lock: prevent deleting', `toggleBackupLock(${p.id}, ${b.id}, ${!b.locked})`, b.status === 'running'),
        bkButton('trash', 'bk-delete', b.locked ? 'Locked: unlock it to delete' : `Delete backup #${b.id}`, `deleteProjectBackup(${p.id}, ${b.id})`, b.status === 'running' || b.locked),
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

function openCreateBackupModal() {
  const p = getSelectedProject();
  if (!p) {
    toast('Select a website first.', 'warning');
    return;
  }
  document.getElementById('backup-note').value = '';
  document.getElementById('backup-ignore').value = '';
  const wrap = document.getElementById('backup-offnode-wrap');
  if (wrap) wrap.hidden = appIsOnCoreServer(p);
  openModal('modal-create-backup');
}

async function submitCreateBackup() {
  const p = getSelectedProject();
  if (!p) return;
  const btn = document.getElementById('backup-create-confirm');
  const note = document.getElementById('backup-note').value.trim();
  const ignore = document.getElementById('backup-ignore').value.split(/[\n,]/).map((x) => x.trim()).filter(Boolean);
  const offNode = !appIsOnCoreServer(p) && Boolean(document.getElementById('backup-offnode-checkbox')?.checked);
  if (btn) btn.disabled = true;
  try {
    const res = await api(`/api/projects/${p.id}/backup`, {
      method: 'POST',
      body: { kind: 'full-snapshot', note: note || (offNode ? 'Off-node archive snapshot' : 'Manual backup'), offNode, ignore },
    });
    closeModal('modal-create-backup');
    toast(res.status === 'running' ? `Backup #${res.backupId} started. It keeps running in the background.` : `Created ${res.storageMode || 'local'} backup #${res.backupId}`, 'success');
    lastBackupStatus.set(res.backupId, res.status || 'complete');
    await loadProjectBackupsTable(p);
  } catch (err) {
    toast(`Backup failed: ${err.message}`, 'error');
  } finally {
    if (btn) btn.disabled = false;
  }
}

async function restoreProjectBackup(projectId, backupId, isOffNode) {
  const sel = document.getElementById(`backup-restore-node-${backupId}`);
  const targetNodeId = isOffNode && sel && sel.value ? Number(sel.value) : null;
  const p = (state.projects || []).find((x) => x.id === projectId);
  const b = backupsById.get(backupId);

  if (!targetNodeId && b?.format === 'archive' && p) {
    const typed = await showPromptDialog({
      title: `Restore backup #${backupId}`,
      message: `Backup #${backupId} (${formatDate(b.ts)}${b.note ? `, "${b.note}"` : ''}) will replace ALL current files of "${p.name}". Files added or changed since then are removed; the current files are kept for 24 hours so you can undo. The app is stopped during the restore and started again if it was running.\n\nType "${p.name}" to confirm:`,
      placeholder: 'Type the app name to confirm',
      confirmText: 'Restore backup',
      danger: true,
    });
    if (typed === null) return;
    if (typed.trim() !== p.name) {
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
    const res = await api(`/api/projects/${projectId}/backups/restore`, { method: 'POST', body });
    if (res && res.status === 'running') {
      toast(`Restoring backup #${backupId}. The app is stopped until it finishes.`, 'info');
      lastRestoreState.set(projectId, 'running');
      if (p) await loadProjectBackupsTable(p);
      return;
    }
    toast(`Restored backup #${backupId}`, 'success');
    if (typeof window.refreshProjects === 'function') await window.refreshProjects();
    await loadReleasesView();
  } catch (err) {
    toast(`Restore failed: ${err.message}`, 'error');
  }
}

function downloadProjectBackup(projectId, backupId) {
  // A normal navigation, so the browser streams the file to disk instead of holding it in memory.
  const a = document.createElement('a');
  a.href = `/api/projects/${projectId}/backups/${backupId}/download`;
  a.download = '';
  document.body.appendChild(a);
  a.click();
  a.remove();
}

async function toggleBackupLock(projectId, backupId, locked) {
  try {
    await api(`/api/projects/${projectId}/backups/${backupId}/lock`, { method: 'PUT', body: { locked } });
    toast(locked ? `Backup #${backupId} locked.` : `Backup #${backupId} unlocked.`, 'info');
    const p = (state.projects || []).find((x) => x.id === projectId);
    if (p) await loadProjectBackupsTable(p);
  } catch (err) {
    toast(err.message, 'error');
  }
}

async function deleteProjectBackup(projectId, backupId) {
  const ok = await showConfirmDialog({
    title: `Delete Backup #${backupId}`,
    message: `Remove backup #${backupId} permanently? This also deletes its file.`,
    confirmText: 'Delete Backup',
    danger: true,
  });
  if (!ok) return;
  try {
    await api(`/api/projects/${projectId}/backups/${backupId}`, { method: 'DELETE' });
    toast(`Deleted backup #${backupId}`, 'info');
    await loadReleasesView();
  } catch (err) {
    toast(`Delete failed: ${err.message}`, 'error');
  }
}

async function loadRollbacksView() {
  const p = getSelectedProject();
  if (!p) return;

  const liveInfo = document.getElementById('rollback-live-info');
  const tbody = document.getElementById('rollbacks-table-body');

  try {
    const releases = await api(`/api/projects/${p.id}/releases`, { noCache: true });
    const live = releases.find((r) => r.live);

    if (live) {
      liveInfo.innerHTML = `Release #${live.id} &bull; Commit <span class="sha-pill">${shortSha(live.sha)}</span> &bull; <em>"${escapeHtml(live.message || 'No commit message')}"</em> &bull; Deployed ${formatRelativeTime(live.deployed_at || live.created_at)}`;
    } else {
      liveInfo.textContent = 'No live release active currently.';
    }

    const eligible = releases.filter((r) => !r.live && r.status === 'succeeded' && r.available);

    if (!eligible.length) {
      tbody.innerHTML = '<tr><td colspan="7" style="text-align: center; padding: 24px;">No earlier successful releases available for rollback yet. Releases are retained automatically.</td></tr>';
      const bar = document.getElementById('rollbacks-pagination');
      if (bar) bar.innerHTML = '';
      return;
    }

    paginateAndRender(eligible, 'rollbacks', 'rollbacks-pagination', (slice) => {
      setHtmlIfChanged(tbody, slice.map((r) => `
        <tr>
          <td><strong>Release #${r.id}</strong></td>
          <td><span class="sha-pill">${shortSha(r.sha)}</span></td>
          <td style="max-width: 260px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis;">${escapeHtml(r.message || '')}</td>
          <td>${escapeHtml(r.author || 'Unknown')}</td>
          <td>${formatDate(r.deployed_at || r.created_at)}</td>
          <td><span class="badge badge-success">Files Verified</span></td>
          <td>
            ${hasPower('deploy') ? `<button class="btn btn-danger btn-sm" onclick="rollbackToRelease(${p.id}, ${r.id})">Instant Rollback</button>` : '<span style="color:var(--text-dim); font-size:12px;">Read-only</span>'}
          </td>
        </tr>
      `).join(''));
    });
  } catch (err) {
    tbody.innerHTML = `<tr><td colspan="7" style="text-align: center; color: var(--danger); padding: 24px;">${escapeHtml(err.message)}</td></tr>`;
  }
}

async function rollbackToRelease(projectId, releaseId) {
  const ok = await showConfirmDialog({
    title: `Instant Rollback to Release #${releaseId}`,
    message: `Are you sure you want to rollback to Release #${releaseId}?\n\nWebDep will atomically switch the active release junction, preserve your environment variables, and restart the website.`,
    confirmText: `Rollback to #${releaseId}`,
    danger: true,
  });
  if (!ok) return;

  try {
    const res = await api(`/api/projects/${projectId}/rollback`, { method: 'POST', body: { releaseId } });
    toast(`Rollback #${res.deploymentId} initiated`, 'info');
    if (typeof window.navigateTo === 'function') {
      window.navigateTo('deployments', projectId);
    }
  } catch (err) {
    toast(`Rollback failed: ${err.message}`, 'error');
  }
}

async function deleteRelease(projectId, releaseId) {
  const ok = await showConfirmDialog({
    title: `Delete Release #${releaseId}`,
    message: `Permanently delete Release #${releaseId} from disk and the control database?\n\nThis release artifact and rollback point will be removed.`,
    confirmText: 'Delete Release',
    danger: true,
  });
  if (!ok) return;

  try {
    await api(`/api/projects/${projectId}/releases/${releaseId}`, { method: 'DELETE' });
    toast(`Release #${releaseId} deleted.`, 'info');
    await loadReleasesView();
  } catch (err) {
    toast(`Delete failed: ${err.message}`, 'error');
  }
}

function openCreateReleaseModal() {
  let p = getSelectedProject();
  if (!p && state.projects && state.projects.length > 0) {
    p = state.projects[0];
    state.selectedProjectId = p.id;
  }
  if (!p) {
    toast('Please create a website first before creating a release.', 'warning');
    if (typeof window.openAddWebsiteModal === 'function') {
      window.openAddWebsiteModal();
    }
    return;
  }

  const targetLabel = document.getElementById('release-modal-target-project');
  const projSelect = document.getElementById('release-project-select');
  const projGroup = document.getElementById('release-project-select-group');

  if (state.projects && state.projects.length > 1) {
    if (projGroup) projGroup.style.display = 'block';
    if (targetLabel) targetLabel.style.display = 'none';
    if (projSelect) {
      projSelect.innerHTML = state.projects.map((proj) => 
        `<option value="${proj.id}" ${p && p.id === proj.id ? 'selected' : ''}>${escapeHtml(proj.name)} (${escapeHtml(proj.slug)})</option>`
      ).join('');
      projSelect.onchange = (e) => {
        const sel = state.projects.find((item) => item.id === Number(e.target.value));
        if (sel) {
          const noteInput = document.getElementById('release-note-input');
          if (noteInput) {
            const d = new Date();
            noteInput.value = `Snapshot release for ${sel.name} - ${d.toLocaleDateString()}`;
          }
        }
      };
    }
  } else {
    if (projGroup) projGroup.style.display = 'none';
    if (targetLabel) {
      targetLabel.style.display = 'block';
      targetLabel.textContent = `${p.name} (${p.slug})`;
    }
  }

  const noteInput = document.getElementById('release-note-input');
  if (noteInput) {
    const d = new Date();
    noteInput.value = `Snapshot release for ${p.name} - ${d.toLocaleDateString()}`;
  }

  openModal('modal-create-release');
}

async function confirmCreateRelease() {
  let p = getSelectedProject();
  const projSelect = document.getElementById('release-project-select');
  if (projSelect && projSelect.value) {
    const override = state.projects.find((x) => x.id === Number(projSelect.value));
    if (override) p = override;
  }

  if (!p) {
    toast('Please select a website first.', 'warning');
    return;
  }

  const source = document.getElementById('release-source-select')?.value || 'deploy';
  const note = (document.getElementById('release-note-input')?.value || '').trim() || `Manual release for ${p.name}`;
  const btn = document.getElementById('confirm-create-release-btn');
  if (btn) {
    btn.disabled = true;
    btn.textContent = source === 'deploy' ? 'Deploying release' : 'Creating snapshot';
  }

  try {
    const res = await api(`/api/projects/${p.id}/releases`, { method: 'POST', body: { source, note } });
    if (source === 'deploy') {
      toast(`Release deployment #${res.deploymentId} started for ${p.name}`, 'success');
      closeModal('modal-create-release');
      if (typeof window.navigateTo === 'function') {
        window.navigateTo('deployments', p.id);
      }
    } else {
      toast(`Release #${res.releaseId} created`, 'success');
      closeModal('modal-create-release');
      await loadReleasesView();
    }
  } catch (err) {
    toast(`Create release failed: ${err.message}`, 'error');
  } finally {
    if (btn) {
      btn.disabled = false;
      btn.textContent = 'Build & Create Release';
    }
  }
}

// Window bindings
window.loadReleasesView = loadReleasesView;
window.loadRollbacksView = loadRollbacksView;
window.rollbackToRelease = rollbackToRelease;
window.deleteRelease = deleteRelease;
window.openCreateReleaseModal = openCreateReleaseModal;
window.confirmCreateRelease = confirmCreateRelease;
window.openCreateBackupModal = openCreateBackupModal;
window.createProjectBackupSnapshot = openCreateBackupModal;
window.submitCreateBackup = submitCreateBackup;
window.downloadProjectBackup = downloadProjectBackup;
window.toggleBackupLock = toggleBackupLock;
window.restoreProjectBackup = restoreProjectBackup;
window.deleteProjectBackup = deleteProjectBackup;

document.getElementById('create-release-btn')?.addEventListener('click', openCreateReleaseModal);
document.getElementById('confirm-create-release-btn')?.addEventListener('click', confirmCreateRelease);

export {
  loadReleasesView,
  loadRollbacksView,
  rollbackToRelease,
  deleteRelease,
  openCreateReleaseModal,
  confirmCreateRelease,
};

