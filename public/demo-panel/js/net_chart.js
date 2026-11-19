/**
 * Live network chart data shared by the Overview chart and the per-node charts on the Nodes view:
 * chart geometry, per-chart state, and merging server samples into a chart's history.
 */

import { syncPerfOffset } from './app_usage.js';

export const OV_NET_WINDOW_MS = 45000;
export const OV_NET_LAG_MS = 2200;
export const OV_NET_SVG_W = 420;
export const OV_NET_SVG_H = 56;
export const OV_NET_TOP = 7;
export const OV_NET_BOTTOM = 50;
export const OV_NET_Y_ZERO_IN = 48.2;
export const OV_NET_Y_ZERO_OUT = OV_NET_BOTTOM;
export const OV_NET_POLL_MS = 1000;

export function createNetChartState() {
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

// Samples further apart than this mean nobody was sampling (tab hidden, other view, server idle).
// Remote nodes report every ~5s, so stay well above that.
export const OV_NET_GAP_MS = 12000;

// Keep only the samples after the last gap, in place. Drawing across a gap would invent a straight
// line through time with no data; restarting the line from real samples is honest.
function dropSamplesBeforeLastGap(samples) {
  for (let k = samples.length - 1; k > 0; k--) {
    if (samples[k].ts - samples[k - 1].ts > OV_NET_GAP_MS) {
      samples.splice(0, k);
      return;
    }
  }
}

export function normalizeAndMergeNetState(netState, net, opts = {}) {
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
  dropSamplesBeforeLastGap(rawSorted);

  const inBps = Math.max(0, Number(net.inboundBps ?? net.inbound_bps) || 0);
  const outBps = Math.max(0, Number(net.outboundBps ?? net.outbound_bps) || 0);
  if (rawSorted.length === 0) {
    // Nothing measured yet: keep the readout, draw nothing.
    return { inBps, outBps, updated: false };
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

  // Only samples the server measured are drawn. No points are invented between server samples
  // (remote nodes report every ~5s): a padded point repeats the last rate and draws a flat line
  // that never happened.
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
  dropSamplesBeforeLastGap(out);
  netState._mergeBuf = existing;
  netState.samples = out;
  syncPerfOffset(netState, netState.samples, 0.05);
  return { inBps, outBps, updated: true };
}
