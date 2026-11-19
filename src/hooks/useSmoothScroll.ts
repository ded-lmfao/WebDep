import { useEffect } from "react";
import Lenis from "lenis";

/**
 * Inertial smooth scrolling for the landing page (wheel, trackpad and in-page #anchor links).
 * Skipped when the visitor asks for reduced motion. Touch keeps the native scroll feel.
 */
export default function useSmoothScroll(enabled: boolean) {
  useEffect(() => {
    if (!enabled) return;
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) return;

    const lenis = new Lenis({
      // Frame-based easing toward the target: steady on both mouse wheels and trackpads.
      lerp: 0.1,
      smoothWheel: true,
      wheelMultiplier: 1,
      anchors: { offset: -72 },
      autoRaf: true,
    });

    return () => lenis.destroy();
  }, [enabled]);
}
