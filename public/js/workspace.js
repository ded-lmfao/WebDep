/**
 * WebDep Workspace Router & Controls Subsystem
 * Workspace tabs navigation, breadcrumbs, banner, and direct process lifecycle controls
 */

import { icon } from './icons.js';
import { loadAccountsView, syncSidebarMode, guardView } from './accounts.js';
import { databaseScope } from './permissions.js';
import {
  state,
  WEBSITE_WORKSPACE_TABS,
  DATABASE_WORKSPACE_TABS,
  escapeHtml,
  getSelectedProject,
  getSelectedDatabase,
  isProjectOnline,
  isProjectMaintenance,
  isDatabaseRunning,
  nodeLabel,
} from './state.js';
import { prefetchWebsiteWorkspace, prefetchDatabaseWorkspace } from './api.js';
import {
  animateViewEnter,
  animateWorkspaceBanner,
  markViewStaggerPending,
  consumePendingViewStagger,
} from './motion_ui.js';

// Icons for the icon-only workspace action buttons (rounded 24px line icons).
const WS_ICON_PATHS = {
  deploy: '<path d="M4.5 16.5c-1.5 1.26-2 5-2 5s3.74-.5 5-2c.71-.84.7-2.13-.09-2.91a2.18 2.18 0 0 0-2.91-.09z"/><path d="m12 15-3-3a22 22 0 0 1 2-3.95A12.88 12.88 0 0 1 22 2c0 2.72-.78 7.5-6 11a22.35 22.35 0 0 1-4 2z"/><path d="M9 12H4s.55-3.03 2-4c1.62-1.08 5 0 5 0"/><path d="M12 15v5s3.03-.55 4-2c1.08-1.62 0-5 0-5"/>',
  start: '<path d="M7 4.5v15a1 1 0 0 0 1.52.85l12-7.5a1 1 0 0 0 0-1.7l-12-7.5A1 1 0 0 0 7 4.5z" fill="currentColor"/>',
  stop: '<path d="M12 2v10"/><path d="M18.4 6.6a9 9 0 1 1-12.77.04"/>',
  restart: '<path d="M21 12a9 9 0 1 1-9-9c2.52 0 4.93 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/>',
  maint: '<path d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94l-3.76 3.76z"/>',
  globe: '<circle cx="12" cy="12" r="10"/><path d="M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20"/><path d="M2 12h20"/>',
  gear: '<path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/>',
  key: '<path d="M2.59 18.4A2 2 0 0 0 2 19.82V21a1 1 0 0 0 1 1h3a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h1a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h.17a2 2 0 0 0 1.42-.59l.81-.81a6.5 6.5 0 1 0-4-4z"/><circle cx="16.5" cy="7.5" r=".5" fill="currentColor"/>',
  trash: '<path d="M3 6h18"/><path d="M19 6v14c0 1-1 2-2 2H7c-1 0-2-1-2-2V6"/><path d="M8 6V4c0-1 1-2 2-2h4c1 0 2 1 2 2v2"/><line x1="10" x2="10" y1="11" y2="17"/><line x1="14" x2="14" y1="11" y2="17"/>',
};

function wsIcon(name) {
  return `<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${WS_ICON_PATHS[name]}</svg>`;
}

let lastBannerEntityKey = null;

// Re-export process controls and lifecycle actions submodule
export * from './workspace_actions.js';

function openWebsiteWorkspace(projectId, tab = null) {
  state.selectedEntityKind = 'project';
  state.selectedProjectId = Number(projectId);
  prefetchWebsiteWorkspace(state.selectedProjectId);
  let targetTab = tab || 'deployments';
  if (targetTab === 'overview') targetTab = 'deployments';
  navigateTo(targetTab, state.selectedProjectId);
}
window.openWebsiteWorkspace = openWebsiteWorkspace;

function openDatabaseWorkspace(dbId, tab = null) {
  state.selectedEntityKind = 'database';
  state.selectedDatabaseId = Number(dbId);
  prefetchDatabaseWorkspace(state.selectedDatabaseId);
  let targetTab = tab || 'deployments';
  if (targetTab === 'overview') targetTab = 'deployments';
  if (!DATABASE_WORKSPACE_TABS.has(targetTab)) targetTab = 'deployments';
  navigateTo(targetTab, `db-${state.selectedDatabaseId}`);
}
window.openDatabaseWorkspace = openDatabaseWorkspace;

function switchWebsiteTab(tab) {
  if (state.selectedEntityKind === 'database') {
    const db = getSelectedDatabase();
    if (!db) {
      navigateTo('databases');
      return;
    }
    const targetTab = DATABASE_WORKSPACE_TABS.has(tab) ? tab : 'deployments';
    navigateTo(targetTab, `db-${db.id}`);
    return;
  }
  const p = getSelectedProject();
  if (!p) {
    navigateTo('projects');
    return;
  }
  navigateTo(tab, p.id);
}
window.switchWebsiteTab = switchWebsiteTab;

function parseOverviewNodeParam(param) {
  if (param === null || param === undefined) return null;
  const raw = String(param).trim();
  const match = /^vps(\d+)$/i.exec(raw) || /^(\d+)$/.exec(raw);
  if (!match) return null;
  const id = Number(match[1]);
  return Number.isInteger(id) && id > 0 ? id : null;
}

