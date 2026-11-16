/**
 * WebDep Overview & Real-Time Monitoring Subsystem
 * Resource usage breakdowns, CPU/RAM/Disk metrics, and live workloads table
 */

import { icon } from './icons.js';
import {
  state,
  setHtmlIfChanged,
  escapeHtml,
  matchesNodeFilter,
  getNodeDisplayName,
  getNodeEffectivePublicIp,
  copyBlurIpButton,
  renderBlurPublicIpPill,
} from './state.js';
import { api } from './api.js';
import {
  consumePendingViewStagger,
  animateOverviewReveal,
  animateNumberValue,
  animateProgressBar,
  animateTableRowsStagger,
  animateSectionShow,
} from './motion_ui.js';
import { renderPipelineStages, waterPipelines } from './deployments.js';
import {
  formatBytesShort,
  formatRate,
  setText,
  setElText,
  syncPerfOffset,
  renderSmoothNetFrame,
} from './app_usage.js';

const OV_NET_WINDOW_MS = 45000;
const OV_NET_LAG_MS = 2200;
const OV_NET_SVG_W = 420;
const OV_NET_SVG_H = 56;
const OV_NET_TOP = 7;
const OV_NET_BOTTOM = 50;
const OV_NET_Y_ZERO_IN = 48.2;
const OV_NET_Y_ZERO_OUT = OV_NET_BOTTOM;
const OV_NET_POLL_MS = 1000;

function createNetChartState() {
  return {
    samples: [],
    perfToServerOffset: 0,
    hasOffset: false,
    playheadTs: null,
    displayMaxRate: 1024,
    hoverRatio: null,
    rafId: null,
    pollTimer: null,
    pollInFlight: false,
    lastFetchAt: 0,
    lastDrawPerf: 0,
    lastLiveServerTs: 0,
    hasDrawnInitial: false,
    _chartEl: null,
    _domReady: false,
    _labelEls: null,
    _mergeBuf: [],
  };
}

const _ovNetState = createNetChartState();
const _nodeNetCharts = new Map();

function getOrCreateNodeNetChart(nodeId) {
  const key = String(nodeId);
  let entry = _nodeNetCharts.get(key);
  if (!entry) {
    entry = {
      state: createNetChartState(),
      cfg: {
        svgW: OV_NET_SVG_W,
        svgH: OV_NET_SVG_H,
        topPad: OV_NET_TOP,
        yZeroIn: OV_NET_Y_ZERO_IN,
        yZeroOut: OV_NET_Y_ZERO_OUT,
        windowMs: OV_NET_WINDOW_MS,
        lagMs: 2600,
        minRawPeak: 0,
        singleSamplePeakText: 'Peak 0 B/s',
        ids: {
          inArea: `ov-node-${key}-in-area`,
          outArea: `ov-node-${key}-out-area`,
          inGlow: `ov-node-${key}-in-glow`,
          outGlow: `ov-node-${key}-out-glow`,
          inLine: `ov-node-${key}-in-line`,
          outLine: `ov-node-${key}-out-line`,
          inDot: `ov-node-${key}-in-dot`,
          outDot: `ov-node-${key}-out-dot`,
          crosshair: `ov-node-${key}-crosshair`,
          peak: `ov-node-${key}-peak`,
          inVal: `ov-node-${key}-in`,
          outVal: `ov-node-${key}-out`,
        },
      },
    };
    _nodeNetCharts.set(key, entry);
  }
  return entry;
}

const OV_NET_CHART_CFG = {
  svgW: OV_NET_SVG_W,
  svgH: OV_NET_SVG_H,
  topPad: OV_NET_TOP,
  yZeroIn: OV_NET_Y_ZERO_IN,
  yZeroOut: OV_NET_Y_ZERO_OUT,
  windowMs: OV_NET_WINDOW_MS,
  lagMs: OV_NET_LAG_MS,
  minRawPeak: 0,
  singleSamplePeakText: 'Peak 0 B/s',
  ids: {
    inArea: 'ov-net-in-area',
    outArea: 'ov-net-out-area',
    inGlow: 'ov-net-in-glow',
    outGlow: 'ov-net-out-glow',
    inLine: 'ov-net-in-line',
    outLine: 'ov-net-out-line',
    inDot: 'ov-net-in-dot',
    outDot: 'ov-net-out-dot',
    crosshair: 'ov-net-crosshair',
    peak: 'ov-net-peak-label',
    inVal: 'overview-net-in-val',
    outVal: 'overview-net-out-val',
  },
};

function stopOverviewNetRaf() {
  if (_ovNetState.rafId !== null && typeof cancelAnimationFrame === 'function') {
    cancelAnimationFrame(_ovNetState.rafId);
  }
  _ovNetState.rafId = null;
  _ovNetState.lastDrawPerf = 0;
  for (const entry of _nodeNetCharts.values()) {
    entry.state.lastDrawPerf = 0;
  }
}

function stopOverviewNetPoller() {
  if (_ovNetState.pollTimer !== null) {
    clearInterval(_ovNetState.pollTimer);
    _ovNetState.pollTimer = null;
  }
  stopOverviewNetRaf();
}

function resetOverviewNetGraph() {
  _ovNetState.samples = [];
  _ovNetState.lastLiveServerTs = 0;
  _ovNetState.hasDrawnInitial = false;
  _ovNetState.playheadTs = null;
  _ovNetState.hasOffset = false;
  for (const entry of _nodeNetCharts.values()) {
    entry.state.samples = [];
    entry.state.lastLiveServerTs = 0;
    entry.state.hasDrawnInitial = false;
    entry.state.playheadTs = null;
    entry.state.hasOffset = false;
  }
}

function ensureOverviewNetDom() {
  if (
    _ovNetState._domReady &&
    _ovNetState._chartEl &&
    _ovNetState._chartEl.isConnected !== false
  ) {
    return _ovNetState._chartEl;
  }
  const chartEl = document.getElementById('overview-net-chart');
  if (!chartEl) return null;
  if (chartEl.querySelector('#ov-net-in-line')) {
    _ovNetState._chartEl = chartEl;
    _ovNetState._domReady = true;
    return chartEl;
  }

  chartEl.innerHTML = `
    <svg class="app-net-svg" viewBox="0 0 ${OV_NET_SVG_W} ${OV_NET_SVG_H}" preserveAspectRatio="none" aria-label="Whole VPS global inbound and outbound network traffic">
      <defs>
        <clipPath id="overview-net-clip">
          <rect x="0" y="0" width="${OV_NET_SVG_W}" height="${OV_NET_SVG_H}"></rect>
        </clipPath>
        <linearGradient id="overviewNetGradIn" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stop-color="#4cc38a" stop-opacity="0.28"/>
          <stop offset="100%" stop-color="#4cc38a" stop-opacity="0.01"/>
        </linearGradient>
        <linearGradient id="overviewNetGradOut" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stop-color="#4f8ef7" stop-opacity="0.26"/>
          <stop offset="100%" stop-color="#4f8ef7" stop-opacity="0.01"/>
        </linearGradient>
      </defs>
      <line x1="0" y1="18" x2="${OV_NET_SVG_W}" y2="18" stroke="rgba(238,241,247,0.045)" stroke-dasharray="3 4"/>
      <line x1="0" y1="34" x2="${OV_NET_SVG_W}" y2="34" stroke="rgba(238,241,247,0.045)" stroke-dasharray="3 4"/>
      <line x1="0" y1="${OV_NET_BOTTOM}" x2="${OV_NET_SVG_W}" y2="${OV_NET_BOTTOM}" stroke="rgba(238,241,247,0.08)"/>
      <g clip-path="url(#overview-net-clip)">
        <path id="ov-net-in-area" d="" fill="url(#overviewNetGradIn)"/>
        <path id="ov-net-out-area" d="" fill="url(#overviewNetGradOut)"/>
        <path id="ov-net-in-glow" d="" fill="none" stroke="rgba(76, 195, 138, 0.22)" stroke-width="5" vector-effect="non-scaling-stroke" stroke-linecap="round" stroke-linejoin="round"/>
        <path id="ov-net-out-glow" d="" fill="none" stroke="rgba(79, 142, 247, 0.22)" stroke-width="5" vector-effect="non-scaling-stroke" stroke-linecap="round" stroke-linejoin="round"/>
        <path id="ov-net-in-line" d="" fill="none" stroke="#4cc38a" stroke-width="2" vector-effect="non-scaling-stroke" stroke-linecap="round" stroke-linejoin="round"/>
        <path id="ov-net-out-line" d="" fill="none" stroke="#4f8ef7" stroke-width="1.8" vector-effect="non-scaling-stroke" stroke-linecap="round" stroke-linejoin="round"/>
      </g>
    </svg>
    <div id="ov-net-in-dot" class="app-net-live-marker net-in" style="opacity:0;"></div>
    <div id="ov-net-out-dot" class="app-net-live-marker net-out" style="opacity:0;"></div>
    <div id="ov-net-crosshair" class="app-net-crosshair" style="display:none;"></div>
    <span id="ov-net-peak-label" class="app-net-scale-chip">Peak 0 B/s</span>`;

  chartEl.onmousemove = (ev) => {
    const rect = chartEl.getBoundingClientRect();
    if (!rect.width) return;
    _ovNetState.hoverRatio = Math.max(0, Math.min(1, (ev.clientX - rect.left) / rect.width));
  };
  chartEl.onmouseleave = () => {
    _ovNetState.hoverRatio = null;
    const cross = document.getElementById('ov-net-crosshair');
    if (cross) cross.style.display = 'none';
  };

  _ovNetState._chartEl = chartEl;
  _ovNetState._domReady = true;
  return chartEl;
}

