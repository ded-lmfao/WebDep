import type { MouseEvent } from "react";
import { m } from "motion/react";
import { EASE_OUT, SectionHeading } from "@/components/Reveal";
import { FEATURES } from "@/data/site";

/** Moves the card's soft spotlight to follow the cursor. */
function trackSpotlight(e: MouseEvent<HTMLDivElement>) {
  const el = e.currentTarget;
  const rect = el.getBoundingClientRect();
  el.style.setProperty("--spot-x", `${e.clientX - rect.left}px`);
  el.style.setProperty("--spot-y", `${e.clientY - rect.top}px`);
}

export default function Features() {
  return (
    <section id="features" className="scroll-mt-20 border-t border-border px-4 py-24 sm:px-6">
      <SectionHeading eyebrow="Features" title="Everything a small PaaS needs" text="Built in, not bolted on." />

      <div className="mx-auto mt-14 grid max-w-5xl gap-px overflow-hidden rounded-2xl border border-border bg-border sm:grid-cols-2 lg:grid-cols-3">
        {FEATURES.map((f, i) => (
          <m.div
            key={f.title}
            onMouseMove={trackSpotlight}
            initial={{ opacity: 0, y: 18 }}
            whileInView={{ opacity: 1, y: 0 }}
            viewport={{ once: true, margin: "-60px" }}
            transition={{ duration: 0.55, delay: (i % 3) * 0.08 + Math.floor(i / 3) * 0.06, ease: EASE_OUT }}
            className="group relative overflow-hidden bg-background p-7"
          >
            <div
              aria-hidden="true"
              className="pointer-events-none absolute inset-0 opacity-0 transition-opacity duration-300 group-hover:opacity-100"
              style={{
                background:
                  "radial-gradient(260px circle at var(--spot-x, 50%) var(--spot-y, 50%), color-mix(in srgb, var(--accent) 12%, transparent), transparent 70%)",
              }}
            />
            <div className="relative">
              <div className="grid size-10 place-items-center rounded-xl border border-border text-accent transition-all duration-300 group-hover:-translate-y-0.5 group-hover:border-accent/40">
                <f.icon className="size-[18px]" strokeWidth={1.75} />
              </div>
              <h3 className="mt-5 text-[15px] font-semibold text-foreground">{f.title}</h3>
              <p className="mt-2 text-sm leading-relaxed text-muted-foreground">{f.text}</p>
            </div>
          </m.div>
        ))}
      </div>
    </section>
  );
}
