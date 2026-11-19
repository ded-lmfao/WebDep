/**
 * Accounts: sign-up requests, forced password change, My account, the admin panel
 * (users, requests, audit), and the per-app Permissions tab.
 */

import {
  state,
  openModal,
  closeModal,
  escapeHtml,
  showConfirmDialog,
  showPromptDialog,
  formatRelativeTime,
  formatDate,
  getSelectedProject,
  getSelectedDatabase,
} from './state.js';
import { api, toast } from './api.js';
import {
  APP_POWER_CATALOG,
  PANEL_ACCOUNT_POWERS,
  powerLabel,
  isAdmin,
  hasPanelPower,
  hasPower,
  databaseScope,
} from './permissions.js';

const ADMIN_VIEWS = ['users', 'requests', 'nodes', 'modules', 'settings', 'admin-audit'];
const USER_LIST_POWERS = ['users_view', 'users_create', 'users_edit', 'requests_review'];

const nav = (view, param) => window.navigateTo?.(view, param);
const byId = (id) => document.getElementById(id);

function initials(u) {
  const src = (u.displayName || u.display_name || u.username || '?').trim();
  const parts = src.split(/\s+/).filter(Boolean);
  return ((parts.length > 1 ? parts[0][0] + parts[1][0] : src.slice(0, 2)) || '?').toUpperCase();
}

function personCell(u, sub) {
  const name = u.displayName || u.display_name || u.username;
  return `<div class="acct-person">
    <span class="acct-avatar" aria-hidden="true">${escapeHtml(initials(u))}</span>
    <span style="min-width:0">
      <div class="acct-person-name">${escapeHtml(name)}</div>
      <div class="acct-person-sub">${sub}</div>
    </span>
  </div>`;
}

function roleLabel(u) {
  if (u.isMainAdmin) return 'Main admin';
  if (u.isAdmin) return 'Admin';
  return 'User';
}

// ============================================================================
// Admin panel visibility
// ============================================================================
function adminViewAllowed(view) {
  switch (view) {
    case 'users': return USER_LIST_POWERS.some((p) => hasPanelPower(p));
    case 'requests': return hasPanelPower('requests_review');
    case 'nodes':
    case 'modules': return hasPanelPower('system_manage');
    case 'settings': return hasPanelPower('settings_manage') || hasPanelPower('system_manage');
    case 'admin-audit': return hasPanelPower('audit');
    default: return false;
  }
}

export function isAdminView(view) {
  return ADMIN_VIEWS.includes(view);
}

/** First admin view this user may open, or null when the admin panel is not for them. */
export function firstAdminView() {
  return ADMIN_VIEWS.find(adminViewAllowed) || null;
}

/** Shows the admin sidebar while an admin view is open, the normal sidebar otherwise. */
export function syncSidebarMode() {
  if (!state.user) return;
  const inAdmin = isAdminView(state.currentView);
  const main = byId('nav-group-main');
  const admin = byId('nav-group-admin');
  if (main) main.hidden = inAdmin;
  if (admin) admin.hidden = !inAdmin;
  const entry = document.querySelector('[data-admin-enter]');
  if (entry) entry.hidden = !firstAdminView();
  ADMIN_VIEWS.forEach((v) => {
    const el = document.querySelector(`#nav-group-admin .nav-item[data-view="${v}"]`);
    if (el) el.hidden = !adminViewAllowed(v);
  });
}

/** Called after sign-in: labels, sidebar, request badge. */
export function applyAccountChrome() {
  if (!state.user) return;
  const role = byId('current-user-role');
  if (role) role.textContent = roleLabel(state.user);
  const name = byId('current-user-name');
  if (name) name.textContent = state.user.displayName || state.user.username;
  const avatar = byId('current-user-avatar');
  if (avatar) avatar.textContent = initials(state.user);
  syncSidebarMode();
  void refreshRequestBadge();
}

document.querySelector('[data-admin-enter]')?.addEventListener('click', () => {
  const v = firstAdminView();
  if (v) nav(v);
});
document.querySelector('[data-admin-exit]')?.addEventListener('click', () => nav('overview'));
byId('user-profile-snippet')?.addEventListener('click', () => nav('account'));

async function refreshRequestBadge() {
  const badges = [byId('badge-requests-count'), byId('badge-admin-requests')];
  if (!state.user || !hasPanelPower('requests_review')) {
    badges.forEach((b) => { if (b) b.hidden = true; });
    return;
  }
  try {
    const list = await api('/api/requests', { noCache: true });
    const n = Array.isArray(list) ? list.filter((r) => r.status === 'pending').length : 0;
    badges.forEach((b) => {
      if (!b) return;
      b.textContent = String(n);
      b.hidden = n === 0;
    });
  } catch (_) { /* badge is best-effort */ }
}
setInterval(() => { if (!document.hidden && state.user) void refreshRequestBadge(); }, 30_000);

// ============================================================================
// Sign-in page: self-registration
// ============================================================================
function showLoginStep(step) {
  const forms = { login: byId('login-form'), register: byId('login-register-form') };
  Object.entries(forms).forEach(([k, el]) => { if (el) el.style.display = k === step ? 'block' : 'none'; });
  const mfa = byId('login-2fa-form');
  if (mfa && step !== 'login') mfa.style.display = 'none';
  const sub = document.querySelector('.login-card-header-text p');
  if (sub) sub.textContent = step === 'register' ? 'Request an account' : 'Sign in to your VPS workspace';
}

function setLoginNotice(text, kind = 'warning') {
  let el = byId('login-notice');
  if (!text) {
    el?.remove();
    return;
  }
  if (!el) {
    el = document.createElement('p');
    el.id = 'login-notice';
    byId('login-form')?.prepend(el);
  }
  el.className = `login-notice is-${kind}`;
  el.textContent = text;
}

