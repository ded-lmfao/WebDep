import MeshDiagram from "@/components/MeshDiagram";
import Reveal, { SectionHeading } from "@/components/Reveal";
import { NODE_FACTS } from "@/data/site";

export default function MultiNode() {
  return (
    <section id="nodes" className="scroll-mt-20 border-t border-border px-4 py-24 sm:px-6">
      <SectionHeading
        eyebrow="Multi-node"
        title="Grow from one VPS to a fleet"
        text="Every server runs the same binary. One becomes the dashboard, the rest join as workers, and all of them link to each other in a private WireGuard mesh."
      />

      <div className="mx-auto mt-14 max-w-5xl">
        <Reveal className="mx-auto max-w-3xl overflow-hidden rounded-3xl border border-border bg-gradient-to-b from-foreground/[0.03] to-transparent p-3 pb-5 sm:p-6">
          <MeshDiagram />
          <p className="mt-3 hidden text-center text-[11px] text-muted-subtle sm:block">Hover a server to see its links.</p>
        </Reveal>

        <div className="mt-14 grid gap-8 sm:grid-cols-2 lg:grid-cols-4">
          {NODE_FACTS.map((f, i) => (
            <Reveal key={f.title} delay={i * 0.08} className="group flex gap-4 lg:flex-col lg:gap-3">
              <div className="grid size-10 shrink-0 place-items-center rounded-xl border border-border text-accent transition-colors duration-300 group-hover:border-accent/50 group-hover:bg-accent/10">
                <f.icon className="size-[18px]" strokeWidth={1.75} />
              </div>
              <div>
                <h3 className="text-[15px] font-semibold text-foreground">{f.title}</h3>
                <p className="mt-1.5 text-sm leading-relaxed text-muted-foreground">{f.text}</p>
              </div>
            </Reveal>
          ))}
        </div>
      </div>
    </section>
  );
}
