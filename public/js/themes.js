/**
 * WebDep Predefined Platform Theme Manager (15 Curated Presets)
 * Instant client-side application + localStorage caching + SQLite platform default sync.
 */

import { state, escapeHtml } from './state.js';
import { api, toast } from './api.js';

export const THEME_STORAGE_KEY = 'webdep.theme';
export const DEFAULT_THEME_ID = 'midnight-ink';

// [id, name, category, [canvas, sidebar, card, accent, text]]; the browser bar uses the canvas colour.
const THEME_ROWS = [
  ['midnight-ink', 'Midnight Ink', 'Dark • Cobalt', ['#0b101d', '#070b14', '#111829', '#4f8ef7', '#eef1f7']],
  ['obsidian-carbon', 'Obsidian Carbon', 'OLED • Cyan', ['#090a0d', '#050608', '#111318', '#38bdf8', '#f1f5f9']],
  ['nordic-frost', 'Nordic Frost', 'Dark • Glacier', ['#1a1f29', '#131720', '#222836', '#88c0d0', '#eceff4']],
  ['emerald-terminal', 'Emerald Terminal', 'Dark • Mint', ['#08110e', '#050b09', '#0e1c17', '#34d399', '#ecfdf5']],
  ['catppuccin-mocha', 'Catppuccin Mocha', 'Dark • Mocha', ['#181825', '#11111b', '#1e1e2e', '#89b4fa', '#cdd6f4']],
  ['tokyo-dusk', 'Tokyo Dusk', 'Dark • Indigo', ['#131521', '#0d0f18', '#1a1d2d', '#7aa2f7', '#c0caf5']],
  ['gruvbox-warm', 'Gruvbox Warm', 'Dark • Amber', ['#1b1917', '#141210', '#24211e', '#e09f3e', '#ebdbb2']],
  ['crimson-ember', 'Crimson Ember', 'Dark • Rose', ['#120c0e', '#0b0708', '#1b1215', '#fb7185', '#f8eff1']],
  ['daylight-crisp', 'Daylight Crisp', 'Light • Sapphire', ['#f3f6fb', '#e9eef6', '#ffffff', '#2563eb', '#0f172a']],
  ['paper-espresso', 'Paper Pine', 'Light • Forest', ['#f4f1ea', '#eae5da', '#fcfbf7', '#15803d', '#1c1917']],
  ['sage-mist', 'Sage Mist', 'Dark • Soft Sage', ['#1b201d', '#161a17', '#222925', '#8fb39b', '#d6ddd7']],
  ['lavender-dusk', 'Lavender Dusk', 'Dark • Soft Violet', ['#1c1b22', '#17161c', '#24232c', '#a99bd6', '#dcd9e6']],
  ['harbor-slate', 'Harbor Slate', 'Dark • Soft Teal', ['#192127', '#141b20', '#202a31', '#7fb4bf', '#d3dee3']],
  ['cocoa-ember', 'Cocoa Ember', 'Dark • Soft Amber', ['#1f1b18', '#191613', '#28231f', '#d1a774', '#e3dbd2']],
  ['soft-linen', 'Soft Linen', 'Light • Dimmed', ['#e7e2d8', '#ddd7cb', '#efebe3', '#4f7f7a', '#38342f']],
];

export const PLATFORM_THEMES = THEME_ROWS.map(([id, name, category, [canvas, sidebar, card, accent, text]]) => ({
  id,
  name,
  category,
  metaColor: canvas,
  preview: { canvas, sidebar, card, accent, text },
}));

export function resolveTheme(themeId) {
  const clean = String(themeId || '').trim().toLowerCase();
  return PLATFORM_THEMES.find((t) => t.id === clean) || PLATFORM_THEMES[0];
}

export function getActiveThemeId() {
  const fromDom = document.documentElement.dataset.theme;
  if (fromDom) return resolveTheme(fromDom).id;
  try {
    const stored = localStorage.getItem(THEME_STORAGE_KEY);
    if (stored) return resolveTheme(stored).id;
  } catch (_) {}
  return DEFAULT_THEME_ID;
}

