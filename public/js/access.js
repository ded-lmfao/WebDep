/**
 * WebDep Granular Access Control Module (Phase 2)
 * Manages per-user, per-resource, per-node & time-bound access grants,
 * built-in role templates, forced session revocation, and access audit logs.
 */

import { state, escapeHtml, formatDate, openModal, closeModal, showConfirmDialog } from './state.js';
import { api, toast } from './api.js';
import { POWER_LABELS } from './settings_users.js';

const accessState = {
  subtab: 'by-user',
  selectedUserId: null,
  selectedResourceKey: '',
  overview: null,
  audit: [],
  loaded: false,
};

function nowUnixSec() {
  return Math.floor(Date.now() / 1000);
}

function formatPowerLabel(power) {
  if (power === '*') return 'All Powers (*)';
  return POWER_LABELS[power] || power;
}

function formatExpiryBadge(expiresAt, serverNow) {
  if (!expiresAt) return '<span class="badge badge-neutral">Permanent</span>';
  const refNow = serverNow || nowUnixSec();
  const rem = expiresAt - refNow;
  if (rem <= 0) return '<span class="badge badge-danger">Expired</span>';
  let label = '';
  if (rem < 3600) {
    label = `${Math.max(1, Math.ceil(rem / 60))}m left`;
  } else if (rem < 86400) {
    const h = Math.floor(rem / 3600);
    const m = Math.floor((rem % 3600) / 60);
    label = `${h}h ${m}m left`;
  } else {
    const d = Math.floor(rem / 86400);
    const h = Math.floor((rem % 86400) / 3600);
    label = `${d}d ${h}h left`;
  }
  const cls = rem < 3600 * 6 ? 'badge-warning' : 'badge-info';
  return `<span class="badge ${cls}" title="Expires ${escapeHtml(formatDate(expiresAt))}">${escapeHtml(label)}</span>`;
}

function activeGrantsForUser(userId) {
  const nowS = accessState.overview?.serverTime || nowUnixSec();
  return (accessState.overview?.grants || []).filter(
    (g) => g.userId === userId && (!g.expiresAt || g.expiresAt > nowS),
  );
}

function findGrant(userId, scopeType, scopeId) {
  const nowS = accessState.overview?.serverTime || nowUnixSec();
  return (accessState.overview?.grants || []).find(
    (g) =>
      g.userId === userId &&
      g.scopeType === scopeType &&
      (scopeId == null ? g.scopeId == null : g.scopeId === scopeId) &&
      (!g.expiresAt || g.expiresAt > nowS),
  );
}

function templateName(presetId) {
  const found = (accessState.overview?.templates || []).find((t) => t.id === presetId);
  return found ? found.name : presetId === 'custom' || !presetId ? 'Custom' : presetId;
}

function renderPowerPills(powers, max = 5) {
  if (!Array.isArray(powers) || powers.length === 0) {
    return '<span class="badge badge-danger">Denied / None</span>';
  }
  if (powers.includes('*')) {
    return '<span class="badge badge-success">Full Scope (*)</span>';
  }
  const shown = powers.slice(0, max).map((p) => `<span class="badge badge-neutral">${escapeHtml(formatPowerLabel(p))}</span>`);
  if (powers.length > max) {
    shown.push(`<span class="badge badge-neutral">+${powers.length - max} more</span>`);
  }
  return `<div style="display:flex;flex-wrap:wrap;gap:4px;">${shown.join('')}</div>`;
}

function effectiveProjectSummary(user, projectId) {
  if (user.isMainAdmin) {
    return { source: 'Main Admin', sourceCls: 'badge-success', preset: '*', powers: ['*'], grant: null };
  }
  const explicit = findGrant(user.id, 'project', projectId);
  if (explicit) {
    return {
      source: 'Explicit App Grant',
      sourceCls: explicit.powers.length ? 'badge-info' : 'badge-danger',
      preset: templateName(explicit.rolePreset),
      powers: explicit.powers,
      grant: explicit,
    };
  }
  const globalGrant = findGrant(user.id, 'global', null);
  const canProjectLegacy =
    user.allProjects || (Array.isArray(user.allowedProjects) && user.allowedProjects.includes(projectId));
  if (globalGrant) {
    const unlocked =
      globalGrant.powers.includes('*') ||
      canProjectLegacy ||
      (Array.isArray(user.allowedProjects) &&
        user.allowedProjects.length === 0 &&
        globalGrant.powers.some((p) =>
          ['projects_view', 'projects_manage', 'deploy', 'logs', 'services_manage', 'domains', 'backups', 'env', 'files', 'files_write'].includes(p),
        ));
    if (!unlocked || globalGrant.powers.length === 0) {
      return { source: 'No Access', sourceCls: 'badge-neutral', preset: '—', powers: [], grant: null };
    }
    return {
      source: 'Global Grant',
      sourceCls: 'badge-neutral',
      preset: templateName(globalGrant.rolePreset),
      powers: globalGrant.powers,
      grant: null,
    };
  }
  if (canProjectLegacy) {
    return {
      source: 'Legacy Allowlist',
      sourceCls: 'badge-neutral',
      preset: 'Legacy',
      powers: user.permissions || [],
      grant: null,
    };
  }
  return { source: 'No Access', sourceCls: 'badge-neutral', preset: '—', powers: [], grant: null };
}