function getOverviewLabelEls() {
  let els = _ovNetState._labelEls;
  if (!els || (els.inValEl && els.inValEl.isConnected === false)) {
    els = {
      inValEl: document.getElementById('overview-net-in-val'),
      outValEl: document.getElementById('overview-net-out-val'),
      ifaceEl: document.getElementById('overview-net-iface'),
      totalsEl: document.getElementById('overview-net-totals'),
    };
    _ovNetState._labelEls = els;
  }
  return els;
}

function drawOverviewNetFrame(nowPerf = 0) {
  _ovNetState.rafId = null;

  if (state.currentView !== 'overview' || (typeof document !== 'undefined' && document.hidden)) {
    stopOverviewNetRaf();
    return;
  }

  let anyActive = false;
  const singleEl = document.getElementById('overview-single-stats-grid');
  if (!singleEl || singleEl.style.display !== 'none') {
    const chartEl = ensureOverviewNetDom();
    if (chartEl && renderSmoothNetFrame(OV_NET_CHART_CFG, _ovNetState, nowPerf)) {
      _ovNetState.hasDrawnInitial = true;
      if (_ovNetState.samples.length >= 2) anyActive = true;
    }
  }

  const multiStrip = document.getElementById('overview-nodes-strip');
  if (multiStrip && multiStrip.style.display !== 'none') {
    for (const [nid, entry] of _nodeNetCharts.entries()) {
      if (!document.getElementById(`ov-node-${nid}-in-line`)) continue;
      if (renderSmoothNetFrame(entry.cfg, entry.state, nowPerf)) {
        entry.state.hasDrawnInitial = true;
        if (entry.state.samples.length >= 2) anyActive = true;
      }
    }
  }

  if (anyActive) {
    startOverviewNetRaf();
  }
}

function startOverviewNetRaf() {
  if (_ovNetState.rafId !== null || state.currentView !== 'overview') return;
  if (typeof document !== 'undefined' && document.hidden) return;
  if (typeof requestAnimationFrame !== 'function') return;

  let maxSamples = _ovNetState.samples.length;
  for (const entry of _nodeNetCharts.values()) {
    if (entry.state.samples.length > maxSamples) {
      maxSamples = entry.state.samples.length;
    }
  }
  if (maxSamples < 2) {
    drawOverviewNetFrame(typeof performance !== 'undefined' ? performance.now() : 0);
    return;
  }
  _ovNetState.rafId = requestAnimationFrame(drawOverviewNetFrame);
}

function normalizeAndMergeNetState(netState, net, opts = {}) {
  if (!net || typeof net !== 'object') return { inBps: 0, outBps: 0, updated: false };
  const rawHistory = Array.isArray(net.history) ? net.history : [];
  const rawSorted = [];
  let normSorted = true;
  let prevNormTs = -Infinity;
  for (let i = 0; i < rawHistory.length; i++) {
    const pt = rawHistory[i];
    if (!pt || typeof pt.ts !== 'number') continue;
    if (pt.ts < prevNormTs) normSorted = false;
    prevNormTs = pt.ts;
    rawSorted.push({
      ts: pt.ts,
      inboundBps: Math.max(0, Number(pt.inboundBps ?? pt.inbound_bps) || 0),
      outboundBps: Math.max(0, Number(pt.outboundBps ?? pt.outbound_bps) || 0),
    });
  }
  if (!normSorted) {
    rawSorted.sort((a, b) => a.ts - b.ts);
  }

  const inBps = Math.max(0, Number(net.inboundBps ?? net.inbound_bps) || 0);
  const outBps = Math.max(0, Number(net.outboundBps ?? net.outbound_bps) || 0);
  if (rawSorted.length === 0) {
    rawSorted.push({ ts: Date.now(), inboundBps: inBps, outboundBps: outBps });
  }

  // Gentle 3-tap weighted smoothing absorbs single-second OS NIC jitter while keeping exact numeric readouts
  const normalized = rawSorted.map((pt, idx) => {
    if (rawSorted.length < 3 || idx === 0 || idx === rawSorted.length - 1) return pt;
    const prev = rawSorted[idx - 1];
    const next = rawSorted[idx + 1];
    return {
      ts: pt.ts,
      inboundBps: 0.22 * prev.inboundBps + 0.56 * pt.inboundBps + 0.22 * next.inboundBps,
      outboundBps: 0.22 * prev.outboundBps + 0.56 * pt.outboundBps + 0.22 * next.outboundBps,
    };
  });

  const latestServerTs = normalized[normalized.length - 1].ts;
  // For remote nodes (5s heartbeats) keep a steady 1s sample stream during live polling so 60fps playhead never stalls
  if (!opts.fromSnapshot && opts.keepContinuousTick && Date.now() - latestServerTs > 1100) {
    const prevLast = normalized[normalized.length - 1];
    normalized.push({
      ts: Date.now(),
      inboundBps: 0.3 * prevLast.inboundBps + 0.7 * inBps,
      outboundBps: 0.3 * prevLast.outboundBps + 0.7 * outBps,
    });
  }
  const effectiveLatestTs = normalized[normalized.length - 1].ts;
  const staleSnapshot = Boolean(opts.fromSnapshot)
    && netState.lastLiveServerTs > 0
    && effectiveLatestTs <= netState.lastLiveServerTs;
  if (staleSnapshot) return { inBps, outBps, updated: false };

  if (!opts.fromSnapshot) {
    netState.lastLiveServerTs = Math.max(netState.lastLiveServerTs, effectiveLatestTs);
  }

  const cutoff = effectiveLatestTs - (OV_NET_WINDOW_MS + 25000);
  const existing = netState.samples;
  const out = netState._mergeBuf;
  out.length = 0;
  let i = 0;
  let j = 0;
  while (i < existing.length && existing[i].ts < cutoff) i++;
  while (j < normalized.length && normalized[j].ts < cutoff) j++;
  while (i < existing.length && j < normalized.length) {
    if (existing[i].ts < normalized[j].ts) {
      out.push(existing[i++]);
    } else if (normalized[j].ts < existing[i].ts) {
      out.push(normalized[j++]);
    } else {
      out.push(normalized[j++]);
      i++;
    }
  }
  while (i < existing.length) out.push(existing[i++]);
  while (j < normalized.length) out.push(normalized[j++]);
  netState._mergeBuf = existing;
  netState.samples = out;
  syncPerfOffset(netState, netState.samples, 0.05);
  return { inBps, outBps, updated: true };
}

