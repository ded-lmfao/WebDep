/**
 * WebDep GitHub token vault (Platform settings).
 * Accounts and permissions live in accounts.js.
 */

import { state, escapeHtml } from './state.js';

// ============================================================================
// GitHub Deployment Tokens Multi-Account Management
// ============================================================================
export function syncGithubTokensDomToDraft() {
  if (!Array.isArray(state.githubTokensDraft)) state.githubTokensDraft = [];
  document.querySelectorAll('#github-tokens-list .gh-token-row').forEach((row) => {
    const idx = Number(row.getAttribute('data-gh-idx'));
    if (!state.githubTokensDraft[idx]) return;
    const lblEl = row.querySelector('.gh-token-label-input');
    const tokEl = row.querySelector('.gh-token-value-input');
    if (lblEl) state.githubTokensDraft[idx].label = lblEl.value;
    if (tokEl) state.githubTokensDraft[idx].token = tokEl.value;
  });
}

export function renderGithubTokensRows() {
  const container = document.getElementById('github-tokens-list');
  if (!container) return;
  const list = state.githubTokensDraft || [{ label: 'Primary Account', token: '', revealed: false }];
  container.innerHTML = list.map((item, idx) => `
    <div class="gh-token-row" data-gh-idx="${idx}" style="display: grid; grid-template-columns: 200px 1fr auto auto; gap: 8px; align-items: center;">
      <input type="text" class="form-control gh-token-label-input" value="${escapeHtml(item.label || '')}" placeholder="Account label (e.g. Personal, Org)" autocomplete="off">
      <input type="${item.revealed ? 'text' : 'password'}" class="form-control env-mono-input gh-token-value-input" value="${escapeHtml(item.token || '')}" placeholder="ghp_token or github_pat_token" autocomplete="new-password">
      <button type="button" class="btn btn-secondary btn-sm" onclick="toggleGithubTokenRowReveal(${idx})" style="min-width: 62px;">
        ${item.revealed ? 'Hide' : 'Show'}
      </button>
      <button type="button" class="btn btn-danger btn-sm" onclick="removeGithubTokenRow(${idx})" title="Remove token">&times;</button>
    </div>
  `).join('');
}

export function addGithubTokenRow() {
  syncGithubTokensDomToDraft();
  if (!Array.isArray(state.githubTokensDraft)) state.githubTokensDraft = [];
  state.githubTokensDraft.push({
    label: `Account #${state.githubTokensDraft.length + 1}`,
    token: '',
    revealed: false,
  });
  renderGithubTokensRows();
}

export function removeGithubTokenRow(idx) {
  syncGithubTokensDomToDraft();
  if (!Array.isArray(state.githubTokensDraft)) return;
  state.githubTokensDraft.splice(idx, 1);
  if (state.githubTokensDraft.length === 0) {
    state.githubTokensDraft.push({ label: 'Primary Account', token: '', revealed: false });
  }
  renderGithubTokensRows();
}

export function toggleGithubTokenRowReveal(idx) {
  syncGithubTokensDomToDraft();
  if (state.githubTokensDraft && state.githubTokensDraft[idx]) {
    state.githubTokensDraft[idx].revealed = !state.githubTokensDraft[idx].revealed;
    renderGithubTokensRows();
  }
}

// Bind window globals for HTML inline event handlers
window.addGithubTokenRow = addGithubTokenRow;
window.removeGithubTokenRow = removeGithubTokenRow;
window.toggleGithubTokenRowReveal = toggleGithubTokenRowReveal;