export function applyTheme(themeId, persistLocal = true) {
  const theme = resolveTheme(themeId);
  document.documentElement.dataset.theme = theme.id;

  const meta = document.querySelector('meta[name="theme-color"]');
  if (meta) meta.setAttribute('content', theme.metaColor);

  if (persistLocal) {
    try {
      localStorage.setItem(THEME_STORAGE_KEY, theme.id);
    } catch (_) {}
  }
  updateThemePickerUI(theme.id);
  return theme;
}

export async function syncThemeFromServer() {
  let hasLocal = false;
  try {
    hasLocal = Boolean(localStorage.getItem(THEME_STORAGE_KEY));
  } catch (_) {}

  try {
    const res = await fetch('/api/theme', { credentials: 'same-origin', cache: 'no-store' });
    if (!res.ok) return;
    const data = await res.json();
    if (data && data.theme) {
      const serverTheme = resolveTheme(data.theme);
      state.platformTheme = serverTheme.id;
      if (!hasLocal) {
        applyTheme(serverTheme.id, true);
      }
    }
  } catch (_) {}
}

function canSaveGlobalSettings() {
  if (!state.user) return false;
  if (state.user.isAdmin || state.user.isMainAdmin) return true;
  const perms = state.user.permissions || [];
  return perms.includes('*') || perms.includes('settings_manage');
}

export async function selectPlatformTheme(themeId) {
  const theme = applyTheme(themeId, true);

  if (canSaveGlobalSettings()) {
    try {
      await api('/api/settings', {
        method: 'PUT',
        body: { panel_theme: theme.id },
      });
      state.platformTheme = theme.id;
      toast(`Applied "${theme.name}" platform theme`, 'success');
    } catch (err) {
      toast(`Applied "${theme.name}" locally (server save failed: ${err.message})`, 'warning');
    }
  } else {
    toast(`Applied "${theme.name}" theme for your browser`, 'success');
  }
}

export function updateThemePickerUI(activeId = getActiveThemeId()) {
  const resolved = resolveTheme(activeId);
  const badge = document.getElementById('settings-active-theme-badge');
  if (badge) {
    badge.textContent = resolved.name;
  }

  const grid = document.getElementById('settings-theme-grid');
  if (!grid) return;

  grid.querySelectorAll('.theme-swatch-card').forEach((card) => {
    const isMatch = card.getAttribute('data-theme-id') === resolved.id;
    card.classList.toggle('active', isMatch);
    card.setAttribute('aria-pressed', isMatch ? 'true' : 'false');
  });
}

export function renderThemePicker() {
  const grid = document.getElementById('settings-theme-grid');
  if (!grid) return;

  const activeId = getActiveThemeId();
  if (!grid.dataset.rendered) {
    grid.innerHTML = PLATFORM_THEMES.map((t) => {
      const p = t.preview;
      const isMatch = t.id === activeId;
      const isLight = t.category.startsWith('Light');
      return `
        <button type="button"
          class="theme-swatch-card${isMatch ? ' active' : ''}"
          data-theme-id="${escapeHtml(t.id)}"
          aria-pressed="${isMatch ? 'true' : 'false'}"
          title="Apply ${escapeHtml(t.name)} theme (${escapeHtml(t.category)})">
          <span class="theme-swatch-dot" style="background: ${p.canvas};" aria-hidden="true">
            <span class="theme-swatch-dot-accent" style="background: ${p.accent};"></span>
          </span>
          <span class="theme-swatch-label">
            <span class="theme-swatch-name">${escapeHtml(t.name)}</span>
            ${isLight ? '<span class="theme-swatch-tag">Light</span>' : ''}
          </span>
          <span class="theme-swatch-check" aria-hidden="true">
            <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round">
              <polyline points="20 6 9 17 4 12"></polyline>
            </svg>
          </span>
        </button>
      `;
    }).join('');

    grid.addEventListener('click', (e) => {
      const btn = e.target.closest('.theme-swatch-card');
      if (!btn) return;
      const tid = btn.getAttribute('data-theme-id');
      if (tid) void selectPlatformTheme(tid);
    });
    grid.dataset.rendered = 'true';
  }

  updateThemePickerUI(activeId);
}

// Expose helpers for inline handlers if needed
window.selectPlatformTheme = selectPlatformTheme;
window.resetPlatformTheme = () => selectPlatformTheme(DEFAULT_THEME_ID);
