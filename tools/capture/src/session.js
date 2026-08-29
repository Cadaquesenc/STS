// What a run knows about itself.
//
// Everything here exists because of one class of mistake: a number that was
// decided by the recorder being read later as a fact about the market.
//
//   • `outcome.follow` was the literal 60 on all 8,881 recorded rows, including
//     the ~14% of coins cut off when the listener stopped. A truncated
//     observation was indistinguishable from a complete one, so every
//     expectancy figure downstream quietly mixed the two. `closeFacts` replaces
//     that constant with what actually happened.
//   • Files were named by calendar day and split at UTC midnight, so one
//     fifteen-hour run became "2026-08-20" and "2026-08-21" — and six analyses
//     treated the second half of a run as an independent holdout day. A session
//     id and a session-named file make that a lookup instead of a forensic
//     exercise.
//   • Uptime was never written down, only inferred from how far apart launches
//     happened to fall. `heartbeats` and `sessions` turn it into a measurement.
//
// Pure functions, no I/O, no clock of their own — so every one of them can be
// tested without a socket, a disk or a wait.

/**
 * The recorded shape, by number.
 *
 * A version that does not move is worse than no version at all: it is a field
 * that reads perfectly, never varies, and cannot show its own failure — the
 * exact defect every other check in this directory exists to catch. It sat at 2
 * through five commits that each changed what a record carries, so two files
 * both stamped `v: 2` could hold entirely different shapes and nothing on
 * either one said so.
 *
 * **Bump this in the same commit as any change to what is written.** The
 * changelog below is the whole contract: a reader with a file and this list can
 * say what is on the row without inspecting which fields happen to be present,
 * which is the guessing game the version exists to end.
 *
 *   1 — the original dated capture. `coins-YYYY-MM-DD.jsonl`, no `sid`, no
 *       session rows, `outcome.follow` a constant 60, no launch `curve`, no
 *       `who[]`. Nothing was ever stamped with this number; it is the name for
 *       the recorded corpus, which carries no `v` at all.
 *   2 — stamped but never distinguished from 1, and never moved while the shape
 *       changed underneath it. Treated here as "some session-era shape".
 *   3 — everything the 2026-08-27 night added, all of it additive:
 *         • `outcome.observedSec` / `complete` / `stopReason` / `gapSec`
 *           replacing the constant `follow`
 *         • `sid` and `seq` on every row; `start`/`tick`/`gap`/`failagg`/`stop`
 *           session rows in the coins file
 *         • `slot`, `sig`, `si`, `connectedForSec`, and `who[].slotsAfter`
 *         • `market.candles[].vsol` / `vtok` / `rsol` / `rtok`
 *         • `outcome.curveAtEntry`, `feeBps`, `zeroFee[]`, `zeroFeeTrades`,
 *           `curveSuspect`, `feeSol`, `sells[]`, `creatorSellAtSec`
 *         • `whoCapped`, `highsCapped`, `lowsCapped`, `sellsCapped`,
 *           `zeroFeeCapped`, all five written on every row rather than only
 *           when true — so absence is a defect and not a synonym for `false`
 *         • `v` itself on the coin, track and failure rows, not only in the
 *           session header
 */
export const SCHEMA = 3;

/**
 * Every schema this build knows how to read.
 *
 * `capture check` refuses a file stamped with anything else rather than
 * grading it, because a checker that silently passes a shape it was not written
 * for is reporting its own ignorance as a clean bill of health. A file with no
 * `v` at all is not unknown — it is the recorded corpus, schema 1 by
 * definition, and it still reads.
 */
export const KNOWN_SCHEMAS = new Set([1, 2, SCHEMA]);

/**
 * What this build can say about a `v` it has met.
 *
 * @returns 'legacy' when there is no version (the dated corpus), 'known' when
 *   this build was written for it, 'ahead' when the file came from a newer
 *   recorder than this one, and 'unknown' for anything else.
 */
export function schemaStatus(v) {
  if (v == null) return 'legacy';
  if (!Number.isInteger(v) || v < 1) return 'unknown';
  if (KNOWN_SCHEMAS.has(v)) return 'known';
  return v > SCHEMA ? 'ahead' : 'unknown';
}