function effectiveDatabaseSummary(user, db) {
  if (user.isMainAdmin) {
    return { source: 'Main Admin', sourceCls: 'badge-success', preset: '*', powers: ['*'], grant: null };
  }
  const explicit = findGrant(user.id, 'database', db.id);
  if (explicit) {
    return {
      source: 'Explicit DB Grant',
      sourceCls: explicit.powers.length ? 'badge-info' : 'badge-danger',
      preset: templateName(explicit.rolePreset),
      powers: explicit.powers,
      grant: explicit,
    };
  }
  if (db.linkedProjectId) {
    const projEff = effectiveProjectSummary(user, db.linkedProjectId);
    const hasDbPower = projEff.powers.includes('*') || projEff.powers.includes('databases') || projEff.powers.includes('databases_manage');
    if (projEff.powers.length && hasDbPower) {
      return {
        source: `Linked App (${projEff.source})`,
        sourceCls: 'badge-neutral',
        preset: projEff.preset,
        powers: projEff.powers.filter((p) => ['*', 'databases', 'databases_manage', 'backups', 'env', 'logs', 'monitor'].includes(p)),
        grant: null,
      };
    }
    return { source: 'No Access', sourceCls: 'badge-neutral', preset: '—', powers: [], grant: null };
  }
  const globalGrant = findGrant(user.id, 'global', null);
  const perms = globalGrant ? globalGrant.powers : (user.permissions || []);
  const canUnlinked = user.allProjects || perms.includes('*') || perms.includes('databases_manage');
  const hasDb = perms.includes('*') || perms.includes('databases') || perms.includes('databases_manage');
  if (canUnlinked && hasDb) {
    return {
      source: globalGrant ? 'Global Grant' : 'Legacy Role',
      sourceCls: 'badge-neutral',
      preset: globalGrant ? templateName(globalGrant.rolePreset) : 'Legacy',
      powers: perms,
      grant: null,
    };
  }
  return { source: 'No Access', sourceCls: 'badge-neutral', preset: '—', powers: [], grant: null };
}

export async function loadAccessView() {
  if (!state.user || !state.user.isMainAdmin) {
    const sec = document.getElementById('view-access');
    if (sec) {
      sec.innerHTML = `<div class="card"><div class="card-body"><p style="color:var(--text-muted);">Only the Main Admin can manage workspace access policies.</p></div></div>`;
    }
    return;
  }

  try {
    const [overview, audit] = await Promise.all([
      api('/api/access/overview', { noCache: true }),
      api('/api/access/audit', { noCache: true }).catch(() => []),
    ]);
    accessState.overview = overview;
    accessState.audit = Array.isArray(audit) ? audit : [];
    accessState.loaded = true;

    const users = overview.users || [];
    if (!accessState.selectedUserId || !users.some((u) => u.id === accessState.selectedUserId)) {
      const nonAdmin = users.find((u) => !u.isMainAdmin) || users[0];
      accessState.selectedUserId = nonAdmin ? nonAdmin.id : null;
    }

    if (!accessState.selectedResourceKey) {
      if (overview.projects?.length) {
        accessState.selectedResourceKey = `project:${overview.projects[0].id}`;
      } else if (overview.databases?.length) {
        accessState.selectedResourceKey = `database:${overview.databases[0].id}`;
      } else if (overview.nodes?.length) {
        accessState.selectedResourceKey = `node:${overview.nodes[0].id}`;
      }
    }

    renderAccessView();
  } catch (err) {
    toast(`Failed to load access control data: ${err.message}`, 'error');
  }
}

function renderAccessSummaryCards() {
  const ov = accessState.overview;
  if (!ov) return;
  const users = ov.users || [];
  const nowS = ov.serverTime || nowUnixSec();
  const activeGrants = (ov.grants || []).filter((g) => !g.expiresAt || g.expiresAt > nowS);
  const expiringSoon = activeGrants.filter((g) => g.expiresAt && g.expiresAt - nowS <= 86400);
  const activeSessionsTotal = users.reduce((acc, u) => acc + (u.activeSessions || 0), 0);

  const elUsers = document.getElementById('access-stat-users');
  const elGrants = document.getElementById('access-stat-grants');
  const elExpiring = document.getElementById('access-stat-expiring');
  const elSessions = document.getElementById('access-stat-sessions');
  if (elUsers) elUsers.textContent = String(users.length);
  if (elGrants) elGrants.textContent = String(activeGrants.length);
  if (elExpiring) elExpiring.textContent = String(expiringSoon.length);
  if (elSessions) elSessions.textContent = String(activeSessionsTotal);
}

function renderAccessSubtabsBar() {
  document.querySelectorAll('#access-subtabs .pill').forEach((btn) => {
    btn.classList.toggle('active', btn.getAttribute('data-subtab') === accessState.subtab);
  });
}