function updateOverviewNetwork(net, opts = {}) {
  if (!net || typeof net !== 'object') return;
  const { inBps, outBps, updated } = normalizeAndMergeNetState(_ovNetState, net, opts);

  if (updated) {
    const { inValEl, outValEl, ifaceEl, totalsEl } = getOverviewLabelEls();
    if (_ovNetState.hoverRatio === null) {
      setElText(inValEl, formatRate(inBps));
      setElText(outValEl, formatRate(outBps));
    }

    const ifaces = Number(net.interfaceCount ?? net.interface_count ?? 0);
    const activeNodeId = getEffectiveOverviewNodeId();
    const scopeLabel = state.overview?.nodeLabel || (activeNodeId
      ? getNodeDisplayName(activeNodeId) || `VPS ${activeNodeId}`
      : (state.selectedNodeFilter && state.selectedNodeFilter !== 'all'
        ? getNodeDisplayName(state.selectedNodeFilter) || 'Selected VPS'
        : 'Whole VPS'));
    setElText(
      ifaceEl,
      ifaces > 0 ? `${scopeLabel} • ${ifaces} NIC${ifaces === 1 ? '' : 's'}` : `${scopeLabel} NIC`
    );

    const rxTotal = Number(net.totalRxBytes ?? net.total_rx_bytes ?? 0);
    const txTotal = Number(net.totalTxBytes ?? net.total_tx_bytes ?? 0);
    setElText(totalsEl, `↓ ${formatBytesShort(rxTotal)}  ↑ ${formatBytesShort(txTotal)}`);
  }

  ensureOverviewNetDom();
  if (!_ovNetState.hasDrawnInitial) {
    drawOverviewNetFrame(typeof performance !== 'undefined' ? performance.now() : 0);
  } else {
    startOverviewNetRaf();
  }
}

async function fetchOverviewNetwork() {
  if (_ovNetState.pollInFlight || !state.user || !state.csrf || state.currentView !== 'overview') return;
  if (typeof document !== 'undefined' && document.hidden) return;
  _ovNetState.pollInFlight = true;
  _ovNetState.lastFetchAt = Date.now();
  try {
    const scopeParam = getEffectiveOverviewNodeParam();
    if (_ovNetState.lastScopeParam !== scopeParam) {
      _ovNetState.lastScopeParam = scopeParam;
      resetOverviewNetGraph();
    }
    const nodeParam = encodeURIComponent(scopeParam);
    const net = await api(`/api/overview/network?nodeId=${nodeParam}`, { noCache: true });
    if (net && typeof net === 'object') {
      updateOverviewNetwork(net, { fromSnapshot: false });
      if (net.perNode && typeof net.perNode === 'object') {
        for (const [nid, nodeNet] of Object.entries(net.perNode)) {
          updateNodeStripNetwork(nid, nodeNet, { fromSnapshot: false, keepContinuousTick: true });
        }
      }
    }
  } catch {
    // Keep last rendered global VPS network graph silently on transient network glitches
  } finally {
    _ovNetState.pollInFlight = false;
  }
}

function ensureOverviewNetPoller() {
  if (!state.user || !state.csrf || state.currentView !== 'overview') {
    stopOverviewNetPoller();
    return;
  }
  startOverviewNetRaf();
  if (_ovNetState.lastLiveServerTs === 0 && !_ovNetState.pollInFlight) {
    void fetchOverviewNetwork();
  }
  if (_ovNetState.pollTimer !== null) return;
  _ovNetState.pollTimer = setInterval(() => {
    if (!state.user || !state.csrf || state.currentView !== 'overview') {
      stopOverviewNetPoller();
      return;
    }
    if (typeof document !== 'undefined' && document.hidden) {
      stopOverviewNetRaf();
      return;
    }
    startOverviewNetRaf();
    if (Date.now() - _ovNetState.lastFetchAt >= OV_NET_POLL_MS - 120) {
      void fetchOverviewNetwork();
    }
  }, OV_NET_POLL_MS);
}

if (typeof document !== 'undefined') {
  document.addEventListener('visibilitychange', () => {
    if (document.hidden) {
      stopOverviewNetRaf();
    } else if (state.currentView === 'overview') {
      startOverviewNetRaf();
    }
  });
}

function getWorkloadIconInfo(r) {
  if (r.kind === 'database') {
    return { name: 'postgres', color: 'var(--text-main)', typeName: 'PostgreSQL Instance' };
  }
  if (r.kind === 'panel') {
    return { name: 'zap', color: 'var(--text-main)', typeName: 'Core Platform Daemon' };
  }

  const pId = r.projectId ?? r.project_id;
  const p = (state.projects || []).find((x) => String(x.id) === String(pId))
    || (state.overview?.projects || []).map((x) => x.project).find((x) => x && String(x.id) === String(pId))
    || {};

  const pt = ((r.projectType || r.project_type || p.project_type || '')).toLowerCase();
  const sc = ((r.startCmd || r.start_cmd || p.start_cmd || '')).toLowerCase();
  const name = ((r.name || p.name || '')).toLowerCase();
  const isService = r.kind === 'service' || p.project_kind === 'service';

  if (isService) {
    if (pt.includes('lavalink') || sc.includes('lavalink') || sc.includes('.jar') || sc.includes('java') || name.includes('lavalink')) {
      return { name: 'java', color: 'var(--text-main)', typeName: 'Java Daemon' };
    }
    if (pt.includes('python') || sc.includes('python') || sc.includes('.py') || sc.startsWith('py ') || name.includes('python')) {
      return { name: 'python', color: 'var(--text-main)', typeName: 'Python Daemon' };
    }
    if (pt.includes('rust') || sc.includes('cargo') || sc.includes('.rs')) {
      return { name: 'rust', color: 'var(--text-main)', typeName: 'Rust Daemon' };
    }
    if (pt.includes('go') || sc.includes('go run') || sc.includes('.go')) {
      return { name: 'go', color: 'var(--text-main)', typeName: 'Go Daemon' };
    }
    if (pt.includes('node') || sc.includes('node') || sc.includes('npm') || sc.includes('yarn') || sc.includes('pnpm') || sc.includes('bun')) {
      return { name: 'nodejs', color: 'var(--text-main)', typeName: 'Node.js Daemon' };
    }
    if (pt.includes('php') || sc.includes('php')) {
      return { name: 'php', color: 'var(--text-main)', typeName: 'PHP Daemon' };
    }
    if (pt.includes('ruby') || sc.includes('ruby') || sc.includes('rails') || sc.includes('puma')) {
      return { name: 'ruby', color: 'var(--text-main)', typeName: 'Ruby Daemon' };
    }
    if (pt.includes('docker') || sc.includes('docker')) {
      return { name: 'docker', color: 'var(--text-main)', typeName: 'Docker Daemon' };
    }
    if (pt.includes('discord') || sc.includes('discord') || name.includes('discord')) {
      return { name: 'nodejs', color: 'var(--text-main)', typeName: 'Backend Service' };
    }
    if (pt.includes('telegram') || sc.includes('telegram') || name.includes('telegram')) {
      return { name: 'python', color: 'var(--text-main)', typeName: 'Backend Service' };
    }
    return { name: 'terminal', color: 'var(--text-main)', typeName: 'Backend Service' };
  }

  // Website kind
  if (pt.includes('node') || pt.includes('express') || sc.includes('node') || sc.includes('npm') || sc.includes('yarn') || sc.includes('pnpm') || sc.includes('bun')) {
    return { name: 'nodejs', color: 'var(--text-main)', typeName: 'Node.js App' };
  }
  if (pt.includes('next')) {
    return { name: 'typescript', color: 'var(--text-main)', typeName: 'Next.js App' };
  }
  if (pt.includes('react') || pt.includes('typescript')) {
    return { name: 'typescript', color: 'var(--text-main)', typeName: 'React / TS App' };
  }
  if (pt.includes('python') || sc.includes('python') || sc.includes('.py')) {
    return { name: 'python', color: 'var(--text-main)', typeName: 'Python Web' };
  }
  if (pt.includes('go') || sc.includes('go run') || sc.includes('.go')) {
    return { name: 'go', color: 'var(--text-main)', typeName: 'Go Web App' };
  }
  if (pt.includes('rust') || sc.includes('cargo') || sc.includes('.rs')) {
    return { name: 'rust', color: 'var(--text-main)', typeName: 'Rust Web App' };
  }
  if (pt.includes('php') || sc.includes('php')) {
    return { name: 'php', color: 'var(--text-main)', typeName: 'PHP Site' };
  }
  if (pt.includes('ruby') || sc.includes('ruby') || sc.includes('rails')) {
    return { name: 'ruby', color: 'var(--text-main)', typeName: 'Ruby Web App' };
  }
  if (pt.includes('docker') || sc.includes('docker')) {
    return { name: 'docker', color: 'var(--text-main)', typeName: 'Docker Container' };
  }

  return { name: 'globe', color: 'var(--text-main)', typeName: p.project_type || 'Website' };
}

