/**
 * WebDep Database Modals & Connection Subsystem
 * Creation wizard, connection details viewer, and network allowlist access dialogs.
 */

import { state, showPromptDialog, copyTextToClipboard, setBtnLoading } from './state.js';
import { api, toast } from './api.js';

export function openCreateDatabaseModal() {
  const modal = document.getElementById('modal-create-database');
  if (!modal) return;

  const getPort = window.getNextAvailablePort || ((p) => p);
  const portInput = document.getElementById('db-create-port');
  if (portInput) {
    portInput.value = getPort(5432);
  }

  generateRandomDbPassword('db-create-password');

  const nameEl = document.getElementById('db-create-name');
  if (nameEl) nameEl.value = '';
  const dbNameEl = document.getElementById('db-create-dbname');
  if (dbNameEl) {
    dbNameEl.value = '';
    delete dbNameEl.dataset.touched;
  }
  const userEl = document.getElementById('db-create-user');
  if (userEl) userEl.value = 'app_user';
  const domCreateInput = document.getElementById('db-create-domain');
  if (domCreateInput) domCreateInput.value = '';
  const suEl = document.getElementById('db-create-superuser');
  if (suEl) suEl.checked = false;
  const accessEl = document.getElementById('db-create-access-mode');
  if (accessEl) accessEl.value = 'local';
  const ipsEl = document.getElementById('db-create-allowed-ips');
  if (ipsEl) ipsEl.value = '';
  toggleDatabaseAccessIpFields('db-create-access-mode', 'db-create-allowed-ips-wrap');

  modal.classList.add('active');
}

// Suggest a PostgreSQL database name from the display name until the user types their own.
document.getElementById('db-create-dbname')?.addEventListener('input', (e) => {
  e.target.dataset.touched = '1';
});
document.getElementById('db-create-name')?.addEventListener('input', (e) => {
  const dbNameEl = document.getElementById('db-create-dbname');
  if (!dbNameEl || dbNameEl.dataset.touched === '1') return;
  let slug = e.target.value.toLowerCase().replace(/[^a-z0-9]+/g, '_').replace(/^_/, '').replace(/_$/, '').slice(0, 48);
  if (slug && /^[0-9]/.test(slug)) slug = `db_${slug}`;
  dbNameEl.value = slug;
});

export function generateRandomDbPassword(targetInputId) {
  const chars = 'abcdefghjkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789-_.~';
  const array = new Uint8Array(24);
  window.crypto.getRandomValues(array);
  let pass = '';
  for (let i = 0; i < array.length; i++) {
    pass += chars[array[i] % chars.length];
  }
  const input = document.getElementById(targetInputId);
  if (input) {
    input.value = pass;
    input.type = 'text';
    setTimeout(() => { input.type = 'password'; }, 3000);
  }
}

export function togglePasswordVisibility(inputId) {
  const input = document.getElementById(inputId);
  if (!input) return;
  input.type = input.type === 'password' ? 'text' : 'password';
  // The database details dialog also masks the password inside its connection URLs.
  if (inputId === 'db-detail-password') {
    const reveal = input.type === 'text';
    ['db-detail-internal-url', 'db-detail-external-url'].forEach((id) => {
      const el = document.getElementById(id);
      if (el && el.dataset.realValue !== undefined) {
        el.value = reveal ? el.dataset.realValue : maskUrlPassword(el.dataset.realValue);
      }
    });
  }
}

function maskUrlPassword(url) {
  return String(url || '').replace(/^([a-z][a-z0-9+.-]*:\/\/[^:@/]+:)([^@]*)@/i, '$1••••••••@');
}

export function toggleDatabaseAccessIpFields(selectId, wrapId) {
  const sel = document.getElementById(selectId);
  const wrap = document.getElementById(wrapId);
  if (!sel || !wrap) return;
  wrap.style.display = sel.value === 'allowlist' ? 'block' : 'none';
}