function navigateTo(viewName, entityParam = null) {
  if (!state.user || !state.csrf || state.pendingMfaToken) {
    showLogin();
    return;
  }
  if (viewName === 'backups') viewName = 'releases';
  state.currentView = viewName;

  if (viewName === 'overview') {
    const inspectNodeId = parseOverviewNodeParam(entityParam);
    state.overviewInspectNodeId = inspectNodeId;
    state.selectedEntityKind = 'project';
    const routeParam = inspectNodeId ? `vps${inspectNodeId}` : null;
    const targetPath = routeParam ? `/overview/${routeParam}` : '/overview';
    const curPath = window.location.pathname || '/';
    if (curPath !== targetPath || window.location.hash) {
      try {
        window.history.pushState({ view: 'overview', param: routeParam }, '', targetPath);
      } catch (_) { }
    }
    renderCurrentView();
    return;
  }
  state.overviewInspectNodeId = null;

  if (typeof entityParam === 'string' && entityParam.startsWith('db-')) {
    const dbId = Number(entityParam.slice(3));
    if (dbId) {
      state.selectedEntityKind = 'database';
      state.selectedDatabaseId = dbId;
    }
  } else if (entityParam !== null && entityParam !== undefined) {
    state.selectedEntityKind = 'project';
    state.selectedProjectId = Number(entityParam);
  } else if (!WEBSITE_WORKSPACE_TABS.has(viewName)) {
    // Switching back to a top-level view clears database workspace scope
    state.selectedEntityKind = 'project';
  }

  let routeParam = entityParam;
  if (!routeParam && WEBSITE_WORKSPACE_TABS.has(viewName) && state.selectedEntityKind === 'database' && state.selectedDatabaseId) {
    routeParam = `db-${state.selectedDatabaseId}`;
  }
  const targetPath = routeParam ? `/${viewName}/${routeParam}` : `/${viewName}`;
  const curPath = window.location.pathname || '/';
  if (curPath !== targetPath || window.location.hash) {
    try {
      window.history.pushState({ view: viewName, param: routeParam ?? null }, '', targetPath);
    } catch (_) { }
  }
  renderCurrentView();
}
window.navigateTo = navigateTo;

function trimEdgeSlashes(path) {
  let start = 0;
  let end = path.length;
  while (start < end && path.charCodeAt(start) === 47) start += 1;
  while (end > start && path.charCodeAt(end - 1) === 47) end -= 1;
  return path.slice(start, end);
}

function handleHashRoute() {
  if (!state.user || !state.csrf || state.pendingMfaToken) {
    resetCleanUrl();
    showLogin();
    return;
  }

  let rawPath = '';
  if (window.location.hash && window.location.hash.startsWith('#/')) {
    rawPath = window.location.hash.slice(2);
  } else {
    rawPath = trimEdgeSlashes(window.location.pathname || '/');
  }

  const parts = (rawPath || 'overview').split('/').filter(Boolean);
  let view = parts[0] || 'overview';
  if (view === 'backups') view = 'releases';

  let routeParam = parts[1] || null;
  if (view === 'overview') {
    state.selectedEntityKind = 'project';
    const inspectNodeId = parseOverviewNodeParam(routeParam);
    state.overviewInspectNodeId = inspectNodeId;
    routeParam = inspectNodeId ? `vps${inspectNodeId}` : null;
  } else if (routeParam && routeParam.startsWith('db-')) {
    state.overviewInspectNodeId = null;
    const dbId = Number(routeParam.slice(3));
    state.selectedEntityKind = 'database';
    if (dbId) state.selectedDatabaseId = dbId;
    if (!DATABASE_WORKSPACE_TABS.has(view)) view = 'deployments';
  } else {
    state.overviewInspectNodeId = null;
    state.selectedEntityKind = 'project';
    const pid = routeParam ? Number(routeParam) : null;
    if (pid) state.selectedProjectId = pid;
  }

  if (!document.getElementById(`view-${view}`)) {
    view = 'overview';
    routeParam = null;
  }

  const canonicalPath = routeParam ? `/${view}/${routeParam}` : `/${view}`;
  if ((window.location.pathname || '/') !== canonicalPath || window.location.hash) {
    try {
      window.history.replaceState({ view, param: routeParam }, '', canonicalPath);
    } catch (_) { }
  }

  state.currentView = view;
  renderCurrentView();
}
window.handleHashRoute = handleHashRoute;

window.addEventListener('popstate', handleHashRoute);
window.addEventListener('hashchange', handleHashRoute);
window.addEventListener('pageshow', (e) => {
  if (e.persisted && (!state.user || !state.csrf)) {
    void initAuth();
  }
});