/** The kinds of row that are about the run rather than about a coin. */
export const SESSION_KINDS = ['start', 'tick', 'gap', 'failagg', 'stop'];

/**
 * An id for one run of the recorder.
 *
 * Base-36 milliseconds plus the pid: sortable, unique across two runs on the
 * same machine, and short enough to sit on every row without being noticed.
 */
export function newSessionId(t = Date.now(), pid = process.pid) {
  return `${t.toString(36)}-${pid.toString(36)}`;
}

/** `YYYYMMDD-HHMM` in UTC — the human half of a session filename. */
export function sessionStamp(t = Date.now()) {
  const iso = new Date(t).toISOString();
  return `${iso.slice(0, 4)}${iso.slice(5, 7)}${iso.slice(8, 10)}-${iso.slice(11, 13)}${iso.slice(14, 16)}`;
}

/**
 * The filename infix for a session: `<sid>-<YYYYMMDD-HHMM>`, so a run's files
 * are `coins-<that>.jsonl`, `tracks-<that>.jsonl` and so on.
 *
 * One file per session and never a split at midnight. The old dated naming is
 * the single mechanical fact that turned one run into a tuning day plus a
 * fictional holdout, and it cannot be undone after the fact because nothing on
 * the row says which run it came from.
 */
export function sessionFile(sid, t = Date.now()) {
  return `${sid}-${sessionStamp(t)}`;
}

/**
 * The four facts that replace `follow: 60`.
 *
 * @param t         when the coin launched
 * @param now       when its record is being written
 * @param follow    the window we said we would watch for, in seconds
 * @param down0     the run's cumulative socket downtime at launch, in ms
 * @param downNow   the same total now — the difference is downtime inside
 *                  *this coin's* window, which is the hole nobody had noticed:
 *                  the follow timer fires whether or not the feed was alive
 * @param reason    'window' if the timer fired, 'shutdown' if the run ended first
 *
 * `gapSec` rounds any non-zero downtime up to one second rather than away, so
 * `gapSec > 0` means exactly the same thing as "the feed dropped during this
 * window" and `complete` can be read straight off it. Rounding a 400 ms outage
 * to zero would put a row back in the state this whole function exists to end:
 * looking complete while not being complete.
 */
export function closeFacts({ t, now, follow, down0 = 0, downNow = 0, reason = 'window', downRatio = 0.2 }) {
  const observedMs = Math.max(0, now - t);
  const gapMs = Math.min(observedMs, Math.max(0, downNow - down0));
  const gapSec = gapMs > 0 ? Math.max(1, Math.round(gapMs / 1000)) : 0;
  // Floored, so a complete window reads exactly `follow` and a truncated one
  // reads strictly less. A timer never fires early, so nothing complete can
  // land below the window by rounding.
  const observedSec = Math.floor(observedMs / 1000);
  // Losing most of a window to an outage is a different failure from being cut
  // off at the end of it, and a reader deciding what to keep needs to tell them
  // apart. Below the ratio the outage is still recorded in `gapSec`.
  const stopReason = observedMs > 0 && gapMs / observedMs > downRatio ? 'socket-down' : reason;
  return {
    observedSec,
    gapSec,
    stopReason,
    // The one flag to branch on: we watched the whole window we promised, and
    // the feed was up for all of it.
    complete: reason === 'window' && gapSec === 0,
    follow,
  };
}

/**
 * Normalise a Solana transaction error into a short stable code, and say
 * whether the shape was recognised.
 *
 * The error kind is the valuable half. `ix3:custom:6002` is pump's slippage
 * error — somebody was outbid — and `ix0:AccountInUse` is contention; they say
 * opposite things about whether a strategy is uncompetitive or merely slow.
 * Unrecognised shapes keep their raw error so an unfamiliar failure mode is
 * never flattened into "other".
 */
