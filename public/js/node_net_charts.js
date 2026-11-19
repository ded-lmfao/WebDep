/**
 * Live per-node network charts on the Nodes view.
 *
 * While the Nodes view is open and the tab is visible, polls /api/overview/network once a second
 * and feeds each node's history (its `perNode` map) into that node card's chart. Chart state lives
 * here, keyed by node id, so it survives the node cards being re-rendered; only the DOM is rebuilt.
 */

import { state } from './state.js';
import { api } from './api.js';
import { formatBytesShort, formatRate, setText, renderSmoothNetFrame } from './app_usage.js';
import {
  OV_NET_WINDOW_MS,
  OV_NET_SVG_W,
  OV_NET_SVG_H,
  OV_NET_TOP,
  OV_NET_BOTTOM,
  OV_NET_Y_ZERO_IN,
  OV_NET_Y_ZERO_OUT,
  OV_NET_POLL_MS,
  createNetChartState,
  normalizeAndMergeNetState,
} from './net_chart.js';

// Remote nodes report every ~5s, so draw a little further behind live than the local-only chart.
const NODE_NET_LAG_MS = 2600;

const charts = new Map();
let pollTimer = null;
let rafId = null;
let pollInFlight = false;

function idPrefix(nodeId) {
  return `node-net-${nodeId}`;
}

function chartFor(nodeId) {
  const key = String(nodeId);
  let entry = charts.get(key);
  if (!entry) {
    const p = idPrefix(key);
    entry = {
      state: createNetChartState(),
      cfg: {
        svgW: OV_NET_SVG_W,
        svgH: OV_NET_SVG_H,
        topPad: OV_NET_TOP,
        yZeroIn: OV_NET_Y_ZERO_IN,
        yZeroOut: OV_NET_Y_ZERO_OUT,
        windowMs: OV_NET_WINDOW_MS,
        lagMs: NODE_NET_LAG_MS,
        minRawPeak: 0,
        singleSamplePeakText: 'Peak 0 B/s',
        ids: {
          inArea: `${p}-in-area`,
          outArea: `${p}-out-area`,
          inGlow: `${p}-in-glow`,
          outGlow: `${p}-out-glow`,
          inLine: `${p}-in-line`,
          outLine: `${p}-out-line`,
          inDot: `${p}-in-dot`,
          outDot: `${p}-out-dot`,
          crosshair: `${p}-crosshair`,
          peak: `${p}-peak`,
          inVal: `${p}-in`,
          outVal: `${p}-out`,
        },
      },
    };
    charts.set(key, entry);
  }
  return entry;
}

/** Network chart markup for one node card. Values are filled in by the poller, not baked in. */
export function nodeNetChartHtml(nodeId) {
  const p = idPrefix(nodeId);
  const gradIn = `${p}-grad-in`;
  const gradOut = `${p}-grad-out`;
  const clip = `${p}-clip`;
  return `
    <div class="node-net-box">
      <div class="overview-net-head">
        <div class="node-metric-label" style="justify-content:flex-start; gap:6px;">
          <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5">
            <polyline points="22 12 18 12 15 21 9 3 6 12 2 12"></polyline>
          </svg>
          <span>Network</span>
        </div>
        <div class="app-net-legend">
          <span class="app-net-pill net-in"><span class="app-net-dot"></span>In <strong id="${p}-in">0 B/s</strong></span>
          <span class="app-net-pill net-out"><span class="app-net-dot"></span>Out <strong id="${p}-out">0 B/s</strong></span>
        </div>
      </div>
      <div class="app-usage-net-chart overview-net-chart" id="${p}-chart">
        <svg class="app-net-svg" viewBox="0 0 ${OV_NET_SVG_W} ${OV_NET_SVG_H}" preserveAspectRatio="none">
          <defs>
            <clipPath id="${clip}"><rect x="0" y="0" width="${OV_NET_SVG_W}" height="${OV_NET_SVG_H}"></rect></clipPath>
            <linearGradient id="${gradIn}" x1="0" y1="0" x2="0" y2="1">
              <stop offset="0%" stop-color="#4cc38a" stop-opacity="0.28"/>
              <stop offset="100%" stop-color="#4cc38a" stop-opacity="0.01"/>
            </linearGradient>
            <linearGradient id="${gradOut}" x1="0" y1="0" x2="0" y2="1">
              <stop offset="0%" stop-color="#4f8ef7" stop-opacity="0.26"/>
              <stop offset="100%" stop-color="#4f8ef7" stop-opacity="0.01"/>
            </linearGradient>
          </defs>
          <line x1="0" y1="18" x2="${OV_NET_SVG_W}" y2="18" stroke="rgba(238,241,247,0.045)" stroke-dasharray="3 4"/>
          <line x1="0" y1="34" x2="${OV_NET_SVG_W}" y2="34" stroke="rgba(238,241,247,0.045)" stroke-dasharray="3 4"/>
          <line x1="0" y1="${OV_NET_BOTTOM}" x2="${OV_NET_SVG_W}" y2="${OV_NET_BOTTOM}" stroke="rgba(238,241,247,0.08)"/>
          <g clip-path="url(#${clip})">
            <path id="${p}-in-area" d="" fill="url(#${gradIn})"/>
            <path id="${p}-out-area" d="" fill="url(#${gradOut})"/>
            <path id="${p}-in-glow" d="" fill="none" stroke="rgba(76, 195, 138, 0.22)" stroke-width="5" vector-effect="non-scaling-stroke" stroke-linecap="round" stroke-linejoin="round"/>
            <path id="${p}-out-glow" d="" fill="none" stroke="rgba(79, 142, 247, 0.22)" stroke-width="5" vector-effect="non-scaling-stroke" stroke-linecap="round" stroke-linejoin="round"/>
            <path id="${p}-in-line" d="" fill="none" stroke="#4cc38a" stroke-width="2" vector-effect="non-scaling-stroke" stroke-linecap="round" stroke-linejoin="round"/>
            <path id="${p}-out-line" d="" fill="none" stroke="#4f8ef7" stroke-width="1.8" vector-effect="non-scaling-stroke" stroke-linecap="round" stroke-linejoin="round"/>
          </g>
        </svg>
        <div id="${p}-in-dot" class="app-net-live-marker net-in" style="opacity:0;"></div>
        <div id="${p}-out-dot" class="app-net-live-marker net-out" style="opacity:0;"></div>
        <div id="${p}-crosshair" class="app-net-crosshair" style="display:none;"></div>
        <span id="${p}-peak" class="app-net-scale-chip">Peak 0 B/s</span>
      </div>
      <div class="stat-meta overview-net-footer">
        <span id="${p}-iface">-- NICs</span>
        <span id="${p}-totals" class="app-net-totals">&darr; 0 B &nbsp;&uarr; 0 B</span>
      </div>
    </div>
  `;
}