/** Refreshes the "Create account" link on the sign-in page. */
export async function refreshRegistrationLink() {
  const hint = byId('login-register-hint');
  if (!hint) return;
  try {
    const res = await fetch('/api/auth/registration', { credentials: 'same-origin' });
    const data = res.ok ? await res.json() : { enabled: false };
    hint.hidden = !data.enabled;
  } catch (_) {
    hint.hidden = true;
  }
}

/** Login errors that explain a request's status get a persistent notice. */
export function noteLoginError(message) {
  if (/waiting for admin approval|was declined/i.test(message || '')) {
    setLoginNotice(message, 'warning');
    return true;
  }
  setLoginNotice('');
  return false;
}

byId('login-register-link')?.addEventListener('click', () => {
  setLoginNotice('');
  showLoginStep('register');
  byId('register-name')?.focus();
});
byId('register-back-btn')?.addEventListener('click', () => showLoginStep('login'));

byId('login-register-form')?.addEventListener('submit', async (e) => {
  e.preventDefault();
  const btn = byId('register-btn');
  const body = {
    displayName: byId('register-name')?.value.trim() || '',
    email: byId('register-email')?.value.trim() || '',
    username: byId('register-username')?.value.trim() || '',
    password: byId('register-password')?.value || '',
  };
  if (body.password.length < 8) {
    toast('Password must be at least 8 characters.', 'warning');
    return;
  }
  if (btn) btn.disabled = true;
  try {
    const res = await api('/api/auth/register', { method: 'POST', body });
    byId('login-register-form')?.reset();
    showLoginStep('login');
    const user = byId('login-username');
    if (user) user.value = body.username;
    setLoginNotice(res.message || 'Request sent. You can sign in once an admin approves it.', 'success');
  } catch (err) {
    toast(err.message, 'error');
  } finally {
    if (btn) btn.disabled = false;
  }
});

// ============================================================================
// Forced password change (accounts created with a temporary password)
// ============================================================================
let forcedResolve = null;

/** Blocks until the user replaces their temporary password. */
export function requirePasswordChange() {
  const overlay = byId('force-password-overlay');
  if (!overlay) return Promise.resolve();
  byId('force-password-form')?.reset();
  const err = byId('force-pw-error');
  if (err) err.hidden = true;
  overlay.hidden = false;
  const login = byId('login-view');
  if (login) login.style.display = 'none';
  setTimeout(() => byId('force-pw-current')?.focus(), 50);
  return new Promise((resolve) => { forcedResolve = resolve; });
}

byId('force-password-form')?.addEventListener('submit', async (e) => {
  e.preventDefault();
  const current = byId('force-pw-current')?.value || '';
  const next = byId('force-pw-next')?.value || '';
  const confirm = byId('force-pw-confirm')?.value || '';
  const err = byId('force-pw-error');
  const fail = (msg) => { if (err) { err.textContent = msg; err.hidden = false; } };
  if (next.length < 8) return fail('New password must be at least 8 characters.');
  if (next !== confirm) return fail("The new passwords don't match.");
  const btn = byId('force-pw-save');
  if (btn) btn.disabled = true;
  try {
    await api('/api/auth/password', { method: 'POST', body: { current, next, revokeOtherSessions: true } });
    const me = await api('/api/auth/me', { noCache: true });
    state.user = me.user;
    state.csrf = me.csrf;
    byId('force-password-overlay').hidden = true;
    toast('Password updated.', 'success');
    const done = forcedResolve;
    forcedResolve = null;
    done?.();
  } catch (ex) {
    fail(ex.message);
  } finally {
    if (btn) btn.disabled = false;
  }
});

byId('force-pw-logout')?.addEventListener('click', async () => {
  try { await api('/api/auth/logout', { method: 'POST' }); } catch (_) { /* ignore */ }
  byId('force-password-overlay').hidden = true;
  forcedResolve = null;
  window.showLogin?.();
});

// ============================================================================
// My account
// ============================================================================
function appName(scope, id) {
  if (id === '*') return scope === 'databases' ? 'All databases' : 'All websites and services';
  if (scope === 'databases') {
    const d = (state.databases || []).find((x) => String(x.id) === String(id));
    return d ? d.name : `Database #${id}`;
  }
  const p = (state.projects || []).find((x) => String(x.id) === String(id));
  return p ? `${p.name}${p.project_kind === 'service' ? ' (service)' : ''}` : `App #${id}`;
}

function appKind(scope, id) {
  if (scope === 'databases') return id === '*' ? 'all-databases' : 'database';
  if (id === '*') return 'all-projects';
  const p = (state.projects || []).find((x) => String(x.id) === String(id));
  return p && p.project_kind === 'service' ? 'service' : 'website';
}

function chips(powers, kind) {
  if (!powers.length) return '<span class="acct-muted">Can open it, no actions</span>';
  if (powers.includes('*')) return '<span class="acct-chip">Everything</span>';
  return powers.map((p) => `<span class="acct-chip">${escapeHtml(powerLabel(p, kind))}</span>`).join('');
}

function renderMyAccess() {
  const body = byId('account-access-body');
  if (!body || !state.user) return;
  if (isAdmin()) {
    body.innerHTML = '<p class="acct-muted" style="margin:4px 0">You are an admin, so you have full access to every app and setting.</p>';
    return;
  }
  const a = state.user.access || { panel: [], projects: {}, databases: {} };
  const rows = [];
  const panel = (a.panel || []).filter((p) => PANEL_ACCOUNT_POWERS.includes(p));
  if (panel.length) {
    rows.push(`<div class="acct-scope"><div class="acct-scope-title">Admin panel</div><div class="acct-chip-row">${chips(panel, '')}</div></div>`);
  }
  for (const scope of ['projects', 'databases']) {
    for (const [id, powers] of Object.entries(a[scope] || {})) {
      const kind = appKind(scope, id);
      rows.push(`<div class="acct-scope"><div class="acct-scope-title">${escapeHtml(appName(scope, id))}</div><div class="acct-chip-row">${chips(powers, kind)}</div></div>`);
    }
  }
  body.innerHTML = rows.length
    ? rows.join('')
    : '<p class="acct-muted" style="margin:4px 0">You don\'t have access to any apps yet. Ask an admin to give you access.</p>';
}

