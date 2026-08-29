/**
 * The live launch feed, as one React component.
 *
 * Same page as ui/index.html and the same two sources behind it: `/api/feed`
 * fills it with what already happened so it is never blank on open, and the
 * event stream at `/api/live` adds what happens while you watch. This exists for
 * shipping the feed inside a React front end; the desktop app runs the plain
 * version, which has no build step.
 *
 * A launch arrives twice on purpose. The socket sees the mint the instant it is
 * created, which is the moment worth showing and the moment nothing is known
 * yet; the structural read lands a few seconds later, once there are trades to
 * measure. So a row appears saying ANALYSING and then answers itself. That is
 * not a loading state — it is how long the question actually takes.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import './launch-feed.css';

const MAX_ROWS = 60;

/* ── formatting ─────────────────────────────────────────────────────────── */

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

// Every pump.fun mint ends in "pump", so the tail identifies nothing. Keep the
// head, which is the part that differs.
const shortMint = (m) => (m && m.length > 16 ? `${m.slice(0, 8)}…${m.slice(-4)}` : m || '—');

const pct = (v) => (Number.isFinite(v) ? `${v.toFixed(1)}%` : null);

/**
 * The funding read, in one line.
 *
 * "ORGANIC" is the only clean answer, and it means the analyser looked and found
 * no wallets moving together — not that it did not look. When it did find them,
 * the tag says which shape it recognised and the number is the share of the
 * opening money those wallets are.
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

/* ── shared class strings ───────────────────────────────────────────────── */

const EYEBROW = 'text-[0.65rem] font-semibold uppercase leading-none tracking-[0.18em] text-[var(--g3)]';
const NUM = 'tabular-nums [font-feature-settings:"tnum"]';
const COLS =
  'grid-cols-[108px_minmax(200px,1.6fr)_118px_152px_176px_232px] ' +
  'max-[1180px]:grid-cols-[92px_minmax(160px,1.6fr)_100px_132px_190px] ' +
  'max-[820px]:grid-cols-[84px_minmax(0,1fr)_150px]';
const HIDE_GRAPH = 'max-[1180px]:hidden';
const HIDE_NUMS = 'max-[820px]:hidden';
const TAG =
  'inline-flex items-center gap-[7px] whitespace-nowrap rounded border border-[var(--g4)] ' +
  'px-2.5 py-[5px] text-[0.6rem] font-bold uppercase tracking-[0.14em] text-[var(--g3)]';

/* ── the data ───────────────────────────────────────────────────────────── */

/**
 * Everything the page knows: the rows on screen and what they add up to.
 *
 * Counted here rather than derived on render because the telemetry is about the
 * whole session, and rows fall off the bottom of the list long before the
 * session ends.
 */
