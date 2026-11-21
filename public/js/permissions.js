/**
 * Client-side permission checks (UI gating only; the server enforces every rule).
 *
 * The signed-in user carries `access` = { panel: [...], projects: { "<id>"|"*": [...] },
 * databases: { "<id>"|"*": [...] } }. Admins (and the main admin) hold every power.
 */

import { state, WEBSITE_WORKSPACE_TABS } from './state.js';

export const PANEL_ACCOUNT_POWERS = [
  'users_view', 'users_create', 'users_edit', 'requests_review',
  'create_websites', 'create_services', 'create_databases',
];

/** Checklist per kind of app, in display order (mirrors the server's kind_catalog). */
export const APP_POWER_CATALOG = {
  website: ['logs', 'files', 'files_write', 'deploy', 'process', 'backups', 'env', 'domains', 'projects_manage', 'activity', 'access_manage', 'app_delete'],
  service: ['logs', 'files', 'files_write', 'deploy', 'process', 'backups', 'env', 'services_manage', 'activity', 'access_manage', 'app_delete'],
  database: ['databases', 'env', 'databases_manage', 'activity', 'access_manage', 'app_delete'],
};
APP_POWER_CATALOG['all-projects'] = Array.from(new Set([...APP_POWER_CATALOG.website, ...APP_POWER_CATALOG.service]));
APP_POWER_CATALOG['all-databases'] = APP_POWER_CATALOG.database;

const LABELS = {
  logs: 'View logs',
  files: 'Read files',
  files_write: 'Edit files',
  deploy: 'Deploy',
  process: 'Start, stop, restart',
  backups: 'Backups',
  env: 'Env variables',
  domains: 'Domains and SSL',
  projects_manage: 'Edit settings',
  services_manage: 'Edit settings',
  activity: 'View activity',
  access_manage: 'Manage access',
  app_delete: 'Delete app (owner)',
  databases: 'View, start, stop',
  databases_manage: 'Manage database',
  users_view: 'View users',
  users_create: 'Create accounts',
  users_edit: 'Edit or disable users',
  requests_review: 'Approve or reject requests',
  create_websites: 'Create websites',
  create_services: 'Create services',
  create_databases: 'Create databases',
  settings_manage: 'Platform settings (earlier)',
  system_manage: 'System and updates (earlier)',
  audit: 'Panel audit log (earlier)',
  monitor: 'Monitoring (earlier)',
  nodes_manage: 'Nodes (earlier)',
};

export function powerLabel(power, kind = '') {
  if (power === 'env' && (kind === 'database' || kind === 'all-databases')) return 'See connection details';
  if (power === 'services_manage' && kind === 'all-projects') return 'Edit service settings';
  if (power === 'projects_manage' && kind === 'all-projects') return 'Edit website settings';
  return LABELS[power] || `${String(power).replace(/_/g, ' ')} (earlier)`;
}

export function isAdmin() {
  return Boolean(state.user && (state.user.isAdmin || state.user.isMainAdmin));
}

function access() {
  return state.user?.access || { panel: [], projects: {}, databases: {} };
}

function holds(list, power) {
  return Array.isArray(list) && (list.includes('*') || list.includes(power));
}

export function hasPanelPower(power) {
  return isAdmin() || holds(access().panel, power);
}

export function hasProjectPower(projectId, power) {
  if (isAdmin()) return true;
  const p = access().projects || {};
  return holds(p[String(projectId)], power) || holds(p['*'], power);
}

export function hasDatabasePower(databaseId, power, linkedProjectId = null) {
  if (isAdmin()) return true;
  const d = access().databases || {};
  return holds(d[String(databaseId)], power)
    || holds(d['*'], power)
    || (linkedProjectId ? hasProjectPower(linkedProjectId, power) : false);
}

/** True when the power is held at panel level or on any app. */
export function hasPowerAnywhere(power) {
  if (isAdmin()) return true;
  const a = access();
  return holds(a.panel, power)
    || Object.values(a.projects || {}).some((l) => holds(l, power))
    || Object.values(a.databases || {}).some((l) => holds(l, power));
}

function linkedProjectOf(db) {
  return db ? (db.linkedProjectId ?? db.linked_project_id ?? null) : null;
}

/** The app the user is looking at (inside a website/service/database workspace), if any. */
export function currentScope() {
  if (!WEBSITE_WORKSPACE_TABS.has(state.currentView)) return null;
  if (state.selectedEntityKind === 'database' && state.selectedDatabaseId) {
    const db = (state.databases || []).find((d) => Number(d.id) === Number(state.selectedDatabaseId));
    return { databaseId: state.selectedDatabaseId, linkedProjectId: linkedProjectOf(db) };
  }
  if (state.selectedProjectId) return { projectId: state.selectedProjectId };
  return null;
}

/**
 * `scope`: omitted = the app in view (or "anywhere" outside a workspace);
 * `'panel'` = panel-level only; `{ projectId }` or `{ databaseId, linkedProjectId }`.
 */
export function hasPower(power, scope) {
  if (!state.user) return false;
  if (isAdmin()) return true;
  const ctx = scope === undefined ? currentScope() : scope;
  if (ctx === 'panel') return hasPanelPower(power);
  if (ctx && ctx.projectId) return hasProjectPower(ctx.projectId, power);
  if (ctx && ctx.databaseId) return hasDatabasePower(ctx.databaseId, power, ctx.linkedProjectId);
  return hasPowerAnywhere(power);
}

export function hasAnyPower(powers, scope) {
  return Array.isArray(powers) && powers.some((p) => hasPower(p, scope));
}

/** Whether the user may create apps of `kind` (`website`, `service`, `database`). */
export function canCreate(kind) {
  const create = { website: 'create_websites', service: 'create_services', database: 'create_databases' }[kind];
  const earlier = { website: 'projects_manage', service: 'services_manage', database: 'databases_manage' }[kind];
  return hasPanelPower(create) || hasPanelPower(earlier);
}

export function databaseScope(db) {
  return { databaseId: db.id, linkedProjectId: linkedProjectOf(db) };
}