function loadAccountView() {
  const u = state.user;
  if (!u) return;
  const set = (id, v) => { const el = byId(id); if (el) el.value = v || ''; };
  set('account-username', u.username);
  set('account-display-name', u.displayName);
  set('account-email', u.email);
  const badge = byId('account-role-badge');
  if (badge) {
    badge.textContent = roleLabel(u);
    badge.className = `badge ${u.isAdmin || u.isMainAdmin ? 'badge-pro' : 'badge-neutral'}`;
  }
  window.renderSecurityStatusSummary?.();
  window.loadPasskeysList?.().catch(() => {});
  renderMyAccess();
}

byId('account-profile-form')?.addEventListener('submit', async (e) => {
  e.preventDefault();
  const btn = byId('account-profile-save');
  if (btn) btn.disabled = true;
  try {
    const updated = await api('/api/auth/profile', {
      method: 'PATCH',
      body: {
        displayName: byId('account-display-name')?.value || '',
        email: byId('account-email')?.value || '',
      },
    });
    state.user = { ...state.user, ...updated, access: updated.access || state.user.access };
    applyAccountChrome();
    loadAccountView();
    toast('Profile saved.', 'success');
  } catch (err) {
    toast(err.message, 'error');
  } finally {
    if (btn) btn.disabled = false;
  }
});

// ============================================================================
// Admin: users
// ============================================================================
let usersCache = [];

function accessSummary(u) {
  if (u.isAdmin || u.isMainAdmin) return '<span class="acct-muted">Full access</span>';
  const a = u.access || {};
  const projects = Object.keys(a.projects || {});
  const dbs = Object.keys(a.databases || {});
  const panel = (a.panel || []).filter((p) => PANEL_ACCOUNT_POWERS.includes(p));
  const parts = [];
  if (projects.includes('*')) parts.push('All websites and services');
  else if (projects.length) parts.push(`${projects.length} website${projects.length === 1 ? '' : 's'}/service${projects.length === 1 ? '' : 's'}`);
  if (dbs.includes('*')) parts.push('All databases');
  else if (dbs.length) parts.push(`${dbs.length} database${dbs.length === 1 ? '' : 's'}`);
  if (panel.length) parts.push(`${panel.length} panel permission${panel.length === 1 ? '' : 's'}`);
  return parts.length ? escapeHtml(parts.join(', ')) : '<span class="acct-muted">No access yet</span>';
}

function canEditUser(u) {
  if (u.isMainAdmin) return Boolean(state.user?.isMainAdmin);
  if (u.isAdmin) return isAdmin();
  if (u.id === state.user?.id) return isAdmin();
  return hasPanelPower('users_edit') || hasPower('access_manage');
}

function renderUsers() {
  const tbody = byId('users-list-body');
  if (!tbody) return;
  const q = (byId('users-search-input')?.value || '').trim().toLowerCase();
  const list = usersCache.filter((u) => !q
    || [u.username, u.displayName, u.email].some((v) => (v || '').toLowerCase().includes(q)));
  if (!list.length) {
    tbody.innerHTML = `<tr><td colspan="7" class="acct-empty">${q ? 'No users match your search.' : 'No users yet.'}</td></tr>`;
    return;
  }
  tbody.innerHTML = list.map((u) => {
    const sub = `@${escapeHtml(u.username)}${u.email ? ` &middot; ${escapeHtml(u.email)}` : ''}`;
    const status = u.disabled
      ? '<span class="badge badge-danger">Disabled</span>'
      : u.presence === 'online'
        ? '<span class="badge badge-success">Online</span>'
        : u.presence === 'idle'
          ? '<span class="badge badge-warning">Idle</span>'
          : '<span class="badge badge-neutral">Offline</span>';
    const roleBadge = `<span class="badge ${u.isAdmin || u.isMainAdmin ? 'badge-pro' : 'badge-neutral'}">${roleLabel(u)}</span>`;
    const tfa = u.totpEnabled ? '<span class="badge badge-success">On</span>' : '<span class="acct-muted">Off</span>';
    const last = u.lastLoginAt ? escapeHtml(formatRelativeTime(u.lastLoginAt)) : '<span class="acct-muted">Never</span>';
    const actions = canEditUser(u) ? `<button class="btn btn-secondary btn-sm" onclick="openUserEditor(${u.id})">Edit</button>` : '';
    return `<tr>
      <td>${personCell(u, sub)}</td>
      <td>${roleBadge}</td>
      <td>${accessSummary(u)}</td>
      <td>${tfa}</td>
      <td>${status}</td>
      <td>${last}</td>
      <td><div class="acct-actions">${actions}</div></td>
    </tr>`;
  }).join('');
}

async function loadUsersView() {
  const createBtn = byId('users-create-btn');
  if (createBtn) createBtn.hidden = !hasPanelPower('users_create');
  const tbody = byId('users-list-body');
  if (tbody && !usersCache.length) tbody.innerHTML = '<tr><td colspan="7" class="acct-empty">Loading users</td></tr>';
  try {
    usersCache = await api('/api/users', { noCache: true });
    renderUsers();
  } catch (err) {
    if (tbody) tbody.innerHTML = `<tr><td colspan="7" class="acct-empty">${escapeHtml(err.message)}</td></tr>`;
  }
}

byId('users-search-input')?.addEventListener('input', renderUsers);
byId('users-create-btn')?.addEventListener('click', () => openUserEditor(null));
setInterval(() => {
  if (state.currentView === 'users' && !document.hidden && !byId('modal-user-editor')?.classList.contains('active')) {
    void loadUsersView();
  }
}, 15_000);