function useLaunchFeed(api) {
  const [rows, setRows] = useState([]);
  const [link, setLink] = useState({ state: 'starting' });
  const [tally, setTally] = useState({
    logged: 0, seen: 0, resolved: 0, refused: 0, devPctSum: 0, devPctN: 0, passedSol: 0,
  });
  const seen = useRef(new Set());

  /**
   * One launch, from either source.
   *
   * `live` is false for the backfill, and it decides two things: those rows do
   * not slide in (they were already there as far as the reader is concerned),
   * and they are not added to the session count — they are already inside the
   * log count the telemetry starts from, so counting them here counted the same
   * forty launches twice.
   */
  const add = useCallback((v, live) => {
    const mint = v?.mint;
    if (!mint) return;

    setRows((prev) => {
      const at = prev.findIndex((r) => r.mint === mint);
      if (at === -1) {
        // A launch that has already fallen off the bottom must not come back
        // when its read lands, out of order and pretending to be new.
        if (seen.current.has(`${mint}:gone`)) return prev;
        // Newest first, and hold the list to a length a person can read.
        const next = [{ ...v, fresh: live }, ...prev];
        for (const r of next.slice(MAX_ROWS)) seen.current.add(`${r.mint}:gone`);
        return next.slice(0, MAX_ROWS);
      }
      if (prev[at].resolved || !v.resolved) return prev;
      const next = prev.slice();
      next[at] = { ...next[at], ...v, fresh: false };
      return next;
    });

    // Each launch is counted once, whether the row survived in the list or not.
    const first = !seen.current.has(mint);
    if (first) seen.current.add(mint);
    const countable = v.resolved && !seen.current.has(`${mint}:done`);
    if (v.resolved) seen.current.add(`${mint}:done`);

    setTally((t) => {
      const next = { ...t };
      if (first && live) next.seen += 1;
      if (countable) {
        next.resolved += 1;
        if (v.rejected) next.refused += 1;
        else next.passedSol += Number(v.solIn) || 0;
        const dev = Number(v.supply?.creatorPct);
        if (Number.isFinite(dev)) { next.devPctSum += dev; next.devPctN += 1; }
      }
      return next;
    });
  }, []);

  // Backfill first, then the stream, so the list is never blank on open and the
  // two never race to insert the same launch.
  useEffect(() => {
    let stop = false;
    let es;

    (async () => {
      try {
        const [feed, status] = await Promise.all([
          fetch(`${api}/api/feed?limit=40`).then((r) => r.json()),
          fetch(`${api}/api/status`).then((r) => r.json()).catch(() => ({})),
        ]);
        if (stop) return;
        setTally((t) => ({ ...t, logged: Number(status.coins) || 0 }));
        // Oldest first, so each insert leaves the newest on top.
        for (const v of (feed.rows || []).slice().reverse()) add(v, false);
      } catch {
        // No backfill is not a broken page. The stream is the live part.
      }
      if (stop) return;

      es = new EventSource(`${api}/api/live`);
      es.addEventListener('launch', (e) => add({ ...JSON.parse(e.data), resolved: false }, true));
      es.addEventListener('verdict', (e) => add(JSON.parse(e.data), true));
      es.addEventListener('link', (e) => setLink(JSON.parse(e.data)));
      es.onerror = () => setLink((l) => ({ ...l, state: 'reconnecting' }));
    })();

    fetch(`${api}/api/link`).then((r) => r.json()).then((l) => !stop && setLink(l)).catch(() => {});
    return () => { stop = true; es?.close(); };
  }, [api, add]);

  return { rows, link, tally };
}

/** One clock for every row. Sixty rows with their own timers is sixty wakeups a
 *  second to redraw the same string. */
function useNow() {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, []);
  return now;
}

/* ── pieces ─────────────────────────────────────────────────────────────── */

function Dot({ tone = 'idle', className = '' }) {
  const colour = tone === 'live' ? 'bg-[var(--green)]'
    : tone === 'down' ? 'bg-[var(--red)]'
    : 'bg-[var(--g3)]';
  const beat = tone === 'live' || tone === 'wait' ? 'lf-breathe' : '';
  return <span className={`inline-block h-[5px] w-[5px] shrink-0 rounded-full ${colour} ${beat} ${className}`} />;
}

function Metric({ value, unit, label, first }) {
  return (
    <div className={`pt-10 pb-[34px] ${first ? '' : 'border-l border-[var(--g4)] pl-7'}`}>
      <b className={`block text-[2.2rem] font-extrabold leading-none tracking-[-0.04em] text-[var(--g1)] ${NUM}`}>
        {value}
        {unit && <small className="ml-[3px] text-[0.9rem] font-bold tracking-normal text-[var(--g3)]">{unit}</small>}
      </b>
      <span className={`${EYEBROW} mt-[13px] block`}>{label}</span>
    </div>
  );
}