// ============================================================================
// View 1: Overview & Live Resource Consumption
// ============================================================================
const _ovDomCache = {
  els: null,
};

function getOverviewDomEls() {
  const cached = _ovDomCache.els;
  if (cached && cached.badgeWeb && cached.badgeWeb.isConnected !== false) {
    return cached;
  }
  const els = {
    badgeWeb: document.getElementById('badge-projects-count'),
    badgeSvc: document.getElementById('badge-services-count'),
    badgeDb: document.getElementById('badge-databases-count'),
    cpuBar: document.getElementById('stat-cpu-bar'),
    cpuMeta: document.getElementById('stat-cpu-meta'),
    memBar: document.getElementById('stat-mem-bar'),
    memMeta: document.getElementById('stat-mem-meta'),
    diskBar: document.getElementById('stat-disk-bar'),
    resPanelUsage: document.getElementById('res-panel-usage'),
    resWorkloadsUsage: document.getElementById('res-workloads-usage'),
    resSystemUsage: document.getElementById('res-system-usage'),
    resFreeMem: document.getElementById('res-free-memory'),
    resCountBadge: document.getElementById('res-workloads-count-badge'),
    resTable: document.getElementById('resource-breakdown-table'),
    actBox: document.getElementById('overview-active-deployment-box'),
    actName: document.getElementById('active-dep-project-name'),
    actKind: document.getElementById('active-dep-kind'),
  };
  _ovDomCache.els = els;
  return els;
}

function formatOverviewMemMbOrGb(bytes) {
  const b = Number(bytes || 0);
  if (!Number.isFinite(b) || b <= 0) return '0 MB';
  const gb = b / 1073741824;
  if (gb >= 1) return `${gb.toFixed(1)} GB`;
  return `${Math.round(b / 1048576)} MB`;
}

function updateNodeStripNetwork(nodeId, net, opts = {}) {
  if (!net || typeof net !== 'object') return;
  const entry = getOrCreateNodeNetChart(nodeId);
  const { inBps, outBps, updated } = normalizeAndMergeNetState(entry.state, net, opts);

  if (updated) {
    if (entry.state.hoverRatio === null) {
      setText(`ov-node-${nodeId}-in`, formatRate(inBps));
      setText(`ov-node-${nodeId}-out`, formatRate(outBps));
    }
    const rxTotal = Number(net.totalRxBytes ?? net.total_rx_bytes ?? 0);
    const txTotal = Number(net.totalTxBytes ?? net.total_tx_bytes ?? 0);
    const ifaces = Number(net.interfaceCount ?? net.interface_count ?? 0);
    setText(`ov-node-${nodeId}-totals`, `↓ ${formatBytesShort(rxTotal)}  ↑ ${formatBytesShort(txTotal)}`);
    if (ifaces > 0) {
      const ifaceEl = document.getElementById(`ov-node-${nodeId}-iface`);
      if (ifaceEl) {
        const nName = ifaceEl.getAttribute('data-node-name') || 'VPS';
        setElText(ifaceEl, `${nName} • ${ifaces} NIC${ifaces === 1 ? '' : 's'}`);
      }
    }
  }

  if (!entry.state.hasDrawnInitial) {
    drawOverviewNetFrame(typeof performance !== 'undefined' ? performance.now() : 0);
  } else {
    startOverviewNetRaf();
  }
}

function getEffectiveOverviewNodeId() {
  const inspectId = Number(state.overviewInspectNodeId);
  return Number.isInteger(inspectId) && inspectId > 0 ? inspectId : null;
}

function getEffectiveOverviewNodeParam() {
  const inspectId = getEffectiveOverviewNodeId();
  return inspectId ? String(inspectId) : 'all';
}

function renderBlurIpPillHtml(node) {
  const pubIp = getNodeEffectivePublicIp(node);
  return pubIp ? renderBlurPublicIpPill(pubIp, { label: 'IP' }) : '';
}

function renderHostedPillsInnerHtml(node) {
  const websites = Number(node?.websiteCount ?? node?.projectCount ?? 0);
  const services = Number(node?.serviceCount ?? 0);
  const dbs = Number(node?.databaseCount ?? 0);
  return `
    <span class="overview-hosted-pill ${websites > 0 ? 'has-items' : ''}"><span class="overview-hosted-pill-dot web"></span>${websites} Website${websites === 1 ? '' : 's'}</span>
    <span class="overview-hosted-pill ${services > 0 ? 'has-items' : ''}"><span class="overview-hosted-pill-dot svc"></span>${services} Service${services === 1 ? '' : 's'}</span>
    <span class="overview-hosted-pill ${dbs > 0 ? 'has-items' : ''}"><span class="overview-hosted-pill-dot db"></span>${dbs} Database${dbs === 1 ? '' : 's'}</span>
  `;
}

function inspectOverviewNode(nodeId) {
  const id = Number(nodeId);
  if (!Number.isInteger(id) || id <= 0) return;
  if (typeof window.navigateTo === 'function') {
    window.navigateTo('overview', `vps${id}`);
  }
}

function renderInspectNodeTopbar(nodes) {
  const topbar = document.getElementById('overview-inspect-topbar');
  const resCard = document.querySelector('#view-overview .resource-breakdown-card');
  const singleGrid = document.getElementById('overview-single-stats-grid');
  const inspectId = getEffectiveOverviewNodeId();

  if (!inspectId) {
    if (topbar) {
      topbar.style.display = 'none';
      setHtmlIfChanged(topbar, '');
    }
    if (resCard) resCard.style.display = 'none';
    return;
  }

  if (singleGrid) singleGrid.style.display = '';
  if (resCard) resCard.style.display = '';
  if (!topbar) return;

  const list = Array.isArray(nodes) ? nodes : [];
  const n = list.find((item) => Number(item.id) === inspectId);
  const isUp = n ? (n.isLocal || n.status === 'online') : true;
  const isDraining = n && n.status === 'draining';
  const statusDot = isUp ? 'online' : isDraining ? 'maintenance' : 'offline';
  const isCore = n ? Boolean(n.isLocal) : false;
  const roleBadge = n
    ? `<span class="overview-role-pill ${isCore ? 'is-core' : 'is-worker'}">${isCore ? 'Core' : 'Worker'}</span>`
    : '';
  const nameLabel = n ? n.name : `VPS ${inspectId}`;
  const blurIpHtml = n ? renderBlurIpPillHtml(n) : '';
  const hostedHtml = n ? renderHostedPillsInnerHtml(n) : '';

  const html = `
    <div class="overview-inspect-topbar-left">
      <button type="button" class="overview-back-btn" onclick="window.navigateTo('overview')">&larr; Back to All VPS</button>
      <span class="status-dot ${statusDot}"></span>
      <span class="overview-node-name">${escapeHtml(nameLabel)}</span>
      ${roleBadge}
      ${blurIpHtml}
    </div>
    <div class="overview-hosted-pills">
      ${hostedHtml}
    </div>
  `;
  topbar.style.display = 'flex';
  setHtmlIfChanged(topbar, html);
}