// ---------------------------------------------------------------------------
// User editor (create and edit)
// ---------------------------------------------------------------------------
let editor = null;

/** Apps the actor may hand out access to, grouped by kind. */
function grantableApps() {
  const admin = isAdmin();
  const groups = { website: [], service: [], database: [] };
  for (const p of state.projects || []) {
    if (admin || hasPower('access_manage', { projectId: p.id })) {
      groups[p.project_kind === 'service' ? 'service' : 'website'].push({ scope: 'project', id: p.id, name: p.name, kind: p.project_kind === 'service' ? 'service' : 'website' });
    }
  }
  for (const d of state.databases || []) {
    if (admin || hasPower('access_manage', databaseScope(d))) {
      groups.database.push({ scope: 'database', id: d.id, name: d.name, kind: 'database' });
    }
  }
  return groups;
}

/** Powers the actor may give on one app (admins: all). */
function actorGrantable(app) {
  const catalog = APP_POWER_CATALOG[app.kind] || [];
  if (isAdmin()) return catalog;
  const scope = app.scope === 'database' ? { databaseId: app.id } : { projectId: app.id };
  return catalog.filter((p) => hasPower(p, scope));
}

function existingPowers(user, app) {
  if (!user) return null;
  const map = (user.access || {})[app.scope === 'database' ? 'databases' : 'projects'] || {};
  const key = app.id === 'all' ? '*' : String(app.id);
  return Object.prototype.hasOwnProperty.call(map, key) ? map[key] : null;
}

function powerChecks(app, selected, allowed, name) {
  const catalog = APP_POWER_CATALOG[app.kind] || [];
  return `<div class="acct-power-grid">${catalog.map((p) => {
    const ok = allowed.includes(p);
    return `<label class="acct-check ${ok ? '' : 'is-locked'}" title="${ok ? '' : "You can only give permissions you have yourself"}">
      <input type="checkbox" data-power-of="${name}" value="${p}" ${selected.includes(p) ? 'checked' : ''} ${ok ? '' : 'disabled'}>
      ${escapeHtml(powerLabel(p, app.kind))}
    </label>`;
  }).join('')}</div>`;
}

function appRow(app, user) {
  const name = `${app.scope}:${app.id}`;
  const current = existingPowers(user, app);
  const allowed = actorGrantable(app);
  const lockedRow = current && current.some((p) => !allowed.includes(p) && p !== '*') && !isAdmin();
  const has = current !== null;
  const summary = has ? (current.length ? current.map((p) => powerLabel(p, app.kind)).join(', ') : 'Can open it') : '';
  return `<div class="acct-app-row" data-app-row="${name}">
    <div class="acct-app-row-head">
      <label class="acct-check ${lockedRow ? 'is-locked' : ''}">
        <input type="checkbox" data-app-toggle="${name}" ${has ? 'checked' : ''} ${lockedRow ? 'disabled' : ''}>
        ${escapeHtml(app.name)}
      </label>
      <span class="acct-app-summary" data-app-summary="${name}">${escapeHtml(summary)}</span>
    </div>
    <div class="acct-app-row-body" data-app-body="${name}" ${has ? '' : 'hidden'}>
      ${powerChecks(app, current || [], lockedRow ? [] : allowed, name)}
    </div>
  </div>`;
}

