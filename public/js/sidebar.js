/**
 * Collapsible sidebar (desktop): full labels, or a narrow icon-only rail.
 * The choice is remembered per browser. index.html applies it before first paint to avoid a jump.
 */

const STORAGE_KEY = 'webdep.sidebarCollapsed';
const CLASS = 'sidebar-collapsed';

function setCollapsed(collapsed) {
  document.documentElement.classList.toggle(CLASS, collapsed);
  const btn = document.getElementById('sidebar-collapse-btn');
  if (btn) {
    btn.setAttribute('aria-expanded', String(!collapsed));
    btn.title = collapsed ? 'Expand sidebar' : 'Collapse sidebar';
    btn.setAttribute('aria-label', btn.title);
  }
  try {
    localStorage.setItem(STORAGE_KEY, collapsed ? '1' : '0');
  } catch (_) {
    // Storage blocked (private mode): the toggle still works for this page view.
  }
}

export function toggleSidebarCollapsed() {
  setCollapsed(!document.documentElement.classList.contains(CLASS));
}

function initSidebar() {
  // Icon-only items need their name on hover.
  document.querySelectorAll('.sidebar .nav-item').forEach((item) => {
    const label = item.querySelector('span:not(.nav-badge)')?.textContent?.trim();
    if (label && !item.title) item.title = label;
  });
  document.getElementById('sidebar-collapse-btn')?.addEventListener('click', toggleSidebarCollapsed);
  setCollapsed(document.documentElement.classList.contains(CLASS));
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', initSidebar);
} else {
  initSidebar();
}
