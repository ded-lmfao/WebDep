/**
 * WebDep live demo: guided tour.
 * Walks a first-time visitor through every part of the panel: it opens the right page, spotlights
 * the element and explains it. Runs only in the full demo (not the `?embed=1` inline preview).
 * Restart it any time from the "Tour" button in the bottom-right corner.
 */
(() => {
  if (new URLSearchParams(window.location.search).has('embed')) return;

  const DONE_KEY = 'webdep.demoTourDone';

  const STEPS = [
    {
      title: 'Welcome to WebDep 👋',
      body: 'This is the real WebDep panel running on a demo backend with two servers already connected. The tour takes about two minutes and shows you every part of it. Nothing you do here touches a real server.',
    },
    {
      view: 'overview',
      target: '.sidebar',
      mobileTarget: '#mobile-bottom-nav',
      title: 'Navigation',
      body: 'Everything lives in this sidebar: <b>Overview</b> for the whole fleet, <b>Websites</b>, <b>Services</b> and <b>Databases</b> for your workloads, <b>Nodes</b> and <b>Modules</b> for your servers, and <b>Platform Settings</b>. The numbers show how many of each you have.',
    },
    {
      view: 'overview',
      target: '.resource-summary-bar',
      title: 'Where your resources go',
      body: 'A one-line summary of memory and CPU: what the WebDep engine itself uses (about 20 MB), what your apps use, what the operating system takes, and how much memory is still free.',
    },
    {
      view: 'overview',
      target: '.resource-breakdown-card',
      title: 'Every app at a glance',
      body: 'Each website, service and database with its live CPU, memory and disk use, its port and status, and the server (node) it runs on. Use the pills at the top to filter by type, and click a row to open that app.',
    },
    {
      view: 'projects',
      target: '#projects-cards-container',
      title: 'Websites',
      body: 'Your web apps: Next.js, Express, FastAPI, Laravel, static sites and more. Each card shows the domain, the commit that is live, the node it runs on and quick Start/Stop buttons.',
    },
    {
      view: 'projects',
      target: '#add-website-header-btn',
      title: 'Adding a website',
      body: 'Point WebDep at a GitHub repository, pick a branch and (optionally) the install, build and start commands. It picks a free port, chooses a server for you and deploys. Push to the branch later and it redeploys automatically.',
    },
    {
      workspace: 1,
      target: '#main-pipeline-track',
      fallbackTarget: '#ws-tabs-bar',
      title: 'Inside a website',
      body: 'Opening an app shows its live deployment pipeline: Pending → Fetching → Pulling → Installing → Building → Deploying → Health check. A failed health check never replaces the running version.',
    },
    {
      workspace: 1,
      target: '#ws-tabs-bar',
      title: 'App tabs',
      body: '<b>Commits</b> shows Git history, <b>Logs</b> streams output live, <b>Files</b> is an in-browser editor, <b>Environment</b> holds encrypted .env secrets, and <b>Releases</b> keeps past builds and backups for one-click rollback.',
    },
    {
      workspace: 1,
      target: '#ws-project-actions',
      title: 'App controls',
      body: 'Start, stop or restart the app, trigger a deploy, or use <b>Move Node</b> to shift it to another server with zero manual copying.',
    },
    {
      view: 'services',
      target: '#services-cards-container',
      title: 'Services',
      body: 'Background workers that run 24/7: Discord bots, queue workers, Lavalink, cron-style jobs. WebDep keeps them running, restarts them if they crash and shows their PID and logs.',
    },
    {
      view: 'databases',
      target: '#databases-cards-container',
      title: 'Databases',
      body: 'Managed PostgreSQL, each in its own isolated cluster with its own password. Choose who may connect (local apps only, the private mesh, or specific IPs) and link it to an app to get <code>DATABASE_URL</code> filled in for you.',
    },
    {
      view: 'nodes',
      target: '#nodes-cards-container',
      title: 'Nodes (your servers)',
      body: 'Every VPS in your fleet with live CPU, memory, disk and network. <b>Freeze</b> stops new deployments landing on a server, <b>Evacuate</b> moves its apps elsewhere, and <b>Rename</b> gives it a friendly name. All servers are linked to each other in a private WireGuard mesh.',
    },
    {
      view: 'nodes',
      target: '#add-node-header-btn',
      title: 'Adding a server',
      body: 'Click <b>Add Node</b> to get a single command with a one-time token. Paste it on the new VPS and it joins the fleet in about a minute.',
    },
    {
      view: 'modules',
      target: '#modules-matrix-container',
      title: 'Modules (runtimes)',
      body: 'Which languages and tools each server has: Git, Node.js, Python, Java, Go, Rust, PHP and more. Install or remove them per server in one click. Try it, it works in the demo.',
    },
    {
      view: 'settings',
      target: '#settings-users-card',
      title: 'Users & access',
      body: 'Invite teammates and give each one an access level (Viewer, Developer, Full access or Custom) and limit them to specific websites and services.',
    },
    {
      view: 'settings',
      target: '#settings-security-card',
      title: 'Security',
      body: 'Sign in with passkeys (Touch ID, Windows Hello), turn on two-factor authentication and see or end your active sessions.',
    },
    {
      view: 'settings',
      target: '#settings-theme-card',
      title: 'Make it yours',
      body: 'Pick one of 15 themes for the whole panel, from dark to light.',
    },
    {
      view: 'overview',
      title: 'You are all set 🎉',
      body: 'Explore freely: start and stop apps, install modules, rename nodes, add users. When you are ready, install WebDep on your own server with one command. You can restart this tour from the <b>Tour</b> button in the bottom-right corner.',
      last: true,
    },
  ];

  // ---------------------------------------------------------------------------
  // Styles
  // ---------------------------------------------------------------------------
  const css = `
  .wd-tour-blocker{position:fixed;inset:0;z-index:9997;background:transparent;cursor:default;touch-action:none}
  .wd-tour-spot{position:fixed;z-index:9998;border-radius:14px;pointer-events:none;
    box-shadow:0 0 0 2px var(--accent,#4f8ef7),0 0 0 9999px rgba(3,6,14,.62);
    transition:top .38s cubic-bezier(.22,1,.36,1),left .38s cubic-bezier(.22,1,.36,1),width .38s cubic-bezier(.22,1,.36,1),height .38s cubic-bezier(.22,1,.36,1),opacity .25s}
  .wd-tour-spot.wd-center{width:0!important;height:0!important;top:50%!important;left:50%!important;box-shadow:0 0 0 9999px rgba(3,6,14,.62)}
  .wd-tour-card{position:fixed;z-index:9999;width:min(360px,calc(100vw - 32px));background:var(--bg-card,#111829);
    color:var(--text-main,#eef1f7);border:1px solid var(--border-medium,rgba(255,255,255,.12));border-radius:16px;
    padding:18px 18px 14px;box-shadow:0 24px 60px rgba(0,0,0,.5);font-size:13.5px;line-height:1.55;
    transition:top .38s cubic-bezier(.22,1,.36,1),left .38s cubic-bezier(.22,1,.36,1),opacity .2s,transform .2s}
  .wd-tour-card.wd-enter{opacity:0;transform:translateY(6px)}
  .wd-tour-card h4{margin:0 0 6px;font-size:15px;font-weight:600;color:var(--text-main,#eef1f7)}
  .wd-tour-card p{margin:0;color:var(--text-muted,#a3a9b8)}
  .wd-tour-card p b{color:var(--text-main,#eef1f7);font-weight:600}
  .wd-tour-card p code{font-size:12px;padding:1px 5px;border-radius:5px;background:var(--bg-input,rgba(255,255,255,.06))}
  .wd-tour-progress{height:3px;border-radius:3px;background:var(--bg-input,rgba(255,255,255,.08));margin:14px 0 12px;overflow:hidden}
  .wd-tour-progress span{display:block;height:100%;background:var(--accent,#4f8ef7);transition:width .35s}
  .wd-tour-foot{display:flex;align-items:center;gap:8px}
  .wd-tour-foot .wd-count{font-size:11.5px;color:var(--text-dim,#7a8296);margin-right:auto}
  .wd-tour-btn{border:1px solid var(--border-medium,rgba(255,255,255,.14));background:transparent;color:var(--text-main,#eef1f7);
    border-radius:9px;padding:6px 12px;font-size:12.5px;font-weight:500;cursor:pointer}
  .wd-tour-btn:hover{background:var(--bg-card-hover,rgba(255,255,255,.05))}
  .wd-tour-btn.wd-primary{background:var(--accent,#4f8ef7);border-color:transparent;color:#fff}
  .wd-tour-btn.wd-primary:hover{filter:brightness(1.08)}
  .wd-tour-skip{background:none;border:0;color:var(--text-dim,#7a8296);font-size:12px;cursor:pointer;padding:6px 4px}
  .wd-tour-skip:hover{color:var(--text-main,#eef1f7)}
  .wd-tour-launch{position:fixed;right:136px;bottom:16px;z-index:9000;display:inline-flex;align-items:center;gap:6px;
    border-radius:999px;padding:8px 14px;font-size:12.5px;font-weight:500;cursor:pointer;
    background:var(--bg-card,#111829);color:var(--text-main,#eef1f7);border:1px solid var(--border-medium,rgba(255,255,255,.14));
    box-shadow:0 10px 28px rgba(0,0,0,.45)}
  .wd-tour-launch:hover{border-color:var(--accent,#4f8ef7)}
  @media (max-width:900px){
    .wd-tour-card{left:16px!important;right:16px;width:auto;top:auto!important;bottom:84px}
    .wd-tour-launch{bottom:80px}
  }`;

  // ---------------------------------------------------------------------------
  // Tour engine
  // ---------------------------------------------------------------------------
  let index = -1;
  let blocker = null;
  let spot = null;
  let card = null;
  let currentTarget = null;
  let token = 0;

  const isVisible = (el) => {
    if (!el) return false;
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0 && getComputedStyle(el).visibility !== 'hidden';
  };
  const isNarrow = () => window.matchMedia('(max-width: 900px)').matches;

  function waitFor(selector, timeoutMs = 2500) {
    return new Promise((resolve) => {
      const started = Date.now();
      const tick = () => {
        const el = selector ? document.querySelector(selector) : null;
        if (el && isVisible(el)) return resolve(el);
        if (!selector || Date.now() - started > timeoutMs) return resolve(null);
        setTimeout(tick, 80);
      };
      tick();
    });
  }

  function ensureUi() {
    if (spot) return;
    spot = document.createElement('div');
    spot.className = 'wd-tour-spot wd-center';
    card = document.createElement('div');
    card.className = 'wd-tour-card wd-enter';
    card.setAttribute('role', 'dialog');
    card.setAttribute('aria-modal', 'true');
    card.setAttribute('aria-live', 'polite');
    // Invisible full-screen layer under the card: while the tour runs, only the card's
    // Back / Skip / Next buttons can be used. Clicks, taps and wheel scrolling elsewhere are swallowed.
    blocker = document.createElement('div');
    blocker.className = 'wd-tour-blocker';
    for (const type of ['click', 'mousedown', 'pointerdown', 'touchstart', 'contextmenu', 'dblclick']) {
      blocker.addEventListener(type, swallow, { passive: false });
    }
    blocker.addEventListener('wheel', swallow, { passive: false });
    blocker.addEventListener('touchmove', swallow, { passive: false });
    document.body.append(blocker, spot, card);
    document.activeElement?.blur?.();
    window.addEventListener('resize', place);
    window.addEventListener('scroll', place, true);
    document.addEventListener('keydown', onKey, true);
  }

  function swallow(e) {
    e.preventDefault();
    e.stopPropagation();
  }

  function teardown() {
    token++;
    blocker?.remove();
    spot?.remove();
    card?.remove();
    blocker = spot = card = currentTarget = null;
    index = -1;
    window.removeEventListener('resize', place);
    window.removeEventListener('scroll', place, true);
    document.removeEventListener('keydown', onKey, true);
  }

  function finish() {
    try { localStorage.setItem(DONE_KEY, '1'); } catch (_) { /* storage blocked */ }
    teardown();
  }

  function onKey(e) {
    if (e.key === 'Escape') {
      swallow(e);
      finish();
      return;
    }
    if (e.key === 'ArrowRight' || e.key === 'ArrowLeft') {
      swallow(e);
      go(index + (e.key === 'ArrowRight' ? 1 : -1));
      return;
    }
    // Keep keyboard focus inside the tour card: Tab cycles its buttons only.
    if (e.key === 'Tab' && card) {
      const buttons = [...card.querySelectorAll('button')];
      if (!buttons.length) return;
      swallow(e);
      const i = buttons.indexOf(document.activeElement);
      const next = e.shiftKey ? (i <= 0 ? buttons.length - 1 : i - 1) : (i + 1) % buttons.length;
      buttons[next].focus();
      return;
    }
    // Any other key aimed at the page (Enter on a focused panel button, Space to scroll, typing)
    // is blocked; keys pressed on the card's own buttons still work.
    if (!card?.contains(document.activeElement)) swallow(e);
  }

  function place() {
    if (!spot || !card) return;
    const el = currentTarget;
    if (!el || !isVisible(el)) {
      spot.classList.add('wd-center');
      if (!isNarrow()) {
        card.style.left = `${Math.max(16, (window.innerWidth - card.offsetWidth) / 2)}px`;
        card.style.top = `${Math.max(16, (window.innerHeight - card.offsetHeight) / 2)}px`;
      }
      return;
    }
    spot.classList.remove('wd-center');
    const r = el.getBoundingClientRect();
    const pad = 6;
    const top = Math.max(4, r.top - pad);
    const left = Math.max(4, r.left - pad);
    const bottom = Math.min(window.innerHeight - 4, r.bottom + pad);
    const right = Math.min(window.innerWidth - 4, r.right + pad);
    Object.assign(spot.style, { top: `${top}px`, left: `${left}px`, width: `${right - left}px`, height: `${bottom - top}px` });

    if (isNarrow()) return; // the card is a bottom sheet on small screens
    const cw = card.offsetWidth;
    const ch = card.offsetHeight;
    const gap = 14;
    let cx;
    let cy;
    if (window.innerWidth - right > cw + gap * 2) {
      cx = right + gap; // right of the target
      cy = Math.min(Math.max(16, top), window.innerHeight - ch - 16);
    } else if (window.innerHeight - bottom > ch + gap) {
      cx = Math.min(Math.max(16, left), window.innerWidth - cw - 16); // below
      cy = bottom + gap;
    } else if (top > ch + gap) {
      cx = Math.min(Math.max(16, left), window.innerWidth - cw - 16); // above
      cy = top - ch - gap;
    } else {
      cx = Math.max(16, (window.innerWidth - cw) / 2); // overlay the middle for very tall targets
      cy = window.innerHeight - ch - 24;
    }
    card.style.left = `${cx}px`;
    card.style.top = `${cy}px`;
  }

  function render(step) {
    const n = STEPS.length;
    const pct = Math.round(((index + 1) / n) * 100);
    card.innerHTML = `
      <h4>${step.title}</h4>
      <p>${step.body}</p>
      <div class="wd-tour-progress"><span style="width:${pct}%"></span></div>
      <div class="wd-tour-foot">
        <span class="wd-count">${index + 1} / ${n}</span>
        ${step.last ? '' : '<button type="button" class="wd-tour-skip" data-act="skip">Skip tour</button>'}
        ${index > 0 ? '<button type="button" class="wd-tour-btn" data-act="back">Back</button>' : ''}
        <button type="button" class="wd-tour-btn wd-primary" data-act="next">${step.last ? 'Start exploring' : index === 0 ? 'Start the tour' : 'Next'}</button>
      </div>`;
    card.querySelector('[data-act="next"]').onclick = () => (step.last ? finish() : go(index + 1));
    card.querySelector('[data-act="back"]')?.addEventListener('click', () => go(index - 1));
    card.querySelector('[data-act="skip"]')?.addEventListener('click', finish);
    card.querySelector('[data-act="next"]').focus({ preventScroll: true });
  }

  async function go(next) {
    if (next < 0 || next >= STEPS.length) return;
    ensureUi();
    index = next;
    const my = ++token;
    const step = STEPS[next];

    if (step.workspace && typeof window.openWebsiteWorkspace === 'function') {
      window.openWebsiteWorkspace(step.workspace, 'deployments');
    } else if (step.view && typeof window.navigateTo === 'function' && window.state?.currentView !== step.view) {
      window.navigateTo(step.view);
    }

    card.classList.add('wd-enter');
    const selector = isNarrow() && step.mobileTarget ? step.mobileTarget : step.target;
    let el = await waitFor(selector);
    if (!el && step.fallbackTarget) el = await waitFor(step.fallbackTarget, 800);
    if (my !== token) return;

    if (el) el.scrollIntoView({ block: 'center', behavior: 'smooth' });
    currentTarget = el;
    render(step);
    // Let smooth scrolling settle before measuring.
    setTimeout(() => {
      if (my !== token) return;
      place();
      card.classList.remove('wd-enter');
    }, el ? 380 : 30);
  }

  function addLauncher() {
    if (document.querySelector('.wd-tour-launch')) return;
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'wd-tour-launch';
    btn.innerHTML = '<span aria-hidden="true">✦</span> Tour';
    btn.title = 'Take the guided tour';
    btn.onclick = () => go(0);
    document.body.appendChild(btn);
  }

  function boot() {
    const style = document.createElement('style');
    style.textContent = css;
    document.head.appendChild(style);

    // Start once the visitor is signed in and the dashboard is showing.
    const started = Date.now();
    const waitForApp = () => {
      if (document.body.classList.contains('auth-verified') && typeof window.navigateTo === 'function') {
        addLauncher();
        let done = false;
        try { done = localStorage.getItem(DONE_KEY) === '1'; } catch (_) { /* storage blocked */ }
        if (!done) setTimeout(() => go(0), 900);
        return;
      }
      if (Date.now() - started < 10 * 60 * 1000) setTimeout(waitForApp, 300);
    };
    waitForApp();
  }

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', boot);
  else boot();
})();