function renderEditor() {
  const { user } = editor;
  const isNew = !user;
  const me = state.user;
  const canRole = Boolean(me?.isMainAdmin) && !(user && user.isMainAdmin);
  const targetIsAdmin = Boolean(user && (user.isAdmin || user.isMainAdmin));
  const groups = grantableApps();
  editor.apps = [...groups.website, ...groups.service, ...groups.database];
  if (isAdmin()) {
    editor.apps.unshift(
      { scope: 'project', id: 'all', name: 'All websites and services (including future ones)', kind: 'all-projects' },
      { scope: 'database', id: 'all', name: 'All databases (including future ones)', kind: 'all-databases' },
    );
  }
  const panelHeld = (user?.access?.panel || []);
  const panelAllowed = PANEL_ACCOUNT_POWERS.filter((p) => hasPanelPower(p));
  const canEditProfile = isNew || isAdmin() || hasPanelPower('users_edit');

  const section = (title, html, note = '') => `<div class="acct-editor-section">
    <div class="acct-editor-heading">${title}</div>${note ? `<p class="acct-editor-note">${note}</p>` : ''}${html}</div>`;

  const profile = `<div class="acct-grid-2">
      <div class="form-group"><label class="form-label" for="ue-name">Full name</label>
        <input id="ue-name" class="form-control" maxlength="80" value="${escapeHtml(user?.displayName || '')}" ${canEditProfile ? '' : 'disabled'}></div>
      <div class="form-group"><label class="form-label" for="ue-email">Email</label>
        <input id="ue-email" type="email" class="form-control" value="${escapeHtml(user?.email || '')}" placeholder="name@company.com" ${canEditProfile ? '' : 'disabled'}></div>
      <div class="form-group"><label class="form-label" for="ue-username">Username</label>
        <input id="ue-username" class="form-control" value="${escapeHtml(user?.username || '')}" ${isNew ? '' : 'disabled'} autocomplete="off"></div>
      <div class="form-group"><label class="form-label" for="ue-password">${isNew ? 'Temporary password' : 'Reset password'}</label>
        <div style="display:flex;gap:6px"><input id="ue-password" type="text" class="form-control" autocomplete="new-password" placeholder="${isNew ? 'At least 8 characters' : 'Leave blank to keep'}" ${canEditProfile ? '' : 'disabled'}>
        <button type="button" class="btn btn-secondary btn-sm" id="ue-generate" ${canEditProfile ? '' : 'disabled'}>Generate</button></div></div>
    </div>
    <label class="acct-check" style="margin-top:4px"><input type="checkbox" id="ue-must-change" ${isNew || user?.mustChangePassword ? 'checked' : ''} ${canEditProfile && !(user?.isMainAdmin) ? '' : 'disabled'}> Must change password at next sign-in</label>
    ${!isNew && !user.isMainAdmin && user.id !== me?.id ? `<label class="acct-check" style="margin-top:8px"><input type="checkbox" id="ue-disabled" ${user.disabled ? 'checked' : ''} ${canEditProfile ? '' : 'disabled'}> Account disabled (signs the user out)</label>` : ''}`;

  const role = canRole ? section('Role', `<select id="ue-role" class="form-control" style="max-width:260px">
      <option value="user" ${!targetIsAdmin ? 'selected' : ''}>User: only what you give them</option>
      <option value="admin" ${targetIsAdmin ? 'selected' : ''}>Admin: full access to everything</option>
    </select>`, 'Only the main admin can make or remove admins.') : '';

  const panel = section('Admin panel permissions', `<div class="acct-power-grid">${PANEL_ACCOUNT_POWERS.map((p) => {
    const ok = panelAllowed.includes(p);
    return `<label class="acct-check ${ok ? '' : 'is-locked'}"><input type="checkbox" data-panel-power value="${p}" ${panelHeld.includes(p) ? 'checked' : ''} ${ok ? '' : 'disabled'}> ${escapeHtml(powerLabel(p))}</label>`;
  }).join('')}</div>`, 'Lets this user help run accounts. Nodes, VPS modules, and platform settings stay admin only.');

  const appGroup = (title, icon, list) => list.length ? `<div class="acct-app-group">
      <div class="acct-app-group-title">${title}</div>${list.map((a) => appRow(a, user)).join('')}</div>` : '';
  const everyApp = editor.apps.filter((a) => a.id === 'all');
  const apps = section('App access', `
      ${appGroup('Every app', '', everyApp)}
      ${appGroup('Websites', '', groups.website)}
      ${appGroup('Services', '', groups.service)}
      ${appGroup('Databases', '', groups.database)}
      ${editor.apps.length ? '' : '<p class="acct-muted">There are no apps you can give access to.</p>'}`,
    'Tick an app to let this user open it, then tick what they can do there. "Manage access" lets them give others the permissions they hold.');

  byId('user-editor-title').textContent = isNew ? 'Create account' : `Edit ${user.displayName || user.username}`;
  byId('user-editor-body').innerHTML = section('Account', profile)
    + role
    + `<div id="ue-access-wrap" ${targetIsAdmin ? 'hidden' : ''}>${panel}${apps}</div>`
    + `<p class="acct-editor-note" id="ue-admin-note" ${targetIsAdmin ? '' : 'hidden'} style="margin-top:16px">Admins have full access to every app and setting.</p>`;

  const footer = byId('user-editor-footer');
  footer.querySelectorAll('[data-editor-extra]').forEach((el) => el.remove());
  if (!isNew && isAdmin() && !user.isMainAdmin && user.id !== me?.id && (me?.isMainAdmin || !user.isAdmin)) {
    footer.insertAdjacentHTML('afterbegin', `<button class="btn btn-danger" data-editor-extra id="ue-delete" style="margin-right:auto">Delete account</button>`);
  }
  if (!isNew && isAdmin() && user.totpEnabled && (me?.isMainAdmin || !user.isAdmin)) {
    footer.insertAdjacentHTML('afterbegin', `<button class="btn btn-secondary" data-editor-extra id="ue-reset-2fa">Reset 2FA</button>`);
  }
  byId('user-editor-save').textContent = isNew ? 'Create account' : 'Save changes';
}

export function openUserEditor(userId) {
  const user = userId ? usersCache.find((u) => u.id === userId) : null;
  if (userId && !user) return;
  editor = { user, apps: [] };
  renderEditor();
  const body = byId('user-editor-body');
  if (body) body.scrollTop = 0;
  openModal('modal-user-editor');
}
window.openUserEditor = openUserEditor;

byId('user-editor-body')?.addEventListener('change', (e) => {
  const t = e.target;
  if (t.matches('[data-app-toggle]')) {
    const name = t.getAttribute('data-app-toggle');
    const body = document.querySelector(`[data-app-body="${name}"]`);
    if (body) body.hidden = !t.checked;
  }
  if (t.matches('[data-power-of], [data-app-toggle]')) {
    const name = t.getAttribute('data-power-of') || t.getAttribute('data-app-toggle');
    const app = editor?.apps.find((a) => `${a.scope}:${a.id}` === name);
    const on = document.querySelector(`[data-app-toggle="${name}"]`)?.checked;
    const powers = Array.from(document.querySelectorAll(`[data-power-of="${name}"]:checked`)).map((c) => c.value);
    const sum = document.querySelector(`[data-app-summary="${name}"]`);
    if (sum && app) sum.textContent = on ? (powers.length ? powers.map((p) => powerLabel(p, app.kind)).join(', ') : 'Can open it') : '';
  }
  if (t.id === 'ue-role') {
    const admin = t.value === 'admin';
    byId('ue-access-wrap').hidden = admin;
    byId('ue-admin-note').hidden = !admin;
  }
});

byId('user-editor-body')?.addEventListener('click', (e) => {
  if (e.target.id !== 'ue-generate') return;
  const alphabet = 'ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789';
  const bytes = crypto.getRandomValues(new Uint8Array(14));
  byId('ue-password').value = Array.from(bytes, (b) => alphabet[b % alphabet.length]).join('');
});