function renderByUserPanel() {
  const container = document.getElementById('access-content-panel');
  const ov = accessState.overview;
  if (!container || !ov) return;
  const users = ov.users || [];
  if (!users.length) {
    container.innerHTML = `<div class="card"><div class="card-body">No users found.</div></div>`;
    return;
  }

  const selected = users.find((u) => u.id === accessState.selectedUserId) || users[0];
  const userGrants = activeGrantsForUser(selected.id);
  const globalGrant = userGrants.find((g) => g.scopeType === 'global' && !g.scopeId);

  const userListHtml = users
    .map((u) => {
      const active = u.id === selected.id;
      const gCount = activeGrantsForUser(u.id).length;
      const statusCls = u.disabled
        ? 'badge-danger'
        : u.presence === 'online'
          ? 'badge-success'
          : u.presence === 'idle'
            ? 'badge-warning'
            : 'badge-neutral';
      const statusText = u.disabled ? 'Disabled' : u.presence || 'offline';
      return `
        <button type="button" class="access-user-row ${active ? 'active' : ''}" data-select-user="${u.id}"
          style="width:100%;display:flex;align-items:center;justify-content:space-between;gap:10px;padding:10px 12px;border-radius:var(--radius-sm);border:1px solid ${active ? 'var(--accent)' : 'var(--border-subtle)'};background:${active ? 'var(--bg-card-hover)' : 'var(--bg-alt)'};text-align:left;cursor:pointer;color:var(--text-main);">
          <div style="min-width:0;">
            <div style="font-weight:600;font-size:13px;display:flex;align-items:center;gap:6px;">
              <span>${escapeHtml(u.username)}</span>
              ${u.isMainAdmin ? '<span class="badge badge-info">Main Admin</span>' : ''}
            </div>
            <div style="font-size:11.5px;color:var(--text-muted);margin-top:2px;">
              ${u.activeSessions || 0} session(s) &bull; ${gCount} explicit grant(s)
            </div>
          </div>
          <span class="badge ${statusCls}">${escapeHtml(statusText)}</span>
        </button>`;
    })
    .join('');

  const nodeRows = (ov.nodes || [])
    .map((n) => {
      const nodeGrant = findGrant(selected.id, 'node', n.id);
      const effPowers = selected.isMainAdmin
        ? ['*']
        : nodeGrant
          ? nodeGrant.powers
          : globalGrant
            ? globalGrant.powers
            : (selected.permissions || []);
      return `
        <tr>
          <td>
            <strong>${escapeHtml(n.name)}</strong>
            <div style="font-size:11.5px;color:var(--text-muted);">${escapeHtml(n.slug)} &bull; <span class="badge badge-success">${escapeHtml(n.status)}</span></div>
          </td>
          <td>${nodeGrant ? `<span class="badge badge-info">Explicit Node Grant (${escapeHtml(templateName(nodeGrant.rolePreset))})</span>` : '<span class="badge badge-neutral">Inherited</span>'}</td>
          <td>${renderPowerPills(effPowers, 4)}</td>
          <td>${nodeGrant ? formatExpiryBadge(nodeGrant.expiresAt, ov.serverTime) : '<span class="badge badge-neutral">—</span>'}</td>
          <td style="text-align:right;white-space:nowrap;">
            ${
              selected.isMainAdmin
                ? '<span style="font-size:12px;color:var(--text-muted);">Protected</span>'
                : `<button class="btn btn-secondary btn-xs" onclick="window.openAccessGrantModal(${selected.id}, 'node', ${n.id})">${nodeGrant ? 'Edit' : 'Grant Node'}</button>
                   ${nodeGrant ? `<button class="btn btn-danger btn-xs" onclick="window.revokeAccessGrant(${nodeGrant.id})">Revoke</button>` : ''}`
            }
          </td>
        </tr>`;
    })
    .join('');

  const projectRows = (ov.projects || [])
    .map((p) => {
      const eff = effectiveProjectSummary(selected, p.id);
      return `
        <tr>
          <td>
            <strong>${escapeHtml(p.name)}</strong>
            <div style="font-size:11.5px;color:var(--text-muted);">/${escapeHtml(p.slug)} &bull; ${escapeHtml(p.kind)}</div>
          </td>
          <td><span class="badge ${eff.sourceCls}">${escapeHtml(eff.source)}</span></td>
          <td>${renderPowerPills(eff.powers, 4)}</td>
          <td>${eff.grant ? formatExpiryBadge(eff.grant.expiresAt, ov.serverTime) : '<span class="badge badge-neutral">—</span>'}</td>
          <td style="text-align:right;white-space:nowrap;">
            ${
              selected.isMainAdmin
                ? '<span style="font-size:12px;color:var(--text-muted);">Full Control</span>'
                : `<button class="btn btn-secondary btn-xs" onclick="window.openAccessGrantModal(${selected.id}, 'project', ${p.id})">${eff.grant ? 'Edit Grant' : 'Override / Grant'}</button>
                   ${
                     eff.grant
                       ? `<button class="btn btn-danger btn-xs" onclick="window.revokeAccessGrant(${eff.grant.id})">Revoke</button>`
                       : eff.powers.length
                         ? `<button class="btn btn-warning btn-xs" title="Explicitly restrict to Read-Only on this app" onclick="window.quickPresetGrant(${selected.id}, 'project', ${p.id}, 'support_read_only')">Downgrade Read-Only</button>`
                         : ''
                   }`
            }
          </td>
        </tr>`;
    })
    .join('');

  const dbRows = (ov.databases || [])
    .map((d) => {
      const eff = effectiveDatabaseSummary(selected, d);
      return `
        <tr>
          <td>
            <strong>${escapeHtml(d.name)}</strong>
            <div style="font-size:11.5px;color:var(--text-muted);">${escapeHtml(d.slug)}</div>
          </td>
          <td><span class="badge ${eff.sourceCls}">${escapeHtml(eff.source)}</span></td>
          <td>${renderPowerPills(eff.powers, 4)}</td>
          <td>${eff.grant ? formatExpiryBadge(eff.grant.expiresAt, ov.serverTime) : '<span class="badge badge-neutral">—</span>'}</td>
          <td style="text-align:right;white-space:nowrap;">
            ${
              selected.isMainAdmin
                ? '<span style="font-size:12px;color:var(--text-muted);">Full Control</span>'
                : `<button class="btn btn-secondary btn-xs" onclick="window.openAccessGrantModal(${selected.id}, 'database', ${d.id})">${eff.grant ? 'Edit Grant' : 'Grant DB'}</button>
                   ${eff.grant ? `<button class="btn btn-danger btn-xs" onclick="window.revokeAccessGrant(${eff.grant.id})">Revoke</button>` : ''}`
            }
          </td>
        </tr>`;
    })
    .join('');

  container.innerHTML = `
    <div style="display:grid;grid-template-columns:280px minmax(0,1fr);gap:16px;align-items:start;">
      <div class="card">
        <div class="card-header" style="display:flex;justify-content:space-between;align-items:center;">
          <h3 style="margin:0;font-size:14px;">Platform Users</h3>
          <button class="btn btn-primary btn-xs" id="access-add-user-btn">+ User</button>
        </div>
        <div class="card-body" style="display:flex;flex-direction:column;gap:8px;max-height:540px;overflow-y:auto;">
          ${userListHtml}
        </div>
      </div>

      <div style="display:flex;flex-direction:column;gap:16px;min-width:0;">
        <div class="card">
          <div class="card-header" style="display:flex;justify-content:space-between;align-items:center;flex-wrap:wrap;gap:10px;">
            <div>
              <h3 style="margin:0;font-size:16px;display:flex;align-items:center;gap:8px;">
                <span>${escapeHtml(selected.username)}</span>
                ${selected.isMainAdmin ? '<span class="badge badge-info">Main Admin</span>' : ''}
                ${selected.totpEnabled ? '<span class="badge badge-success">2FA Enabled</span>' : '<span class="badge badge-neutral">2FA Off</span>'}
                ${selected.disabled ? '<span class="badge badge-danger">Disabled</span>' : ''}
              </h3>
              <div style="font-size:12px;color:var(--text-muted);margin-top:4px;">
                Active sessions: <strong>${selected.activeSessions || 0}</strong> &bull;
                Legacy scope: <strong>${selected.allProjects ? 'All Websites (*)' : `${(selected.allowedProjects || []).length} website(s)`}</strong>
              </div>
            </div>
            <div style="display:flex;gap:6px;flex-wrap:wrap;">
              ${
                !selected.isMainAdmin
                  ? `<button class="btn btn-primary btn-xs" onclick="window.openAccessGrantModal(${selected.id}, 'global', null)">${globalGrant ? 'Edit Global Role' : '+ Global Grant'}</button>
                     <button class="btn btn-secondary btn-xs" onclick="openEditUserModal(${selected.id})">Edit Credentials / Legacy</button>
                     <button class="btn btn-warning btn-xs" onclick="window.forceRevokeUserSessions(${selected.id}, '${escapeHtml(selected.username)}')">Revoke Sessions</button>
                     ${selected.totpEnabled ? `<button class="btn btn-secondary btn-xs" onclick="adminResetUser2fa(${selected.id}, '${escapeHtml(selected.username)}')">Reset 2FA</button>` : ''}
                     <button class="btn btn-danger btn-xs" onclick="toggleUserDisabled(${selected.id}, ${selected.disabled ? 'false' : 'true'})">${selected.disabled ? 'Enable' : 'Disable'}</button>`
                  : '<span class="badge badge-success">Root Identity</span>'
              }
            </div>
          </div>
          ${
            !selected.isMainAdmin
              ? `<div class="card-body" style="display:flex;align-items:center;justify-content:space-between;flex-wrap:wrap;gap:10px;border-top:1px solid var(--border-subtle);">
                  <div>
                    <div style="font-size:12px;color:var(--text-muted);margin-bottom:4px;">
                      Effective Global Role: <strong>${globalGrant ? escapeHtml(templateName(globalGrant.rolePreset)) : 'Legacy User Permissions'}</strong>
                      ${globalGrant ? formatExpiryBadge(globalGrant.expiresAt, ov.serverTime) : ''}
                    </div>
                    ${renderPowerPills(globalGrant ? globalGrant.powers : (selected.permissions || []), 10)}
                  </div>
                  ${globalGrant ? `<button class="btn btn-danger btn-xs" onclick="window.revokeAccessGrant(${globalGrant.id})">Clear Global Grant</button>` : ''}
                </div>`
              : ''
          }
        </div>

        <div class="card">
          <div class="card-header" style="display:flex;justify-content:space-between;align-items:center;">
            <h4 style="margin:0;font-size:14px;">Allowed Nodes &amp; Infrastructure</h4>
            <span class="badge badge-neutral">Phase 3 Multi-Node Ready</span>
          </div>
          <div class="card-body" style="padding:0;">
            <div class="table-responsive">
              <table class="table">
                <thead><tr><th>Node</th><th>Source</th><th>Effective Powers</th><th>Expiry</th><th style="text-align:right;">Actions</th></tr></thead>
                <tbody>${nodeRows || '<tr><td colspan="5" style="text-align:center;padding:18px;">No nodes registered.</td></tr>'}</tbody>
              </table>
            </div>
          </div>
        </div>

        <div class="card">
          <div class="card-header" style="display:flex;justify-content:space-between;align-items:center;">
            <h4 style="margin:0;font-size:14px;">Per-App &amp; Service Access Matrix</h4>
            ${!selected.isMainAdmin ? `<button class="btn btn-secondary btn-xs" onclick="window.openAccessGrantModal(${selected.id}, 'project', null)">+ Grant App Access</button>` : ''}
          </div>
          <div class="card-body" style="padding:0;">
            <div class="table-responsive">
              <table class="table">
                <thead><tr><th>App / Service</th><th>Access Source</th><th>Effective Powers</th><th>Expiry</th><th style="text-align:right;">Actions</th></tr></thead>
                <tbody>${projectRows || '<tr><td colspan="5" style="text-align:center;padding:18px;">No websites or services configured.</td></tr>'}</tbody>
              </table>
            </div>
          </div>
        </div>

        <div class="card">
          <div class="card-header" style="display:flex;justify-content:space-between;align-items:center;">
            <h4 style="margin:0;font-size:14px;">Managed Databases Access</h4>
            ${!selected.isMainAdmin ? `<button class="btn btn-secondary btn-xs" onclick="window.openAccessGrantModal(${selected.id}, 'database', null)">+ Grant DB Access</button>` : ''}
          </div>
          <div class="card-body" style="padding:0;">
            <div class="table-responsive">
              <table class="table">
                <thead><tr><th>Database</th><th>Access Source</th><th>Effective Powers</th><th>Expiry</th><th style="text-align:right;">Actions</th></tr></thead>
                <tbody>${dbRows || '<tr><td colspan="5" style="text-align:center;padding:18px;">No managed databases provisioned.</td></tr>'}</tbody>
              </table>
            </div>
          </div>
        </div>
      </div>
    </div>`;

  container.querySelectorAll('[data-select-user]').forEach((btn) => {
    btn.addEventListener('click', () => {
      accessState.selectedUserId = Number(btn.getAttribute('data-select-user'));
      renderByUserPanel();
    });
  });
  document.getElementById('access-add-user-btn')?.addEventListener('click', () => {
    document.getElementById('create-user-modal-btn')?.click();
  });
}

