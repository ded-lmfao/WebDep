import { useEffect, useState } from "react";
import { m } from "motion/react";
import { Github, Moon, Play, Sun } from "lucide-react";
import { GITHUB_REPO_URL } from "@/data/site";

type Theme = "light" | "dark";

const LINKS = [
  { id: "panel", label: "Panel" },
  { id: "features", label: "Features" },
  { id: "nodes", label: "Multi-node" },
  { id: "install", label: "Install" },
];

export default function Nav({
  theme,
  onThemeChange,
  onOpenDemo,
}: {
  theme: Theme;
  onThemeChange: (t: Theme) => void;
  onOpenDemo: () => void;
}) {
  const [scrolled, setScrolled] = useState(false);
  const [activeId, setActiveId] = useState("");

  useEffect(() => {
    const onScroll = () => setScrolled(window.scrollY > 8);
    onScroll();
    window.addEventListener("scroll", onScroll, { passive: true });
    return () => window.removeEventListener("scroll", onScroll);
  }, []);

  // Highlight the link of the section currently in the middle of the screen.
  useEffect(() => {
    const sections = LINKS.map((l) => document.getElementById(l.id)).filter((el): el is HTMLElement => el !== null);
    const observer = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (entry.isIntersecting) setActiveId(entry.target.id);
        }
      },
      { rootMargin: "-45% 0px -50% 0px" },
    );
    sections.forEach((s) => observer.observe(s));
    const onTop = () => {
      if (window.scrollY < 200) setActiveId("");
    };
    window.addEventListener("scroll", onTop, { passive: true });
    return () => {
      observer.disconnect();
      window.removeEventListener("scroll", onTop);
    };
  }, []);

  return (
    <m.header
      initial={{ y: -16, opacity: 0 }}
      animate={{ y: 0, opacity: 1 }}
      transition={{ duration: 0.5, ease: [0.22, 1, 0.36, 1] }}
      className={`sticky top-0 z-40 transition-[background-color,border-color] duration-300 ${
        scrolled ? "border-b border-border bg-background/95" : "border-b border-transparent"
      }`}
    >
      <nav className="mx-auto flex h-16 max-w-6xl items-center justify-between gap-4 px-4 sm:px-6">
        <a href="#top" className="flex items-center gap-2.5 text-foreground">
          <img src="./images/logo.png" alt="" className="size-7 rounded-md" />
          <span className="text-[15px] font-semibold tracking-tight">WebDep</span>
        </a>

        <div className="hidden items-center gap-1 md:flex">
          {LINKS.map((l) => (
            <a
              key={l.id}
              href={`#${l.id}`}
              className={`relative rounded-full px-3 py-1.5 text-sm transition-colors ${
                activeId === l.id ? "text-foreground" : "text-muted-foreground hover:text-foreground"
              }`}
            >
              {activeId === l.id ? (
                <m.span
                  layoutId="nav-active"
                  className="absolute inset-0 rounded-full bg-foreground/[0.07]"
                  transition={{ type: "spring", stiffness: 400, damping: 34 }}
                />
              ) : null}
              <span className="relative">{l.label}</span>
            </a>
          ))}
        </div>

        <div className="flex items-center gap-1">
          <a
            href={GITHUB_REPO_URL}
            target="_blank"
            rel="noreferrer"
            aria-label="WebDep on GitHub"
            className="grid size-9 place-items-center rounded-md text-muted-foreground transition-colors hover:text-foreground"
          >
            <Github className="size-[18px]" />
          </a>
          <button
            type="button"
            aria-label="Toggle color theme"
            onClick={() => onThemeChange(theme === "dark" ? "light" : "dark")}
            className="grid size-9 place-items-center rounded-md text-muted-foreground transition-colors hover:text-foreground"
          >
            <m.span key={theme} initial={{ rotate: -90, opacity: 0 }} animate={{ rotate: 0, opacity: 1 }} transition={{ duration: 0.25 }}>
              {theme === "dark" ? <Sun className="size-[18px]" /> : <Moon className="size-[18px]" />}
            </m.span>
          </button>
          <button
            type="button"
            onClick={onOpenDemo}
            className="ml-1 inline-flex h-9 items-center gap-1.5 rounded-full bg-foreground px-4 text-sm font-medium text-background transition-opacity hover:opacity-90"
          >
            <Play className="size-3.5 fill-current" />
            Live demo
          </button>
        </div>
      </nav>
    </m.header>
  );
}
