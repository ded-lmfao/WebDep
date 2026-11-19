import { useId } from "react";

/** Tux silhouette (belly and eyes cut out so it reads on any background). */
export function LinuxIcon({ className = "size-5" }: { className?: string }) {
  // useId() contains characters that are unsafe inside url(#...), so keep only letters and digits.
  const mask = `tux-${useId().replace(/[^a-zA-Z0-9]/g, "")}`;
  return (
    <svg viewBox="0 0 24 24" className={className} aria-hidden="true">
      <defs>
        <mask id={mask}>
          <rect width="24" height="24" fill="white" />
          <ellipse cx="12" cy="15.4" rx="3.7" ry="4.7" fill="black" />
          <circle cx="10.4" cy="6.4" r="1.05" fill="black" />
          <circle cx="13.6" cy="6.4" r="1.05" fill="black" />
        </mask>
      </defs>
      <g mask={`url(#${mask})`} fill="currentColor">
        <path d="M12 1.8c-2.6 0-4.1 2.1-4.1 4.7 0 1.4-.6 2.4-1.5 3.7C5.1 12 4.3 14 4.3 16.1c0 3 3 5.4 7.7 5.4s7.7-2.4 7.7-5.4c0-2.1-.8-4.1-2.1-5.9-.9-1.3-1.5-2.3-1.5-3.7 0-2.6-1.5-4.7-4.1-4.7z" />
      </g>
      <path d="M10.6 8.2h2.8L12 9.9z" fill="currentColor" opacity="0.65" />
      <ellipse cx="8.4" cy="21.3" rx="2.6" ry="1.3" fill="currentColor" />
      <ellipse cx="15.6" cy="21.3" rx="2.6" ry="1.3" fill="currentColor" />
    </svg>
  );
}

/** Four-pane window logo. */
export function WindowsIcon({ className = "size-5" }: { className?: string }) {
  return (
    <svg viewBox="0 0 24 24" className={className} aria-hidden="true" fill="currentColor">
      <path d="M3 5.4 10.4 4.4v7.1H3zM11.4 4.2 21 2.9v8.6h-9.6zM3 12.5h7.4v7.1L3 18.6zM11.4 12.5H21v8.6l-9.6-1.3z" />
    </svg>
  );
}
