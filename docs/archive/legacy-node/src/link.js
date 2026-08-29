// How far behind the chain we are, and whether we are still on it.
//
// The dashboard used to answer "is the stream up?" by looking at its own
// EventSource — a socket from the browser to a server on the same machine,
// which is up whenever the page is. So the light stayed green through every
// disconnect the Solana socket ever had, which is precisely the moment it
// existed to warn about. This holds the state of the real link instead.
//
// The latency figure is not invented and is not a ping. Every pump.fun event
// carries the block's own timestamp — `ts` on both CreateEvent and TradeEvent,
// decoded in pump.js and until now thrown away — so the lag is
//
//     when we received it  −  the timestamp of the block it was in
//
// which is the whole path: block production, confirmation, the provider's fan
// out, the network, and this process getting round to it. Measured against
// mainnet on 16 Aug 2026 it was a median 1.7 s over 1,214 consecutive events,
// range 654 ms to 2.8 s, with no sample landing in the future.
//
// Two things about that number have to be said out loud or it will be misread:
//
//   • Block timestamps are whole seconds. One sample is therefore worth ±1s and
//     nothing more. Only the distribution means anything, so `n` is reported
//     next to every quantile and a report with too few samples says so instead
//     of printing a confident median of four events.
//   • It is time behind the *block clock*, not round-trip time to the provider.
//     A faster endpoint moves it; so does the confirmed commitment this
//     subscribes at, which is most of the second.
//
// A negative sample means the local clock is ahead of the validators' and the
// figure is measuring machine drift rather than the link. Those are counted and
// reported rather than clamped to zero, because a clock that is wrong is worth
// knowing about and silently flooring it would hide it.

/** How many lag samples to keep. Twenty seconds of a busy tape, roughly. */
export const LAG_WINDOW = 600;

/** Below this many samples a quantile is not worth printing. */
export const MIN_SAMPLES = 20;

export class LinkHealth {
  constructor({ endpoint = null, window = LAG_WINDOW } = {}) {
    this.endpoint = endpoint;
    this.window = window;
    this.lags = [];
    this.events = 0;
    this.withTimestamp = 0;
    this.ahead = 0; // samples where our clock was ahead of the block's
    this.gaps = 0;
    this.missedMs = 0;
    this.lastGap = null;
    this.since = Date.now();
  }

  /**
   * One decoded event's lag, from the block timestamp it carries.
   * Events without one are still counted, so `withTimestamp` shows what share
   * of the tape this is actually measuring rather than implying it is all of it.
   */
  sample(blockTsSec, at = Date.now()) {
    this.events++;
    const ts = Number(blockTsSec);
    if (!Number.isFinite(ts) || ts <= 0) return;
    this.withTimestamp++;
    const lag = at - ts * 1000;
    if (lag < 0) this.ahead++;
    this.lags.push(lag);
    if (this.lags.length > this.window) this.lags.splice(0, this.lags.length - this.window);
  }

  /** A disconnection that has now been recovered from, and what it cost. */
  gap({ ms, reason } = {}) {
    this.gaps++;
    this.missedMs += Number(ms) || 0;
    this.lastGap = { ms: Number(ms) || 0, reason: reason ?? null, at: Date.now() };
  }

  /**
   * The lag distribution over the window, or nulls with a reason.
   *
   * Deliberately null rather than 0 on a thin sample: an unmeasured link and a
   * link with no delay have to read differently, which is the same rule the
   * rest of the board follows for ratios.
   */
  lag() {
    const n = this.lags.length;
    if (n < MIN_SAMPLES) {
      return { n, enough: false, p50: null, p95: null, min: null, max: null, aheadOfClock: this.ahead };
    }
    const sorted = [...this.lags].sort((a, b) => a - b);
    const at = (p) => sorted[Math.min(n - 1, Math.floor(n * p))];
    return {
      n,
      enough: true,
      p50: Math.round(at(0.5)),
      p95: Math.round(at(0.95)),
      min: Math.round(sorted[0]),
      max: Math.round(sorted[n - 1]),
      aheadOfClock: this.ahead,
    };
  }

  /**
   * The whole picture, assembled with whatever the socket knows about itself.
   *
   * `socket` is the ws.js Socket. Reading its fields here rather than teaching
   * it to report keeps the reconnect logic in one place and the presentation in
   * another; the fields read are the ones it already maintains for its own use.
   */
  report(socket = null, now = Date.now()) {
    const lastMessageAt = socket?.lastMessageAt || 0;
    const down = socket?.downSince ?? null;
    const stopped = socket?.stopped ?? false;
    const state = stopped ? 'stopped' : down !== null ? 'reconnecting' : lastMessageAt ? 'connected' : 'connecting';
    return {
      endpoint: this.endpoint, // already redacted by the caller; keys never land here
      state,
      // How long since anything at all arrived. The one number that catches a
      // socket that is open and silent, which no connection flag ever will.
      quietMs: lastMessageAt ? now - lastMessageAt : null,
      downSince: down,
      downMs: down === null ? null : now - down,
      attempt: socket?.attempt ?? 0,
      upSince: this.since,
      events: this.events,
      withTimestamp: this.withTimestamp,
      gaps: this.gaps,
      missedMs: Math.round(this.missedMs),
      lastGap: this.lastGap,
      lag: this.lag(),
    };
  }
}