function renderByResourcePanel() {
  const container = document.getElementById('access-content-panel');
  const ov = accessState.overview;
  if (!container || !ov) return;

  const options = [
    ...(ov.projects || []).map((p) => ({ key: `project:${p.id}`, label: `App / Service: ${p.name} (/${p.slug})`, scopeType: 'project', item: p })),
    ...(ov.databases || []).map((d) => ({ key: `database:${d.id}`, label: `Database: ${d.name} (${d.slug})`, scopeType: 'database', item: d })),
    ...(ov.nodes || []).map((n) => ({ key: `node:${n.id}`, label: `Node: ${n.name}`, scopeType: 'node', item: n })),
  ];
  const current = options.find((o) => o.key === accessState.selectedResourceKey) || options[0];

  const rows = (ov.users || [])
    .map((u) => {
      if (!current) return '';
      let eff;
      if (current.scopeType === 'project') {
        eff = effectiveProjectSummary(u, current.item.id);
      } else if (current.scopeType === 'database') {
        eff = effectiveDatabaseSummary(u, current.item);
      } else {
        const g = findGrant(u.id, 'node', current.item.id);
        const globalG = findGrant(u.id, 'global', null);
        eff = u.isMainAdmin
          ? { source: 'Main Admin', sourceCls: 'badge-success', powers: ['*'], grant: null }
          : g
            ? { source: 'Explicit Node Grant', sourceCls: 'badge-info', powers: g.powers, grant: g }
            : { source: globalG ? 'Global Grant' : 'Legacy Role', sourceCls: 'badge-neutral', powers: globalG ? globalG.powers : (u.permissions || []), grant: null };
      }

      return `
        <tr>
          <td>
            <strong>${escapeHtml(u.username)}</strong>
            ${u.disabled ? '<span class="badge badge-danger" style="margin-left:6px;">Disabled</span>' : ''}
          </td>
          <td><span class="badge ${eff.sourceCls}">${escapeHtml(eff.source)}</span></td>
          <td>${renderPowerPills(eff.powers, 6)}</td>
          <td>${eff.grant ? formatExpiryBadge(eff.grant.expiresAt, ov.serverTime) : '<span class="badge badge-neutral">—</span>'}</td>
          <td style="text-align:right;white-space:nowrap;">
            ${
              u.isMainAdmin
                ? '<span style="font-size:12px;color:var(--text-muted);">Always Full Access</span>'
                : `<button class="btn btn-secondary btn-xs" onclick="window.openAccessGrantModal(${u.id}, '${current.scopeType}', ${current.item.id})">${eff.grant ? 'Edit' : 'Grant'}</button>
                   <button class="btn btn-secondary btn-xs" onclick="window.quickPresetGrant(${u.id}, '${current.scopeType}', ${current.item.id}, 'support_read_only')">Read-Only</button>
                   ${eff.grant ? `<button class="btn btn-danger btn-xs" onclick="window.revokeAccessGrant(${eff.grant.id})">Revoke</button>` : ''}`
            }
          </td>
        </tr>`;
    })
    .join('');

  container.innerHTML = `
    <div class="card">
      <div class="card-header" style="display:flex;justify-content:space-between;align-items:center;flex-wrap:wrap;gap:12px;">
        <div>
          <h3 style="margin:0;font-size:15px;">Who Has Access?</h3>
          <div style="font-size:12px;color:var(--text-muted);margin-top:2px;">Inspect and override permissions for every user against a specific app, database, or node.</div>
        </div>
        <select id="access-resource-select" class="form-control" style="min-width:280px;max-width:420px;">
          ${options.map((o) => `<option value="${escapeHtml(o.key)}" ${current && o.key === current.key ? 'selected' : ''}>${escapeHtml(o.label)}</option>`).join('')}
        </select>
      </div>
      <div class="card-body" style="padding:0;">
        <div class="table-responsive">
          <table class="table">
            <thead><tr><th>User</th><th>Resolution Source</th><th>Effective Powers</th><th>Expiry</th><th style="text-align:right;">1-Click Actions</th></tr></thead>
            <tbody>${rows || '<tr><td colspan="5" style="text-align:center;padding:20px;">No resources or users found.</td></tr>'}</tbody>
          </table>
        </div>
      </div>
    </div>`;

  document.getElementById('access-resource-select')?.addEventListener('change', (e) => {
    accessState.selectedResourceKey = e.target.value;
    renderByResourcePanel();
  });
}

