import { useEffect, useRef, useState } from "react";
import { m } from "motion/react";
import { EASE_OUT } from "@/components/Reveal";

interface MeshNode {
  id: string;
  x: number;
  y: number;
  name: string;
  role: string;
  ip: string;
  core?: boolean;
}

// Wide enough for the longest label ("ap-southeast · 2 apps") at 10.5px monospace.
const W = 210;
const H = 64;

// Diamond layout: every server links to every other one (WebDep peers all nodes over WireGuard).
const NODES: MeshNode[] = [
  { id: "core", x: 340, y: 62, name: "Web-Core", role: "dashboard · mumbai", ip: "10.200.0.1", core: true },
  { id: "fra", x: 115, y: 222, name: "Frankfurt", role: "eu-central · 3 apps", ip: "10.200.0.2" },
  { id: "vir", x: 565, y: 222, name: "Virginia", role: "us-east · 4 apps", ip: "10.200.0.4" },
  { id: "sgp", x: 340, y: 382, name: "Singapore", role: "ap-southeast · 2 apps", ip: "10.200.0.3" },
];

type LinkKind = "control" | "peer";
interface MeshLink {
  a: string;
  b: string;
  kind: LinkKind;
}

const LINKS: MeshLink[] = [
  { a: "core", b: "fra", kind: "control" },
  { a: "core", b: "vir", kind: "control" },
  { a: "core", b: "sgp", kind: "control" },
  { a: "fra", b: "vir", kind: "peer" },
  { a: "fra", b: "sgp", kind: "peer" },
  { a: "vir", b: "sgp", kind: "peer" },
];

const COLORS = {
  down: "var(--accent)",
  up: "var(--positive)",
  peer: "#a78bfa",
};

const byId = (id: string) => NODES.find((n) => n.id === id)!;

function Packet({ pathId, color, reverse, dur, begin }: { pathId: string; color: string; reverse?: boolean; dur: number; begin: number }) {
  // A dot with a faint halo: same glow look as a blur filter, at a fraction of the paint cost.
  return (
    <g opacity="0">
      <circle r="7" fill={color} opacity="0.18" />
      <circle r="3" fill={color} />
      <animateMotion
        dur={`${dur}s`}
        begin={`${begin}s`}
        repeatCount="indefinite"
        keyPoints={reverse ? "1;0" : "0;1"}
        keyTimes="0;1"
        calcMode="spline"
        keySplines="0.45 0 0.25 1"
      >
        <mpath href={`#${pathId}`} />
      </animateMotion>
      <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.15;0.85;1" dur={`${dur}s`} begin={`${begin}s`} repeatCount="indefinite" />
    </g>
  );
}