function updateBreadcrumbs() {
  const secEl = document.getElementById('breadcrumb-section');
  if (!secEl) return;

  const isInsideCard = WEBSITE_WORKSPACE_TABS.has(state.currentView);
  const tabLabels = {
    deployments: 'Overview',
    github: 'GitHub & Commits',
    logs: 'Live Logs',
    env: 'Environment (.env)',
    files: 'Files',
    releases: 'Releases',
    rollbacks: 'Rollbacks',
    monitor: 'Monitoring',
    audit: 'Activity',
    access: 'Permissions',
  };

  if (isInsideCard && state.selectedEntityKind === 'database') {
    const db = getSelectedDatabase();
    if (db) {
      secEl.innerHTML = `<span style="cursor:pointer; color:var(--text-muted);" onclick="navigateTo('databases')">Databases</span> &rsaquo; <strong style="color:var(--text-main);">${escapeHtml(db.name)}</strong> &rsaquo; <span style="color:var(--accent-light);">${escapeHtml(tabLabels[state.currentView] || state.currentView)}</span>`;
      return;
    }
  }

  const p = getSelectedProject();
  if (isInsideCard && p) {
    const parentView = p.project_kind === 'service' ? 'services' : 'projects';
    const parentLabel = p.project_kind === 'service' ? 'Services' : 'Websites';
    secEl.innerHTML = `<span style="cursor:pointer; color:var(--text-muted);" onclick="navigateTo('${parentView}')">${parentLabel}</span> &rsaquo; <strong style="color:var(--text-main);">${escapeHtml(p.name)}</strong> &rsaquo; <span style="color:var(--accent-light);">${escapeHtml(tabLabels[state.currentView] || state.currentView)}</span>`;
  } else if (state.currentView === 'overview' && state.overviewInspectNodeId) {
    const nodeList = Array.isArray(state.overview?.nodes) && state.overview.nodes.length ? state.overview.nodes : (state.nodes || []);
    const node = nodeList.find((n) => Number(n.id) === Number(state.overviewInspectNodeId));
    const inspectLabel = node ? nodeLabel(node) : `VPS ${state.overviewInspectNodeId}`;
    secEl.innerHTML = `<span style="cursor:pointer; color:var(--text-muted);" onclick="navigateTo('overview')">System Overview</span> &rsaquo; <strong style="color:var(--text-main);">${escapeHtml(inspectLabel)}</strong>`;
  } else {
    const globalLabels = {
      overview: 'System Overview',
      projects: 'Hosted Websites',
      services: 'Backend Services',
      databases: 'Managed Databases',
      nodes: 'Cluster Nodes',
      modules: 'VPS Modules',
      settings: 'Platform Settings',
      account: 'My Account',
      users: 'Admin Panel › Users',
      requests: 'Admin Panel › Requests',
      'admin-audit': 'Admin Panel › Audit',
      ips: 'Admin Panel › IP Addresses',
      moves: 'Admin Panel › Move Apps',
      limits: 'Admin Panel › App Limits',
    };
    secEl.textContent = globalLabels[state.currentView] || state.currentView;
  }
}