function renderOverviewNodesStrip(nodes) {
  const strip = document.getElementById('overview-nodes-strip');
  const singleGrid = document.getElementById('overview-single-stats-grid');
  if (!strip) return;
  const list = Array.isArray(nodes) ? nodes : [];
  const inspectId = getEffectiveOverviewNodeId();
  const showSeparateStrips = !inspectId && list.length > 0;

  if (!showSeparateStrips) {
    if (singleGrid) singleGrid.style.display = '';
    strip.style.display = 'none';
    strip._structSig = '';
    setHtmlIfChanged(strip, '');
    return;
  }

  if (singleGrid) singleGrid.style.display = 'none';
  strip.style.display = 'flex';

  let bestNodeId = null;
  let maxFreeRam = -1;
  if (list.length > 1) {
    for (const n of list) {
      const isUp = n.isLocal || n.status === 'online';
      if (isUp && Number(n.memFree || 0) > maxFreeRam) {
        maxFreeRam = Number(n.memFree || 0);
        bestNodeId = n.id;
      }
    }
  }

  // Build structural HTML without mutating SVG path `d` attributes or live KPI values
  // so `setHtmlIfChanged` keeps the 60fps SVG DOM intact across periodic refreshes.
  const structSig = list
    .map((n) => `${n.id}:${n.status}:${n.name}:${Boolean(n.isLocal)}:${getNodeEffectivePublicIp(n)}:${n.id === bestNodeId}`)
    .join('|');

  if (strip._structSig !== structSig) {
    strip._structSig = structSig;
    strip.innerHTML = list.map((n) => {
      const isUp = n.isLocal || n.status === 'online';
      const isDraining = n.status === 'draining';
      const isBestHeadroom = bestNodeId != null && Number(n.id) === Number(bestNodeId);
      const statusDot = isUp ? 'online' : isDraining ? 'maintenance' : 'offline';
      const isCore = Boolean(n.isLocal);
      const blurIpHtml = renderBlurIpPillHtml(n);
      const gradInId = `ovNodeGradIn-${n.id}`;
      const gradOutId = `ovNodeGradOut-${n.id}`;
      const clipId = `ovNodeClip-${n.id}`;

      return `
        <div class="overview-node-block">
          <div class="overview-node-block-head">
            <div class="overview-node-header-left">
              <span class="status-dot ${statusDot}"></span>
              <span class="overview-node-name">${escapeHtml(n.name)}</span>
              <span class="overview-role-pill ${isCore ? 'is-core' : 'is-worker'}">${isCore ? 'Core' : 'Worker'}</span>
              ${blurIpHtml}
              <div class="overview-hosted-pills" id="ov-node-${n.id}-hosted"></div>
              ${isBestHeadroom ? '<span class="badge badge-success" style="font-size:10px;">Most free RAM</span>' : ''}
            </div>
            <div style="display:flex; gap:6px; align-items:center;">
              <button type="button" class="btn btn-secondary btn-sm" style="padding:4px 11px; font-size:11px; font-weight:600;" onclick="window.inspectOverviewNode(${n.id})">Inspect</button>
            </div>
          </div>
          <div class="stats-grid overview-stats-grid">
            <div class="stat-card stat-card-network">
              <div class="overview-net-head">
                <div class="stat-label" style="justify-content:flex-start; gap:6px;">
                  <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5">
                    <polyline points="22 12 18 12 15 21 9 3 6 12 2 12"></polyline>
                  </svg>
                  Network
                </div>
                <div class="app-net-legend">
                  <span class="app-net-pill net-in"><span class="app-net-dot"></span>In <strong id="ov-node-${n.id}-in">0 B/s</strong></span>
                  <span class="app-net-pill net-out"><span class="app-net-dot"></span>Out <strong id="ov-node-${n.id}-out">0 B/s</strong></span>
                </div>
              </div>
              <div class="app-usage-net-chart overview-net-chart" id="ov-node-${n.id}-chart">
                <svg class="app-net-svg" viewBox="0 0 ${OV_NET_SVG_W} ${OV_NET_SVG_H}" preserveAspectRatio="none">
                  <defs>
                    <clipPath id="${clipId}">
                      <rect x="0" y="0" width="${OV_NET_SVG_W}" height="${OV_NET_SVG_H}"></rect>
                    </clipPath>
                    <linearGradient id="${gradInId}" x1="0" y1="0" x2="0" y2="1">
                      <stop offset="0%" stop-color="#4cc38a" stop-opacity="0.28"/>
                      <stop offset="100%" stop-color="#4cc38a" stop-opacity="0.01"/>
                    </linearGradient>
                    <linearGradient id="${gradOutId}" x1="0" y1="0" x2="0" y2="1">
                      <stop offset="0%" stop-color="#4f8ef7" stop-opacity="0.26"/>
                      <stop offset="100%" stop-color="#4f8ef7" stop-opacity="0.01"/>
                    </linearGradient>
                  </defs>
                  <line x1="0" y1="18" x2="${OV_NET_SVG_W}" y2="18" stroke="rgba(238,241,247,0.045)" stroke-dasharray="3 4"/>
                  <line x1="0" y1="34" x2="${OV_NET_SVG_W}" y2="34" stroke="rgba(238,241,247,0.045)" stroke-dasharray="3 4"/>
                  <line x1="0" y1="${OV_NET_BOTTOM}" x2="${OV_NET_SVG_W}" y2="${OV_NET_BOTTOM}" stroke="rgba(238,241,247,0.08)"/>
                  <g clip-path="url(#${clipId})">
                    <path id="ov-node-${n.id}-in-area" d="" fill="url(#${gradInId})"/>
                    <path id="ov-node-${n.id}-out-area" d="" fill="url(#${gradOutId})"/>
                    <path id="ov-node-${n.id}-in-glow" d="" fill="none" stroke="rgba(76, 195, 138, 0.22)" stroke-width="5" vector-effect="non-scaling-stroke" stroke-linecap="round" stroke-linejoin="round"/>
                    <path id="ov-node-${n.id}-out-glow" d="" fill="none" stroke="rgba(79, 142, 247, 0.22)" stroke-width="5" vector-effect="non-scaling-stroke" stroke-linecap="round" stroke-linejoin="round"/>
                    <path id="ov-node-${n.id}-in-line" d="" fill="none" stroke="#4cc38a" stroke-width="2" vector-effect="non-scaling-stroke" stroke-linecap="round" stroke-linejoin="round"/>
                    <path id="ov-node-${n.id}-out-line" d="" fill="none" stroke="#4f8ef7" stroke-width="1.8" vector-effect="non-scaling-stroke" stroke-linecap="round" stroke-linejoin="round"/>
                  </g>
                </svg>
                <div id="ov-node-${n.id}-in-dot" class="app-net-live-marker net-in" style="opacity:0;"></div>
                <div id="ov-node-${n.id}-out-dot" class="app-net-live-marker net-out" style="opacity:0;"></div>
                <div id="ov-node-${n.id}-crosshair" class="app-net-crosshair" style="display:none;"></div>
                <span id="ov-node-${n.id}-peak" class="app-net-scale-chip">Peak 0 B/s</span>
              </div>
              <div class="stat-meta overview-net-footer">
                <span id="ov-node-${n.id}-iface" data-node-name="${escapeHtml(n.name)}">${escapeHtml(n.name)}</span>
                <span id="ov-node-${n.id}-totals" class="app-net-totals">&darr; 0 B &nbsp;&uarr; 0 B</span>
              </div>
            </div>

            <div class="stat-card">
              <div class="stat-label">CPU</div>
              <div class="stat-value" id="ov-node-${n.id}-cpu-pct">0%</div>
              <div class="progress-bar"><div class="progress-fill green" id="ov-node-${n.id}-cpu-bar" style="width:0%;"></div></div>
              <div class="stat-meta" id="ov-node-${n.id}-cpu-meta">-- Cores</div>
            </div>

            <div class="stat-card">
              <div class="stat-label">RAM</div>
              <div class="stat-value" id="ov-node-${n.id}-mem-pct">0%</div>
              <div class="progress-bar"><div class="progress-fill green" id="ov-node-${n.id}-mem-bar" style="width:0%;"></div></div>
              <div class="stat-meta" id="ov-node-${n.id}-mem-meta">-- used</div>
            </div>

            <div class="stat-card">
              <div class="stat-label">Disk Free</div>
              <div class="stat-value" id="ov-node-${n.id}-disk-free">-- GB</div>
              <div class="progress-bar"><div class="progress-fill green" id="ov-node-${n.id}-disk-bar" style="width:0%;"></div></div>
              <div class="stat-meta" id="ov-node-${n.id}-disk-meta">-- total</div>
            </div>
          </div>
        </div>
      `;
    }).join('');
  }

  for (const n of list) {
    const chartBox = document.getElementById(`ov-node-${n.id}-chart`);
    if (chartBox && !chartBox._hoverWired) {
      chartBox._hoverWired = true;
      const chartEntry = getOrCreateNodeNetChart(n.id);
      chartBox.onmousemove = (ev) => {
        const rect = chartBox.getBoundingClientRect();
        if (!rect.width) return;
        chartEntry.state.hoverRatio = Math.max(0, Math.min(1, (ev.clientX - rect.left) / rect.width));
      };
      chartBox.onmouseleave = () => {
        chartEntry.state.hoverRatio = null;
        const cross = document.getElementById(`ov-node-${n.id}-crosshair`);
        if (cross) cross.style.display = 'none';
      };
    }

    const cpuPct = Math.round(Number(n.cpuPct || 0));
    const cpuCores = Number(n.cpuCores || 1);
    const memPct = Math.round(Number(n.memPct || 0));
    const usedMb = Math.round(Number(n.memUsed || 0) / 1048576);
    const totalMb = Math.round(Number(n.memTotal || 0) / 1048576);
    const diskPct = Math.min(100, Math.max(0, Number(n.diskPct || 0)));
    const freeGb = (Number(n.diskFree || 0) / 1073741824).toFixed(1);
    const totalGb = (Number(n.diskTotal || 0) / 1073741824).toFixed(1);
    const hostedEl = document.getElementById(`ov-node-${n.id}-hosted`);
    if (hostedEl) {
      setHtmlIfChanged(hostedEl, renderHostedPillsInnerHtml(n));
    }
    setText(`ov-node-${n.id}-cpu-pct`, `${cpuPct}%`);
    const cpuBar = document.getElementById(`ov-node-${n.id}-cpu-bar`);
    if (cpuBar) {
      const cls = `progress-fill ${cpuPct > 80 ? 'rose' : cpuPct > 50 ? 'amber' : 'green'}`;
      if (cpuBar.className !== cls) cpuBar.className = cls;
      animateProgressBar(cpuBar, cpuPct);
    }
    setText(`ov-node-${n.id}-cpu-meta`, `${cpuCores} Core${cpuCores === 1 ? '' : 's'} (${cpuPct}% total)`);

    setText(`ov-node-${n.id}-mem-pct`, `${memPct}%`);
    const memBar = document.getElementById(`ov-node-${n.id}-mem-bar`);
    if (memBar) {
      const cls = `progress-fill ${memPct > 85 ? 'rose' : memPct > 65 ? 'amber' : 'green'}`;
      if (memBar.className !== cls) memBar.className = cls;
      animateProgressBar(memBar, memPct);
    }
    setText(`ov-node-${n.id}-mem-meta`, `${usedMb} MB / ${totalMb} MB`);

    setText(`ov-node-${n.id}-disk-free`, `${freeGb} GB`);
    const diskBar = document.getElementById(`ov-node-${n.id}-disk-bar`);
    if (diskBar) {
      const cls = `progress-fill ${diskPct > 85 ? 'rose' : diskPct > 70 ? 'amber' : 'green'}`;
      if (diskBar.className !== cls) diskBar.className = cls;
      animateProgressBar(diskBar, diskPct);
    }
    setText(`ov-node-${n.id}-disk-meta`, `${totalGb} GB total (${diskPct}% used)`);

    updateNodeStripNetwork(
      n.id,
      {
        inboundBps: n.inboundBps,
        outboundBps: n.outboundBps,
        totalRxBytes: n.totalRxBytes,
        totalTxBytes: n.totalTxBytes,
        interfaceCount: n.interfaceCount,
        history: n.netHistory,
      },
      { fromSnapshot: true },
    );
  }
}

