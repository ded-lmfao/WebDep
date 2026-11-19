import { Suspense, lazy, useCallback, useEffect, useState } from "react";
import { LazyMotion, MotionConfig } from "motion/react";
import Features from "@/components/Features";
import Footer from "@/components/Footer";
import Hero from "@/components/Hero";
import Install from "@/components/Install";
import LivePanel from "@/components/LivePanel";
import MultiNode from "@/components/MultiNode";
import Nav from "@/components/Nav";
import useSmoothScroll from "@/hooks/useSmoothScroll";

const PanelDemo = lazy(() => import("@/components/PanelDemo"));
const loadMotionFeatures = () => import("@/lib/motionFeatures").then((mod) => mod.default);

type Theme = "light" | "dark";

function initialTheme(): Theme {
  try {
    const saved = window.localStorage.getItem("webdep-site-theme");
    if (saved === "light" || saved === "dark") return saved;
  } catch {
    // Storage blocked: fall through to the default.
  }
  return "dark";
}

export default function App() {
  const [theme, setTheme] = useState<Theme>(initialTheme);
  const [demoMode, setDemoMode] = useState(() => window.location.hash === "#demo");
  useSmoothScroll(!demoMode);

  useEffect(() => {
    const root = document.documentElement;
    root.classList.toggle("dark", theme === "dark");
    root.setAttribute("data-theme", theme);
    root.style.colorScheme = theme;
    try {
      window.localStorage.setItem("webdep-site-theme", theme);
    } catch {
      // Ignore storage failures in private browsing.
    }
  }, [theme]);

  useEffect(() => {
    const onHash = () => setDemoMode(window.location.hash === "#demo");
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  const openDemo = useCallback(() => {
    setDemoMode(true);
    if (window.location.hash !== "#demo") window.location.hash = "demo";
    window.scrollTo({ top: 0, behavior: "auto" });
  }, []);

  const exitDemo = useCallback(() => {
    setDemoMode(false);
    if (window.location.hash === "#demo") {
      window.history.replaceState(null, "", window.location.pathname + window.location.search);
    }
  }, []);

  return (
    <LazyMotion features={loadMotionFeatures} strict>
    <MotionConfig reducedMotion="user">
      {demoMode ? (
        <Suspense fallback={<div className="min-h-screen bg-[#0b101d]" />}>
          <PanelDemo onExit={exitDemo} />
        </Suspense>
      ) : (
        <div className="min-h-screen overflow-x-clip bg-background text-foreground">
          <Nav theme={theme} onThemeChange={setTheme} onOpenDemo={openDemo} />
          <main>
            <Hero onOpenDemo={openDemo} />
            <LivePanel onOpenDemo={openDemo} />
            <Features />
            <MultiNode />
            <Install />
          </main>
          <Footer onOpenDemo={openDemo} />
        </div>
      )}
    </MotionConfig>
    </LazyMotion>
  );
}