export function classifyErr(err) {
  if (err == null) return { e: 'none', keepRaw: false };
  if (typeof err === 'string') return { e: err, keepRaw: false };
  if (typeof err !== 'object') return { e: '?', keepRaw: true };

  const k = Object.keys(err)[0];
  if (k !== 'InstructionError') return { e: k ?? '?', keepRaw: true };

  const pair = err[k];
  if (!Array.isArray(pair)) return { e: 'InstructionError', keepRaw: true };
  const [ix, detail] = pair;
  if (typeof detail === 'string') return { e: `ix${ix}:${detail}`, keepRaw: false };
  if (detail && typeof detail === 'object' && typeof detail.Custom === 'number') {
    return { e: `ix${ix}:custom:${detail.Custom}`, keepRaw: false };
  }
  const dk = detail && typeof detail === 'object' ? Object.keys(detail)[0] : null;
  return { e: `ix${ix}:${dk ?? '?'}`, keepRaw: true };
}

/**
 * A deterministic 1-in-`rate` sample keyed on the signature itself.
 *
 * On-chain failures outnumber successes about 14 to 1, so keeping every one of
 * them is a real storage decision rather than a free one. Keying the sample on
 * the signature rather than on a counter keeps it independent of arrival order
 * — a burst from one contested slot is no likelier to be kept than a quiet
 * moment — and makes it reproducible from the signature alone.
 *
 * The rate is written on every sampled row and into the `start` record. A
 * sample whose rate is not recorded is not a sample, it is a hole, which is the
 * same defect as `follow: 60`.
 *
 * FNV-1a, because this codebase has no dependencies.
 */
export function sampled(sig, rate) {
  if (!sig) return false;
  if (rate <= 1) return rate === 1;
  let h = 2166136261;
  for (let i = 0; i < sig.length; i++) {
    h ^= sig.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return (h >>> 0) % rate === 0;
}

/**
 * Rebuild what each session was actually up to, from its own rows.
 *
 * Uptime is `connected ticks ÷ ticks`, a measured number, because the heartbeat
 * is written every `heartbeatMs` whether or not anything happened. Before this,
 * an outage and a quiet market produced the same file and the only way to tell
 * them apart was to reconstruct bursts from launch timestamps.
 *
 * A session with no `stop` row was killed; its end is the last thing it wrote,
 * which the heartbeat bounds to within one interval.
 *
 * @param rows session rows (`k` set) in any order
 */
export function sessions(rows) {
  const bySid = new Map();
  const get = (sid) => {
    if (!bySid.has(sid)) {
      bySid.set(sid, {
        sid, from: null, to: null, ticks: 0, connected: 0, gaps: 0, gapMs: 0,
        ended: 'open', heartbeatMs: null, launches: null, trades: null, failed: 0,
      });
    }
    return bySid.get(sid);
  };

  for (const r of rows) {
    if (!r || typeof r !== 'object' || !SESSION_KINDS.includes(r.k)) continue;
    const s = get(r.sid ?? null);
    if (typeof r.t === 'number') {
      s.from = s.from === null ? r.t : Math.min(s.from, r.t);
      s.to = s.to === null ? r.t : Math.max(s.to, r.t);
    }
    if (r.k === 'start') s.heartbeatMs = r.policy?.heartbeatMs ?? null;
    else if (r.k === 'tick') {
      s.ticks++;
      if (r.connected) s.connected++;
      if (typeof r.launches === 'number') s.launches = r.launches;
      if (typeof r.trades === 'number') s.trades = r.trades;
    } else if (r.k === 'gap') {
      s.gaps++;
      s.gapMs += r.ms || 0;
    } else if (r.k === 'failagg') s.failed += r.n || 0;
    else if (r.k === 'stop') s.ended = 'stop';
  }

  return [...bySid.values()]
    .map((s) => ({
      ...s,
      spanSec: s.from !== null && s.to !== null ? Math.round((s.to - s.from) / 1000) : 0,
      // No ticks means no measurement. Saying so beats reporting 100%, which is
      // what the other tool in this house did for a listener that ran 0.4% of
      // the time it claimed to cover.
      uptime: s.ticks > 0 ? s.connected / s.ticks : null,
    }))
    .sort((a, b) => (a.from ?? 0) - (b.from ?? 0));
}