export async function handleCreateDatabaseSubmit(e) {
  e.preventDefault();
  const btn = document.getElementById('db-create-submit-btn');
  const name = document.getElementById('db-create-name')?.value.trim();
  const db_name = document.getElementById('db-create-dbname')?.value.trim();
  const db_user = document.getElementById('db-create-user')?.value.trim();
  const db_password = document.getElementById('db-create-password')?.value.trim();
  const port = Number(document.getElementById('db-create-port')?.value);
  const is_superuser = document.getElementById('db-create-superuser')?.checked;
  const access_mode = document.getElementById('db-create-access-mode')?.value;
  const allowed_ips = document.getElementById('db-create-allowed-ips')?.value.trim();
  const domain = document.getElementById('db-create-domain')?.value.trim() || null;

  if (access_mode === 'allowlist' && !allowed_ips) {
    toast('Please specify at least one remote IP or CIDR for the allowlist.', 'warning');
    return;
  }

  const progressBox = document.getElementById('db-create-progress-container');
  const progressStep = document.getElementById('db-create-progress-step');
  const progressBar = document.getElementById('db-create-progress-bar');
  const progressDetail = document.getElementById('db-create-progress-detail');

  if (progressBox) {
    progressBox.style.display = 'block';
    if (progressBar) {
      progressBar.className = 'progress-fill green';
      progressBar.style.width = '20%';
    }
    if (progressStep) progressStep.textContent = 'Stage 1/5';
    if (progressDetail) progressDetail.textContent = 'Allocating dedicated TCP port and preparing cluster directory';
  }

  const renderPipeline = window.renderDatabasePipeline || (() => {});
  renderPipeline('sandbox', 'running', 'Allocating port and sandbox');

  const t1 = setTimeout(() => {
    renderPipeline('initdb', 'running', 'Running initdb');
    if (progressBar) progressBar.style.width = '45%';
    if (progressStep) progressStep.textContent = 'Stage 2/5';
    if (progressDetail) progressDetail.textContent = 'Initializing cluster via initdb with SCRAM-SHA-256 credentials';
  }, 750);

  const t2 = setTimeout(() => {
    renderPipeline('security', 'running', 'Applying HBA rules');
    if (progressBar) progressBar.style.width = '70%';
    if (progressStep) progressStep.textContent = 'Stage 3/5';
    if (progressDetail) progressDetail.textContent = 'Configuring pg_hba.conf client auth and UFW firewall rules';
  }, 1600);

  const t3 = setTimeout(() => {
    renderPipeline('spawning', 'running', 'Spawning pg_ctl engine');
    if (progressBar) progressBar.style.width = '90%';
    if (progressStep) progressStep.textContent = 'Stage 4/5';
    if (progressDetail) progressDetail.textContent = 'Spawning isolated PostgreSQL process container via pg_ctl';
  }, 2500);

  setBtnLoading(btn, true, 'Create & Start Database', 'Provisioning');
  try {
    const created = await api('/api/databases', {
      method: 'POST',
      timeoutMs: 60000,
      body: {
        name,
        dbName: db_name,
        dbUser: db_user,
        dbPassword: db_password,
        port,
        isSuperuser: is_superuser,
        accessMode: access_mode,
        allowedIps: allowed_ips,
        domain: domain || undefined,
      },
    });

    clearTimeout(t1);
    clearTimeout(t2);
    clearTimeout(t3);

    renderPipeline('healthy', 'success', `Online (${created.name})`);
    if (progressBar) progressBar.style.width = '100%';
    if (progressStep) {
      progressStep.textContent = 'Ready (5/5)';
      progressStep.className = 'badge badge-success';
    }
    if (progressDetail) progressDetail.textContent = `PostgreSQL database '${created.name}' listening on port ${created.port}.`;

    toast(`PostgreSQL database '${created.name}' provisioned and started on port ${created.port}.`, 'success');

    setTimeout(() => {
      document.getElementById('modal-create-database')?.classList.remove('active');
      if (progressBox) progressBox.style.display = 'none';
    }, 700);

    if (window.loadDatabases) await window.loadDatabases();
    if (window.renderDatabasesList) window.renderDatabasesList();
  } catch (err) {
    clearTimeout(t1);
    clearTimeout(t2);
    clearTimeout(t3);
    renderPipeline('initdb', 'failed', 'Provisioning failed');
    if (progressBar) {
      progressBar.style.width = '100%';
      progressBar.className = 'progress-fill red';
    }
    if (progressStep) {
      progressStep.textContent = 'Failed';
      progressStep.className = 'badge badge-danger';
    }
    if (progressDetail) progressDetail.textContent = `Error: ${err.message}`;
    if (window.handleNoFreePort?.(err)) return;
    toast(`Database creation failed: ${err.message}`, 'error');
  } finally {
    setBtnLoading(btn, false, 'Create & Start Database');
  }
}

