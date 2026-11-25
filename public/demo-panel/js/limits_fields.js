/**
 * Resource limit fields (CPU, memory, disk, backups) shared by the create forms and the
 * admin App Configs tab. Every field uses 0 or empty for "no limit".
 */

const MB_PER_GB = 1024;

/** Size typed with a unit -> MB (0 when empty). */
export function diskToMb(value, unit) {
  const n = Math.max(0, Number(value) || 0);
  return Math.round(unit === 'GB' ? n * MB_PER_GB : n);
}

/** MB -> the form's value and unit (GB when it divides evenly, else MB). */
export function mbToDisk(mb) {
  const n = Math.max(0, Number(mb) || 0);
  if (n && n % MB_PER_GB === 0) return { value: n / MB_PER_GB, unit: 'GB' };
  return { value: n || '', unit: 'MB' };
}

export function positiveInt(value) {
  return Math.max(0, Math.floor(Number(value) || 0));
}

function fieldsHtml(prefix, withBackups) {
  const id = (name) => `${prefix}-limit-${name}`;
  return `<details class="limits-fields">
    <summary>Resource limits <span class="acct-muted">(optional)</span></summary>
    <p class="form-hint" style="margin:6px 0 10px">Leave a field empty, or 0, for no limit. Only admins can change these later.</p>
    <div class="limits-grid">
      <div class="form-group">
        <label class="form-label" for="${id('cpu')}">CPU</label>
        <input type="number" id="${id('cpu')}" class="form-control" min="0" step="10" placeholder="Percent of one core">
      </div>
      <div class="form-group">
        <label class="form-label" for="${id('memory')}">Memory (MB)</label>
        <input type="number" id="${id('memory')}" class="form-control" min="0" step="64" placeholder="Megabytes of memory">
      </div>
      <div class="form-group">
        <label class="form-label" for="${id('disk')}">Disk</label>
        <div class="limits-disk">
          <input type="number" id="${id('disk')}" class="form-control" min="0" placeholder="Size">
          <select id="${id('disk-unit')}" class="form-control" aria-label="Disk size unit"><option value="MB">MB</option><option value="GB">GB</option></select>
        </div>
        <span class="form-hint">The app is stopped if it grows past this.</span>
      </div>
      ${withBackups ? `<div class="form-group">
        <label class="form-label" for="${id('backups')}">Backups</label>
        <input type="number" id="${id('backups')}" class="form-control" min="0" placeholder="Number of backups">
        <span class="form-hint">Most backups it can hold at a time.</span>
      </div>` : ''}
    </div>
  </details>`;
}

const val = (id) => document.getElementById(id)?.value ?? '';

/** The limits typed into the fields of `prefix` (all 0 when untouched). */
export function readLimitFields(prefix) {
  const id = (name) => `${prefix}-limit-${name}`;
  return {
    cpu: positiveInt(val(id('cpu'))),
    memoryMb: positiveInt(val(id('memory'))),
    diskMb: diskToMb(val(id('disk')), val(id('disk-unit'))),
    backups: positiveInt(val(id('backups'))),
  };
}

export function resetLimitFields(prefix) {
  document.getElementById(`${prefix}-limits-slot`)?.querySelectorAll('input').forEach((i) => { i.value = ''; });
  const unit = document.getElementById(`${prefix}-limit-disk-unit`);
  if (unit) unit.value = 'MB';
}

/** The fields only make sense when creating; editing happens in Admin Panel > App Configs. */
export function setLimitFieldsVisible(prefix, visible) {
  const slot = document.getElementById(`${prefix}-limits-slot`);
  if (slot) slot.hidden = !visible;
}

// [prefix, includes the backups field]. Databases have no backups.
for (const [prefix, withBackups] of [['website', true], ['service', true], ['db-create', false]]) {
  const slot = document.getElementById(`${prefix}-limits-slot`);
  if (slot) slot.innerHTML = fieldsHtml(prefix, withBackups);
}