export default function MeshDiagram() {
  const [hover, setHover] = useState<string | null>(null);
  const linkActive = (l: MeshLink) => !hover || l.a === hover || l.b === hover;
  const svgRef = useRef<SVGSVGElement>(null);

  // Run the packet animations only while the diagram is on screen.
  useEffect(() => {
    const svg = svgRef.current;
    if (!svg) return;
    svg.pauseAnimations();
    const io = new IntersectionObserver(([entry]) => {
      if (entry.isIntersecting) svg.unpauseAnimations();
      else svg.pauseAnimations();
    });
    io.observe(svg);
    return () => io.disconnect();
  }, []);

  return (
    <div className="relative">
      <svg ref={svgRef} viewBox="0 0 680 450" className="h-auto w-full" role="img" aria-label="Four servers fully connected in a WireGuard mesh: the Web-Core dashboard and three workers">
        <defs>
          <pattern id="mesh-dots" width="22" height="22" patternUnits="userSpaceOnUse">
            <circle cx="1" cy="1" r="1" fill="var(--foreground)" opacity="0.07" />
          </pattern>
          <radialGradient id="mesh-core-glow">
            <stop offset="0%" stopColor="var(--accent)" stopOpacity="0.32" />
            <stop offset="100%" stopColor="var(--accent)" stopOpacity="0" />
          </radialGradient>
        </defs>

        <rect width="680" height="450" fill="url(#mesh-dots)" />

        {/* Links */}
        {LINKS.map((l, i) => {
          const a = byId(l.a);
          const b = byId(l.b);
          const id = `mesh-path-${l.a}-${l.b}`;
          const active = linkActive(l);
          const peer = l.kind === "peer";
          const dur = 2.6 + (i % 3) * 0.45;
          return (
            <g key={id} style={{ opacity: active ? 1 : 0.18, transition: "opacity 300ms" }}>
              <m.path
                id={id}
                d={`M${a.x} ${a.y} L${b.x} ${b.y}`}
                stroke={peer ? COLORS.peer : "var(--accent)"}
                strokeOpacity={peer ? 0.35 : 0.4}
                strokeWidth="1.4"
                strokeDasharray={peer ? "4 6" : undefined}
                fill="none"
                initial={{ pathLength: 0 }}
                whileInView={{ pathLength: 1 }}
                viewport={{ once: true }}
                transition={{ duration: 1, delay: 0.3 + i * 0.12, ease: EASE_OUT }}
              />
              {peer ? (
                <>
                  <Packet pathId={id} color={COLORS.peer} dur={dur} begin={1.6 + i * 0.3} />
                  <Packet pathId={id} color={COLORS.peer} dur={dur} begin={1.6 + i * 0.3 + dur / 2} reverse />
                </>
              ) : (
                <>
                  <Packet pathId={id} color={COLORS.down} dur={dur} begin={1.3 + i * 0.35} />
                  <Packet pathId={id} color={COLORS.up} dur={dur} begin={1.3 + i * 0.35 + dur / 2} reverse />
                </>
              )}
            </g>
          );
        })}

        {/* Centre badge: the private network every node shares */}
        <m.g
          initial={{ opacity: 0, scale: 0.8 }}
          whileInView={{ opacity: 1, scale: 1 }}
          viewport={{ once: true }}
          transition={{ duration: 0.6, delay: 1.1, ease: EASE_OUT }}
          style={{ transformOrigin: "340px 222px" }}
        >
          <rect x="262" y="206" width="156" height="32" rx="16" fill="var(--background)" stroke="var(--border)" />
          {/* Lock glyph, vertically centred on the badge (y = 222) */}
          <path d="M283.5 219.5 v-2.5 a3.5 3.5 0 0 1 7 0 v2.5" fill="none" stroke="#a78bfa" strokeWidth="1.6" strokeLinecap="round" />
          <rect x="281" y="219.5" width="12" height="9" rx="2.2" fill="#a78bfa" />
          <circle cx="287" cy="224" r="1.3" fill="var(--background)" />
          <text x="300" y="222" dominantBaseline="central" fill="var(--muted-foreground)" fontSize="11" fontFamily="JetBrains Mono, monospace">WireGuard mesh</text>
        </m.g>

        {/* Nodes */}
        {NODES.map((n, i) => (
          <m.g
            key={n.id}
            onMouseEnter={() => setHover(n.id)}
            onMouseLeave={() => setHover(null)}
            initial={{ opacity: 0, y: 12 }}
            whileInView={{ opacity: 1, y: 0 }}
            viewport={{ once: true }}
            transition={{ duration: 0.6, delay: 0.15 + i * 0.12, ease: EASE_OUT }}
            style={{ cursor: "default" }}
          >
            {n.core ? (
              <circle cx={n.x} cy={n.y} r="115" fill="url(#mesh-core-glow)" />
            ) : null}
            <rect
              x={n.x - W / 2}
              y={n.y - H / 2}
              width={W}
              height={H}
              rx="16"
              fill="var(--card)"
              stroke={n.core ? "var(--accent)" : hover === n.id ? "var(--muted-subtle)" : "var(--border)"}
              strokeOpacity={n.core ? 0.65 : 1}
              style={{ transition: "stroke 200ms" }}
            />
            {/* Server glyph */}
            <g transform={`translate(${n.x - W / 2 + 14} ${n.y - 14})`}>
              <rect width="28" height="28" rx="8" fill={n.core ? "var(--accent)" : "var(--foreground)"} fillOpacity={n.core ? 0.16 : 0.06} />
              <rect x="8" y="8" width="12" height="4.5" rx="1.2" fill="none" stroke={n.core ? "var(--accent)" : "var(--muted-foreground)"} strokeWidth="1.3" />
              <rect x="8" y="15.5" width="12" height="4.5" rx="1.2" fill="none" stroke={n.core ? "var(--accent)" : "var(--muted-foreground)"} strokeWidth="1.3" />
            </g>
            <text x={n.x - W / 2 + 52} y={n.y - 4} fill="var(--foreground)" fontSize="13.5" fontWeight="600">{n.name}</text>
            <circle cx={n.x + W / 2 - 16} cy={n.y - 9} r="3.5" fill="var(--positive)">
              <animate attributeName="opacity" values="1;0.35;1" dur={`${2 + i * 0.35}s`} repeatCount="indefinite" />
            </circle>
            <text x={n.x - W / 2 + 52} y={n.y + 14} fill="var(--muted-foreground)" fontSize="10.5" fontFamily="JetBrains Mono, monospace">
              {hover === n.id ? n.ip : n.role}
            </text>
          </m.g>
        ))}
      </svg>

      <div className="mt-2 flex flex-wrap items-center justify-center gap-2 text-[11px] text-muted-foreground">
        {[
          { c: COLORS.down, l: "Deploys & commands" },
          { c: COLORS.up, l: "Metrics & logs" },
          { c: COLORS.peer, l: "Node ↔ node" },
        ].map((k) => (
          <span key={k.l} className="inline-flex items-center gap-1.5 rounded-full border border-border px-2.5 py-1 whitespace-nowrap">
            <span className="size-1.5 rounded-full" style={{ background: k.c, boxShadow: `0 0 8px ${k.c}` }} />
            {k.l}
          </span>
        ))}
      </div>
    </div>
  );
}
