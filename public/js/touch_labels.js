/**
 * Icon-only buttons show their name in a hover tooltip (`title`). Touch screens have no hover,
 * so a long press shows the same name in a small label, and every icon button gets an
 * accessible name for screen readers.
 */
const ICON_SELECTOR = '.btn-icon[title], button[title]:not(:has(span, strong))';
const HOLD_MS = 450;

function nameIcons(root = document) {
  root.querySelectorAll?.(ICON_SELECTOR).forEach((el) => {
    if (!el.hasAttribute('aria-label') && el.title) el.setAttribute('aria-label', el.title);
  });
}

new MutationObserver((records) => {
  for (const r of records) {
    r.addedNodes.forEach((n) => {
      if (n.nodeType === 1) {
        if (n.matches?.(ICON_SELECTOR)) nameIcons(n.parentNode || document);
        else nameIcons(n);
      }
    });
  }
}).observe(document.documentElement, { childList: true, subtree: true });
nameIcons();

let tip = null;
let timer = null;
let shownFor = null;

function hideTip() {
  clearTimeout(timer);
  tip?.remove();
  tip = null;
}

document.addEventListener('touchstart', (e) => {
  const btn = e.target.closest?.(ICON_SELECTOR);
  hideTip();
  if (!btn || !btn.title) return;
  timer = setTimeout(() => {
    const r = btn.getBoundingClientRect();
    tip = document.createElement('div');
    tip.className = 'touch-label';
    tip.setAttribute('role', 'tooltip');
    tip.textContent = btn.title;
    document.body.appendChild(tip);
    const w = tip.offsetWidth;
    const left = Math.min(Math.max(8, r.left + r.width / 2 - w / 2), window.innerWidth - w - 8);
    tip.style.left = `${left}px`;
    tip.style.top = `${Math.max(8, r.top - tip.offsetHeight - 8)}px`;
    shownFor = btn;
  }, HOLD_MS);
}, { passive: true });

document.addEventListener('touchmove', hideTip, { passive: true });
document.addEventListener('touchend', (e) => {
  clearTimeout(timer);
  if (tip && shownFor) {
    // A long press only shows the label; it must not also press the button.
    e.preventDefault();
    setTimeout(hideTip, 1200);
  }
  shownFor = null;
});
document.addEventListener('contextmenu', (e) => {
  if (e.target.closest?.(ICON_SELECTOR) && matchMedia('(hover: none)').matches) e.preventDefault();
});
