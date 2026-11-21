/** Admin Panel › App Limits: CPU, memory, disk, and backup limits for every app. */

import { state, escapeHtml, nodeLabel } from './state.js';
import { api, toast } from './api.js';
import { diskToMb, mbToDisk, positiveInt } from './limits_fields.js';

let rows = [];
let loaded = false;

const KIND_TEXT = { website: 'Website', service: 'Service', database: 'Database' };

function nodeName(nodeId) {
  const nodes = state.nodes || [];
  const n = nodes.find((x) => Number(x.id) === Number(nodeId)) || nodes.find((x) => x.isLocal);
  return n ? nodeLabel(n) : '-';
}

const rowKey = (r) => `${r.kind}:${r.id}`;

function numberCell(r, field, placeholder, step = 1) {
  const v = Number(r[field]) || 0;
  return `<input type="number" class="form-control lim-input" min="0" step="${step}" value="${v || ''}" placeholder="${placeholder}" data-lim-field="${field}" aria-label="${placeholder}">`;
}

function renderRow(r) {
  const disk = mbToDisk(r.diskMb);
  const isDb = r.kind === 'database';
  return `<tr data-lim-row="${escapeHtml(rowKey(r))}">
    <td><div class="acct-person-name">${escapeHtml(r.name)}</div></td>
    <td>${KIND_TEXT[r.kind] || escapeHtml(r.kind)}</td>
    <td>${escapeHtml(nodeName(r.nodeId))}</td>
    <td>${numberCell(r, 'cpu', 'No limit', 10)}</td>
    <td>${numberCell(r, 'memoryMb', 'No limit', 64)}</td>
    <td><div class="limits-disk">
      <input type="number" class="form-control lim-input" min="0" value="${disk.value}" placeholder="No limit" data-lim-field="disk" aria-label="Disk size">
      <select class="form-control" data-lim-field="diskUnit" aria-label="Disk size unit"><option value="MB" ${disk.unit === 'MB' ? 'selected' : ''}>MB</option><option value="GB" ${disk.unit === 'GB' ? 'selected' : ''}>GB</option></select>
    </div></td>
    <td>${isDb ? '<span class="acct-muted">-</span>' : numberCell(r, 'backups', 'No limit')}</td>
    <td style="text-align:right"><button type="button" class="btn btn-primary btn-sm" data-lim-save disabled>Save</button></td>
  </tr>`;
}

function render() {
  const body = document.getElementById('limits-list-body');
  if (!body) return;
  const q = (document.getElementById('limits-search')?.value || '').trim().toLowerCase();
  const list = rows.filter((r) => !q
    || [r.name, KIND_TEXT[r.kind], nodeName(r.nodeId)].some((v) => String(v || '').toLowerCase().includes(q)));
  body.innerHTML = list.length
    ? list.map(renderRow).join('')
    : `<tr><td colspan="8" class="acct-empty">${loaded ? 'No apps match.' : 'Loading...'}</td></tr>`;
}

export async function loadLimitsView() {
  try {
    const res = await api('/api/limits', { noCache: true });
    rows = res.limits || [];
    loaded = true;
    const banner = document.getElementById('limits-banner');
    if (banner) {
      const e = res.enforcement;
      banner.hidden = !e || e.works;
      if (e && !e.works) banner.textContent = `CPU and memory limits are not enforced on this server: ${e.reason}. Disk and backup limits still work.`;
    }
  } catch (err) {
    toast(err.message, 'error');
  }
  render();
}

function readRow(tr) {
  const get = (f) => tr.querySelector(`[data-lim-field="${f}"]`)?.value ?? '';
  return {
    cpu: positiveInt(get('cpu')),
    memoryMb: positiveInt(get('memoryMb')),
    diskMb: diskToMb(get('disk'), get('diskUnit') || 'MB'),
    backups: positiveInt(get('backups')),
  };
}

document.getElementById('limits-search')?.addEventListener('input', render);

const listBody = document.getElementById('limits-list-body');
listBody?.addEventListener('input', (e) => {
  const tr = e.target.closest('[data-lim-row]');
  if (tr) tr.querySelector('[data-lim-save]').disabled = false;
});
listBody?.addEventListener('change', (e) => {
  const tr = e.target.closest('[data-lim-row]');
  if (tr) tr.querySelector('[data-lim-save]').disabled = false;
});

listBody?.addEventListener('click', async (e) => {
  const btn = e.target.closest('[data-lim-save]');
  if (!btn) return;
  const tr = btn.closest('[data-lim-row]');
  const row = rows.find((r) => rowKey(r) === tr.dataset.limRow);
  if (!row) return;
  const limits = readRow(tr);
  const path = row.kind === 'database' ? `/api/databases/${row.id}/limits` : `/api/projects/${row.id}/limits`;
  btn.disabled = true;
  try {
    const res = await api(path, { method: 'PUT', body: limits });
    Object.assign(row, limits);
    tr.outerHTML = renderRow(row);
    toast(res && res.appliesOnRestart ? 'Saved. CPU and memory apply the next time the database starts.' : 'Limits saved.', 'success');
  } catch (err) {
    btn.disabled = false;
    toast(err.message, 'error');
  }
});

window.loadLimitsView = loadLimitsView;
