import { m } from "motion/react";
import { Play } from "lucide-react";
import { EASE_OUT } from "@/components/Reveal";
import { GITHUB_RELEASES_URL, GITHUB_REPO_URL } from "@/data/site";

export default function Footer({ onOpenDemo }: { onOpenDemo: () => void }) {
  return (
    <footer className="border-t border-border px-4 sm:px-6">
      <m.div
        initial={{ opacity: 0, y: 20 }}
        whileInView={{ opacity: 1, y: 0 }}
        viewport={{ once: true }}
        transition={{ duration: 0.7, ease: EASE_OUT }}
        className="mx-auto max-w-3xl py-24 text-center"
      >
        <h2 className="text-3xl font-semibold tracking-[-0.02em] text-foreground sm:text-4xl">See it for yourself</h2>
        <p className="mt-4 text-muted-foreground">No sign-up. The demo runs entirely in your browser.</p>
        <button
          type="button"
          onClick={onOpenDemo}
          className="mt-8 inline-flex h-11 items-center gap-2 rounded-full bg-foreground px-6 text-sm font-medium text-background transition-transform active:scale-[0.97]"
        >
          <Play className="size-4 fill-current" />
          Open the live demo
        </button>
      </m.div>

      <div className="mx-auto flex max-w-6xl flex-col items-center justify-between gap-3 border-t border-border py-8 text-sm text-muted-foreground sm:flex-row">
        <div className="flex items-center gap-2">
          <img src="./images/logo.png" alt="" className="size-5 rounded" />
          <span>WebDep · MIT License</span>
        </div>
        <div className="flex items-center gap-5">
          <a href={GITHUB_REPO_URL} target="_blank" rel="noreferrer" className="transition-colors hover:text-foreground">GitHub</a>
          <a href={GITHUB_RELEASES_URL} target="_blank" rel="noreferrer" className="transition-colors hover:text-foreground">Releases</a>
        </div>
      </div>
    </footer>
  );
}