function Decision({ row }) {
  if (!row.resolved) {
    return <span className={TAG}><Dot tone="wait" className="!h-1 !w-1" />Analysing</span>;
  }
  if (row.rejected) {
    // The refusal carries its reason. A filter that throws work away without
    // saying why is indistinguishable from one that is broken. The rule name
    // sits beside the tag rather than under it so the row keeps its height.
    return (
      <>
        {row.refusedOn && (
          <span className="whitespace-nowrap text-right text-[0.6rem] text-[var(--g3)] max-[1180px]:hidden">
            {row.refusedOn}
          </span>
        )}
        <span
          className={`${TAG} border-[rgba(239,68,68,0.32)] bg-[rgba(239,68,68,0.05)] text-[var(--red)]`}
          title={row.blocking?.[0] || ''}
        >
          <span className="h-1 w-1 rounded-full bg-[var(--red)]" />
          Rejected
        </span>
      </>
    );
  }
  // Three answers, not two. Refused is one thing; cleared the refusals but not
  // worth acting on is another, and it is most launches. Colouring those green
  // would spend the only colour on the page on nothing.
  const score = row.score != null ? ` · score ${row.score}` : '';
  return row.eligible ? (
    <span
      className={`${TAG} border-[rgba(34,197,94,0.34)] bg-[rgba(34,197,94,0.06)] text-[var(--green)]`}
      title={`passed the structural checks and cleared the candidate bar${score}`}
    >
      <span className="h-1 w-1 rounded-full bg-[var(--green)]" />
      Immediate_launch
    </span>
  ) : (
    <span className={TAG} title={`nothing refused it, nothing recommends it${score}`}>
      <Dot className="!h-1 !w-1" />
      Passed
    </span>
  );
}

function Row({ row, now }) {
  const [fresh, setFresh] = useState(!!row.fresh);
  useEffect(() => {
    if (!fresh) return undefined;
    const id = setTimeout(() => setFresh(false), 240);
    return () => clearTimeout(id);
  }, [fresh]);

  const supply = row.supply || {};
  const sybil = row.sybil || {};
  const dev = pct(supply.creatorPct);
  const blockPct = Number.isFinite(supply.launchBlockPct) ? supply.launchBlockPct : supply.bundlePct;
  const block = pct(blockPct);
  const blockBad = !!(sybil.bundledLaunch || sybil.overCoordinated || Number(blockPct) >= 35);
  const wallets = Number(supply.launchBlockWallets) || supply.bundleWallets?.length || 0;
  // The launch block is the deployer plus every wallet tied to them. With none,
  // it is the deployer's own number a second time — so it is dimmed, and the
  // rows worth looking at in this column are the ones that are a real block.
  const alone = wallets <= 1;
  const graph = fundingGraph(sybil);

  return (
    <div
      className={`lf-row group relative grid ${COLS} items-center gap-5 border-b border-[var(--g4)] px-[22px] py-[13px] ` +
        `transition-colors duration-200 last:border-b-0 hover:bg-[rgba(255,255,255,0.018)] ` +
        `max-[1180px]:gap-4 max-[820px]:gap-3.5 max-[820px]:px-4 ${fresh ? 'lf-new' : ''}`}
    >
      <div className="min-w-0">
        <div className={`text-[0.75rem] font-semibold text-[var(--g2)] transition-colors group-hover:text-[var(--g1)] ${NUM}`}>
          {since(row.t, now)}
        </div>
        <div className="mt-[3px] font-mono text-[0.6rem] text-[var(--g3)]">{clock(row.t)}</div>
      </div>

      <div className="min-w-0">
        <div className="flex min-w-0 items-baseline">
          <span className="text-[0.82rem] font-bold tracking-[-0.01em] text-[var(--g1)]">
            {(row.symbol || '????').slice(0, 12)}
          </span>
          <span className="ml-2 truncate text-[0.72rem] text-[var(--g2)] transition-colors group-hover:text-[var(--g1)]">{(row.name || '').slice(0, 48)}</span>
        </div>
        <span className="mt-[3px] block truncate font-mono text-[0.62rem] text-[var(--g3)]" title={row.mint}>
          {shortMint(row.mint)}
        </span>
      </div>

      <div className={`min-w-0 text-right ${HIDE_NUMS}`}>
        {dev ? (
          <>
            <div className={`text-[0.82rem] font-semibold ${supply.rejected ? 'text-[var(--red)]' : 'text-[var(--g1)]'} ${NUM}`}>{dev}</div>
            {supply.estimated && <div className="mt-[3px] text-[0.6rem] text-[var(--g3)]">estimated</div>}
          </>
        ) : (
          <div className={`text-[0.82rem] text-[var(--g3)] ${NUM}`}>{row.resolved ? 'unknown' : '—'}</div>
        )}
      </div>

      <div className={`min-w-0 text-right ${HIDE_NUMS}`}>
        {block ? (
          <>
            <div className={`text-[0.82rem] ${NUM} ${blockBad ? 'font-semibold text-[var(--red)]' : alone ? 'text-[var(--g2)]' : 'font-semibold text-[var(--g1)]'}`}>{block}</div>
            {!alone && <div className="mt-[3px] text-[0.6rem] text-[var(--g3)]">{wallets} wallets</div>}
          </>
        ) : (
          <div className={`text-[0.82rem] text-[var(--g3)] ${NUM}`}>{row.resolved ? 'unknown' : '—'}</div>
        )}
      </div>

      <div className={`min-w-0 ${HIDE_GRAPH}`}>
        <div className={`truncate font-mono text-[0.68rem] tracking-[0.04em] ${graph.bad ? 'text-[var(--red)]' : graph.faint ? 'text-[var(--g3)]' : 'text-[var(--g2)]'}`}>
          {row.resolved ? graph.text : '—'}
        </div>
        {row.resolved && sybil.early > 0 && (
          <div className="mt-[3px] text-[0.6rem] text-[var(--g3)]">{sybil.organic} of {sybil.early} independent</div>
        )}
      </div>

      <div className="flex min-w-0 items-center justify-end gap-[11px]">
        <Decision row={row} />
      </div>
    </div>
  );
}