function renderTemplatesPanel() {
  const container = document.getElementById('access-content-panel');
  const templates = accessState.overview?.templates || [];
  if (!container) return;

  const cards = templates
    .map(
      (t) => `
      <div class="card">
        <div class="card-header" style="display:flex;justify-content:space-between;align-items:center;gap:8px;">
          <div>
            <h3 style="margin:0;font-size:15px;">${escapeHtml(t.name)}</h3>
            <code style="font-size:11px;color:var(--text-muted);">${escapeHtml(t.id)}</code>
          </div>
          <button class="btn btn-primary btn-xs" onclick="window.applyTemplateModal('${escapeHtml(t.id)}')">Apply Template</button>
        </div>
        <div class="card-body" style="display:flex;flex-direction:column;gap:10px;">
          <p style="margin:0;font-size:13px;color:var(--text-muted);">${escapeHtml(t.description)}</p>
          <div style="font-size:12px;color:var(--text-muted);">Valid scopes: ${(t.scopes || []).map((s) => `<span class="badge badge-neutral">${escapeHtml(s)}</span>`).join(' ')}</div>
          <div>${renderPowerPills(t.powers || [], 12)}</div>
        </div>
      </div>`,
    )
    .join('');

  container.innerHTML = `<div style="display:grid;grid-template-columns:repeat(auto-fit,minmax(300px,1fr));gap:14px;">${cards}</div>`;
}

