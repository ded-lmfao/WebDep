/**
 * Limit alerts
 * Shows a pop-up when an app the user can see reaches or crosses its CPU, memory or disk limit.
 * The server records these as events (checked after an app starts and every 2 minutes after).
 */

import { state } from './state.js';
import { api } from './api.js';

const SEEN_KEY = 'webdep.limitEventsSeen';
const POLL_MS = 20_000;
let timer = null;
let showing = false;

function readSeen() {
  try {
    const v = localStorage.getItem(SEEN_KEY);
    return v === null ? null : Number(v);
  } catch (_) {
    return null;
  }
}

function writeSeen(id) {
  try {
    localStorage.setItem(SEEN_KEY, String(id));
  } catch (_) { /* storage unavailable: events may show again, which is harmless */ }
}

async function poll() {
  if (showing || document.hidden || !state.user || !state.csrf) return;
  // Never stack on top of another dialog; the events are still waiting on the server.
  if (document.getElementById('dialog-modal')?.classList.contains('active')) return;
  const seen = readSeen();
  let res;
  try {
    res = await api(`/api/limit-events${seen === null ? '' : `?since=${seen}`}`, { noCache: true });
  } catch (_) {
    return;
  }
  writeSeen(res.latestId ?? seen ?? 0);
  // First time on this browser: start from now instead of replaying old events.
  if (seen === null || !Array.isArray(res.events) || !res.events.length) return;
  showing = true;
  try {
    for (const e of res.events) {
      await window.showLimitDialog(e.limit, e.message);
    }
  } finally {
    showing = false;
  }
}

export function startLimitAlerts() {
  if (timer) return;
  timer = setInterval(() => void poll(), POLL_MS);
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden) void poll();
  });
  void poll();
}
