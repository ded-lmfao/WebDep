import { m } from "motion/react";
import { ArrowRight, Play } from "lucide-react";
import { CommandLine } from "@/components/CopyButton";
import { EASE_OUT } from "@/components/Reveal";
import { HERO_INSTALL, STATS } from "@/data/site";

const HEADLINE = [
  ["Your", "servers."],
  ["Your", "PaaS."],
];

export default function Hero({ onOpenDemo }: { onOpenDemo: () => void }) {
  return (
    <section id="top" className="relative overflow-hidden">
      {/* Soft glow behind the headline. */}
      <m.div
        aria-hidden="true"
        initial={{ opacity: 0, scale: 0.8 }}
        animate={{ opacity: 1, scale: 1 }}
        transition={{ duration: 1.4, ease: EASE_OUT }}
        className="pointer-events-none absolute top-[-18rem] left-1/2 h-[36rem] w-[60rem] -translate-x-1/2 rounded-full bg-[radial-gradient(closest-side,color-mix(in_srgb,var(--accent)_22%,transparent),transparent)]"
      />

      <div className="relative mx-auto max-w-3xl px-4 pt-20 pb-14 text-center sm:px-6 sm:pt-28">
        <m.a
          href="#nodes"
          initial={{ opacity: 0, y: 8 }}
          animate={{ opacity: 1, y: 0 }}
          transition={{ duration: 0.5, ease: EASE_OUT }}
          className="inline-flex items-center gap-2 rounded-full border border-border bg-foreground/[0.03] px-3 py-1 text-xs text-muted-foreground transition-colors hover:text-foreground"
        >
          <span className="relative flex size-2">
            <span className="absolute inline-flex size-full animate-ping rounded-full bg-positive opacity-60" />
            <span className="relative inline-flex size-2 rounded-full bg-positive" />
          </span>
          v3 · multi-node clusters
          <ArrowRight className="size-3" />
        </m.a>

        <h1 className="mt-6 text-5xl font-semibold tracking-[-0.035em] text-foreground sm:text-7xl">
          {HEADLINE.map((line, li) => (
            <span key={line.join(" ")} className={`block ${li === 1 ? "text-muted-foreground" : ""}`}>
              {line.map((word, wi) => (
                <m.span
                  key={word}
                  initial={{ opacity: 0, y: 28 }}
                  animate={{ opacity: 1, y: 0 }}
                  transition={{ duration: 0.7, delay: 0.08 + (li * 2 + wi) * 0.08, ease: EASE_OUT }}
                  className={`inline-block ${wi === 0 ? "mr-[0.22em]" : ""}`}
                >
                  {word}
                </m.span>
              ))}
            </span>
          ))}
        </h1>

        <m.p
          initial={{ opacity: 0, y: 12 }}
          animate={{ opacity: 1, y: 0 }}
          transition={{ duration: 0.6, delay: 0.42, ease: EASE_OUT }}
          className="mx-auto mt-6 max-w-xl text-lg leading-relaxed text-muted-foreground"
        >
          Deploy websites, background services and PostgreSQL across one or many VPSes from a single
          dashboard. One Rust binary. No Docker.
        </m.p>

        <m.div
          initial={{ opacity: 0, y: 12 }}
          animate={{ opacity: 1, y: 0 }}
          transition={{ duration: 0.6, delay: 0.52, ease: EASE_OUT }}
          className="mt-9 flex flex-wrap items-center justify-center gap-3"
        >
          <button
            type="button"
            onClick={onOpenDemo}
            className="group inline-flex h-11 items-center gap-2 rounded-full bg-foreground px-6 text-sm font-medium text-background transition-transform active:scale-[0.97]"
          >
            <Play className="size-4 fill-current" />
            Try the live demo
          </button>
          <a
            href="#install"
            className="group inline-flex h-11 items-center gap-2 rounded-full border border-border px-6 text-sm font-medium text-foreground transition-colors hover:bg-foreground/[0.04]"
          >
            Install in one line
            <ArrowRight className="size-4 transition-transform group-hover:translate-x-0.5" />
          </a>
        </m.div>

        <m.div
          initial={{ opacity: 0, y: 12 }}
          animate={{ opacity: 1, y: 0 }}
          transition={{ duration: 0.6, delay: 0.62, ease: EASE_OUT }}
          className="mx-auto mt-8 max-w-xl text-left"
        >
          <CommandLine command={HERO_INSTALL} />
        </m.div>

        <m.dl
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          transition={{ duration: 0.8, delay: 0.8 }}
          className="mx-auto mt-12 grid max-w-xl grid-cols-3 gap-4"
        >
          {STATS.map((s) => (
            <div key={s.label}>
              <dt className="font-mono text-base font-medium text-foreground sm:text-lg">{s.value}</dt>
              <dd className="mt-1 text-xs leading-snug text-muted-foreground">{s.label}</dd>
            </div>
          ))}
        </m.dl>
      </div>
    </section>
  );
}
