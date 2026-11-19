/**
 * Phone layout for data tables: CSS turns each `table.table` row into a card below 680px wide
 * (see "Tables as cards" in responsive.css) and labels every value with its column name through
 * `td[data-label]`. This module keeps those labels in sync with each table's header row, including
 * tables that are re-rendered later, so no individual table renderer has to know about phones.
 */

function headerLabels(table) {
  const row = table.tHead?.rows?.[0];
  if (!row) return null;
  const labels = [];
  for (const th of row.cells) {
    const text = th.textContent.trim().replace(/\s+/g, ' ');
    const span = Math.max(1, th.colSpan || 1);
    for (let i = 0; i < span; i++) labels.push(text);
  }
  return labels;
}

function labelTable(table) {
  const labels = headerLabels(table);
  if (!labels) return;
  for (const body of table.tBodies) {
    for (const row of body.rows) {
      let col = 0;
      for (const cell of row.cells) {
        // A cell spanning the whole row (empty states, "loading…") stays a plain block.
        const label = cell.colSpan > 1 ? '' : (labels[col] || '');
        if (cell.getAttribute('data-label') !== label) cell.setAttribute('data-label', label);
        col += Math.max(1, cell.colSpan || 1);
      }
    }
  }
}

let scheduled = false;
function labelAllTables() {
  scheduled = false;
  document.querySelectorAll('table.table').forEach(labelTable);
}

function scheduleLabelling() {
  if (scheduled) return;
  scheduled = true;
  requestAnimationFrame(labelAllTables);
}

function init() {
  labelAllTables();
  // Only structure changes matter; attribute writes above do not re-trigger the observer.
  new MutationObserver(scheduleLabelling).observe(document.body, { childList: true, subtree: true });
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', init);
} else {
  init();
}
