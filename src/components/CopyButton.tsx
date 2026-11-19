import { useEffect, useRef, useState } from "react";
import { AnimatePresence, m } from "motion/react";
import { Check, Copy } from "lucide-react";

export default function CopyButton({ text, label = "Copy command" }: { text: string; label?: string }) {
  const [copied, setCopied] = useState(false);
  const timer = useRef<number | null>(null);

  useEffect(() => () => {
    if (timer.current !== null) window.clearTimeout(timer.current);
  }, []);

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      return;
    }
    setCopied(true);
    if (timer.current !== null) window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setCopied(false), 1600);
  };

  return (
    <button
      type="button"
      onClick={copy}
      aria-label={copied ? "Copied" : label}
      className="relative grid size-8 shrink-0 place-items-center rounded-md text-muted-foreground transition-colors hover:bg-foreground/[0.06] hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
    >
      <AnimatePresence initial={false} mode="popLayout">
        <m.span
          key={copied ? "done" : "copy"}
          initial={{ opacity: 0, scale: 0.6 }}
          animate={{ opacity: 1, scale: 1 }}
          exit={{ opacity: 0, scale: 0.6 }}
          transition={{ duration: 0.16 }}
          className="grid place-items-center"
        >
          {copied ? <Check className="size-4 text-positive" /> : <Copy className="size-4" />}
        </m.span>
      </AnimatePresence>
    </button>
  );
}

export function CommandLine({ command, prompt = "$" }: { command: string; prompt?: string }) {
  return (
    <div className="flex items-center gap-2 rounded-xl border border-border bg-surface-deep/80 py-1.5 pr-1.5 pl-4">
      <span aria-hidden="true" className="font-mono text-sm text-accent select-none">
        {prompt}
      </span>
      <code className="min-w-0 flex-1 overflow-x-auto font-mono text-[13px] whitespace-nowrap text-foreground/90 [scrollbar-width:none]">
        {command}
      </code>
      <CopyButton text={command} />
    </div>
  );
}