function renderDatabaseWorkspaceHeader(banner) {
  const d = getSelectedDatabase();
  if (!d) {
    banner.style.display = 'none';
    return;
  }

  banner.style.display = 'block';
  const resItem = (state.overview?.resources || []).find((x) => (x.databaseId === d.id || x.database_id === d.id));
  const isRunning = isDatabaseRunning(d, resItem);
  const isStarting = d.status === 'starting';
  const isStopping = d.status === 'stopping';
  const isError = d.status === 'error';

  const dot = document.getElementById('ws-status-dot');
  if (dot) dot.className = `status-dot ${isRunning ? 'online' : (isStarting || isStopping) ? 'maintenance' : 'offline'}`;

  const title = document.getElementById('ws-project-title');
  if (title) title.textContent = d.name;

  const breadcrumbSlug = document.getElementById('ws-breadcrumb-slug');
  if (breadcrumbSlug) breadcrumbSlug.textContent = d.name;

  const backCrumb = document.querySelector('.ws-back-crumb');
  if (backCrumb) {
    backCrumb.setAttribute('onclick', "navigateTo('databases')");
    backCrumb.setAttribute('title', 'Back to Managed Databases');
    const s = backCrumb.querySelector('span');
    if (s) s.textContent = 'Databases';
  }

  const badge = document.getElementById('ws-status-badge');
  if (badge) {
    if (isRunning) {
      badge.className = 'badge badge-success';
      badge.textContent = 'Online';
    } else if (isStarting) {
      badge.className = 'badge badge-warning';
      badge.textContent = 'Starting';
    } else if (isStopping) {
      badge.className = 'badge badge-warning';
      badge.textContent = 'Stopping';
    } else if (isError) {
      badge.className = 'badge badge-danger';
      badge.textContent = 'Error';
    } else {
      badge.className = 'badge badge-danger';
      badge.textContent = 'Stopped';
    }
  }

  const pidFromContainer = (d.containerId || d.container_id || '').startsWith('pid-')
    ? Number((d.containerId || d.container_id).slice(4))
    : null;
  const effectivePid = d.pid || resItem?.pid || pidFromContainer;
  const diskStr = d.diskFormatted || d.disk_formatted || resItem?.diskFormatted || resItem?.disk_formatted || '';
  const accessMode = d.accessMode || d.access_mode || 'local';
  const dbName = d.dbName || d.db_name || 'app';
  const dbUser = d.dbUser || d.db_user || 'app_user';
  const linkedName = d.linkedProjectName || d.linked_project_name || '';

  const bannerSig = `db:${d.id}:${d.name}:${d.status}:${isRunning ? 1 : 0}:${d.port}:${accessMode}:${d.domain || ''}:${dbName}:${dbUser}:${linkedName}:${diskStr}:${effectivePid || ''}`;
  if (state.lastWsBannerSig !== bannerSig) {
    state.lastWsBannerSig = bannerSig;

    const links = document.getElementById('ws-project-links');
    if (links) {
      let accessText = `${icon('lock', { size: 11 })} Localhost`;
      if (accessMode === 'allowlist') accessText = `${icon('shield', { size: 11 })} IP whitelist`;
      else if (accessMode === 'public') accessText = `${icon('globe', { size: 11 })} Public internet`;

      links.innerHTML = `
        ${d.domain ? `
          <span class="ws-meta-item" style="color:var(--accent-light); font-weight:600; display:inline-flex; align-items:center; gap:4px;" title="Assigned Database Domain">
            ${icon('globe', { size: 12 })}
            <span>${escapeHtml(d.domain)}</span>
          </span>
          <span class="ws-meta-sep">&bull;</span>
        ` : ''}
        <span class="ws-meta-item" style="color:var(--accent-light); font-weight:600; display:inline-flex; align-items:center; gap:4px;">
          ${icon('postgres', { size: 14 })}
          <span>PostgreSQL Cluster</span>
        </span>
        <span class="ws-meta-sep">&bull;</span>
        <span class="ws-meta-item" title="Database &amp; Role">
          <code class="ws-meta-sha">${escapeHtml(dbUser)}@${escapeHtml(dbName)}</code>
        </span>
        ${d.port ? `
          <span class="ws-meta-sep">&bull;</span>
          <span class="ws-meta-item" title="Listening Port">Port <strong style="color:var(--text-main); font-weight:600;">${d.port}</strong></span>
        ` : ''}
        ${diskStr ? `
          <span class="ws-meta-sep">&bull;</span>
          <span class="ws-meta-item" title="On-Disk Storage">Disk <strong style="color:var(--text-main); font-weight:600;">${diskStr}</strong></span>
        ` : ''}
        <span class="ws-meta-sep">&bull;</span>
        <span class="ws-meta-item" title="Network Access Policy (locked at creation)" style="font-size:11px;">${accessText}</span>
        ${linkedName ? `
          <span class="ws-meta-sep">&bull;</span>
          <span class="ws-meta-item" title="Linked App">Linked <strong style="color:var(--text-main); font-weight:600;">${escapeHtml(linkedName)}</strong></span>
        ` : ''}
        ${(isRunning && effectivePid) ? `
          <span class="ws-meta-sep">&bull;</span>
          <span class="ws-meta-item" title="Master Postmaster PID">PID <strong style="color:var(--text-main); font-weight:600;">${effectivePid}</strong></span>
        ` : ''}
      `;
    }

    const actions = document.getElementById('ws-project-actions');
    if (actions) {
      const dbScope = databaseScope(d);
      const canManage = typeof window.hasPower === 'function' ? window.hasPower('app_delete', dbScope) : false;
      const canControl = typeof window.hasAnyPower === 'function' ? window.hasAnyPower(['databases', 'databases_manage'], dbScope) : false;
      const canCreds = typeof window.hasAnyPower === 'function' ? window.hasAnyPower(['databases_manage', 'env'], dbScope) : false;
      const safeName = escapeHtml(d.name).replace(/'/g, '&#39;');
      actions.innerHTML = `
        <div class="ws-actions-group">
          ${canControl ? `
            <button class="btn btn-sm btn-icon ${isRunning ? 'ws-btn-stop' : 'ws-btn-start'}" id="ws-btn-db-toggle-${d.id}" onclick="toggleDatabaseState(${d.id})" title="${isRunning ? 'Stop database' : isStarting ? 'Starting' : isStopping ? 'Stopping' : 'Start database'}" aria-label="${isRunning ? 'Stop database' : 'Start database'}" ${(isStarting || isStopping) ? 'disabled' : ''}>
              ${isRunning
            ? wsIcon('stop')
            : isStarting
              ? `<span class="btn-spinner"></span>`
              : isStopping
                ? `<span class="btn-spinner"></span>`
                : wsIcon('start')}
            </button>
            <button class="btn btn-sm btn-icon ws-btn-restart" onclick="restartDatabase(${d.id})" title="Restart PostgreSQL cluster" aria-label="Restart PostgreSQL cluster" ${(isStarting || isStopping) ? 'disabled' : ''}>
              ${wsIcon('restart')}
            </button>
          ` : ''}
        </div>

        <div class="ws-actions-group">
          ${canCreds ? `
            <button class="btn btn-sm btn-icon ws-btn-settings" onclick="showDatabaseDetails(${d.id})" title="Connection details and credentials" aria-label="Connection details">
              ${wsIcon('key')}
            </button>
          ` : ''}
          ${canManage ? `
            <button class="btn btn-sm btn-icon ws-btn-delete" onclick="deleteDatabase(${d.id}, '${safeName}')" title="Delete database" aria-label="Delete database">
              ${wsIcon('trash')}
            </button>
          ` : ''}
        </div>
      `;
    }
  }

  const depTabLabel = document.getElementById('ws-tab-deployments-label');
  if (depTabLabel) depTabLabel.textContent = 'Overview';

  const dbTabPowerMap = {
    deployments: ['databases', 'databases_manage'],
    logs: ['databases', 'databases_manage', 'logs'],
    files: ['databases', 'databases_manage', 'files', 'files_write'],
    audit: ['databases', 'databases_manage', 'audit', 'activity'],
    access: ['access_manage'],
  };

  const tabsBar = document.getElementById('ws-tabs-bar');
  let activeBtn = null;
  document.querySelectorAll('#ws-tabs-bar .ws-tab-item').forEach((btn) => {
    const tab = btn.getAttribute('data-ws-tab');
    const isAllowed = DATABASE_WORKSPACE_TABS.has(tab);
    const reqPowers = dbTabPowerMap[tab];
    const hasAccess = !reqPowers || (typeof window.hasAnyPower === 'function' ? window.hasAnyPower(reqPowers) : true);
    const isActive = tab === state.currentView;
    btn.style.display = (isAllowed && hasAccess) ? '' : 'none';
    btn.classList.toggle('active', isActive);
    if (isActive) activeBtn = btn;
  });

  const activeTabSig = `db:${d.id}:${state.currentView}`;
  if (tabsBar && activeBtn && state.lastCenteredWsTab !== activeTabSig) {
    requestAnimationFrame(() => {
      if (centerActiveWebsiteTab(true)) state.lastCenteredWsTab = activeTabSig;
      setTimeout(() => {
        if (centerActiveWebsiteTab(true)) state.lastCenteredWsTab = activeTabSig;
      }, 80);
    });
  }
}

function renderWebsiteWorkspaceHeader() {
  const banner = document.getElementById('website-workspace-banner');
  if (!banner) return;

  const isInsideCard = WEBSITE_WORKSPACE_TABS.has(state.currentView);
  if (!isInsideCard) {
    lastBannerEntityKey = null;
    banner.style.display = 'none';
    return;
  }

  const triggerBannerMotion = (key) => {
    if (!key || banner.style.display === 'none') return;
    if (key !== lastBannerEntityKey) {
      lastBannerEntityKey = key;
      animateWorkspaceBanner(banner);
    }
  };

  if (state.selectedEntityKind === 'database') {
    renderDatabaseWorkspaceHeader(banner);
    triggerBannerMotion(state.selectedDatabaseId ? `db:${state.selectedDatabaseId}` : null);
    return;
  }

  const p = getSelectedProject();
  if (!p) {
    lastBannerEntityKey = null;
    banner.style.display = 'none';
    return;
  }

  banner.style.display = 'block';
  triggerBannerMotion(`project:${p.id}`);
  const resItem = (state.overview?.resources || []).find((x) => (x.projectId === p.id || x.project_id === p.id));
  const isOnline = isProjectOnline(p, resItem);
  const isMaintenance = isProjectMaintenance(p, resItem);
  const isService = p.project_kind === 'service';

  const dot = document.getElementById('ws-status-dot');
  if (dot) dot.className = `status-dot ${isOnline ? 'online' : isMaintenance ? 'maintenance' : 'offline'}`;

  const title = document.getElementById('ws-project-title');
  if (title) title.textContent = p.name;

  const breadcrumbSlug = document.getElementById('ws-breadcrumb-slug');
  if (breadcrumbSlug) breadcrumbSlug.textContent = p.name;

  const backCrumb = document.querySelector('.ws-back-crumb');
  if (backCrumb) {
    if (isService) {
      backCrumb.setAttribute('onclick', "navigateTo('services')");
      backCrumb.setAttribute('title', 'Back to Backend Services');
      const s = backCrumb.querySelector('span');
      if (s) s.textContent = 'Services';
    } else {
      backCrumb.setAttribute('onclick', "navigateTo('projects')");
      backCrumb.setAttribute('title', 'Back to Hosted Websites');
      const s = backCrumb.querySelector('span');
      if (s) s.textContent = 'Websites';
    }
  }

  const badge = document.getElementById('ws-status-badge');
  if (badge) {
    if (isOnline) {
      badge.className = 'badge badge-success';
      badge.textContent = 'Online';
    } else if (isMaintenance) {
      badge.className = 'badge badge-warning';
      badge.textContent = 'Maintenance';
    } else {
      badge.className = 'badge badge-danger';
      badge.textContent = 'Stopped';
    }
  }

  const effectivePid = p.app_pid || p.status?.pid || resItem?.pid;
  const diskStr = p.diskFormatted || p.disk_formatted || resItem?.diskFormatted || resItem?.disk_formatted || '';

  const bannerSig = `${p.id}:${p.name}:${p.slug}:${p.app_status}:${isOnline ? 1 : 0}:${isMaintenance ? 1 : 0}:${effectivePid || ''}:${p.port}:${p.branch}:${p.deployed_sha}:${(p.domains || []).join(',')}:${p.project_kind}:${p.project_type}:${p.access_mode || ''}:${p.repo_url || ''}:${diskStr}`;
  if (state.lastWsBannerSig !== bannerSig) {
    state.lastWsBannerSig = bannerSig;

    const links = document.getElementById('ws-project-links');
    if (links) {
      const sha = p.deployed_sha ? shortSha(p.deployed_sha) : 'Not deployed';
      if (isService) {
        let typeLabel = `${icon('server', { size: 14 })} Backend Service`;
        const pt = (p.project_type || '').toLowerCase();
        const sc = (p.start_cmd || '').toLowerCase();
        if (pt.includes('lavalink') || sc.includes('lavalink') || sc.includes('.jar') || sc.includes('java')) typeLabel = `${icon('java', { size: 14 })} Java Audio Server`;
        else if (pt.includes('python') || sc.includes('python') || sc.includes('.py')) typeLabel = `${icon('python', { size: 14 })} Python Daemon`;
        else if (pt.includes('rust') || sc.includes('cargo') || sc.includes('.rs')) typeLabel = `${icon('rust', { size: 14 })} Rust Daemon`;
        else if (pt.includes('go') || sc.includes('go run') || sc.includes('.go')) typeLabel = `${icon('go', { size: 14 })} Go Daemon`;
        else if (pt.includes('node') || sc.includes('node') || sc.includes('npm')) typeLabel = `${icon('nodejs', { size: 14 })} Node.js Worker`;
        else if (pt.includes('php') || sc.includes('php')) typeLabel = `${icon('php', { size: 14 })} PHP Service`;
        else if (pt.includes('ruby') || sc.includes('ruby') || sc.includes('rails')) typeLabel = `${icon('ruby', { size: 14 })} Ruby Daemon`;
        else if (pt.includes('docker') || sc.includes('docker')) typeLabel = `${icon('docker', { size: 14 })} Docker Container`;
        else if (pt.includes('worker')) typeLabel = `${icon('cpu', { size: 14 })} Queue Worker`;

        const isLocal = !p.repo_url || p.repo_url === 'local' || pt.includes('lavalink');
        const serviceDomainLink = (p.domains && p.domains.length > 0)
          ? `<a class="ws-meta-link active" href="https://${escapeHtml(p.domains[0])}" target="_blank" style="color:var(--accent-light); font-weight:600;" title="Custom Domain: https://${escapeHtml(p.domains[0])}">
               ${icon('globe', { size: 12 })}
               <span>${escapeHtml(p.domains[0])}</span>
             </a><span class="ws-meta-sep">&bull;</span>`
          : '';

        let accessText = `${icon('lock', { size: 11 })} Localhost`;
        if (p.access_mode === 'allowlist') accessText = `${icon('shield', { size: 11 })} Allowlist`;
        else if (p.access_mode === 'public') accessText = `${icon('globe', { size: 11 })} Public`;

        links.innerHTML = `
          ${serviceDomainLink}
          <span class="ws-meta-item" style="color:var(--accent-light); font-weight:600; display: inline-flex; align-items: center; gap: 4px;">
            ${typeLabel}
          </span>
          <span class="ws-meta-sep">&bull;</span>
          ${isLocal ? `
            <span class="ws-meta-item" title="Deployment Mode">
              <span class="badge badge-neutral" style="font-size:11px; padding: 2px 8px;">${icon('folder', { size: 11 })} Manual Upload</span>
            </span>
          ` : `
            <span class="ws-meta-item" title="Git Branch">
              <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5"><line x1="6" y1="3" x2="6" y2="15"></line><circle cx="18" cy="6" r="3"></circle><circle cx="6" cy="18" r="3"></circle><path d="M18 9a9 9 0 0 1-9 9"></path></svg>
              <span style="color:var(--text-main); font-weight:600;">${escapeHtml(p.branch || 'main')}</span>
            </span>
            <span class="ws-meta-sep">&bull;</span>
            <span class="ws-meta-item" title="Deployed Commit SHA">
              <code class="ws-meta-sha">${escapeHtml(sha)}</code>
            </span>
          `}
          ${p.port ? `
            <span class="ws-meta-sep">&bull;</span>
            <span class="ws-meta-item" title="Listening Port">Port <strong style="color:var(--text-main); font-weight:600;">${p.port}</strong></span>
          ` : ''}
          ${diskStr ? `
            <span class="ws-meta-sep">&bull;</span>
            <span class="ws-meta-item" title="On-Disk Storage">Disk <strong style="color:var(--text-main); font-weight:600;">${diskStr}</strong></span>
          ` : ''}
          <span class="ws-meta-sep">&bull;</span>
          <span class="ws-meta-item" title="Click to configure Network Access Policy" style="font-size:11px; cursor:pointer;" onclick="openEditServiceModal(${p.id})">${accessText}</span>
          ${(isOnline && effectivePid) ? `
            <span class="ws-meta-sep">&bull;</span>
            <span class="ws-meta-item" title="Process PID">PID <strong style="color:var(--text-main); font-weight:600;">${effectivePid}</strong></span>
          ` : ''}
        `;
      } else {
        const url = p.port ? `http://127.0.0.1:${p.port}` : null;
        const customDomains = Array.isArray(p.domains) ? p.domains.filter(Boolean) : [];
        const primaryLinkHtml = customDomains.length
          ? `<a class="ws-meta-link active" href="https://${escapeHtml(customDomains[0])}" target="_blank" style="color:var(--accent-light); font-weight:600;" title="Open https://${escapeHtml(customDomains[0])}">
               ${icon('globe', { size: 12 })}
               <span>${escapeHtml(customDomains[0])}</span>
             </a>`
          : url
            ? `<a class="ws-meta-link active" href="${url}" target="_blank" title="Open ${url}">
                 <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"></path><polyline points="15 3 21 3 21 9"></polyline><line x1="10" y1="14" x2="21" y2="3"></line></svg>
                 <span>${url}</span>
               </a>`
            : '';

        let accessText = `${icon('lock', { size: 11 })} Localhost`;
        if (p.access_mode === 'allowlist') accessText = `${icon('shield', { size: 11 })} Allowlist`;
        else if (p.access_mode === 'public') accessText = `${icon('globe', { size: 11 })} Public`;

        const isLocalSite = !p.repo_url || p.repo_url === 'local';
        links.innerHTML = `
          ${primaryLinkHtml ? `${primaryLinkHtml}<span class="ws-meta-sep">&bull;</span>` : ''}
          ${isLocalSite ? `
            <span class="ws-meta-item" title="Files are uploaded in the Files tab and deployed as a snapshot">
              ${icon('folder', { size: 12 })} Manual upload
            </span>
            <span class="ws-meta-sep">&bull;</span>
            <span class="ws-meta-item">${p.deployed_sha ? 'Deployed' : '<code class="ws-meta-sha">Not deployed</code>'}</span>
          ` : `
            <span class="ws-meta-item" title="Git Branch">
              <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5"><line x1="6" y1="3" x2="6" y2="15"></line><circle cx="18" cy="6" r="3"></circle><circle cx="6" cy="18" r="3"></circle><path d="M18 9a9 9 0 0 1-9 9"></path></svg>
              <span style="color:var(--text-main); font-weight:600;">${escapeHtml(p.branch || 'main')}</span>
            </span>
            <span class="ws-meta-sep">&bull;</span>
            <span class="ws-meta-item" title="Deployed Commit SHA">
              <code class="ws-meta-sha">${escapeHtml(sha)}</code>
            </span>
          `}
          ${p.port ? `
            <span class="ws-meta-sep">&bull;</span>
            <span class="ws-meta-item" title="Local Port">Port <strong style="color:var(--text-main); font-weight:600;">${p.port}</strong></span>
          ` : ''}
          ${diskStr ? `
            <span class="ws-meta-sep">&bull;</span>
            <span class="ws-meta-item" title="On-Disk Storage">Disk <strong style="color:var(--text-main); font-weight:600;">${diskStr}</strong></span>
          ` : ''}
          <span class="ws-meta-sep">&bull;</span>
          <span class="ws-meta-item" title="Click to configure Network Access Policy" style="font-size:11px; cursor:pointer;" onclick="openEditWebsiteModal(${p.id})">${accessText}</span>
          ${(isOnline && effectivePid) ? `
            <span class="ws-meta-sep">&bull;</span>
            <span class="ws-meta-item" title="Process PID">PID <strong style="color:var(--text-main); font-weight:600;">${effectivePid}</strong></span>
          ` : ''}
        `;
      }
    }

    const actions = document.getElementById('ws-project-actions');
    if (actions) {
      const I = Object.fromEntries(Object.keys(WS_ICON_PATHS).map((k) => [k, wsIcon(k)]));
      const localSource = !p.repo_url || p.repo_url === 'local';
      const canDeploy = hasPower('deploy') && (!isService || !localSource);
      const canProcess = hasPower('process');
      const canManage = hasPower(isService ? 'services_manage' : 'projects_manage');
      // Websites must be deployed once before they can start; manual services run from their files directly.
      const neverDeployed = !isService && !p.deployed_sha;
      const settingsAction = isService ? `openEditServiceModal(${p.id})` : `openEditWebsiteModal(${p.id})`;

      const canMaint = !isService && canProcess;
      const canDomains = hasPower('domains');
      const canDelete = hasPower('app_delete');

      actions.innerHTML = `
        <div class="ws-actions-group">
          ${canDeploy ? `<button class="btn btn-sm btn-icon ws-btn-deploy" onclick="deployProject(${p.id})" title="${localSource ? 'Deploy the files uploaded in the Files tab' : 'Pull the latest commit and deploy it'}" aria-label="Deploy">${I.deploy}</button>` : ''}
          ${canProcess ? `
            ${isOnline || isMaintenance
              ? `<button class="btn btn-sm btn-icon ws-btn-stop" onclick="stopProject(${p.id})" title="Stop the process" aria-label="Stop">${I.stop}</button>`
              : `<button class="btn btn-sm btn-icon ws-btn-start" onclick="startProject(${p.id})" aria-label="Start" ${neverDeployed ? 'disabled title="Deploy this website once before starting it"' : 'title="Start the process"'}>${I.start}</button>`}
            ${isOnline ? `<button class="btn btn-sm btn-icon ws-btn-restart" onclick="restartProject(${p.id})" title="Restart the process" aria-label="Restart the process">${I.restart}</button>` : ''}
          ` : ''}
        </div>
        <div class="ws-actions-group">
          ${isMaintenance ? `<span class="badge badge-warning" title="Visitors see the maintenance page">${I.maint} Maintenance</span>` : ''}
          ${canMaint ? `<button class="btn btn-sm btn-icon ws-btn-maint ${isMaintenance ? 'is-on' : ''}" onclick="toggleMaintenanceMode(${p.id})" title="${isMaintenance ? 'Exit maintenance mode' : 'Maintenance mode: show visitors a maintenance page'}" aria-label="${isMaintenance ? 'Exit maintenance mode' : 'Maintenance mode'}" aria-pressed="${isMaintenance ? 'true' : 'false'}">${I.maint}</button>` : ''}
          ${canDomains ? `<button class="btn btn-sm btn-icon ws-btn-domains" onclick="openNginxModal(${p.id})" title="Domains and SSL certificates" aria-label="Domains and SSL">${I.globe}</button>` : ''}
          ${canManage ? `<button class="btn btn-sm btn-icon ws-btn-settings" onclick="${settingsAction}" title="${isService ? 'Service settings' : 'Website settings'}" aria-label="Settings">${I.gear}</button>` : ''}
          ${canDelete ? `<button class="btn btn-sm btn-icon ws-btn-delete" onclick="deleteProject(${p.id})" title="Delete this ${isService ? 'service' : 'website'}" aria-label="Delete this ${isService ? 'service' : 'website'}">${I.trash}</button>` : ''}
        </div>
      `;
    }
  }

  const tabPowerMap = {
    logs: ['logs'],
    files: ['files', 'files_write'],
    env: ['env'],
    audit: ['audit', 'activity'],
    monitor: ['monitor'],
    access: ['access_manage'],
  };
  const hiddenSecondaryTabs = new Set(['rollbacks', 'monitor']);

  const depTabLabel = document.getElementById('ws-tab-deployments-label');
  if (depTabLabel) {
    depTabLabel.textContent = 'Overview';
  }

  const tabsBar = document.getElementById('ws-tabs-bar');
  let activeBtn = null;
  document.querySelectorAll('#ws-tabs-bar .ws-tab-item').forEach((btn) => {
    const tab = btn.getAttribute('data-ws-tab');
    const reqPowers = tabPowerMap[tab];
    const isActive = tab === state.currentView;
    const isLocalService = isService && (!p.repo_url || p.repo_url === 'local' || (p.project_type && p.project_type.includes('lavalink')));
    const isLocalWebsite = !isService && (!p.repo_url || p.repo_url === 'local');
    if (isLocalService && (tab === 'github' || tab === 'releases' || tab === 'rollbacks')) {
      btn.style.display = 'none';
    } else if (isLocalWebsite && tab === 'github') {
      // No repository to show commits for.
      btn.style.display = 'none';
    } else if (tab === 'env' && isService) {
      btn.style.display = 'none';
    } else if ((reqPowers && !hasAnyPower(reqPowers)) || (hiddenSecondaryTabs.has(tab) && !isActive)) {
      btn.style.display = 'none';
    } else {
      btn.style.display = '';
    }
    btn.classList.toggle('active', isActive);
    if (isActive) activeBtn = btn;
  });

  const activeTabSig = `${p.id}:${state.currentView}`;
  if (tabsBar && activeBtn && state.lastCenteredWsTab !== activeTabSig) {
    requestAnimationFrame(() => {
      if (centerActiveWebsiteTab(true)) {
        state.lastCenteredWsTab = activeTabSig;
      }
      setTimeout(() => {
        if (centerActiveWebsiteTab(true)) {
          state.lastCenteredWsTab = activeTabSig;
        }
      }, 80);
    });
  }
}

function centerActiveWebsiteTab(smooth = true) {
  const tabsBar = document.getElementById('ws-tabs-bar');
  if (!tabsBar || tabsBar.clientWidth === 0) return false;
  const activeBtn = tabsBar.querySelector('.ws-tab-item.active');
  if (!activeBtn || activeBtn.offsetWidth === 0) return false;

  const barRect = tabsBar.getBoundingClientRect();
  const btnRect = activeBtn.getBoundingClientRect();
  const delta = (btnRect.left + btnRect.width / 2) - (barRect.left + barRect.width / 2);
  const maxScroll = Math.max(0, tabsBar.scrollWidth - tabsBar.clientWidth);
  const targetLeft = Math.min(maxScroll, Math.max(0, tabsBar.scrollLeft + delta));

  if (Math.abs(tabsBar.scrollLeft - targetLeft) > 2) {
    tabsBar.scrollTo({
      left: targetLeft,
      behavior: smooth ? 'smooth' : 'auto',
    });
  }
  return true;
}
window.addEventListener('resize', () => centerActiveWebsiteTab(false));


function renderCurrentView() {
  state.currentView = guardView(state.currentView);
  syncSidebarMode();
  updateBreadcrumbs();
  renderWebsiteWorkspaceHeader();

  // Close SSE log stream when leaving the logs view so it never holds browser sockets open
  if (state.currentView !== 'logs' && state.logEventSource) {
    state.logEventSource.close();
    state.logEventSource = null;
  }
  if (state.currentView !== 'overview' && typeof window.stopOverviewNetPoller === 'function') {
    window.stopOverviewNetPoller();
  }

  // Close mobile sidebar if open
  document.querySelector('.sidebar')?.classList.remove('mobile-open');
  document.getElementById('sidebar-backdrop')?.classList.remove('active');

  // Update sidebar active classes (highlight 'projects', 'services', or 'databases' when inside a workspace)
  const isInsideCard = WEBSITE_WORKSPACE_TABS.has(state.currentView);
  let activeSidebarView = state.currentView;
  if (isInsideCard) {
    if (state.selectedEntityKind === 'database') {
      activeSidebarView = 'databases';
    } else {
      const curP = getSelectedProject();
      activeSidebarView = (curP && curP.project_kind === 'service') ? 'services' : 'projects';
    }
  }
  document.querySelectorAll('.nav-item, .user-snippet[data-view]').forEach((item) => {
    item.classList.toggle('active', item.getAttribute('data-view') === activeSidebarView);
  });
  document.querySelectorAll('#mobile-bottom-nav .mobile-nav-item[data-mobile-view]').forEach((item) => {
    item.classList.toggle('active', item.getAttribute('data-mobile-view') === activeSidebarView);
  });

  // Switch view section visibility
  const activeSection = document.getElementById(`view-${state.currentView}`);
  const isViewChange = Boolean(activeSection && !activeSection.classList.contains('active'));

  document.querySelectorAll('.view-section').forEach((sec) => {
    sec.classList.remove('active');
  });

  if (activeSection) {
    activeSection.classList.add('active');
    if (isViewChange) {
      animateViewEnter(activeSection);
      markViewStaggerPending(state.currentView);
    }
  }

  // Load relevant view data
  switch (state.currentView) {
    case 'overview': void loadOverview(); break;
    case 'projects': void loadProjectsView(); break;
    case 'services': void loadServicesView(); break;
    case 'databases': void loadDatabasesView(); break;
    case 'github': void loadGithubView(); break;
    case 'deployments': void loadDeploymentsView(); break;
    case 'releases': void loadReleasesView(); break;
    case 'rollbacks': void loadRollbacksView(); break;
    case 'logs': void loadLogsView(); break;
    case 'env': void loadEnvView(); break;
    case 'files': void loadFilesView(); break;
    case 'monitor': void loadMonitorView(); break;
    case 'settings':
      // Settings content is static: the view fade is its only entrance (a second card stagger on
      // top of it made the page jitter).
      consumePendingViewStagger('settings');
      void loadSettingsView();
      break;
    case 'audit': void loadAuditView(); break;
    case 'nodes': if (window.loadNodesView) void window.loadNodesView(); break;
    case 'modules': if (window.loadModulesView) void window.loadModulesView(); break;
    case 'ips': if (window.loadIpsView) void window.loadIpsView(); break;
    case 'moves': if (window.loadMovesView) void window.loadMovesView(); break;
    case 'limits': if (window.loadLimitsView) void window.loadLimitsView(); break;
    case 'account':
    case 'users':
    case 'requests':
    case 'admin-audit':
    case 'access':
      loadAccountsView(state.currentView);
      break;
  }
}


window.renderWebsiteWorkspaceHeader = renderWebsiteWorkspaceHeader;
window.updateBreadcrumbs = updateBreadcrumbs;
window.centerActiveWebsiteTab = centerActiveWebsiteTab;
window.renderCurrentView = renderCurrentView;

export {
  openWebsiteWorkspace,
  openDatabaseWorkspace,
  switchWebsiteTab,
  navigateTo,
  handleHashRoute,
  updateBreadcrumbs,
  renderWebsiteWorkspaceHeader,
  centerActiveWebsiteTab,
  renderCurrentView,
};