function renderResourceBreakdown(resources) {
  const tbody = getOverviewDomEls().resTable || document.getElementById('resource-breakdown-table');
  if (!tbody) return;
  const list = resources || state.overview?.resources || [];
  const filter = state.resourceFilter || 'all';
  const inspectId = getEffectiveOverviewNodeId();
  const nodeScoped = inspectId
    ? list.filter((r) => Number(r?.nodeId ?? r?.node_id ?? 0) === inspectId)
    : list.filter((r) => matchesNodeFilter(r));
  const filtered = filter === 'all' ? nodeScoped.filter((r) => r.kind !== 'panel') : nodeScoped.filter((r) => r.kind === filter);

  if (!filtered.length) {
    let emptyMsg = 'No active workloads found.';
    let emptyAction = '';
    if (filter === 'website') {
      emptyMsg = 'No hosted websites created yet.';
      emptyAction = hasPower('projects_manage') ? '<button class="btn btn-primary btn-sm" style="margin-top: 10px;" onclick="openAddWebsiteModal()">+ Add Website</button>' : '';
    } else if (filter === 'service') {
      emptyMsg = 'No backend services created yet.';
      emptyAction = hasPower('services_manage') ? '<button class="btn btn-primary btn-sm" style="margin-top: 10px;" onclick="openAddServiceModal()">+ Add Service</button>' : '';
    } else if (filter === 'database') {
      emptyMsg = 'No managed databases created yet.';
      emptyAction = hasAnyPower(['databases', 'databases_manage']) ? '<button class="btn btn-primary btn-sm" style="margin-top: 10px;" onclick="navigateTo(\'databases\')">Go to Databases &rarr;</button>' : '';
    } else if (filter === 'all') {
      emptyMsg = 'No workloads created yet. Deploy websites, backend services or databases to monitor their live resource usage.';
      const btns = [
        hasPower('projects_manage') ? '<button class="btn btn-primary btn-sm" onclick="openAddWebsiteModal()">+ Add Website</button>' : '',
        hasPower('services_manage') ? '<button class="btn btn-secondary btn-sm" onclick="openAddServiceModal()">+ Add Service</button>' : '',
        hasAnyPower(['databases', 'databases_manage']) ? '<button class="btn btn-secondary btn-sm" onclick="navigateTo(\'databases\')">Go to Databases &rarr;</button>' : '',
      ].filter(Boolean).join('');
      emptyAction = btns ? `<div style="margin-top: 12px; display: flex; gap: 8px; justify-content: center;">${btns}</div>` : '';
    }
    setHtmlIfChanged(tbody, `<tr><td colspan="8" style="text-align: center; padding: 32px 16px; color: var(--text-dim);"><div style="font-size: 13.5px;">${emptyMsg}</div>${emptyAction}</td></tr>`);
    return;
  }

  const projById = new Map();
  for (const item of (state.overview?.projects || [])) {
    if (item && item.project) projById.set(item.project.id, item.project);
  }
  for (const x of (state.projects || [])) {
    if (x) projById.set(x.id, x);
  }

  setHtmlIfChanged(tbody, filtered.map((r) => {
    const isRunning = r.status === 'running';
    const isMaintenance = r.status === 'maintenance';
    const kindLabel = r.kind ? r.kind.charAt(0).toUpperCase() + r.kind.slice(1).toLowerCase() : 'Workload';
    const kindBadgeClass = `badge-res-${r.kind}`;
    const iconInfo = getWorkloadIconInfo(r);
    const kindIconSvg = icon(iconInfo.name, { size: 24, cls: `res-icon-${r.kind}` });

    const rNodeId = r.nodeId ?? r.node_id;
    const nodeName = (state.nodes || []).length > 1 && rNodeId != null ? getNodeDisplayName(rNodeId) : '';
    const nodeTag = nodeName ? `<span class="badge badge-neutral" style="font-size:10px; margin-left:6px;">${escapeHtml(nodeName)}</span>` : '';

    let subtitle = '';
    if (r.kind === 'panel') {
      subtitle = `<span style="color:var(--text-dim)">Core Platform Daemon</span>${nodeTag}`;
    } else if (r.kind === 'database') {
      subtitle = `<span style="color:var(--text-dim)">PostgreSQL Instance</span>${nodeTag}`;
    } else if (r.kind === 'service') {
      const sha = r.commitSha ? `<span class="sha-pill" style="font-size:10px; margin-left:4px;">${shortSha(r.commitSha)}</span>` : '';
      subtitle = `<span style="color:var(--text-dim)">Backend Service</span>${sha}${nodeTag}`;
    } else {
      const dom = r.domain ? `<a href="http://${escapeHtml(r.domain)}" target="_blank" class="domain-badge" style="font-size:11px;" onclick="event.stopPropagation()">${escapeHtml(r.domain)}</a>` : (r.port ? `<span style="color:var(--text-dim)">${r.port}</span>` : '<span style="color:var(--text-dim)">Local only</span>');
      const sha = r.commitSha ? `<span class="sha-pill" style="font-size:10px; margin-left:4px;">${shortSha(r.commitSha)}</span>` : '';
      subtitle = `${dom}${sha}${nodeTag}`;
    }

    const pId = r.projectId ?? r.project_id;
    const p = (pId != null && projById.get(pId)) || {};
    const effectivePid = r.pid || p.app_pid || p.status?.pid;

    const portPid = [
      r.port ? `${r.port}` : null,
      effectivePid ? `<span class="res-pid-chip">PID ${effectivePid}</span>` : null,
    ].filter(Boolean).join(' ') || '<span style="color:var(--text-dim)">--</span>';

    const cpuVal = (r.cpuPct || 0).toFixed(1);
    const cpuWarm = r.cpuPct > 40 ? 'hot' : r.cpuPct > 15 ? 'warm' : '';
    const memVal = r.memoryFormatted || '0 MB';
    const memPctVal = (r.memoryPct || 0).toFixed(1);
    const memWarm = r.memoryPct > 30 ? 'hot' : r.memoryPct > 15 ? 'warm' : '';
    const diskVal = r.diskFormatted || '0 MB';
    const diskPctVal = (r.diskPct || 0).toFixed(1);
    const diskWarm = (r.diskPct || 0) > 20 ? 'hot' : (r.diskPct || 0) > 5 ? 'warm' : '';

    const dbId = r.databaseId ?? r.database_id;
    let actionBtn = '';
    if (r.kind === 'panel') {
      actionBtn = '<button class="btn btn-secondary btn-sm" onclick="event.stopPropagation(); navigateTo(\'settings\')">Settings &rarr;</button>';
    } else if (r.kind === 'database') {
      actionBtn = '<button class="btn btn-secondary btn-sm" onclick="event.stopPropagation(); navigateTo(\'databases\')">Databases &rarr;</button>';
    } else if (r.kind === 'service') {
      actionBtn = '<button class="btn btn-secondary btn-sm" onclick="event.stopPropagation(); navigateTo(\'services\')">Services &rarr;</button>';
    } else {
      actionBtn = '<button class="btn btn-secondary btn-sm" onclick="event.stopPropagation(); navigateTo(\'projects\')">Websites &rarr;</button>';
    }

    const navClick = pId
      ? `style="cursor: pointer;" onclick="openWebsiteWorkspace(${pId})"`
      : (r.kind === 'database'
        ? (dbId ? `style="cursor: pointer;" onclick="openDatabaseWorkspace(${dbId}, 'deployments')"` : 'style="cursor: pointer;" onclick="navigateTo(\'databases\')"')
        : 'style="cursor: pointer;" onclick="navigateTo(\'settings\')"');

    return `
      <tr>
        <td ${navClick}>
          <div class="res-workload-title">
            <div class="res-workload-icon" style="color: ${iconInfo.color};">${kindIconSvg}</div>
            <div>
              <div class="res-workload-name">${escapeHtml(r.name)}</div>
              <div class="res-workload-sub">${subtitle}</div>
            </div>
          </div>
        </td>
        <td ${navClick}><span class="${kindBadgeClass}">${kindLabel}</span></td>
        <td>${portPid}</td>
        <td>
          <span class="status-dot ${isRunning ? 'online' : isMaintenance ? 'maintenance' : 'offline'}"></span>
          <span style="font-size: 12px; font-weight: 500; margin-left: 6px; color: ${isRunning ? 'var(--success)' : isMaintenance ? 'var(--warning)' : 'var(--text-muted)'};">${isRunning ? 'Running' : isMaintenance ? 'Maintenance' : 'Stopped'}</span>
        </td>
        <td>
          <div class="res-metric-cell">
            <div class="res-metric-head">
              <span style="color: ${isRunning ? 'var(--text-main)' : 'var(--text-dim)'}; font-weight: 600;">${isRunning ? `${cpuVal}%` : '0%'}</span>
              <span style="color: var(--text-dim); font-size: 11px;">CPU</span>
            </div>
            <div class="res-mini-bar">
              <div class="res-mini-fill res-cpu-fill ${cpuWarm}" style="width: ${isRunning ? Math.min(100, Math.max(2, r.cpuPct * 2)) : 0}%;"></div>
            </div>
          </div>
        </td>
        <td>
          <div class="res-metric-cell">
            <div class="res-metric-head">
              <span style="color: ${isRunning ? 'var(--text-main)' : 'var(--text-dim)'}; font-weight: 600;">${isRunning ? memVal : '0 MB'}</span>
              <span style="color: var(--text-dim); font-size: 11px;">${isRunning ? `${memPctVal}% VPS` : '--'}</span>
            </div>
            <div class="res-mini-bar">
              <div class="res-mini-fill res-mem-fill ${memWarm}" style="width: ${isRunning ? Math.min(100, Math.max(2, r.memoryPct)) : 0}%;"></div>
            </div>
          </div>
        </td>
        <td>
          <div class="res-metric-cell">
            <div class="res-metric-head">
              <span style="color: var(--text-main); font-weight: 600;">${diskVal}</span>
              <span style="color: var(--text-dim); font-size: 11px;">${diskPctVal}% Disk</span>
            </div>
            <div class="res-mini-bar">
              <div class="res-mini-fill res-disk-fill ${diskWarm}" style="width: ${(r.diskPct || 0) > 0 ? Math.min(100, Math.max(2, (r.diskPct || 0) * 2)) : 0}%;"></div>
            </div>
          </div>
        </td>
        <td style="text-align: right;">
          ${actionBtn}
        </td>
      </tr>
    `;
  }).join(''));
}

