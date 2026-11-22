/** Admin Panel › App Configs: ports, CPU, memory, disk, and backup settings for every app. */

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
  return `<tr data-lim-row="${escapeHtml(rowKey(r))}">
    <td><div class="acct-person-name">${escapeHtml(r.name)}</div></td>
    <td>${KIND_TEXT[r.kind] || escapeHtml(r.kind)}</td>
    <td>${escapeHtml(nodeName(r.nodeId))}</td>
    <td><input type="number" class="form-control lim-input lim-port" min="1" max="65535" value="${r.port || ''}" placeholder="Next free" data-lim-field="port" aria-label="Port"></td>
    <td><input type="text" class="form-control lim-input lim-extra" value="${escapeHtml((r.extraPorts || []).join(', '))}" placeholder="Ports or ranges" data-lim-field="extraPorts" autocomplete="off" aria-label="Extra ports"></td>
    <td>${numberCell(r, 'cpu', 'No limit', 10)}</td>
    <td>${numberCell(r, 'memoryMb', 'No limit', 64)}</td>
    <td><div class="limits-disk">
      <input type="number" class="form-control lim-input" min="0" value="${disk.value}" placeholder="No limit" data-lim-field="disk" aria-label="Disk size">
      <select class="form-control" data-lim-field="diskUnit" aria-label="Disk size unit"><option value="MB" ${disk.unit === 'MB' ? 'selected' : ''}>MB</option><option value="GB" ${disk.unit === 'GB' ? 'selected' : ''}>GB</option></select>
    </div></td>
    <td>${numberCell(r, 'backups', 'No limit')}</td>
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
    : `<tr><td colspan="10" class="acct-empty">${loaded ? 'No apps match.' : 'Loading...'}</td></tr>`;
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
    const iso = document.getElementById('isolation-banner');
    if (iso) {
      const i = res.isolation;
      iso.hidden = !i || i.active;
      if (i && !i.requested) iso.textContent = 'App isolation is turned off (WEBDEP_ISOLATE_APPS=0): apps run as the same server user as the panel. Remove that setting to give each app its own namespaces, a private view of the file system and an unprivileged user.';
      else if (i && !i.active) iso.textContent = `App isolation cannot be used on this server: ${i.reason}. Apps run without it.`;
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

function readPorts(tr) {
  const get = (f) => tr.querySelector(`[data-lim-field="${f}"]`)?.value ?? '';
  const port = Number(get('port')) || null;
  const extra = String(get('extraPorts')).trim();
  return { port, extra };
}

/** Saves what changed: the main port, the extra ports, then the limits. Returns a short summary. */
async function savePorts(row, tr) {
  const base = row.kind === 'database' ? `/api/databases/${row.id}` : `/api/projects/${row.id}`;
  const { port, extra } = readPorts(tr);
  const notes = [];
  if (port && port !== Number(row.port)) {
    const res = await api(`${base}/port`, { method: 'PUT', body: { port } });
    row.port = port;
    if (res && res.linkedAppNeedsRestart) notes.push('Restart the linked app to use the new database port.');
  }
  const wanted = extra ? extra.split(/[\s,]+/).filter(Boolean).join(',') : '';
  const current = (row.extraPorts || []).join(',');
  if (wanted !== current) {
    const res = await api(`${base}/extra-ports`, { method: 'PUT', body: { ports: extra } });
    row.extraPorts = res && Array.isArray(res.extraPorts) ? res.extraPorts : [];
    if (res && res.firewallNote) notes.push(res.firewallNote);
    else if (row.kind !== 'database') notes.push('Restart the app to use its extra ports.');
  }
  return notes;
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
    const notes = await savePorts(row, tr);
    const res = await api(path, { method: 'PUT', body: limits });
    Object.assign(row, limits);
    tr.outerHTML = renderRow(row);
    if (res && res.appliesOnRestart) notes.push('CPU and memory apply the next time the database starts.');
    toast(['Saved.', ...notes].join(' '), 'success');
  } catch (err) {
    btn.disabled = false;
    toast(err.message, 'error');
    // A port may have been saved before the failure: show what is stored now.
    void loadLimitsView();
  }
});

window.loadLimitsView = loadLimitsView;