/* ── the page ───────────────────────────────────────────────────────────── */

export default function LaunchFeed({ api = '', onCta }) {
  const { rows, link, tally } = useLaunchFeed(api);
  const now = useNow();
  const glow = useRef(null);

  // The pointer's light, moved on the frame rather than the event.
  useEffect(() => {
    let queued = false;
    let at = { x: 0, y: 0 };
    const move = (e) => {
      at = { x: e.clientX, y: e.clientY };
      if (glow.current) glow.current.dataset.on = 'true';
      if (queued) return;
      queued = true;
      requestAnimationFrame(() => {
        queued = false;
        if (glow.current) glow.current.style.transform = `translate3d(${at.x}px, ${at.y}px, 0)`;
      });
    };
    const leave = () => { if (glow.current) glow.current.dataset.on = 'false'; };
    window.addEventListener('pointermove', move, { passive: true });
    window.addEventListener('pointerleave', leave);
    return () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerleave', leave);
    };
  }, []);

  /**
   * The socket's state, in the nav.
   *
   * This reads the watcher's own view of its Solana connection, not the page's
   * connection to the server — the page's own stream is up whenever the page is,
   * so using it here would report a healthy link through every disconnect.
   */
  const nav = useMemo(() => {
    const up = link.state === 'up' || link.state === 'open';
    const host = link.endpoint ? String(link.endpoint).replace(/^wss?:\/\//, '').split('/')[0] : null;
    const lag = link.lag?.enough ? `${link.lag.p50}ms` : null;
    const mins = link.upSince ? (Date.now() - link.upSince) / 60000 : 0;
    return {
      up,
      tone: up ? 'live' : link.state === 'off' || link.state === 'down' ? 'down' : 'wait',
      endpoint: host ? `${host}${lag ? `: ${lag}` : ''}` : 'no endpoint',
      state: up ? 'WS CONNECTED'
        : link.state === 'off' ? 'WS OFF — NO LISTENER'
        : `WS ${String(link.state || 'unknown').toUpperCase()}`,
      rate: mins > 0.2 && tally.seen ? `${(tally.seen / mins).toFixed(1)} LAUNCHES/MIN` : '— LAUNCHES/MIN',
    };
  }, [link, tally.seen]);

  const metrics = [
    { value: (tally.logged + tally.seen).toLocaleString(), label: 'Launches tracked' },
    { value: tally.resolved ? ((tally.refused / tally.resolved) * 100).toFixed(1) : '—', unit: '%', label: 'Sybil rejection' },
    { value: tally.devPctN ? (tally.devPctSum / tally.devPctN).toFixed(1) : '—', unit: '%', label: 'Avg deployer supply' },
    { value: tally.passedSol ? tally.passedSol.toFixed(tally.passedSol >= 100 ? 0 : 1) : '—', unit: ' SOL', label: 'Passed opening flow' },
  ];

  return (
    <div className="lf-root min-h-screen overflow-x-hidden pt-[54px] text-[13px] leading-normal">
      <div className="lf-grain" aria-hidden />
      <div ref={glow} className="lf-glow" data-on="false" aria-hidden />

      <header className="fixed inset-x-0 top-0 z-50 flex h-[54px] items-center gap-7 border-b border-[var(--g4)] bg-black/70 px-7 backdrop-blur-[14px] backdrop-saturate-150">
        <div className="shrink-0 whitespace-nowrap text-[0.8rem] font-extrabold tracking-[-0.01em] text-[var(--g1)]">
          STS <i className="mx-[2px] not-italic text-[var(--g3)]">//</i> 01
        </div>
        <div className={`${EYEBROW} flex min-w-0 flex-1 items-center gap-4 overflow-hidden max-[820px]:hidden`}>
          <span className="flex items-center gap-2 whitespace-nowrap"><Dot tone={nav.tone} />{nav.endpoint}</span>
          <span className="text-[var(--g4)]">/</span>
          <span className="whitespace-nowrap">{nav.state}</span>
          <span className="text-[var(--g4)]">/</span>
          <span className={`whitespace-nowrap ${NUM}`}>{nav.rate}</span>
        </div>
        <button
          type="button"
          onClick={onCta}
          className="shrink-0 rounded-[5px] bg-white px-4 py-[9px] text-[0.68rem] font-bold uppercase tracking-[0.06em] text-black transition-opacity duration-200 hover:opacity-80"
        >
          Request access
        </button>
      </header>

      <main className="relative z-[1] mx-auto max-w-[1400px] px-7 max-[820px]:px-[18px]">
        <section
          aria-label="Session telemetry"
          className="grid grid-cols-4 border-b border-[var(--g4)] max-[1180px]:grid-cols-2 max-[1180px]:[&>div:nth-child(3)]:border-l-0 max-[1180px]:[&>div:nth-child(3)]:pl-0 max-[1180px]:[&>div:nth-child(n+3)]:border-t max-[1180px]:[&>div:nth-child(n+3)]:border-[var(--g4)]"
        >
          {metrics.map((m, i) => <Metric key={m.label} {...m} first={i === 0} />)}
        </section>

        <div className="mt-[46px] mb-[15px] flex items-baseline justify-between gap-5">
          <div className="flex items-center gap-2.5">
            <Dot tone={nav.up ? 'live' : 'wait'} />
            <span className={EYEBROW}>Live — Solana mainnet · pump.fun</span>
          </div>
          <span className={EYEBROW}>{rows.length} in view</span>
        </div>

        <section
          aria-label="Live launch feed"
          aria-live="polite"
          className="overflow-hidden rounded-md border border-[var(--g4)] bg-[var(--g5)]"
        >
          <div className={`grid ${COLS} items-center gap-5 border-b border-[var(--g4)] bg-white/[0.012] px-[22px] py-3.5 max-[820px]:px-4`}>
            <div className={EYEBROW}>Time</div>
            <div className={EYEBROW}>Token</div>
            <div className={`${EYEBROW} text-right ${HIDE_NUMS}`}>Deployer</div>
            <div className={`${EYEBROW} text-right ${HIDE_NUMS}`}>Launch block</div>
            <div className={`${EYEBROW} ${HIDE_GRAPH}`}>Funding graph</div>
            <div className={`${EYEBROW} text-right`}>Decision</div>
          </div>

          {rows.map((row) => <Row key={row.mint} row={row} now={now} />)}

          {!rows.length && (
            <div className="px-6 py-24 text-center">
              <span className={EYEBROW}>Listening</span>
              <p className="mt-3 text-[0.72rem] text-[var(--g3)]">
                The first launch will appear here the moment it lands on chain.
              </p>
            </div>
          )}
        </section>

        <p className="mt-4 mb-16 text-[0.65rem] leading-[1.7] text-[var(--g3)]">
          Deployer and launch-block shares are measured over the first seconds of trading, then held.
          A launch is refused on structure alone — supply held by the deployer, the share of the opening
          money that moved as one operator, or selling that invalidates the opening.
        </p>
      </main>
    </div>
  );
}
