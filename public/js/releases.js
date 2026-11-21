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

async function loadProjectBackupsTable(p) {
  const tbody = document.getElementById('backups-table-body');
  if (!tbody || !p) return;
  try {
    const backups = await api(`/api/projects/${p.id}/backups`, { noCache: true });
    if (!Array.isArray(backups) || !backups.length) {
      setHtmlIfChanged(tbody, '<tr><td colspan="6" style="text-align: center; padding: 20px;">No backups or snapshots recorded yet.</td></tr>');
      return;
    }
    const onlineNodes = (state.nodes || []).filter((n) => n.status === 'online');
    const html = backups.map((b) => {
      const isOffNode = b.storageMode === 'off-node';
      const storageBadge = isOffNode
        ? '<span class="badge badge-info">Off-Node (Core)</span>'
        : '<span class="badge badge-neutral">Local</span>';
      const nodeOptions = onlineNodes.length
        ? onlineNodes.map((n) =>
            `<option value="${n.id}" ${Number(n.id) === Number(p.node_id) ? 'selected' : ''}>${escapeHtml(nodeLabel(n))}</option>`
          ).join('')
        : '<option value="">Current Node</option>';
      return `
        <tr>
          <td><strong>#${b.id}</strong> <span style="font-size:11.5px; color:var(--text-dim);">${escapeHtml(b.kind || '')}</span></td>
          <td>${storageBadge}</td>
          <td>${formatBackupSize(b.size)}</td>
          <td style="max-width:220px; white-space:nowrap; overflow:hidden; text-overflow:ellipsis;">${escapeHtml(b.note || '')}</td>
          <td>${formatDate(b.ts)}</td>
          <td>
            <div style="display:flex; gap:6px; align-items:center; flex-wrap:wrap;">
              ${isOffNode && onlineNodes.length && state.user?.isAdmin ? `<select id="backup-restore-node-${b.id}" class="form-control" style="width:130px; height:28px; font-size:11.5px; padding:2px 6px;">${nodeOptions}</select>` : ''}
              ${hasPower('backups') ? `<button class="btn btn-primary btn-sm" onclick="restoreProjectBackup(${p.id}, ${b.id}, ${isOffNode})">Restore</button>` : ''}
              ${hasPower('backups') ? `<button class="btn btn-danger btn-sm" onclick="deleteProjectBackup(${p.id}, ${b.id})">Delete</button>` : ''}
            </div>
          </td>
        </tr>
      `;
    }).join('');
    setHtmlIfChanged(tbody, html);
  } catch (err) {
    setHtmlIfChanged(tbody, `<tr><td colspan="6" style="text-align: center; color: var(--danger); padding: 20px;">${escapeHtml(err.message)}</td></tr>`);
  }
}

async function createProjectBackupSnapshot() {
  const p = getSelectedProject();
  if (!p) {
    toast('Select a website first.', 'warning');
    return;
  }
  const offNode = Boolean(document.getElementById('backup-offnode-checkbox')?.checked);
  const btn = document.getElementById('create-backup-snapshot-btn');
  if (btn) btn.disabled = true;
  try {
    const res = await api(`/api/projects/${p.id}/backup`, {
      method: 'POST',
      body: {
        kind: 'full-snapshot',
        note: offNode ? 'Off-node archive snapshot' : 'Manual snapshot',
        offNode,
      },
    });
    toast(`Created ${res.storageMode || 'local'} backup #${res.backupId}`, 'success');
    await loadReleasesView();
  } catch (err) {
    toast(`Backup failed: ${err.message}`, 'error');
  } finally {
    if (btn) btn.disabled = false;
  }
}

async function restoreProjectBackup(projectId, backupId, isOffNode) {
  const sel = document.getElementById(`backup-restore-node-${backupId}`);
  const targetNodeId = isOffNode && sel && sel.value ? Number(sel.value) : null;
  const ok = await showConfirmDialog({
    title: `Restore Backup #${backupId}`,
    message: targetNodeId
      ? `Restore off-node backup #${backupId} onto Node #${targetNodeId}?`
      : `Restore backup #${backupId} on the active node?`,
    confirmText: 'Restore Backup',
    danger: true,
  });
  if (!ok) return;
  try {
    const body = targetNodeId ? { backupId, targetNodeId } : { backupId };
    await api(`/api/projects/${projectId}/backups/restore`, { method: 'POST', body });
    toast(`Restored backup #${backupId}`, 'success');
    if (typeof window.refreshProjects === 'function') await window.refreshProjects();
    await loadReleasesView();
  } catch (err) {
    toast(`Restore failed: ${err.message}`, 'error');
  }
}

async function deleteProjectBackup(projectId, backupId) {
  const ok = await showConfirmDialog({
    title: `Delete Backup #${backupId}`,
    message: `Remove backup #${backupId} permanently?`,
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
window.createProjectBackupSnapshot = createProjectBackupSnapshot;
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

