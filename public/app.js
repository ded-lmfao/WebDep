/**
 * WebDep — Frontend Application Controller (Modular ES6 Entrypoint)
 * Single-Page Reactive Client for Self-Hosted Multi-Website Deployment Platform
 */

// Core Services & Shared Reactive State
import './js/icons.js';
import './js/touch_labels.js';
import { state, WEBSITE_WORKSPACE_TABS } from './js/state.js';
import {
  animateModalClose,
  animateMobileDrawer,
  initGlobalMotionInteractions,
} from './js/motion_ui.js';
import { syncThemeFromServer } from './js/themes.js';
import { initAuth } from './js/auth.js';

// Domain Views & Subsystems
import { loadOverview } from './js/overview.js';
import { loadProjects, renderProjectsList, openAddWebsiteModal } from './js/websites.js';
import { renderServicesList } from './js/services.js';
import { navigateTo, renderCurrentView, renderWebsiteWorkspaceHeader } from './js/workspace.js';
import { loadDatabases, renderDatabasesList } from './js/databases.js';
import { loadDeploymentsView, loadReleasesView, loadBackupsView, loadRollbacksView, waterPipelines } from './js/deployments.js';
import { loadMonitorView } from './js/settings.js';

// Explorer & Logs Subsystems (Initialize listeners & exports)
import './js/files.js';
import './js/logs.js';
import './js/cluster.js';
import './js/modules.js';
import './js/app_limits.js';
import './js/sidebar.js';
import './js/table_cards.js';
import './js/accounts.js';

// ============================================================================
// Global Polling & Live Updates (Low Resource & Page Visibility Aware)
// ============================================================================
let pollInFlight = false;

async function pollTick() {
  if (pollInFlight || document.hidden || state.filesOpenPath) return;
  pollInFlight = true;
  try {
    if (state.currentView === 'overview') {
      await loadOverview();
      return;
    }

    // Keep sidebar badges (databases, projects, services & nodes) up-to-date on non-overview tabs
    await Promise.all([
      loadProjects().catch(() => {}),
      loadDatabases().catch(() => {}),
      window.loadNodes ? window.loadNodes().catch(() => {}) : Promise.resolve()
    ]);

    if (state.currentView === 'nodes') {
      if (window.loadNodesView) await window.loadNodesView();
    } else if (state.currentView === 'modules') {
      if (window.loadModulesView) await window.loadModulesView();
    } else if (state.currentView === 'services') {
      renderServicesList();
    } else if (state.currentView === 'projects') {
      renderProjectsList();
    } else if (state.currentView === 'deployments') {
      renderWebsiteWorkspaceHeader();
      await loadDeploymentsView();
    } else if (state.currentView === 'monitor') {
      renderWebsiteWorkspaceHeader();
      await loadMonitorView();
    } else if (state.currentView === 'releases') {
      renderWebsiteWorkspaceHeader();
      await loadReleasesView();
    } else if (state.currentView === 'backups') {
      renderWebsiteWorkspaceHeader();
      await loadBackupsView();
    } else if (state.currentView === 'rollbacks') {
      renderWebsiteWorkspaceHeader();
      await loadRollbacksView();
    } else if (state.currentView === 'databases') {
      const anyLooping = Object.values(waterPipelines).some((p) => p && p.isLooping);
      if (!anyLooping) {
        renderDatabasesList();
      }
    } else if (WEBSITE_WORKSPACE_TABS.has(state.currentView)) {
      renderWebsiteWorkspaceHeader();
    }
  } finally {
    pollInFlight = false;
  }
}

function startPolling() {
  if (state.overviewTimer) clearInterval(state.overviewTimer);
  state.overviewTimer = setInterval(pollTick, 4500);
}
window.startPolling = startPolling;

// Low-power background mode: instantly resume fresh data when user re-focuses tab
document.addEventListener('visibilitychange', () => {
  if (!document.hidden) {
    void pollTick();
  }
});

window.addEventListener('beforeunload', () => {
  if (state.logEventSource) {
    state.logEventSource.close();
    state.logEventSource = null;
  }
});


// Global project selector binding
document.getElementById('global-project-select')?.addEventListener('change', (e) => {
  const val = e.target.value;
  state.selectedProjectId = val ? Number(val) : null;
  renderCurrentView();
});