function renderExpiringAndAuditPanel() {
  const container = document.getElementById('access-content-panel');
  const ov = accessState.overview;
  if (!container || !ov) return;
  const nowS = ov.serverTime || nowUnixSec();
  const usersById = new Map((ov.users || []).map((u) => [u.id, u.username]));
  const projsById = new Map((ov.projects || []).map((p) => [p.id, p.name]));
  const dbsById = new Map((ov.databases || []).map((d) => [d.id, d.name]));
  const nodesById = new Map((ov.nodes || []).map((n) => [n.id, n.name]));

  const scopeLabel = (g) => {
    if (g.scopeType === 'global') return 'Global Scope';
    if (g.scopeType === 'project') return `App: ${projsById.get(g.scopeId) || `#${g.scopeId}`}`;
    if (g.scopeType === 'database') return `DB: ${dbsById.get(g.scopeId) || `#${g.scopeId}`}`;
    if (g.scopeType === 'node') return `Node: ${nodesById.get(g.scopeId) || `#${g.scopeId}`}`;
    return `${g.scopeType}:${g.scopeId}`;
  };

  const timebound = (ov.grants || [])
    .filter((g) => g.expiresAt != null)
    .sort((a, b) => a.expiresAt - b.expiresAt);

  const expRows = timebound
    .map(
      (g) => `
      <tr>
        <td><strong>${escapeHtml(usersById.get(g.userId) || `User #${g.userId}`)}</strong></td>
        <td>${escapeHtml(scopeLabel(g))}</td>
        <td><span class="badge badge-info">${escapeHtml(templateName(g.rolePreset))}</span></td>
        <td>${formatExpiryBadge(g.expiresAt, nowS)}</td>
        <td style="text-align:right;white-space:nowrap;">
          <button class="btn btn-secondary btn-xs" onclick="window.extendAccessGrant(${g.id}, 86400)">+24h</button>
          <button class="btn btn-danger btn-xs" onclick="window.revokeAccessGrant(${g.id})">Revoke</button>
        </td>
      </tr>`,
    )
    .join('');

  const auditRows = (accessState.audit || [])
    .map(
      (a) => `
      <tr>
        <td style="white-space:nowrap;font-family:var(--font-mono);font-size:12px;">${escapeHtml(formatDate(a.ts))}</td>
        <td><strong>${escapeHtml(a.actor)}</strong></td>
        <td><span class="badge badge-neutral">${escapeHtml(a.action)}</span></td>
        <td>${escapeHtml(a.target || '—')}</td>
        <td style="font-size:12px;color:var(--text-muted);">${escapeHtml(a.detail || '—')}</td>
      </tr>`,
    )
    .join('');

  container.innerHTML = `
    <div style="display:flex;flex-direction:column;gap:16px;">
      <div class="card">
        <div class="card-header" style="display:flex;justify-content:space-between;align-items:center;">
          <h3 style="margin:0;font-size:14px;">Time-Bound Access Grants</h3>
          <span class="badge badge-neutral">${timebound.length} scheduled</span>
        </div>
        <div class="card-body" style="padding:0;">
          <div class="table-responsive">
            <table class="table">
              <thead><tr><th>User</th><th>Scope</th><th>Preset</th><th>Remaining</th><th style="text-align:right;">Actions</th></tr></thead>
              <tbody>${expRows || '<tr><td colspan="5" style="text-align:center;padding:20px;">No temporary / expiring grants active.</td></tr>'}</tbody>
            </table>
          </div>
        </div>
      </div>

      <div class="card">
        <div class="card-header" style="display:flex;justify-content:space-between;align-items:center;">
          <h3 style="margin:0;font-size:14px;">Access &amp; Identity Audit Feed</h3>
          <button class="btn btn-secondary btn-xs" onclick="window.loadAccessView()">Refresh</button>
        </div>
        <div class="card-body" style="padding:0;">
          <div class="table-responsive">
            <table class="table">
              <thead><tr><th>Timestamp</th><th>Actor</th><th>Action</th><th>Target</th><th>Detail</th></tr></thead>
              <tbody>${auditRows || '<tr><td colspan="5" style="text-align:center;padding:20px;">No access audit events recorded yet.</td></tr>'}</tbody>
            </table>
          </div>
        </div>
      </div>
    </div>`;
}

function renderAccessView() {
  renderAccessSummaryCards();
  renderAccessSubtabsBar();
  if (accessState.subtab === 'by-user') renderByUserPanel();
  else if (accessState.subtab === 'by-resource') renderByResourcePanel();
  else if (accessState.subtab === 'templates') renderTemplatesPanel();
  else renderExpiringAndAuditPanel();
}

function populateGrantModalScopes(scopeType, selectedScopeId) {
  const ov = accessState.overview;
  const scopeGroup = document.getElementById('grant-scope-id-group');
  const scopeSel = document.getElementById('grant-scope-id');
  if (!ov || !scopeSel || !scopeGroup) return;

  if (scopeType === 'global') {
    scopeGroup.style.display = 'none';
    scopeSel.innerHTML = '';
    return;
  }

  scopeGroup.style.display = '';
  const items =
    scopeType === 'project'
      ? ov.projects || []
      : scopeType === 'database'
        ? ov.databases || []
        : ov.nodes || [];
  scopeSel.innerHTML = items
    .map(
      (it) =>
        `<option value="${it.id}" ${Number(selectedScopeId) === it.id ? 'selected' : ''}>${escapeHtml(it.name)}</option>`,
    )
    .join('');
}

function applyPresetCheckboxes(presetId) {
  const ov = accessState.overview;
  const tpl = (ov?.templates || []).find((t) => t.id === presetId);
  const checks = document.querySelectorAll('.grant-power-check');
  if (!tpl) return;
  if (tpl.powers.includes('*')) {
    checks.forEach((cb) => {
      cb.checked = true;
    });
  } else {
    const set = new Set(tpl.powers);
    checks.forEach((cb) => {
      cb.checked = set.has(cb.value);
    });
  }
}

function renderGrantPowerCheckboxes(selectedPowers = []) {
  const box = document.getElementById('grant-powers-checklist');
  if (!box) return;
  const all = selectedPowers.includes('*');
  const set = new Set(selectedPowers);
  const rawKeys = accessState.overview?.knownPowers || Object.keys(POWER_LABELS);
  const keys = rawKeys.filter((k) => k !== '*');
  box.innerHTML = keys
    .map(
      (k) => `
      <label style="display:flex;align-items:center;gap:8px;font-size:12.5px;cursor:pointer;">
        <input type="checkbox" class="grant-power-check" value="${escapeHtml(k)}" ${all || set.has(k) ? 'checked' : ''}>
        <span>${escapeHtml(formatPowerLabel(k))}</span>
      </label>`,
    )
    .join('');
}

export function openAccessGrantModal(userId = null, scopeType = 'project', scopeId = null, presetOverride = null) {
  const ov = accessState.overview;
  if (!ov) return;
  const nonAdmins = (ov.users || []).filter((u) => !u.isMainAdmin);
  if (!nonAdmins.length) {
    toast('Create a non-admin user first before assigning granular grants.', 'warning');
    return;
  }

  const userSel = document.getElementById('grant-user-id');
  const scopeTypeSel = document.getElementById('grant-scope-type');
  const presetSel = document.getElementById('grant-role-preset');
  const expirySel = document.getElementById('grant-expiry-preset');

  const chosenUserId = userId || accessState.selectedUserId || nonAdmins[0].id;
  if (userSel) {
    userSel.innerHTML = nonAdmins
      .map((u) => `<option value="${u.id}" ${u.id === chosenUserId ? 'selected' : ''}>${escapeHtml(u.username)}</option>`)
      .join('');
  }
  if (scopeTypeSel) scopeTypeSel.value = scopeType || 'project';
  populateGrantModalScopes(scopeType || 'project', scopeId);

  const effectiveScopeId =
    scopeType === 'global'
      ? null
      : Number(document.getElementById('grant-scope-id')?.value || scopeId || 0) || null;
  const existing = findGrant(chosenUserId, scopeType || 'project', effectiveScopeId);

  const presetVal = presetOverride || existing?.rolePreset || 'frontend_deployer';
  if (presetSel) {
    const opts = (ov.templates || []).map((t) => `<option value="${escapeHtml(t.id)}">${escapeHtml(t.name)}</option>`);
    opts.push('<option value="custom">Custom Powers</option>');
    presetSel.innerHTML = opts.join('');
    presetSel.value = presetVal;
  }

  if (existing && !presetOverride) {
    renderGrantPowerCheckboxes(existing.powers || []);
  } else {
    renderGrantPowerCheckboxes([]);
    if (presetVal !== 'custom') applyPresetCheckboxes(presetVal);
  }

  if (expirySel) expirySel.value = 'permanent';
  openModal('modal-access-grant');
}

export async function quickPresetGrant(userId, scopeType, scopeId, rolePreset) {
  try {
    await api('/api/access/grants', {
      method: 'POST',
      body: { userId, scopeType, scopeId: scopeId ?? null, rolePreset },
    });
    toast(`Applied "${templateName(rolePreset)}" grant.`, 'success');
    await loadAccessView();
  } catch (err) {
    toast(`Failed to apply grant: ${err.message}`, 'error');
  }
}

export async function revokeAccessGrant(grantId) {
  const ok = await showConfirmDialog({
    title: 'Revoke Access Grant',
    message: 'Revoke this access grant immediately? Active log streams and permissions for this scope will terminate live.',
    confirmText: 'Revoke Access',
    danger: true,
  });
  if (!ok) return;
  try {
    await api(`/api/access/grants/${grantId}`, { method: 'DELETE' });
    toast('Access grant revoked immediately.', 'success');
    await loadAccessView();
  } catch (err) {
    toast(`Failed to revoke grant: ${err.message}`, 'error');
  }
}

export async function extendAccessGrant(grantId, addSeconds) {
  const g = (accessState.overview?.grants || []).find((item) => item.id === grantId);
  if (!g) return;
  const base = Math.max(nowUnixSec(), g.expiresAt || nowUnixSec());
  try {
    await api('/api/access/grants', {
      method: 'POST',
      body: {
        userId: g.userId,
        scopeType: g.scopeType,
        scopeId: g.scopeId,
        rolePreset: g.rolePreset,
        powers: g.powers,
        expiresAt: base + addSeconds,
      },
    });
    toast('Extended grant expiration by 24 hours.', 'success');
    await loadAccessView();
  } catch (err) {
    toast(`Failed to extend grant: ${err.message}`, 'error');
  }
}

export async function forceRevokeUserSessions(userId, username) {
  const ok = await showConfirmDialog({
    title: `Revoke Sessions for ${username}`,
    message: `Terminate every active browser session and live stream for "${username}" immediately?`,
    confirmText: 'Revoke All Sessions',
    danger: true,
  });
  if (!ok) return;
  try {
    const res = await api(`/api/access/users/${userId}/revoke-sessions`, { method: 'POST' });
    toast(`Revoked ${res.revokedCount || 0} active session(s) for "${username}".`, 'success');
    await loadAccessView();
  } catch (err) {
    toast(`Failed to revoke sessions: ${err.message}`, 'error');
  }
}

document.querySelectorAll('#access-subtabs .pill').forEach((btn) => {
  btn.addEventListener('click', () => {
    const next = btn.getAttribute('data-subtab');
    if (!next) return;
    accessState.subtab = next;
    renderAccessView();
  });
});

document.getElementById('access-new-grant-btn')?.addEventListener('click', () => {
  openAccessGrantModal(accessState.selectedUserId, 'project', null);
});

document.getElementById('grant-scope-type')?.addEventListener('change', (e) => {
  populateGrantModalScopes(e.target.value, null);
});

document.getElementById('grant-role-preset')?.addEventListener('change', (e) => {
  if (e.target.value !== 'custom') {
    applyPresetCheckboxes(e.target.value);
  }
});

document.getElementById('save-access-grant-btn')?.addEventListener('click', async (e) => {
  e.preventDefault();
  const userId = Number(document.getElementById('grant-user-id')?.value || 0);
  const scopeType = document.getElementById('grant-scope-type')?.value || 'project';
  const scopeId = scopeType === 'global' ? null : Number(document.getElementById('grant-scope-id')?.value || 0) || null;
  const rolePreset = document.getElementById('grant-role-preset')?.value || 'custom';
  const checkedPowers = Array.from(document.querySelectorAll('.grant-power-check:checked')).map((cb) => cb.value);
  const expiryKey = document.getElementById('grant-expiry-preset')?.value || 'permanent';

  const ov = accessState.overview;
  const tpl = (ov?.templates || []).find((t) => t.id === rolePreset);
  const powers = tpl && tpl.powers.includes('*') && checkedPowers.length === Object.keys(POWER_LABELS).length ? ['*'] : checkedPowers;

  const durationMap = { '1h': 3600, '24h': 86400, '7d': 604800, '30d': 2592000 };
  const expiresAt = durationMap[expiryKey] ? nowUnixSec() + durationMap[expiryKey] : null;

  if (!userId) {
    toast('Select a user first.', 'warning');
    return;
  }

  try {
    await api('/api/access/grants', {
      method: 'POST',
      body: { userId, scopeType, scopeId, rolePreset, powers, expiresAt },
    });
    closeModal('modal-access-grant');
    toast('Access grant saved.', 'success');
    await loadAccessView();
  } catch (err) {
    toast(`Failed to save access grant: ${err.message}`, 'error');
  }
});

window.loadAccessView = loadAccessView;
window.openAccessGrantModal = openAccessGrantModal;
window.quickPresetGrant = quickPresetGrant;
window.revokeAccessGrant = revokeAccessGrant;
window.extendAccessGrant = extendAccessGrant;
window.forceRevokeUserSessions = forceRevokeUserSessions;
window.applyTemplateModal = (templateId) => {
  const tpl = (accessState.overview?.templates || []).find((t) => t.id === templateId);
  const defaultScope = tpl?.scopes?.[0] || 'project';
  openAccessGrantModal(accessState.selectedUserId, defaultScope, null, templateId);
};