byId('user-editor-footer')?.addEventListener('click', async (e) => {
  const user = editor?.user;
  if (!user) return;
  if (e.target.id === 'ue-delete') {
    const ok = await showConfirmDialog({
      title: 'Delete account',
      message: `Delete ${user.username}'s account? They are signed out and lose all access. This can't be undone.`,
      confirmText: 'Delete account',
      danger: true,
    });
    if (!ok) return;
    try {
      await api(`/api/users/${user.id}`, { method: 'DELETE' });
      closeModal('modal-user-editor');
      toast(`Deleted ${user.username}.`, 'success');
      await loadUsersView();
    } catch (err) { toast(err.message, 'error'); }
  }
  if (e.target.id === 'ue-reset-2fa') {
    const ok = await showConfirmDialog({ title: 'Reset 2FA', message: `Turn off two-factor authentication for ${user.username}? They can set it up again from My account.`, confirmText: 'Reset 2FA', danger: true });
    if (!ok) return;
    try {
      await api(`/api/users/${user.id}/reset-2fa`, { method: 'POST' });
      toast(`2FA reset for ${user.username}.`, 'success');
      await loadUsersView();
      openUserEditor(user.id);
    } catch (err) { toast(err.message, 'error'); }
  }
});

function collectAppSelections() {
  return (editor?.apps || []).map((app) => {
    const name = `${app.scope}:${app.id}`;
    const toggle = document.querySelector(`[data-app-toggle="${name}"]`);
    const on = Boolean(toggle?.checked);
    const powers = Array.from(document.querySelectorAll(`[data-power-of="${name}"]:checked`)).map((c) => c.value);
    return { app, on, powers, locked: Boolean(toggle?.disabled) };
  });
}

const sameSet = (a, b) => a.length === b.length && a.every((x) => b.includes(x));

byId('user-editor-save')?.addEventListener('click', async () => {
  if (!editor) return;
  const { user } = editor;
  const isNew = !user;
  const btn = byId('user-editor-save');
  const val = (id) => byId(id)?.value ?? '';
  const role = byId('ue-role')?.value;
  const password = val('ue-password').trim();
  const panelPowers = Array.from(document.querySelectorAll('[data-panel-power]:checked')).map((c) => c.value);
  const selections = collectAppSelections();

  if (isNew && password.length < 8) {
    toast('Temporary password must be at least 8 characters.', 'warning');
    return;
  }
  if (!isNew && password && password.length < 8) {
    toast('New password must be at least 8 characters.', 'warning');
    return;
  }

  btn.disabled = true;
  try {
    if (isNew) {
      const body = {
        username: val('ue-username').trim(),
        email: val('ue-email').trim(),
        displayName: val('ue-name').trim(),
        password,
        mustChangePassword: Boolean(byId('ue-must-change')?.checked),
        role: role || 'user',
        panelPowers: role === 'admin' ? [] : panelPowers,
        grants: role === 'admin' ? [] : selections.filter((s) => s.on).map((s) => ({ scope: s.app.scope, id: s.app.id, powers: s.powers })),
      };
      await api('/api/users', { method: 'POST', body });
      toast(`Account created for ${body.username}.`, 'success');
    } else {
      const body = {};
      if (!byId('ue-name')?.disabled) {
        body.displayName = val('ue-name').trim();
        body.email = val('ue-email').trim();
        body.mustChangePassword = Boolean(byId('ue-must-change')?.checked);
        if (password) body.password = password;
        if (byId('ue-disabled')) body.disabled = Boolean(byId('ue-disabled').checked);
      }
      if (role) body.role = role;
      const targetAdmin = role ? role === 'admin' : Boolean(user.isAdmin || user.isMainAdmin);
      if (!targetAdmin && !byId('ue-access-wrap')?.hidden) {
        const before = (user.access?.panel || []).filter((p) => PANEL_ACCOUNT_POWERS.includes(p));
        if (!sameSet(before, panelPowers)) body.panelPowers = panelPowers;
      }
      if (Object.keys(body).length) {
        await api(`/api/users/${user.id}`, { method: 'PUT', body });
      }
      if (!targetAdmin) {
        for (const s of selections) {
          if (s.locked) continue;
          const before = existingPowers(user, s.app);
          const path = `/api/access/${s.app.scope}/${s.app.id}/${user.id}`;
          if (!s.on && before !== null) {
            await api(path, { method: 'DELETE' });
          } else if (s.on && (before === null || !sameSet(before, s.powers))) {
            await api(path, { method: 'PUT', body: { powers: s.powers } });
          }
        }
      }
      toast(`Saved ${user.username}.`, 'success');
    }
    closeModal('modal-user-editor');
    await loadUsersView();
  } catch (err) {
    toast(err.message, 'error');
  } finally {
    btn.disabled = false;
  }
});

// ============================================================================
// Admin: requests
// ============================================================================
let requestsFilter = 'pending';

async function loadRequestsView() {
  const tbody = byId('requests-list-body');
  if (!tbody) return;
  try {
    const list = await api(`/api/requests?status=${requestsFilter}`, { noCache: true });
    if (!list.length) {
      tbody.innerHTML = `<tr><td colspan="5" class="acct-empty">${requestsFilter === 'pending' ? 'No pending requests.' : 'No requests yet.'}</td></tr>`;
    } else {
      tbody.innerHTML = list.map((r) => {
        const statusBadge = r.status === 'pending'
          ? '<span class="badge badge-warning">Pending</span>'
          : r.status === 'approved'
            ? `<span class="badge badge-success">Approved</span>${r.reviewedBy ? `<div class="acct-muted">by ${escapeHtml(r.reviewedBy)}</div>` : ''}`
            : `<span class="badge badge-danger">Rejected</span>${r.reason ? `<div class="acct-muted">${escapeHtml(r.reason)}</div>` : ''}`;
        const actions = r.status === 'pending'
          ? `<button class="btn btn-primary btn-sm" onclick="approveAccountRequest(${r.id})">Approve</button>
             <button class="btn btn-secondary btn-sm" onclick="rejectAccountRequest(${r.id})">Reject</button>`
          : (isAdmin() ? `<button class="btn btn-secondary btn-sm" onclick="deleteAccountRequest(${r.id})">Remove</button>` : '');
        return `<tr>
          <td>${personCell({ displayName: r.displayName, username: r.username }, `@${escapeHtml(r.username)}`)}</td>
          <td>${escapeHtml(r.email)}</td>
          <td title="${escapeHtml(formatDate(r.createdAt))}">${escapeHtml(formatRelativeTime(r.createdAt))}${r.ip && r.ip !== 'unknown' ? `<div class="acct-muted">${escapeHtml(r.ip)}</div>` : ''}</td>
          <td>${statusBadge}</td>
          <td><div class="acct-actions">${actions}</div></td>
        </tr>`;
      }).join('');
    }
  } catch (err) {
    tbody.innerHTML = `<tr><td colspan="5" class="acct-empty">${escapeHtml(err.message)}</td></tr>`;
  }
  void refreshRequestBadge();
}

