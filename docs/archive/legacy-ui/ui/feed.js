// The feed.
//
// Two sources and one list. `/api/feed` fills the page with the launches that
// already happened so it is never blank on open, and the event stream adds the
// ones that happen while you watch. Both hand over the same shape, so a row does
// not know or care which one it came from.
//
// A launch arrives twice on purpose. The socket sees the mint the instant it is
// created, which is the moment worth showing and the moment nothing is known
// yet; the structural read lands a few seconds later, once there are trades to
// measure. So a row appears saying ANALYSING and then answers itself. That is
// not a loading state — it is how long the question actually takes.

(() => {
  'use strict';

  const MAX_ROWS = 200;

  const $ = (id) => document.getElementById(id);
  const rowsEl = $('rows');
  const emptyEl = $('empty');

  /** mint -> { el, t, resolved } */
  const rows = new Map();
  // Mints that have already fallen off the bottom. Without this, a verdict
  // arriving for a trimmed launch would put it back at the top, out of order and
  // pretending to be new.
  const dropped = new Set();

  // What the telemetry bar is counting. Everything here is this session plus
  // the size of the log on disk; nothing is carried over or assumed.
  const tally = {
    logged: 0,      // launches already in the log when the page opened
    seen: 0,        // launches seen since
    resolved: 0,    // launches the structural read has answered
    refused: 0,
    devPctSum: 0,
    devPctN: 0,
    passedSol: 0,
  };

  /* ── formatting ──────────────────────────────────────────────────────── */

  const esc = (s) =>
    String(s ?? '').replace(/[&<>"']/g, (c) => (
      { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]
    ));

  const pad = (n) => String(n).padStart(2, '0');

  /** How long ago, in the shortest form that is still exact enough to act on. */
  function since(t, now) {
    const s = Math.max(0, Math.round((now - t) / 1000));
    if (s < 60) return `${s}s ago`;
    if (s < 3600) return `${Math.floor(s / 60)}m ${pad(s % 60)}s`;
    const h = Math.floor(s / 3600);
    return h < 24 ? `${h}h ${pad(Math.floor((s % 3600) / 60))}m` : `${Math.floor(h / 24)}d`;
  }

  const clock = (t) => {
    const d = new Date(t);
    return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
  };

  const shortMint = (m) => (m && m.length > 16 ? `${m.slice(0, 8)}…${m.slice(-4)}` : m || '—');

  const pct = (v) => (Number.isFinite(v) ? `${v.toFixed(1)}%` : null);

  /**
   * The funding read, in one line.
   *
   * "ORGANIC" is the only clean answer, and it means the analyser looked and
   * found no wallets moving together — not that it did not look. When it did
   * find them, the tag says which shape it recognised and the number is the
   * share of the opening money those wallets are.
   */
  function fundingGraph(sybil) {
    if (!sybil || !sybil.early) return { text: '—', bad: false, faint: true };
    if (!sybil.clusters) return { text: 'ORGANIC', bad: false, faint: false };
    const share = Number(sybil.clusterShare) || 0;
    const tag = sybil.sharedFunder ? 'SHARED_FUNDER'
      : sybil.sameInstant ? 'SAME_INSTANT'
      : sybil.identicalSizing ? 'IDENTICAL_SIZE'
      : 'CLUSTERED';
    return {
      text: `${tag}: ${share.toFixed(2)}`,
      bad: !!(sybil.bundledLaunch || sybil.overCoordinated),
      faint: false,
    };
  }

  /* ── rows ────────────────────────────────────────────────────────────── */

  function skeleton(l) {
    const el = document.createElement('div');
    el.className = 'row entry is-new';
    el.dataset.mint = l.mint;
    el.innerHTML = `
      <div class="cell">
        <div class="c-age num lift" data-age>—</div>
        <div class="c-clock mono">${clock(l.t)}</div>
      </div>
      <div class="cell">
        <div class="c-line">
          <span class="c-tick">${esc((l.symbol || '????').slice(0, 12))}</span>
          <span class="c-name lift">${esc((l.name || '').slice(0, 48))}</span>
        </div>
        <span class="c-id mono" title="${esc(l.mint)}">${esc(shortMint(l.mint))}</span>
      </div>
      <div class="cell c-dev-cell" data-dev>
        <div class="c-val none num">—</div>
      </div>
      <div class="cell c-block-cell" data-block>
        <div class="c-val none num">—</div>
      </div>
      <div class="cell c-graph-cell" data-graph>
        <div class="c-graph" style="color:var(--g3)">—</div>
      </div>
      <div class="cell right" data-decision>
        <span class="tag"><span class="dot wait"></span>Analysing</span>
      </div>`;
    // The animation only belongs to the row's first quarter second. Left on, it
    // replays every time the browser recalculates style on a hovered row.
    setTimeout(() => el.classList.remove('is-new'), 240);
    return el;
  }

  /** Fill in everything the structural read answered. */
  function resolve(el, v) {
    const supply = v.supply || {};
    const sybil = v.sybil || {};

    const dev = pct(supply.creatorPct);
    el.querySelector('[data-dev]').innerHTML = dev
      ? `<div class="c-val num ${supply.rejected ? 'bad' : ''}">${dev}</div>` +
        (supply.estimated ? '<div class="c-sub">estimated</div>' : '')
      : '<div class="c-val none num">unknown</div>';

    // The launch-block figure is the deployer plus every wallet the analyser
    // tied to them, because a bundle split across ten addresses is one position.
    const blockPct = Number.isFinite(supply.launchBlockPct) ? supply.launchBlockPct : supply.bundlePct;
    const block = pct(blockPct);
    const blockBad = !!(sybil.bundledLaunch || sybil.overCoordinated || (Number(blockPct) >= 35));
    const wallets = Number(supply.launchBlockWallets) || (supply.bundleWallets ? supply.bundleWallets.length : 0);
    const alone = wallets <= 1;
    el.querySelector('[data-block]').innerHTML = block
      ? `<div class="c-val num ${blockBad ? 'bad' : alone ? 'quiet' : ''}">${block}</div>` +
        (alone ? '' : `<div class="c-sub">${wallets} wallets</div>`)
      : '<div class="c-val none num">unknown</div>';

    const g = fundingGraph(sybil);
    el.querySelector('[data-graph]').innerHTML =
      `<div class="c-graph ${g.bad ? 'bad' : ''}"${g.faint ? ' style="color:var(--g3)"' : ''}>${esc(g.text)}</div>` +
      (sybil.early ? `<div class="c-sub">${sybil.organic} of ${sybil.early} independent</div>` : '');

    // Three answers, not two. Refused is one thing; cleared the refusals but
    // not worth acting on is another, and it is most launches. Colouring those
    // green would spend the only colour on the page on nothing.
    //
    // The refusal carries its reason on hover. A filter that throws work away
    // without saying why is indistinguishable from one that is broken.
    const why = v.blocking && v.blocking.length ? v.blocking[0] : '';
    const score = v.score != null ? v.score : 0;
    const sybil = v.sybil || {};

    let verdict, verdictClass, verdictTitle;
    if (v.rejected) {
      verdict = 'NOT BUY';
      verdictClass = 'reject';
      verdictTitle = v.refusedOn
        ? `${v.refusedOn} — ${esc(why)}`
        : esc(why) || 'structural refusal';
    } else if (v.eligible && score >= 60) {
      verdict = 'BUY';
      verdictClass = 'buy';
      verdictTitle = `score ${score} · strong fundamentals, passed all checks`;
    } else if (v.eligible || score >= 40) {
      verdict = 'WATCH';
      verdictClass = 'watch';
      verdictTitle = `score ${score} — passed structure but not strong enough to buy`;
    } else {
      verdict = 'NOT BUY';
      verdictClass = 'reject';
      verdictTitle = `score ${score} — too weak to act on`;
    }

    el.querySelector('[data-decision]').innerHTML =
      (v.refusedOn && v.rejected ? `<span class="c-sub reason">${esc(v.refusedOn)}</span>` : '')
      + `<span class="tag ${verdictClass}" title="${esc(verdictTitle)}"><span class="dot"></span>${verdict}</span>`;
  }

  /**
   * A launch has been seen. Put it at the top, or find the row it belongs to.
   *
   * `count` is false for the backfill: those launches are already inside the
   * log count the telemetry starts from, and adding them to the session count as
   * well was counting the same forty launches twice.
   */
  function launch(l, count = true) {
    if (!l || !l.mint || dropped.has(l.mint)) return;
    let entry = rows.get(l.mint);
    if (!entry) {
      const el = skeleton(l);
      rowsEl.prepend(el);
      entry = { el, t: l.t || Date.now(), resolved: false };
      rows.set(l.mint, entry);
      if (count) tally.seen++;
      trim();
      emptyEl.hidden = true;
      paintAges();
      paintTelemetry();
    }
    return entry;
  }

  /** The structural read for a launch, whether or not its row exists yet. */
  function verdict(v, count = true) {
    if (!v || !v.mint || dropped.has(v.mint)) return;
    const entry = launch(v, count) || rows.get(v.mint);
    if (!entry || entry.resolved) return;
    entry.resolved = true;
    resolve(entry.el, v);

    tally.resolved++;
    if (v.rejected) tally.refused++;
    else tally.passedSol += Number(v.solIn) || 0;
    const dev = Number(v.supply?.creatorPct);
    if (Number.isFinite(dev)) { tally.devPctSum += dev; tally.devPctN++; }
    paintTelemetry();
  }

  /** Hold the list to a length a person can actually read. */
  function trim() {
    while (rowsEl.children.length > MAX_ROWS) {
      const last = rowsEl.lastElementChild;
      rows.delete(last.dataset.mint);
      dropped.add(last.dataset.mint);
      last.remove();
    }
    $('stream-count').textContent = `${rowsEl.children.length} in view`;
  }

  /* ── painting ────────────────────────────────────────────────────────── */

  // One clock for every row. Sixty rows with their own timers is sixty wakeups
  // a second to redraw the same string.
  function paintAges() {
    const now = Date.now();
    for (const { el, t } of rows.values()) {
      const age = el.querySelector('[data-age]');
      if (age) age.textContent = since(t, now);
    }
  }
  setInterval(paintAges, 1000);

  function paintTelemetry() {
    $('m-tracked').textContent = (tally.logged + tally.seen).toLocaleString();

    $('m-reject').innerHTML = tally.resolved
      ? `${((tally.refused / tally.resolved) * 100).toFixed(1)}<small>%</small>`
      : '—<small>%</small>';

    $('m-dev').innerHTML = tally.devPctN
      ? `${(tally.devPctSum / tally.devPctN).toFixed(1)}<small>%</small>`
      : '—<small>%</small>';

    $('m-vol').innerHTML = tally.passedSol
      ? `${tally.passedSol.toFixed(tally.passedSol >= 100 ? 0 : 1)}<small> SOL</small>`
      : '—<small> SOL</small>';
  }

  /**
   * The socket's state, in the nav.
   *
   * This reads the watcher's own view of its Solana connection, not the page's
   * connection to this server — the page's own stream is up whenever the page
   * is, so using it here would report a healthy link through every disconnect.
   */
  function paintLink(l) {
    const dot = $('link-dot');
    const host = l.endpoint ? String(l.endpoint).replace(/^wss?:\/\//, '').split('/')[0] : null;
    const lag = l.lag && l.lag.enough ? `${l.lag.p50}ms` : null;
    const up = l.state === 'up' || l.state === 'open';

    dot.className = `dot ${up ? 'live' : l.state === 'off' || l.state === 'down' ? 'down' : 'wait'}`;
    $('link-endpoint').innerHTML =
      `<span class="dot ${dot.className.replace('dot ', '')}" id="link-dot"></span> ` +
      esc(host ? `${host}${lag ? `: ${lag}` : ''}` : 'no endpoint');
    $('link-state').textContent =
      up ? 'WS CONNECTED' : l.state === 'off' ? 'WS OFF — NO LISTENER' : `WS ${String(l.state || 'unknown').toUpperCase()}`;
    $('feed-dot').className = `dot ${up ? 'live' : 'wait'}`;

    // Launches per minute, measured over how long the socket has been up. The
    // rate is what tells you the feed is alive when nothing is passing.
    const mins = l.upSince ? (Date.now() - l.upSince) / 60000 : 0;
    $('link-rate').textContent = mins > 0.2 && tally.seen
      ? `${(tally.seen / mins).toFixed(1)} LAUNCHES/MIN`
      : '— LAUNCHES/MIN';
  }

  /* ── sources ─────────────────────────────────────────────────────────── */

  async function backfill() {
    try {
      const [feed, status] = await Promise.all([
        fetch('/api/feed?limit=200').then((r) => r.json()),
        fetch('/api/status').then((r) => r.json()).catch(() => ({})),
      ]);
      tally.logged = Number(status.coins) || 0;
      // Oldest first, so prepending each one leaves the newest on top.
      for (const v of (feed.rows || []).slice().reverse()) {
        launch(v, false);
        if (v.resolved) verdict(v, false);
      }
      if (!rowsEl.children.length) emptyEl.hidden = false;
      paintTelemetry();
    } catch {
      // No backfill is not a broken page. The stream is the live part.
    }
  }

  function stream() {
    const es = new EventSource('/api/live');
    es.addEventListener('launch', (e) => launch(JSON.parse(e.data)));
    es.addEventListener('verdict', (e) => verdict(JSON.parse(e.data)));
    es.addEventListener('link', (e) => paintLink(JSON.parse(e.data)));
    es.onerror = () => {
      $('link-dot').className = 'dot down';
      $('feed-dot').className = 'dot down';
      $('link-state').textContent = 'STREAM RECONNECTING';
    };
  }

  /* ── the pointer's light ─────────────────────────────────────────────── */

  const glow = $('glow');
  let queued = false;
  let at = { x: 0, y: 0 };
  window.addEventListener('pointermove', (e) => {
    at = { x: e.clientX, y: e.clientY };
    glow.classList.add('on');
    if (queued) return;
    queued = true;
    requestAnimationFrame(() => {
      queued = false;
      glow.style.transform = `translate3d(${at.x}px, ${at.y}px, 0)`;
    });
  }, { passive: true });
  window.addEventListener('pointerleave', () => glow.classList.remove('on'));

  /* ── double-click a row → Axiom Trade ──────────────────────────────── */

  const AXIOM_URL = 'https://axiom.trade/t/';

  rowsEl.addEventListener('dblclick', (e) => {
    const row = e.target.closest('.row');
    if (!row || !row.dataset.mint) return;
    window.open(AXIOM_URL + row.dataset.mint, '_blank', 'noopener');
  });

  fetch('/api/link').then((r) => r.json()).then(paintLink).catch(() => {});
  backfill().then(stream);
})();