// Sidebar navigation items
document.querySelectorAll('.nav-item').forEach((item) => {
  item.addEventListener('click', () => {
    const view = item.getAttribute('data-view');
    if (view) navigateTo(view);
  });
});

document.getElementById('overview-refresh-btn')?.addEventListener('click', loadOverview);
document.getElementById('add-website-btn')?.addEventListener('click', openAddWebsiteModal);


// Mobile navigation drawer toggle
const mobileToggle = document.getElementById('mobile-sidebar-toggle');
const sidebarEl = document.querySelector('.sidebar');
const backdropEl = document.getElementById('sidebar-backdrop');
if (mobileToggle && sidebarEl && backdropEl) {
  mobileToggle.addEventListener('click', () => {
    const nextOpen = !sidebarEl.classList.contains('mobile-open');
    animateMobileDrawer(sidebarEl, backdropEl, nextOpen);
  });
  backdropEl.addEventListener('click', () => {
    animateMobileDrawer(sidebarEl, backdropEl, false);
  });
}


document.querySelectorAll('[data-close-modal]').forEach((btn) => {
  btn.addEventListener('click', (e) => {
    const modal = e.target.closest('.modal-overlay');
    if (modal) animateModalClose(modal);
  });
});

// ---------- Action menus (".menu-wrap" > [data-menu-toggle] + ".menu") ----------
function closeAllMenus(except = null) {
  document.querySelectorAll('.menu-wrap.open').forEach((wrap) => {
    if (wrap === except) return;
    wrap.classList.remove('open');
    wrap.querySelector('[data-menu-toggle]')?.setAttribute('aria-expanded', 'false');
  });
}

document.addEventListener('click', (e) => {
  const toggle = e.target.closest('[data-menu-toggle]');
  if (toggle) {
    const wrap = toggle.closest('.menu-wrap');
    const willOpen = !wrap.classList.contains('open');
    closeAllMenus(wrap);
    wrap.classList.toggle('open', willOpen);
    toggle.setAttribute('aria-expanded', String(willOpen));
    if (willOpen) {
      // Keep the menu inside the viewport on narrow screens.
      const menu = wrap.querySelector('.menu');
      if (menu) {
        menu.classList.remove('menu-left');
        const r = menu.getBoundingClientRect();
        if (r.left < 8) menu.classList.add('menu-left');
        menu.querySelector('.menu-item:not([disabled])')?.focus({ preventScroll: true });
      }
    }
    return;
  }
  if (e.target.closest('.menu .menu-item')) {
    closeAllMenus();
    return;
  }
  if (!e.target.closest('.menu')) closeAllMenus();
});

document.addEventListener('keydown', (e) => {
  const menu = e.target.closest?.('.menu-wrap.open .menu');
  if (!menu || (e.key !== 'ArrowDown' && e.key !== 'ArrowUp')) return;
  const items = [...menu.querySelectorAll('.menu-item:not([disabled])')];
  const idx = items.indexOf(document.activeElement);
  const next = e.key === 'ArrowDown' ? (idx + 1) % items.length : (idx - 1 + items.length) % items.length;
  items[next]?.focus();
  e.preventDefault();
});

window.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape') return;
  const openMenu = document.querySelector('.menu-wrap.open');
  if (openMenu) {
    closeAllMenus();
    openMenu.querySelector('[data-menu-toggle]')?.focus();
    return;
  }
  // Close only the dialog on top (the last one opened in DOM order wins when stacked).
  const open = [...document.querySelectorAll('.modal-overlay.active')];
  const top = open.sort((a, b) => (Number(getComputedStyle(a).zIndex) || 0) - (Number(getComputedStyle(b).zIndex) || 0)).pop();
  if (top) animateModalClose(top);
});


// Strip single-shot hard-reload cache-buster query param if returning from self-update
try {
  const curUrl = new URL(window.location.href);
  if (curUrl.searchParams.has('_updated')) {
    curUrl.searchParams.delete('_updated');
    const cleanUrl = curUrl.pathname + (curUrl.search || '') + (curUrl.hash || '');
    window.history.replaceState(window.history.state, '', cleanUrl || '/');
  }
} catch (_) {}

// Start application
initGlobalMotionInteractions();
void syncThemeFromServer();
void initAuth();