function filterResourceBreakdown(filter) {
  state.resourceFilter = filter;
  document.querySelectorAll('#resource-filter-pills .pill').forEach((btn) => {
    btn.classList.toggle('active', btn.getAttribute('data-res-filter') === filter);
  });
  renderResourceBreakdown();
  animateTableRowsStagger(document.getElementById('resource-breakdown-table'));
};

async function loadOverview() {
  try {
    const scopeParam = getEffectiveOverviewNodeParam();
    if (_ovNetState.lastScopeParam && _ovNetState.lastScopeParam !== scopeParam) {
      resetOverviewNetGraph();
    }
    _ovNetState.lastScopeParam = scopeParam;
    const nodeParam = encodeURIComponent(scopeParam);
    const data = await api(`/api/overview?nodeId=${nodeParam}`);
    state.overview = data;
    if (Array.isArray(data.projects)) {
      state.projects = data.projects.map((x) => x.project);
      if (state.projects.length > 0 && !state.selectedProjectId) {
        state.selectedProjectId = state.projects[0].id;
      }
    }

    const els = getOverviewDomEls();

    // Server Stats & Sidebar Counts
    const m = data.monitor || {};
    let webCount = 0;
    let svcCount = 0;
    const dataProjects = Array.isArray(data.projects) ? data.projects : [];
    for (let i = 0; i < dataProjects.length; i++) {
      const p = dataProjects[i]?.project;
      if (!p) continue;
      if (p.project_kind === 'service') svcCount++;
      else webCount++;
    }

    // Keep navigation sidebar counts updated immediately without extra network calls
    setElText(els.badgeWeb, String(webCount));
    setElText(els.badgeSvc, String(svcCount));
    if (els.badgeDb) {
      let dbCountFromResources = 0;
      if (Array.isArray(data.resources)) {
        for (let i = 0; i < data.resources.length; i++) {
          if (data.resources[i]?.kind === 'database') dbCountFromResources++;
        }
      }
      const dbCountFromState = Array.isArray(state.databases) ? state.databases.length : 0;
      setElText(els.badgeDb, String(Math.max(dbCountFromResources, dbCountFromState)));
    }

    if (data.network) {
      updateOverviewNetwork(data.network, { fromSnapshot: true });
    }
    ensureOverviewNetPoller();

    const cpuPct = m.cpu?.pct || 0;
    const cpuCores = m.cpu?.cores || 1;
    animateNumberValue('stat-cpu-pct', cpuPct, (v) => `${Math.round(v)}%`);
    const cpuBar = els.cpuBar;
    if (cpuBar) {
      const nextCls = `progress-fill ${cpuPct > 80 ? 'rose' : cpuPct > 50 ? 'amber' : 'green'}`;
      if (cpuBar.className !== nextCls) cpuBar.className = nextCls;
      animateProgressBar(cpuBar, cpuPct);
    }
    const cpuMetaLabel = data.nodeLabel
      ? `${data.nodeLabel} • ${cpuCores} Core${cpuCores === 1 ? '' : 's'} (${cpuPct}% total)`
      : `${cpuCores} Core${cpuCores === 1 ? '' : 's'} (${cpuPct}% total)`;
    setElText(els.cpuMeta, cpuMetaLabel);

    const memPct = m.memory?.pct || 0;
    const usedMb = ((m.memory?.used || 0) / 1048576).toFixed(0);
    const totalMb = ((m.memory?.total || 0) / 1048576).toFixed(0);
    animateNumberValue('stat-mem-pct', memPct, (v) => `${Math.round(v)}%`);
    const memBar = els.memBar;
    if (memBar) {
      const nextCls = `progress-fill ${memPct > 85 ? 'rose' : memPct > 65 ? 'amber' : 'green'}`;
      if (memBar.className !== nextCls) memBar.className = nextCls;
      animateProgressBar(memBar, memPct);
    }
    setElText(els.memMeta, `${usedMb} MB / ${totalMb} MB`);

    if (m.disk) {
      const diskPct = Math.min(100, Math.max(0, Number(m.disk.pct) || 0));
      const freeGb = Number((m.disk.free / 1073741824).toFixed(1));
      const totalGb = (m.disk.total / 1073741824).toFixed(1);
      animateNumberValue('stat-disk-free', freeGb, (v) => `${v.toFixed(1)} GB`);
      const diskBar = els.diskBar;
      if (diskBar) {
        const nextCls = `progress-fill ${diskPct > 85 ? 'rose' : diskPct > 70 ? 'amber' : 'green'}`;
        if (diskBar.className !== nextCls) diskBar.className = nextCls;
        animateProgressBar(diskBar, diskPct);
      }
      setText('stat-disk-meta', `${totalGb} GB total (${diskPct}% used)`);
    }

    // Active resource summary bar
    const sum = data.summary || {};
    setElText(els.resPanelUsage, `${sum.panelMemoryFormatted || '--'} • ${(sum.panelCpu || 0).toFixed(1)}% CPU`);
    setElText(els.resWorkloadsUsage, `${sum.totalManagedMemoryFormatted || '--'} • ${(sum.totalManagedCpu || 0).toFixed(1)}% CPU`);
    setElText(els.resSystemUsage, `${sum.systemOverheadMemoryFormatted || '--'} • ${(sum.systemOverheadCpu || 0).toFixed(1)}% CPU`);
    if (els.resFreeMem) {
      const totalMbInt = Math.round((m.memory?.total || 0) / 1048576);
      const usedMbInt = Math.round((m.memory?.used || 0) / 1048576);
      const availMb = Math.max(0, totalMbInt - usedMbInt);
      setElText(els.resFreeMem, `${availMb} MB available`);
    }
    setElText(els.resCountBadge, `${sum.runningWorkloadsCount || 0} / ${sum.totalWorkloadsCount || 0} Running`);

    // Render per-VPS separate capacity comparison cards or single-VPS inspect view
    renderInspectNodeTopbar(data.nodes);
    renderOverviewNodesStrip(data.nodes);
    renderResourceBreakdown(data.resources);
    if (typeof window.updateBreadcrumbs === 'function') {
      window.updateBreadcrumbs();
    }

    // Keep service pipelines synchronized with live telemetry & deployments
    if (state.currentView === 'services') {
      (state.projects || []).filter((p) => p.project_kind === 'service').forEach((p) => {
        if (typeof syncServiceCardPipeline === 'function') syncServiceCardPipeline(p);
      });
    }

    // Active deployment box (check inline style instead of getComputedStyle to avoid forced reflow)
    const active = dataProjects.find((x) => x.active);
    const actBox = els.actBox;
    const wasActBoxHidden = actBox && (actBox.style.display === 'none' || actBox.style.display === '');
    const pOverview = waterPipelines['overview-pipeline-track'];
    if (active && active.active) {
      if (actBox) {
        if (actBox.style.display !== 'block') actBox.style.display = 'block';
        if (wasActBoxHidden) animateSectionShow(actBox);
      }
      setElText(els.actName, active.project.name);
      const kindRaw = active.active.kind || 'deploy';
      setElText(els.actKind, kindRaw.charAt(0).toUpperCase() + kindRaw.slice(1).toLowerCase());
      renderPipelineStages('overview-pipeline-track', active.active.stage, active.active.status, active.active.id);
    } else if (pOverview && pOverview.isLooping && pOverview.currentProgress < 6.95) {
      if (actBox) {
        if (actBox.style.display !== 'block') actBox.style.display = 'block';
        if (wasActBoxHidden) animateSectionShow(actBox);
      }
      setElText(els.actKind, 'Deployed');
      renderPipelineStages('overview-pipeline-track', 'health_checking', 'success', pOverview.lastDeploymentId);
    } else if (actBox && actBox.style.display !== 'none') {
      actBox.style.display = 'none';
    }

    if (consumePendingViewStagger('overview')) {
      animateOverviewReveal();
    }

  } catch (err) {
    console.error('Overview load error:', err);
  }
}



window.loadOverview = loadOverview;
window.renderResourceBreakdown = renderResourceBreakdown;
window.filterResourceBreakdown = filterResourceBreakdown;
window.stopOverviewNetPoller = stopOverviewNetPoller;
window.resetOverviewNetGraph = resetOverviewNetGraph;
window.inspectOverviewNode = inspectOverviewNode;
window.copyOverviewNodeIp = copyBlurIpButton;

export {
  loadOverview,
  renderResourceBreakdown,
  filterResourceBreakdown,
  updateOverviewNetwork,
  stopOverviewNetPoller,
  resetOverviewNetGraph,
  inspectOverviewNode,
};