function isNodesViewLive() {
  return state.currentView === 'nodes' && !(typeof document !== 'undefined' && document.hidden);
}

function applyNodeNet(nodeId, net, opts) {
  if (!net || typeof net !== 'object') return;
  const entry = chartFor(nodeId);
  const { inBps, outBps, updated } = normalizeAndMergeNetState(entry.state, net, opts);
  if (!updated) return;
  const p = idPrefix(nodeId);
  if (entry.state.hoverRatio === null) {
    setText(`${p}-in`, formatRate(inBps));
    setText(`${p}-out`, formatRate(outBps));
  }
  const rx = Number(net.totalRxBytes ?? net.total_rx_bytes ?? 0);
  const tx = Number(net.totalTxBytes ?? net.total_tx_bytes ?? 0);
  setText(`${p}-totals`, `↓ ${formatBytesShort(rx)}  ↑ ${formatBytesShort(tx)}`);
  const ifaces = Number(net.interfaceCount ?? net.interface_count ?? 0);
  if (ifaces > 0) setText(`${p}-iface`, `${ifaces} NIC${ifaces === 1 ? '' : 's'}`);
}

function drawFrame(nowPerf = 0) {
  rafId = null;
  if (!isNodesViewLive()) return;
  let anyActive = false;
  for (const [nid, entry] of charts.entries()) {
    if (!document.getElementById(`${idPrefix(nid)}-in-line`)) continue;
    if (renderSmoothNetFrame(entry.cfg, entry.state, nowPerf) && entry.state.samples.length >= 2) {
      anyActive = true;
    }
  }
  if (anyActive) startRaf();
}

function startRaf() {
  if (rafId !== null || !isNodesViewLive() || typeof requestAnimationFrame !== 'function') return;
  rafId = requestAnimationFrame(drawFrame);
}

function stopRaf() {
  if (rafId !== null && typeof cancelAnimationFrame === 'function') cancelAnimationFrame(rafId);
  rafId = null;
  for (const entry of charts.values()) entry.state.lastDrawPerf = 0;
}

async function poll() {
  if (pollInFlight || !state.user || !isNodesViewLive()) return;
  pollInFlight = true;
  try {
    const net = await api('/api/overview/network?nodeId=all', { noCache: true });
    if (net && net.perNode && typeof net.perNode === 'object') {
      for (const [nid, nodeNet] of Object.entries(net.perNode)) {
        applyNodeNet(nid, nodeNet, { fromSnapshot: false });
      }
      startRaf();
    }
  } catch {
    // Keep the last drawn charts on a transient network error.
  } finally {
    pollInFlight = false;
  }
}

export function stopNodeNetCharts() {
  if (pollTimer !== null) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
  stopRaf();
}

function wireHover(nodeId) {
  const box = document.getElementById(`${idPrefix(nodeId)}-chart`);
  if (!box || box._hoverWired) return;
  box._hoverWired = true;
  const entry = chartFor(nodeId);
  box.onmousemove = (ev) => {
    const rect = box.getBoundingClientRect();
    if (!rect.width) return;
    entry.state.hoverRatio = Math.max(0, Math.min(1, (ev.clientX - rect.left) / rect.width));
  };
  box.onmouseleave = () => {
    entry.state.hoverRatio = null;
    const cross = document.getElementById(`${idPrefix(nodeId)}-crosshair`);
    if (cross) cross.style.display = 'none';
  };
}

/**
 * Call after the node cards are (re)rendered: wires hover on the fresh chart DOM, redraws from the
 * kept samples straight away, and makes sure the poller is running while the Nodes view is open.
 */
export function syncNodeNetCharts(nodeIds) {
  for (const id of nodeIds) {
    chartFor(id);
    wireHover(id);
  }
  if (!isNodesViewLive()) {
    stopNodeNetCharts();
    return;
  }
  drawFrame(typeof performance !== 'undefined' ? performance.now() : 0);
  if (pollTimer === null) {
    void poll();
    pollTimer = setInterval(() => {
      if (state.currentView !== 'nodes') {
        stopNodeNetCharts();
        return;
      }
      void poll();
    }, OV_NET_POLL_MS);
  }
}

if (typeof document !== 'undefined') {
  document.addEventListener('visibilitychange', () => {
    if (document.hidden) {
      stopRaf();
    } else if (state.currentView === 'nodes') {
      void poll();
      startRaf();
    }
  });
}