byId('requests-filter')?.addEventListener('click', (e) => {
  const b = e.target.closest('[data-requests-filter]');
  if (!b) return;
  requestsFilter = b.getAttribute('data-requests-filter');
  document.querySelectorAll('#requests-filter .pill').forEach((p) => p.classList.toggle('active', p === b));
  void loadRequestsView();
});

window.approveAccountRequest = async (id) => {
  try {
    const res = await api(`/api/requests/${id}/approve`, { method: 'POST' });
    await loadRequestsView();
    const canSetAccess = isAdmin() || hasPower('access_manage');
    if (canSetAccess && res.userId) {
      const ok = await showConfirmDialog({
        title: 'Account created',
        message: 'The account exists now but has no access yet. Give them access to apps?',
        confirmText: 'Set access',
      });
      if (ok) {
        usersCache = await api('/api/users', { noCache: true });
        openUserEditor(res.userId);
      }
    } else {
      toast('Request approved. The account is ready.', 'success');
    }
  } catch (err) { toast(err.message, 'error'); }
};

window.rejectAccountRequest = async (id) => {
  const reason = await showPromptDialog({
    title: 'Reject request',
    message: 'Optional: why is this request rejected? Only reviewers see the reason.',
    placeholder: 'Reason (optional)',
    confirmText: 'Reject',
    danger: true,
  });
  if (reason === null || reason === undefined || reason === false) return;
  try {
    await api(`/api/requests/${id}/reject`, { method: 'POST', body: { reason: String(reason || '') } });
    toast('Request rejected.', 'success');
    await loadRequestsView();
  } catch (err) { toast(err.message, 'error'); }
};

window.deleteAccountRequest = async (id) => {
  try {
    await api(`/api/requests/${id}`, { method: 'DELETE' });
    await loadRequestsView();
  } catch (err) { toast(err.message, 'error'); }
};

// ============================================================================
// Admin: audit
// ============================================================================
let auditCache = [];

function renderAdminAudit() {
  const tbody = byId('admin-audit-body');
  if (!tbody) return;
  const q = (byId('admin-audit-search')?.value || '').trim().toLowerCase();
  const list = auditCache.filter((r) => !q
    || [r.actor, r.action, r.target, r.detail, r.projectName].some((v) => String(v || '').toLowerCase().includes(q)));
  tbody.innerHTML = list.length ? list.slice(0, 500).map((r) => `<tr>
      <td title="${escapeHtml(formatDate(r.ts))}">${escapeHtml(formatRelativeTime(r.ts))}</td>
      <td>${escapeHtml(r.actor || '')}</td>
      <td><code>${escapeHtml(r.action || '')}</code></td>
      <td>${escapeHtml(r.target || r.projectName || '')}</td>
      <td class="acct-muted">${escapeHtml(r.detail || '')}</td>
    </tr>`).join('') : '<tr><td colspan="5" class="acct-empty">No activity yet.</td></tr>';
}

async function loadAdminAuditView() {
  try {
    const rows = await api('/api/audit?limit=1000', { noCache: true });
    auditCache = Array.isArray(rows) ? rows : [];
    renderAdminAudit();
  } catch (err) {
    const tbody = byId('admin-audit-body');
    if (tbody) tbody.innerHTML = `<tr><td colspan="5" class="acct-empty">${escapeHtml(err.message)}</td></tr>`;
  }
}
byId('admin-audit-search')?.addEventListener('input', renderAdminAudit);

// ============================================================================
// App Permissions tab (websites, services, databases)
// ============================================================================
let accessOpen = new Set();

function currentApp() {
  if (state.selectedEntityKind === 'database') {
    const d = getSelectedDatabase();
    return d ? { scope: 'database', id: d.id, name: d.name } : null;
  }
  const p = getSelectedProject();
  return p ? { scope: 'project', id: p.id, name: p.name } : null;
}

/** Whether the Permissions tab should show for the app in view. */
export function canManageCurrentAppAccess() {
  return isAdmin() || hasPower('access_manage');
}

async function loadAccessView() {
  const body = byId('access-view-body');
  const app = currentApp();
  if (!body) return;
  if (!app) {
    body.innerHTML = '<p class="acct-muted">Open a website, service, or database first.</p>';
    return;
  }
  const title = byId('access-view-title');
  if (title) title.textContent = `${app.name}: permissions`;
  try {
    const view = await api(`/api/access/${app.scope}/${app.id}`, { noCache: true });
    renderAccessView(view);
  } catch (err) {
    body.innerHTML = `<p class="acct-muted">${escapeHtml(err.message)}</p>`;
  }
}

