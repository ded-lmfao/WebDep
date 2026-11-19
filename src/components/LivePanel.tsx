import { useEffect, useRef, useState } from "react";
import { m, useInView } from "motion/react";
import { Maximize2 } from "lucide-react";
import { EASE_OUT, SectionHeading } from "@/components/Reveal";
import { DEMO_VIEWS } from "@/data/site";

/**
 * The real WebDep panel running on the demo backend, embedded inline. Tabs switch the panel's own
 * pages, so what visitors see here is exactly the product, not a screenshot.
 *
 * Performance: the panel shares the page's main thread (same-origin iframe), so it only loads when
 * the section comes near, and is hidden (`display: none`, which stops its rendering and chart
 * animations) whenever it is scrolled off screen.
 */
export default function LivePanel({ onOpenDemo }: { onOpenDemo: () => void }) {
  const frameRef = useRef<HTMLIFrameElement>(null);
  const boxRef = useRef<HTMLDivElement>(null);
  const near = useInView(boxRef, { once: true, margin: "300px" });
  const onScreen = useInView(boxRef, { margin: "100px" });
  const [active, setActive] = useState("overview");
  const [loaded, setLoaded] = useState(false);

  const go = (view: string) => {
    setActive(view);
    frameRef.current?.contentWindow?.postMessage({ type: "webdep-demo:navigate", view }, window.location.origin);
  };

  // Keep the tab bar in sync when the visitor navigates inside the panel itself.
  useEffect(() => {
    if (!loaded || !onScreen) return;
    const id = window.setInterval(() => {
      try {
        const path = frameRef.current?.contentWindow?.location.pathname || "";
        const view = path.split("/").filter(Boolean).pop() || "";
        if (DEMO_VIEWS.some((v) => v.id === view)) setActive(view);
      } catch {
        // Not ready yet.
      }
    }, 800);
    return () => window.clearInterval(id);
  }, [loaded, onScreen]);

  return (
    <section id="panel" className="scroll-mt-20 px-4 pb-24 sm:px-6">
      <SectionHeading
        eyebrow="Live panel"
        title="This is the real dashboard"
        text="Click around. It runs the actual WebDep panel on a demo backend, with two servers already connected."
      />

      <div className="mx-auto mt-10 max-w-6xl">
        <div className="relative mx-auto mb-4 flex max-w-full gap-1 overflow-x-auto rounded-full border border-border bg-foreground/[0.03] p-1 [scrollbar-width:none] sm:w-fit">
          {DEMO_VIEWS.map((v) => (
            <button
              key={v.id}
              type="button"
              onClick={() => go(v.id)}
              className={`relative shrink-0 rounded-full px-3.5 py-1.5 text-[13px] transition-colors ${
                active === v.id ? "text-background" : "text-muted-foreground hover:text-foreground"
              }`}
            >
              {active === v.id ? (
                <m.span
                  layoutId="panel-tab"
                  className="absolute inset-0 rounded-full bg-foreground"
                  transition={{ type: "spring", stiffness: 420, damping: 34 }}
                />
              ) : null}
              <span className="relative">{v.label}</span>
            </button>
          ))}
        </div>

        <m.div
          ref={boxRef}
          initial={{ opacity: 0, y: 32 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true, margin: "-80px" }}
          transition={{ duration: 0.8, ease: EASE_OUT }}
          className="relative overflow-hidden rounded-2xl border border-border bg-[#0b101d] shadow-[0_30px_80px_-40px_color-mix(in_srgb,var(--accent)_40%,transparent)]"
        >
          <div className="flex h-10 items-center gap-2 border-b border-white/[0.06] bg-[#0d1322] px-4">
            <span className="size-3 rounded-full bg-white/10" />
            <span className="size-3 rounded-full bg-white/10" />
            <span className="size-3 rounded-full bg-white/10" />
            <span className="mx-auto truncate rounded-md bg-white/[0.04] px-3 py-0.5 font-mono text-[11px] text-white/45">
              panel.your-domain.com/{active}
            </span>
            <button
              type="button"
              onClick={onOpenDemo}
              aria-label="Open the demo full screen"
              className="grid size-7 place-items-center rounded-md text-white/50 transition-colors hover:bg-white/[0.06] hover:text-white"
            >
              <Maximize2 className="size-3.5" />
            </button>
          </div>
          <div className="relative h-[560px] [contain:strict] sm:h-[680px]">
            {near ? (
              <iframe
                ref={frameRef}
                title="WebDep live panel"
                src="./demo-panel/index.html?embed=1"
                loading="lazy"
                onLoad={() => setLoaded(true)}
                style={{ display: onScreen ? "block" : "none" }}
                className={`absolute inset-0 h-full w-full border-0 transition-opacity duration-500 ${loaded ? "opacity-100" : "opacity-0"}`}
              />
            ) : null}
            {!loaded ? (
              <div className="absolute inset-0 grid place-items-center">
                <div className="size-6 animate-spin rounded-full border-2 border-white/15 border-t-white/60" />
              </div>
            ) : null}
          </div>
        </m.div>
      </div>
    </section>
  );
}
