// Runs before the page paints: applies the saved theme and sidebar state, so there is no flash.
try {
  var savedTheme = localStorage.getItem('webdep.theme');
  if (savedTheme) document.documentElement.dataset.theme = savedTheme;
  if (localStorage.getItem('webdep.sidebarCollapsed') === '1') document.documentElement.classList.add('sidebar-collapsed');
} catch (_) {}