export function showDatabaseDetails(id) {
  const db = (state.databases || []).find((d) => d.id === Number(id));
  if (!db) return;

  const modal = document.getElementById('modal-database-details');
  if (!modal) return;

  const accessMode = db.accessMode || db.access_mode || 'local';
  const title = document.getElementById('db-details-title');
  if (title) title.textContent = `Connection Details — ${db.name}`;
  // Connection URLs embed the password: show them masked like the password field, copy the real value.
  const setUrl = (el, url) => {
    if (!el) return;
    el.dataset.realValue = url;
    el.value = maskUrlPassword(url);
  };
  const extEl = document.getElementById('db-detail-external-url');
  const extVal = db.externalUrl || db.external_url || '';
  setUrl(document.getElementById('db-detail-internal-url'), db.internalUrl || db.internal_url || '');
  setUrl(extEl, extVal);
  const nameEl = document.getElementById('db-detail-dbname');
  if (nameEl) nameEl.value = db.dbName || db.db_name || '';
  const userEl = document.getElementById('db-detail-user');
  if (userEl) userEl.value = db.dbUser || db.db_user || '';
  const portEl = document.getElementById('db-detail-port');
  if (portEl) portEl.value = db.port || 5432;
  const passEl = document.getElementById('db-detail-password');
  if (passEl) {
    passEl.value = db.dbPassword || db.db_password || '';
    passEl.type = 'password';
  }
  const psqlEl = document.getElementById('db-detail-psql');
  if (psqlEl) {
    psqlEl.value = db.psqlCommand || db.psql_command || '';
  }

  const extWrap = document.getElementById('db-detail-external-url-wrap');
  if (extWrap) {
    const isLocalOnly = accessMode === 'local' && !db.domain;
    extWrap.style.display = isLocalOnly ? 'none' : 'block';
  }

  modal.classList.add('active');
}

export async function deleteDatabase(id, name) {
  const typed = await showPromptDialog({
    title: `Delete database "${name}"`,
    message: `This stops the database and permanently deletes all of its data on disk. It can't be undone.\n\nType "${name}" to confirm:`,
    placeholder: 'Type the name to confirm',
    confirmText: 'Delete database',
    danger: true,
  });
  if (typed === null) return;
  if (typed.trim() !== name) {
    toast(`The name didn't match, so nothing was deleted.`, 'warning');
    return;
  }

  try {
    toast(`Deleting database '${name}'`, 'info');
    await api(`/api/databases/${id}`, { method: 'DELETE' });
    toast(`Database '${name}' deleted.`, 'success');
    const wasInsideDb = window.state?.selectedEntityKind === 'database' && Number(window.state.selectedDatabaseId) === Number(id);
    if (window.loadDatabases) await window.loadDatabases();
    if (wasInsideDb && window.state) {
      window.state.selectedDatabaseId = null;
      window.state.selectedEntityKind = 'project';
      if (typeof window.navigateTo === 'function') window.navigateTo('databases');
    } else if (window.renderDatabasesList) {
      window.renderDatabasesList();
    }
  } catch (err) {
    toast(`Failed to delete database: ${err.message}`, 'error');
  }
}

export function copyElementValue(elemId) {
  const el = document.getElementById(elemId);
  if (!el) return;
  void copyTextToClipboard(el.dataset.realValue ?? el.value, 'Copied to clipboard.');
}

// Bind globals for inline HTML event handlers
window.openCreateDatabaseModal = openCreateDatabaseModal;
window.generateRandomDbPassword = generateRandomDbPassword;
window.togglePasswordVisibility = togglePasswordVisibility;
window.toggleDatabaseAccessIpFields = toggleDatabaseAccessIpFields;
window.handleCreateDatabaseSubmit = handleCreateDatabaseSubmit;
window.showDatabaseDetails = showDatabaseDetails;
window.deleteDatabase = deleteDatabase;
window.copyElementValue = copyElementValue;