function renderAccessView(view) {
  const body = byId('access-view-body');
  const kindWord = view.kind === 'database' ? 'database' : view.kind;
  const intro = isAdmin()
    ? `Admins always have full access. Add people here and tick what they can do on this ${kindWord}.`
    : `You can give others only the permissions you have on this ${kindWord}. Greyed-out items are ones you don't have.`;
  const add = view.candidates.length ? `<div class="acct-add-row">
      <select id="access-add-select" class="form-control">
        <option value="">Add a person</option>
        ${view.candidates.map((c) => `<option value="${c.id}">${escapeHtml(c.displayName ? `${c.displayName} (@${c.username})` : c.username)}</option>`).join('')}
      </select>
      <button class="btn btn-primary" id="access-add-btn">Add</button>
    </div>` : '';

  const rows = view.users.map((u) => {
    const open = accessOpen.has(u.id) && !u.viaAll;
    const summary = u.powers.length ? u.powers.map((p) => powerLabel(p, view.kind)).join(', ') : 'Can open it, no actions';
    const sub = u.viaAll
      ? `Has access to every ${view.scope === 'database' ? 'database' : 'website and service'}: ${escapeHtml(summary)}`
      : escapeHtml(summary);
    const checks = `<div class="acct-power-grid">${view.catalog.map((p) => {
      const ok = u.editable && view.grantable.includes(p);
      return `<label class="acct-check ${ok ? '' : 'is-locked'}" title="${ok ? '' : "You can only give permissions you have yourself"}">
        <input type="checkbox" data-access-user="${u.id}" value="${p}" ${u.powers.includes(p) ? 'checked' : ''} ${ok ? '' : 'disabled'}>
        ${escapeHtml(powerLabel(p, view.kind))}
      </label>`;
    }).join('')}</div>`;
    return `<div class="acct-access-item ${open ? 'is-open' : ''}" data-access-item="${u.id}">
      <div class="acct-access-head" data-access-toggle="${u.id}">
        ${personCell(u, sub)}
        ${u.viaAll ? '<span class="badge badge-neutral">From every-app access</span>' : `
          ${u.editable ? `<button class="btn btn-secondary btn-sm" data-access-remove="${u.id}" title="Remove access">Remove</button>` : '<span class="badge badge-neutral" title="This person has permissions you don\'t">Locked</span>'}
          <svg class="acct-chevron" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="6 9 12 15 18 9"></polyline></svg>`}
      </div>
      ${open ? `<div class="acct-access-body">${checks}</div>` : ''}
    </div>`;
  }).join('');

  body.innerHTML = `<div class="acct-callout">${intro}</div>
    ${add}
    <div class="acct-access-list">${rows || '<p class="acct-empty" style="margin:0">Nobody besides admins can use this yet.</p>'}</div>`;
  body.dataset.scope = view.scope;
  body.dataset.id = String(view.id);
}

function accessPath(userId) {
  const body = byId('access-view-body');
  return `/api/access/${body.dataset.scope}/${body.dataset.id}/${userId}`;
}

byId('access-view-body')?.addEventListener('click', async (e) => {
  const remove = e.target.closest('[data-access-remove]');
  if (remove) {
    e.stopPropagation();
    const id = Number(remove.getAttribute('data-access-remove'));
    const ok = await showConfirmDialog({ title: 'Remove access', message: 'This person will no longer be able to open this app.', confirmText: 'Remove', danger: true });
    if (!ok) return;
    try {
      await api(accessPath(id), { method: 'DELETE' });
      accessOpen.delete(id);
      toast('Access removed.', 'success');
      await loadAccessView();
    } catch (err) { toast(err.message, 'error'); }
    return;
  }
  if (e.target.id === 'access-add-btn') {
    const id = Number(byId('access-add-select')?.value || 0);
    if (!id) {
      toast('Pick a person to add.', 'warning');
      return;
    }
    try {
      await api(accessPath(id), { method: 'PUT', body: { powers: [] } });
      accessOpen.add(id);
      await loadAccessView();
      toast('Added. Now tick what they can do.', 'success');
    } catch (err) { toast(err.message, 'error'); }
    return;
  }
  const toggle = e.target.closest('[data-access-toggle]');
  if (toggle && !e.target.closest('input, label')) {
    const id = Number(toggle.getAttribute('data-access-toggle'));
    if (accessOpen.has(id)) accessOpen.delete(id); else accessOpen.add(id);
    await loadAccessView();
  }
});

byId('access-view-body')?.addEventListener('change', async (e) => {
  const t = e.target;
  if (!t.matches('[data-access-user]')) return;
  const id = Number(t.getAttribute('data-access-user'));
  const powers = Array.from(document.querySelectorAll(`[data-access-user="${id}"]:checked`)).map((c) => c.value);
  try {
    await api(accessPath(id), { method: 'PUT', body: { powers } });
    toast('Saved.', 'success', 1500);
    await loadAccessView();
  } catch (err) {
    t.checked = !t.checked;
    toast(err.message, 'error');
  }
});

// ============================================================================
// Platform settings: self-registration toggle
// ============================================================================
export async function loadRegistrationSetting() {
  const box = byId('setting-allow-registration');
  const card = byId('settings-accounts-card');
  if (card) card.hidden = !hasPanelPower('settings_manage');
  if (!box || !hasPanelPower('settings_manage')) return;
  try {
    const res = await api('/api/auth/registration', { noCache: true });
    box.checked = Boolean(res.enabled);
  } catch (_) { /* leave as is */ }
}

byId('setting-allow-registration')?.addEventListener('change', async (e) => {
  const on = e.target.checked;
  try {
    await api('/api/settings', { method: 'PUT', body: { allow_registration: on ? 'true' : 'false' } });
    toast(on ? 'Self-registration is on.' : 'Self-registration is off.', 'success');
  } catch (err) {
    e.target.checked = !on;
    toast(err.message, 'error');
  }
});

// ============================================================================
// View router hook
// ============================================================================
export function loadAccountsView(view) {
  switch (view) {
    case 'account': loadAccountView(); break;
    case 'users': void loadUsersView(); break;
    case 'requests': void loadRequestsView(); break;
    case 'admin-audit': void loadAdminAuditView(); break;
    case 'access': void loadAccessView(); break;
    default: break;
  }
}

/** Sends people away from admin views they may not open (deep links, revoked powers). */
export function guardView(view) {
  if (isAdminView(view) && !adminViewAllowed(view)) return 'overview';
  if (view === 'access' && !canManageCurrentAppAccess()) return 'deployments';
  return view;
}
