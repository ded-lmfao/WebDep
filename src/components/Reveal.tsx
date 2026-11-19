import type { ReactNode } from "react";
import { m } from "motion/react";

export const EASE_OUT = [0.22, 1, 0.36, 1] as const;

/** Fades and lifts its children in once, when they scroll into view. */
export default function Reveal({
  children,
  delay = 0,
  y = 16,
  className,
}: {
  children: ReactNode;
  delay?: number;
  y?: number;
  className?: string;
}) {
  return (
    <m.div
      className={className}
      initial={{ opacity: 0, y }}
      whileInView={{ opacity: 1, y: 0 }}
      viewport={{ once: true, margin: "-80px" }}
      transition={{ duration: 0.6, delay, ease: EASE_OUT }}
    >
      {children}
    </m.div>
  );
}

export function SectionHeading({ eyebrow, title, text }: { eyebrow: string; title: string; text?: string }) {
  return (
    <Reveal className="mx-auto max-w-2xl text-center">
      <p className="font-mono text-xs tracking-wide text-accent uppercase">{eyebrow}</p>
      <h2 className="mt-3 text-3xl font-semibold tracking-[-0.02em] text-foreground sm:text-4xl">{title}</h2>
      {text ? <p className="mt-4 text-base leading-relaxed text-muted-foreground">{text}</p> : null}
    </Reveal>
  );
}
