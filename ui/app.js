// The wiring between the window and the engine.
//
// Everything on screen comes from one of two places and nowhere else:
//
//   1. `get_ingestion_metrics`, polled. Free of side effects on the Rust side,
//      so the window is allowed to ask as fast as it repaints.
//   2. `stream_telemetry`, pushed. One channel, opened once, carrying every
//      line the engine publishes until the window closes.
//
// There is deliberately no third place. Nothing here invents a value, carries
// one forward after the feed stops, or falls back to a plausible default when a
// field is missing: a number on a trading surface has to be either something the
// engine said or an em dash, because those are the only two things a person can
// safely act on. Where the backend has no answer yet, the em dash from
// `index.html` is left exactly where it is.

// ---------------------------------------------------------------------------
// the bridge
// ---------------------------------------------------------------------------

// Tauri injects `__TAURI_INTERNALS__` into every window regardless of config;
// the friendlier `__TAURI__` bundle only appears when `withGlobalTauri` is on,
// and it is not on in `tauri.conf.json`. So the public API is preferred when it
// exists and the internals are used when it does not — which means this file
// works today and keeps working unchanged the moment that flag is flipped.
const internals = window.__TAURI_INTERNALS__;
const globalApi = window.__TAURI__;

const invoke =
  globalApi?.core?.invoke ??
  (internals ? (cmd, payload = {}) => internals.invoke(cmd, payload) : null);

/// A port of Tauri's `Channel` for the case where the global bundle is absent.
///
/// The wire format is not ours to choose: the Rust side sends `{index, message}`
/// frames that may arrive out of order and a final `{index, end}` marker, and
/// the payload is recognised as a channel by the literal string this serialises
/// to. Reordering is the whole reason this is not four lines — an event handed
/// to the UI out of sequence would put a later candidate above an earlier one.
class LocalChannel {
  #onmessage;
  #nextIndex = 0;
  #pending = {};
  #endIndex;

  constructor(onmessage) {
    // Defaulted rather than left undefined: a channel built before its handler
    // is attached still receives frames, and dropping them on the floor is
    // better than throwing inside Tauri's callback dispatcher.
    this.#onmessage = onmessage ?? (() => {});
    this.id = internals.transformCallback((raw) => {
      const index = raw.index;

      if ("end" in raw) {
        // The stream is done once every frame before the marker has been
        // delivered, which may not have happened yet.
        if (index === this.#nextIndex) this.#cleanup();
        else this.#endIndex = index;
        return;
      }

      if (index === this.#nextIndex) {
        this.#onmessage(raw.message);
        this.#nextIndex += 1;
        // Anything that arrived early is now in order; drain as far as it goes.
        while (this.#nextIndex in this.#pending) {
          this.#onmessage(this.#pending[this.#nextIndex]);
          delete this.#pending[this.#nextIndex];
          this.#nextIndex += 1;
        }
        if (this.#nextIndex === this.#endIndex) this.#cleanup();
      } else {
        this.#pending[index] = raw.message;
      }
    });
  }

  #cleanup() {
    internals.unregisterCallback(this.id);
  }

  // The accessor pair upstream has. Without it `channel.onmessage = fn` sets an
  // ordinary property that nothing reads, and the channel goes quiet.
  set onmessage(handler) {
    this.#onmessage = handler ?? (() => {});
  }

  get onmessage() {
    return this.#onmessage;
  }

  // Both hooks, because `JSON.stringify` calls `toJSON` first and Tauri's
  // replacer looks for the other one. Defining only one works until the day the
  // other path is taken.
  __TAURI_TO_IPC_KEY__() {
    return `__CHANNEL__:${this.id}`;
  }

  toJSON() {
    return `__CHANNEL__:${this.id}`;
  }
}

const ChannelCtor =
  globalApi?.core?.Channel ?? (internals ? LocalChannel : null);

// ---------------------------------------------------------------------------
// units
// ---------------------------------------------------------------------------

const LAMPORTS_PER_SOL = 1_000_000_000;
const MS_PER_SLOT = 400;
const BPS_DENOMINATOR = 10_000;

// --- the curve's own constants ---------------------------------------------
//
// Every one of these is a mirror of a value the Rust side already holds, and
// each is named here rather than inlined so the day a protocol parameter moves
// there is one line to change on this side and a grep that finds it.

/// Real SOL at which a pump.fun curve migrates. `ingestion::PUMP_GRADUATION_LAMPORTS`.
const PUMP_GRADUATION_LAMPORTS = 85 * LAMPORTS_PER_SOL;

/// Total swap fee in basis points. `replay::DEFAULT_FEE_BPS`.
const CURVE_FEE_BPS = 100;

/// The median first buy in the observed corpus, in lamports.
///
/// REPLAY_AND_SIMULATION_SPEC.md §15.2: of 37 288 first buys, the median is
/// 0.52 SOL. It is here because the sandwich threshold below is a number of SOL
/// and a number of SOL means nothing without something to compare it against.
/// This is a measurement rather than a policy, which is why the badge is drawn
/// off it — it says what happens to a typical buy on this curve, not what the
/// engine intends to do.
const CORPUS_MEDIAN_FIRST_BUY_LAMPORTS = 520_000_000;

// How often the ingestion counters are asked for. The brief says 100ms and the
// command is documented as safe to poll at repaint rate, so this is that.
const POLL_MS = 100;
// Lifecycle changes at human speed and the call touches a lock, so it gets its
// own slower loop rather than riding along with the counters.
const STATUS_POLL_MS = 1000;

const DASH = "—";

// ---------------------------------------------------------------------------
// SOL price
// ---------------------------------------------------------------------------
//
// There were two implementations of this after the merge and only one can be
// right, because `wireSolPrice` was declared twice in one module — the second
// declaration silently wins and the first becomes dead code that still looks
// live. The one that survives is the one further down this file, next to
// `renderSolPrice`; this is what was here instead, and why it is not.
//
// It opened at `148`, wrote `$148.00` into the cell, and set
// `currentSolPriceCents = 14800` — **without telling the engine.** So from the
// first repaint the window showed a price the engine did not have. `SolPrice`
// starts at `UNKNOWN`, every market-cap threshold is written in dollars and
// every chain number arrives in lamports, so until the price is actually sent
// the two never meet and every candidate reads as too small to trade. An
// operator looking at that window sees a plausible price over a quiet market
// and has no way to tell it from a market that is quiet.
//
// That is the exact failure the surviving control is built to prevent: it opens
// `unset`, draws unset as a warning rather than as a blank, and only ever shows
// a number the engine has confirmed back. A default that is right most days is
// worse than no default, because the day it is wrong it is invisible.

// ---------------------------------------------------------------------------
// token name cache (pump.fun API)
// ---------------------------------------------------------------------------

/// Whether the window may ask pump.fun what a curve account is called.
///
/// **Off, and off is a decision rather than an oversight.** The lookup below is
/// a `fetch` from the renderer to a third party, one per new candidate, on a
/// feed that does hundreds a minute on a busy launch. Three things are wrong
/// with having it on by default and none of them is the code:
///
///   1. **It is an outbound leak of what STS is watching.** Every curve account
///      the radar sees is sent to an endpoint the venue operates, in real time,
///      as it is seen. This engine's entire thesis is being early to something;
///      telling the venue's API which accounts it is early to is the one piece
///      of information it should be least willing to give away.
///   2. **The window is meant to work with no network.** `styles.css` embeds
///      rather than links its fonts for that reason and says so; the headless
///      suite reaches nothing; the README calls the system local-first with no
///      mandatory hosted control plane. One `fetch` in the renderer makes the
///      cockpit's behaviour depend on somebody else's uptime and CORS policy.
///   3. **It is unbounded.** No timeout, no rate limit, no backoff, and a bare
///      catch that swallows every failure, on the hot path of a feed with no
///      ceiling on arrivals.
///
/// And a fourth thing, which is the one that settles it: **as shipped it barely
/// delivers.** `resolveTokenName` is fire-and-forget, and `getTokenDisplay` is
/// read about ten lines later in the same tick — so the first sighting of an
/// account is always a cache miss, and the name only ever paints if that same
/// account produces another radar event after the fetch has resolved. The
/// egress is paid on every account; the name arrives for the subset that update
/// again. This is not a working feature being traded away for privacy.
///
/// **What turning it off costs.** The radar search consults this cache for
/// symbol and name, so with the flag off it matches on account and creator
/// only: a ticker finds nothing. The input says `search account / creator` for
/// exactly that reason, and that is the whole of the loss.
///
/// **What turning it on properly would cost.** The honest home is the engine,
/// not the window — it can hold a timeout, a rate limit and one egress point.
/// But `reqwest` is not a dependency here and adding it is not free: this
/// crate's own manifest argues a feature down for pulling ~90 crates and a
/// second TLS stack into a process that holds keys. The cheap path already
/// exists and has precedent — `alerting.rs` hand-rolls its webhook POST over
/// the `native-tls` that is here anyway, for that stated reason — so a GET
/// beside it is the shape to copy. Say the price before quoting the fix.
///
/// None of that makes a name beside an account a bad idea, and the code below
/// is kept whole rather than deleted. Flip this to `true` to take the trade as
/// it stands.
const TOKEN_NAMES_FROM_PUMP_FUN = false;

const tokenNameCache = new Map();
const tokenNamePending = new Set();

async function resolveTokenName(account) {
  if (!TOKEN_NAMES_FROM_PUMP_FUN) return;
  if (tokenNameCache.has(account) || tokenNamePending.has(account)) return;
  tokenNamePending.add(account);
  try {
    const resp = await fetch(`https://frontend-api-v3.pump.fun/coins/${account}`);
    if (!resp.ok) return;
    const data = await resp.json();
    tokenNameCache.set(account, {
      name: data.name ?? null,
      symbol: data.symbol ?? null,
      imageUri: data.imageUri ?? null,
    });
  } catch {
    // Network or CORS failure — leave it blank, try again later is not worth it.
  } finally {
    tokenNamePending.delete(account);
  }
}

function getTokenDisplay(account) {
  const info = tokenNameCache.get(account);
  if (!info) return null;
  const parts = [];
  if (info.symbol) parts.push(info.symbol);
  if (info.name && info.name !== info.symbol) parts.push(info.name);
  return parts.length > 0 ? parts.join(" · ") : null;
}

// ---------------------------------------------------------------------------
// toast notifications
// ---------------------------------------------------------------------------

const MAX_TOASTS = 5;
const TOAST_LIFETIME_MS = 6000;

function showToast(message, level = "dim", lifetime = TOAST_LIFETIME_MS) {
  const container = region("toast-container");
  if (!container) return;

  const toast = document.createElement("div");
  toast.className = `toast is-${level}`;

  const msg = document.createElement("span");
  msg.className = "toast-message";
  msg.textContent = message;

  const dismiss = document.createElement("span");
  dismiss.className = "toast-dismiss";
  dismiss.textContent = "\u00d7";
  dismiss.addEventListener("click", () => removeToast(toast));

  toast.append(msg, dismiss);
  container.appendChild(toast);

  // The overflow is dropped **now**, not faded out.
  //
  // This loop is why. `removeToast` animates: it adds a class and schedules the
  // detach, so the node it was given is still a child when it returns and
  // `childElementCount` has not moved. Asking it to make room in a `while`
  // therefore asks the same question about the same node forever.
  //
  // And forever is the word. The loop never yields, so the detach it is waiting
  // on can never run: this is not a stall that resolves, it is a renderer that
  // stops. What triggers it is a sixth toast alive at once — `MAX_TOASTS` is 5
  // and `TOAST_LIFETIME_MS` is six seconds, so six qualifying candidates inside
  // six seconds is the whole condition. On a launch feed that is not a race, it
  // is a Tuesday.
  //
  // A toast being pushed off the top by newer ones has nothing to animate
  // anyway: it is leaving because it was superseded, not because it expired.
  while (container.childElementCount > MAX_TOASTS) {
    container.firstElementChild.remove();
  }

  setTimeout(() => removeToast(toast), lifetime);
}

/// Fades one toast out and detaches it when the transition has run.
///
/// Idempotent, which it has to be: a toast can be dismissed by hand and expire
/// on its own timer, and both paths land here. Without the guard the second
/// call schedules a second detach against a node the first one already took,
/// and the `removing` class goes back on an element mid-transition.
///
/// It never removes synchronously. The caller that needs that — the overflow
/// in `showToast` — detaches the node itself, and the comment there says why.
function removeToast(toast) {
  if (!toast || !toast.parentNode) return;
  if (toast.dataset.removing === "true") return;
  toast.dataset.removing = "true";
  toast.classList.add("removing");
  setTimeout(() => toast.remove(), 150);
}

// ---------------------------------------------------------------------------
// radar stats
// ---------------------------------------------------------------------------

let radarStatsSeen = 0;
let radarStatsFast = 0;
let radarStatsGrad = 0;
let radarFirstSlotTime = null;
let radarSlotCounts = [];

function updateRadarStats(entry) {
  radarStatsSeen++;

  if (entry.route === "fastPath") radarStatsFast++;
  if (entry.view.curveComplete || (entry.view.curveProgressBps ?? 0) >= GRADUATING_BPS) {
    radarStatsGrad++;
  }

  const now = Date.now();
  radarSlotCounts.push(now);
  const oneMinAgo = now - 60000;
  radarSlotCounts = radarSlotCounts.filter((t) => t > oneMinAgo);

  setText("stats-seen", String(radarStatsSeen));
  setText("stats-fast", String(radarStatsFast));
  setText("stats-grad", String(radarStatsGrad));
  setText("stats-rate", String(radarSlotCounts.length));
}

// ---------------------------------------------------------------------------
// radar search filter
// ---------------------------------------------------------------------------

let radarSearchQuery = "";

function wireRadarSearch() {
  const input = field("radar-search");
  if (!input) return;
  input.addEventListener("input", () => {
    radarSearchQuery = input.value.trim().toLowerCase();
    applyRadarFilter();
  });
}

function candidateMatchesSearch(entry) {
  if (!radarSearchQuery) return true;
  const q = radarSearchQuery;
  if (entry.view.account?.toLowerCase().includes(q)) return true;
  if (entry.view.creator?.toLowerCase().includes(q)) return true;
  const nameInfo = tokenNameCache.get(entry.view.account);
  if (nameInfo?.symbol?.toLowerCase().includes(q)) return true;
  if (nameInfo?.name?.toLowerCase().includes(q)) return true;
  return false;
}

/// Lamports as SOL, always to the same number of decimals.
///
/// Fixed width matters more than precision here: `tabular-nums` lines digits up
/// only if there are the same number of them, and a column that switches
/// between `1.2` and `1.23456` shimmers on every frame.
function sol(lamports, decimals = 3) {
  if (typeof lamports !== "number" || !Number.isFinite(lamports)) return DASH;
  return (lamports / LAMPORTS_PER_SOL).toFixed(decimals);
}

/// Basis points as a percentage, one decimal. 10_000 bps is 100%.
function bps(value, decimals = 1) {
  if (typeof value !== "number" || !Number.isFinite(value)) return DASH;
  return (value / 100).toFixed(decimals);
}

/// Millionths as a percentage, one decimal. 1_000_000 millionths is 100%.
///
/// The sibling of `bps` for the other unit the engine reports ratios in. Both
/// exist rather than one converting into the other, because a number that has
/// been through two divisions in the window is a number the window is now
/// responsible for, and every ratio here is meant to be the engine's answer
/// rendered rather than the window's answer computed.
function micropct(value, decimals = 1) {
  if (typeof value !== "number" || !Number.isFinite(value)) return DASH;
  return (value / 10_000).toFixed(decimals);
}

function count(value) {
  if (typeof value !== "number" || !Number.isFinite(value)) return DASH;
  return Math.trunc(value).toLocaleString("en-US");
}

function rate(value, decimals = 1) {
  if (typeof value !== "number" || !Number.isFinite(value)) return DASH;
  return value.toFixed(decimals);
}

/// Microseconds as milliseconds. The dispatch budget is written in micros and
/// read in millis, and reading it in micros means counting zeroes.
function micros(value) {
  if (typeof value !== "number" || !Number.isFinite(value)) return DASH;
  return `${(value / 1000).toFixed(2)}ms`;
}

/// A duration as the largest unit that still says something useful.
function duration(ms) {
  if (typeof ms !== "number" || !Number.isFinite(ms) || ms < 0) return DASH;
  const secs = Math.floor(ms / 1000);
  if (secs < 60) return `${secs}s`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ${String(mins % 60).padStart(2, "0")}m`;
  return `${Math.floor(hours / 24)}d ${hours % 24}h`;
}

/// Wall-clock time, for the event trail. Local, seconds resolution.
function clock(atMs) {
  if (typeof atMs !== "number" || !Number.isFinite(atMs)) return DASH;
  return new Date(atMs).toLocaleTimeString("en-GB", { hour12: false });
}

/// A signed number of SOL, always with its sign and always the same width.
///
/// The sign is the whole point of a delta column: `1.310` and `-1.310` are
/// opposite facts, and a column that only marks one of them reads as the other
/// half the time.
function signedSol(lamports, decimals = 3) {
  if (typeof lamports !== "number" || !Number.isFinite(lamports)) return DASH;
  const value = lamports / LAMPORTS_PER_SOL;
  const sign = value > 0 ? "+" : value < 0 ? "\u2212" : "";
  return `${sign}${Math.abs(value).toFixed(decimals)}`;
}

/// A signed whole number, same rule.
function signedInt(value) {
  if (typeof value !== "number" || !Number.isFinite(value)) return DASH;
  const rounded = Math.trunc(value);
  const sign = rounded > 0 ? "+" : rounded < 0 ? "\u2212" : "";
  return `${sign}${Math.abs(rounded)}`;
}

/// A change between two lamport quantities, in basis points.
///
/// In `BigInt` because the intermediate is the value times ten thousand: a
/// market cap of a few hundred SOL is already past 2^53 once it is scaled, and
/// a float there silently loses the last digits of exactly the number this
/// column exists to show. Rounds half away from zero, so a delta is never
/// reported as smaller than it was.
function deltaBps(now, before) {
  if (!Number.isFinite(now) || !Number.isFinite(before) || before <= 0) return null;
  const a = BigInt(Math.trunc(now));
  const b = BigInt(Math.trunc(before));
  const scaled = (a - b) * BigInt(BPS_DENOMINATOR);
  const half = b / 2n;
  const rounded = scaled >= 0n ? (scaled + half) / b : -((-scaled + half) / b);
  return Number(rounded);
}

/// The smallest victim buy that makes front-running it profitable, in lamports.
///
/// REPLAY_AND_SIMULATION_SPEC.md §15.2 derives the condition from the sign of
/// the attacker's profit derivative at zero attacker size:
///
/// ```text
/// β > φ / (1 − φ)          equivalently     b > φ·y / (1 − φ)²
/// ```
///
/// with `φ` the fee fraction and `y` the **virtual** SOL reserve — the
/// price-setting one, not the executable one. Strictly below this, no front-run
/// of any size clears fees, before landing costs are even counted.
///
/// Written as integers throughout: `b = f·y·10000 / (10000 − f)²`. Rounds up,
/// because a floor reported below the true one is a curve that looks safe to be
/// sandwiched on when it is not.
function sandwichFloorLamports(virtualSolReserves) {
  if (typeof virtualSolReserves !== "number" || !Number.isFinite(virtualSolReserves)) {
    return null;
  }
  if (virtualSolReserves <= 0) return null;
  const y = BigInt(Math.trunc(virtualSolReserves));
  const fee = BigInt(CURVE_FEE_BPS);
  const denominator = BigInt(BPS_DENOMINATOR);
  const numerator = fee * y * denominator;
  const divisor = (denominator - fee) * (denominator - fee);
  return Number((numerator + divisor - 1n) / divisor);
}

/// The sandwich threshold as a ratio rather than as a size, in basis points.
///
/// `sandwichFloorLamports` above answers "how big must a buy be"; this answers
/// "how large a fraction of the reserve must it be", which is the form §15.2
/// states the condition in:
///
/// ```text
/// β > φ / (1 − φ)
/// ```
///
/// Rounded up, so a β reported strictly above this number is genuinely above
/// the threshold. The reverse is not guaranteed — at the boundary the rounding
/// and the inequality disagree by design — which is why the flag on a row comes
/// from `sandwichAboveThreshold` and not from comparing against this.
const SANDWICH_THRESHOLD_BPS = Math.ceil(
  (CURVE_FEE_BPS * BPS_DENOMINATOR) / (BPS_DENOMINATOR - CURVE_FEE_BPS),
);

/// `β` for one observed buy, in basis points.
///
/// §15.2 writes β as the victim's **net** input over the virtual SOL reserve
/// the buy arrives at:
///
/// ```text
/// β = b(1 − φ) / y
/// ```
///
/// and `b(1 − φ)` is exactly what a pump.fun buy adds to the reserve — the fee
/// is taken before the curve sees it. So the change in real SOL between two
/// observations of one account *is* the numerator, with nothing grossed up to
/// get there. `y` is the reserve **before** the buy, which is the previous
/// observation's: the current one already has the buy in it, and measuring a
/// buy against the reserve it produced understates every large one.
///
/// Floored, and in `BigInt` because the numerator is a lamport count times ten
/// thousand — an 85 SOL pool is already past 2^53 once it is scaled.
function sandwichBetaBps(flowLamports, priorVirtualSolReserves) {
  if (!Number.isFinite(flowLamports) || flowLamports <= 0) return null;
  if (!Number.isFinite(priorVirtualSolReserves) || priorVirtualSolReserves <= 0) return null;
  const net = BigInt(Math.trunc(flowLamports));
  const y = BigInt(Math.trunc(priorVirtualSolReserves));
  return Number((net * BigInt(BPS_DENOMINATOR)) / y);
}

/// Whether a front-run of any size at all clears fees against this buy.
///
/// The inequality with both divisions cleared out of it:
///
/// ```text
/// β > φ / (1 − φ)     ⟺     net / y > F / (10⁴ − F)     ⟺     net(10⁴ − F) > F·y
/// ```
///
/// Two multiplications and one comparison, no division, so the boundary is
/// decided by the inequality rather than by a rounding mode — the same
/// discipline `backtest::sandwich_viable` keeps on the Rust side, and the
/// reason this is the field the row is flagged from.
///
/// False here is a statement about the curve, not about the block market: below
/// the threshold no attacker size pays, before any landing cost is counted.
function sandwichAboveThreshold(flowLamports, priorVirtualSolReserves) {
  if (!Number.isFinite(flowLamports) || flowLamports <= 0) return false;
  if (!Number.isFinite(priorVirtualSolReserves) || priorVirtualSolReserves <= 0) return false;
  const net = BigInt(Math.trunc(flowLamports));
  const y = BigInt(Math.trunc(priorVirtualSolReserves));
  const fee = BigInt(CURVE_FEE_BPS);
  return net * (BigInt(BPS_DENOMINATOR) - fee) > fee * y;
}

/// The gross buy that a net inflow of this size was the remainder of.
///
/// `b = net · 10⁴ / (10⁴ − F)`. It exists so the detail can put a victim buy and
/// the sandwich floor beside each other in the same unit — the floor is a gross
/// size and the flow is a net one, and the two differ by the fee. Floored, so a
/// buy is never reported as larger than it was and therefore never reported as
/// clearing a floor it did not clear.
function victimGrossLamports(flowLamports) {
  if (!Number.isFinite(flowLamports) || flowLamports <= 0) return null;
  const net = BigInt(Math.trunc(flowLamports));
  const denominator = BigInt(BPS_DENOMINATOR) - BigInt(CURVE_FEE_BPS);
  return Number((net * BigInt(BPS_DENOMINATOR)) / denominator);
}

/// The median of a list of numbers. Empty is not zero, so it is `null`.
function median(values) {
  if (!values.length) return null;
  const sorted = [...values].sort((a, b) => a - b);
  const middle = sorted.length >> 1;
  return sorted.length % 2 === 1
    ? sorted[middle]
    : (sorted[middle - 1] + sorted[middle]) / 2;
}

/// A base58 key, shortened so a column of them is a column.
///
/// The last four characters are kept because that is the half a person actually
/// recognises an address by; cutting the tail instead would make every key on
/// screen look the same.
function shortKey(key) {
  if (typeof key !== "string" || key.length < 12) return key || DASH;
  return `${key.slice(0, 4)}${"…"}${key.slice(-4)}`;
}

/// A hex digest, shortened. Wider than a key on both ends: two fixtures are
/// told apart by their hashes and four characters of hex is sixteen bits.
function shortHash(hash) {
  if (typeof hash !== "string" || hash.length <= 20) return hash || DASH;
  return `${hash.slice(0, 8)}${"…"}${hash.slice(-8)}`;
}

// ---------------------------------------------------------------------------
// dom access
// ---------------------------------------------------------------------------

const fieldCache = new Map();

function field(name) {
  if (!fieldCache.has(name)) {
    fieldCache.set(name, document.querySelector(`[data-field="${name}"]`));
  }
  return fieldCache.get(name);
}

/// Writes a value, but only when it changed.
///
/// At ten frames a second an unconditional write is ten style recalculations a
/// second per cell for text that is usually identical, and it also wipes any
/// text the user was midway through selecting.
function setText(name, value) {
  const el = field(name);
  if (el && el.textContent !== value) el.textContent = value;
}

function setAttr(name, attr, value) {
  const el = field(name);
  if (el && el.getAttribute(attr) !== value) el.setAttribute(attr, value);
}

const regionCache = new Map();

function region(name) {
  if (!regionCache.has(name)) {
    regionCache.set(name, document.querySelector(`[data-region="${name}"]`));
  }
  return regionCache.get(name);
}

/// Shows the rows or the empty state, never both.
///
/// The empty states are not decoration. Each one says whether nothing has
/// happened or nothing is arriving, and those are indistinguishable on a blank
/// pane and mean opposite things, so the pane is never left simply blank.
function setPopulated(name, populated) {
  const rows = region(`${name}-rows`);
  const empty = region(`${name}-empty`);
  if (rows) rows.hidden = !populated;
  if (empty) empty.hidden = populated;
}

function cell(className, text) {
  const span = document.createElement("span");
  span.className = className;
  span.textContent = text;
  return span;
}

// ---------------------------------------------------------------------------
// connection state
// ---------------------------------------------------------------------------

// Whether the last call to the backend worked. A window opened outside Tauri —
// or one whose engine has gone — must not keep displaying the last numbers it
// saw as though they were current.
let bridgeLive = false;

function markBridge(live, note) {
  if (bridgeLive === live) return;
  bridgeLive = live;
  if (!live) {
    setAttr("mode", "data-mode", "unknown");
    const label = field("mode")?.querySelector(".label");
    if (label) label.textContent = note ?? "no engine";
    field("mode")?.querySelector(".dot")?.classList.remove("is-live", "is-warn", "is-halt");
    clearIngestion();
    // Not "replay is off". The engine has gone, and whether a fixture was
    // driving the numbers on screen a second ago is now unknown.
    markReplayUnknown();
  }
}

/// Puts every ingestion-fed cell back to an em dash.
///
/// Called when the backend stops answering. Stale counters are worse than no
/// counters: `dropped 0` from four minutes ago reads exactly like a healthy
/// feed right now.
function clearIngestion() {
  for (const name of [
    "slot", "endpoints", "uptime", "breaker",
    "frames-per-sec", "candidates-per-sec", "dropped-msgs", "dispatch-latency",
  ]) {
    setText(name, DASH);
  }
  for (const provider of ["helius", "quicknode", "triton"]) {
    setText(`${provider}-latency`, DASH);
    setText(`${provider}-state`, "state unknown");
    const dot = field(`${provider}-latency`)?.closest(".tick")?.querySelector(".dot");
    dot?.classList.remove("is-live", "is-warn", "is-halt");
  }
}

// ---------------------------------------------------------------------------
// 1. ingestion metrics, polled
// ---------------------------------------------------------------------------

// `IngestionSnapshot` and `EndpointStatus` are `rename_all = "camelCase"` on the
// Rust side, so every key below is the camelCase of the field in ingestion.rs.
// The three status-bar providers are matched on `EndpointStatus.provider`, whose
// serialised values are `helius`, `quickNode` and `triton`.
const PROVIDER_FIELDS = {
  helius: "helius",
  quickNode: "quicknode",
  triton: "triton",
};

const HEALTH_DOT = {
  healthy: "is-live",
  degraded: "is-warn",
  unhealthy: "is-halt",
  unknown: null,
};

function renderIngestion(snapshot) {
  if (!snapshot) return;

  // Rates first: these are the two numbers that say whether the socket is
  // keeping up, and they are per-window rather than averaged over the run.
  setText("frames-per-sec", rate(snapshot.framesPerSec));
  setText("candidates-per-sec", rate(snapshot.candidatesPerSec, 2));

  // Every way a message can be lost, added up. Split three ways in the struct
  // because the causes differ; shown as one number because any of them being
  // non-zero means the same thing to whoever is watching — something upstream
  // is faster than something downstream.
  const dropped =
    (snapshot.droppedFastPath ?? 0) +
    (snapshot.droppedStandard ?? 0) +
    (snapshot.droppedWal ?? 0);
  setText("dropped-msgs", count(dropped));
  const droppedEl = field("dropped-msgs");
  droppedEl?.classList.toggle("warn", dropped > 0);

  // Mean receipt-to-dispatch, and whether it is inside the budget the engine
  // was built to. Over budget is amber rather than red: it is a latency the
  // engine noticed and counted, not a failure.
  setText("dispatch-latency", micros(snapshot.dispatchMeanUs));
  const dispatchEl = field("dispatch-latency");
  dispatchEl?.classList.toggle("warn", (snapshot.overBudget ?? 0) > 0);

  setText(
    "endpoints",
    `${snapshot.healthyEndpoints ?? 0}/${snapshot.endpoints?.length ?? 0}`,
  );

  renderEndpoints(snapshot.endpoints ?? []);
}

function renderEndpoints(endpoints) {
  for (const endpoint of endpoints) {
    const name = PROVIDER_FIELDS[endpoint.provider];
    if (!name) continue;

    // p50, because p95 is the number that spikes and p50 is the number that
    // describes the connection. p95 goes in the tooltip for when it matters.
    setText(`${name}-latency`, endpoint.connected ? `${endpoint.latencyP50Ms}ms` : DASH);

    // The screen-reader text carries the state the colour carries, because a
    // dot is the whole of the visual signal here and colour alone is not a
    // signal everyone receives.
    const state = endpoint.connected
      ? `${endpoint.health}, p50 ${endpoint.latencyP50Ms} ms`
      : endpoint.backoffRemainingMs > 0
        ? `disconnected, retrying in ${Math.ceil(endpoint.backoffRemainingMs / 1000)}s`
        : "disconnected";
    setText(`${name}-state`, state);

    const tick = field(`${name}-latency`)?.closest(".tick");
    const dot = tick?.querySelector(".dot");
    if (dot) {
      dot.classList.remove("is-live", "is-warn", "is-halt");
      // Not connected is not the same as unhealthy, and both are worth seeing:
      // a disconnected endpoint that is still inside its backoff is a normal
      // reconnect, so it is amber rather than red until the failures pile up.
      const cls = endpoint.connected
        ? HEALTH_DOT[endpoint.health]
        : endpoint.consecutiveFailures >= 3
          ? "is-halt"
          : "is-warn";
      if (cls) dot.classList.add(cls);
    }
    if (tick) {
      tick.title =
        `${endpoint.url} · ${endpoint.transport}\n` +
        `p50 ${endpoint.latencyP50Ms}ms · p95 ${endpoint.latencyP95Ms}ms\n` +
        `${count(endpoint.frames)} frames · ${count(endpoint.connects)} connects · ` +
        `${endpoint.consecutiveFailures} failures in a row`;
    }
  }
}

async function pollIngestion() {
  if (!invoke) return;
  try {
    renderIngestion(await invoke("get_ingestion_metrics"));
    markBridge(true);
  } catch (err) {
    markBridge(false);
    console.warn("[sts] ingestion metrics unavailable", err);
  }
}

// A self-scheduling timeout rather than `setInterval`, so a round trip that
// takes longer than the interval delays the next call instead of stacking a
// queue of them behind it. Paused while the window is hidden: polling a surface
// nobody is looking at costs the engine the same as polling one they are.
function scheduleIngestionPoll() {
  window.setTimeout(async () => {
    if (!document.hidden) await pollIngestion();
    scheduleIngestionPoll();
  }, POLL_MS);
}

// ---------------------------------------------------------------------------
// engine status, polled slowly
// ---------------------------------------------------------------------------

function renderEngineStatus(status) {
  if (!status) return;

  setText("uptime", duration(status.uptimeMs));

  // The kill switch and the circuit breaker are two different things, and this
  // used to write both of them into `breaker-detail` — the governor row labelled
  // "circuit breaker", which `onRiskSnapshot` also writes. Two sources on one
  // cell is last-writer-wins, and which one won depended on which event arrived
  // last. The halt belongs in the top bar, where it has its own cell; the
  // governor row belongs to the risk snapshot alone.
  setText("breaker", status.killSwitchArmed ? "armed" : "clear");
  const breaker = field("breaker");
  breaker?.classList.toggle("halt", !!status.killSwitchArmed);
  if (breaker) {
    breaker.title = status.killSwitchArmed && status.killSwitchAtMs
      ? `halted ${clock(status.killSwitchAtMs)}`
      : "The kill switch has not been pulled.";
  }

  const mode = field("mode");
  const label = mode?.querySelector(".label");
  const dot = mode?.querySelector(".dot");
  dot?.classList.remove("is-live", "is-warn", "is-halt");

  // Lifecycle is not an operating mode, and the difference is the whole point.
  // `EngineStatus` says whether the process is up; `OperatingMode` — live,
  // paper, shadow — says whether it is allowed to sign anything, and no command
  // returns it. So `running` is shown as shadow, which is what lib.rs documents
  // this build as doing: watch, write it down, sign nothing. It is deliberately
  // never rendered as "live" off a lifecycle flag, because a person reading
  // "live" off this bar would be reading a claim the backend never made.
  switch (status.state) {
    case "running":
      setAttr("mode", "data-mode", "unknown");
      if (label) label.textContent = "shadow";
      if (mode) {
        mode.title =
          "The engine is up. No operating mode is reported by the backend yet, " +
          "and this build signs nothing.";
      }
      dot?.classList.add("is-warn");
      break;
    case "halted":
      setAttr("mode", "data-mode", "halted");
      if (label) label.textContent = "halted";
      dot?.classList.add("is-halt");
      break;
    case "shuttingDown":
      setAttr("mode", "data-mode", "halted");
      if (label) label.textContent = "stopping";
      dot?.classList.add("is-halt");
      break;
    default:
      setAttr("mode", "data-mode", "unknown");
      if (label) label.textContent = "stopped";
      break;
  }
}

async function pollEngineStatus() {
  if (!invoke) return;
  try {
    renderEngineStatus(await invoke("get_engine_status"));
    markBridge(true);
  } catch (err) {
    markBridge(false);
  }
}

function scheduleStatusPoll() {
  window.setTimeout(async () => {
    if (!document.hidden) {
      await pollEngineStatus();
      await pollMetrics();
      await pollBundleTelemetry();
      // Replay pushes its status on telemetry as well. This is the fallback for
      // a window that was opened midway through a run and has not been told
      // anything yet, and it stops asking the moment the engine says it has no
      // such command.
      await pollReplayStatus();
      // The journal is a query against SQLite and the alert status is a read of
      // counters, so both sit on the slow cadence rather than the hundred
      // millisecond one. Alerts themselves do not wait for this: they arrive
      // pushed, on their own channel.
      await pollJournal();
      await pollAlertStatus();
      await pollGeyser();
      // The forensic report for whatever is selected. On the slow cadence
      // because it is a read of a stored report that only changes when
      // somebody runs an analysis, and on the cadence at all because that
      // somebody may not be this window — a report recorded while a subject is
      // already selected has to appear without the operator clicking again.
      if (selectedAccount) await loadClusterReport(selectedAccount);
    }
    scheduleStatusPoll();
  }, STATUS_POLL_MS);
}

// ---------------------------------------------------------------------------
// the engine's own numbers
// ---------------------------------------------------------------------------

// `get_metrics` reads atomics and nothing else — no lock, no database, no side
// effect — so the window is allowed to ask for it as fast as it repaints. It is
// asked on the slow cadence anyway: every number on it is either a total over
// the whole run or a percentile over one, and neither of those says anything
// new ten times a second.
//
// Everything in the status bar left of the queue is what ingestion saw. These
// three are what the engine did with it, which is a different question and the
// one that says whether the panes above are the feed or a sample of it.

// Set false the first time the call comes back saying the command is not there,
// so a build without it is asked once rather than once a second forever.
let metricsSupported = true;

const BACKPRESSURE_DOT = {
  nominal: "is-live",
  elevated: "is-warn",
  saturated: "is-halt",
};

/// Writes the queue band, the tick percentile, and what is on the network.
///
/// Every cell here goes back to an em dash when the field behind it is missing
/// rather than to a zero. `0%` of capacity and "the engine has not said" are
/// opposite facts and they are one keystroke apart on this bar.
function renderMetrics(metrics) {
  if (!metrics) return;

  // --- the queue ----------------------------------------------------------
  const feed = metrics.feed ?? {};
  const state = typeof feed.state === "string" ? feed.state : null;
  const fill = Number.isFinite(feed.fillPercent) ? feed.fillPercent : null;

  setText("backpressure", state === null || fill === null ? DASH : `${state} ${fill}%`);
  // The band is a colour on a dot, and a colour is not a thing a reader can
  // hear. This is the same fact in words, for the row's accessible name.
  setText(
    "backpressure-state",
    state === null ? "queue state unknown" : `queue ${state}, ${fill}% of capacity`,
  );

  const queueTick = field("backpressure")?.closest(".tick");
  const dot = queueTick?.querySelector(".dot");
  if (dot) {
    dot.classList.remove("is-live", "is-warn", "is-halt");
    const cls = BACKPRESSURE_DOT[state];
    if (cls) dot.classList.add(cls);
  }
  if (queueTick) {
    queueTick.title =
      state === null
        ? "The engine has not reported a queue state."
        : `${count(feed.depth)} of ${count(feed.capacity)} frames queued, deepest ${count(feed.deepest)}\n` +
          `${count(feed.ingested)} ingested · ${count(feed.dropped)} dropped · ${bps(feed.lossBps)}% lost\n` +
          `${count(feed.transitions)} band crossings this run`;
  }

  // --- how long a tick takes ----------------------------------------------
  // p50 on the bar and the tail in the tooltip. A median that has not moved
  // while p99 has doubled is the shape of a problem that only shows up under
  // load, and a single number on a status bar cannot carry both.
  const slots = metrics.slots ?? {};
  const processing = slots.processingUs ?? {};
  setText("tick-p50", micros(processing.p50Us));

  const tickEl = field("tick-p50")?.closest(".tick");
  if (tickEl) {
    tickEl.title = Number.isFinite(processing.p50Us)
      ? `${count(slots.ticks)} ticks · newest slot ${count(slots.newestSlot)}\n` +
        `p50 ${micros(processing.p50Us)} · p95 ${micros(processing.p95Us)} · p99 ${micros(processing.p99Us)}\n` +
        `${count(slots.missed)} missed · ${count(slots.regressions)} regressions`
      : "The engine has not timed a tick yet.";
  }

  // --- what is on the network ---------------------------------------------
  const execution = metrics.execution ?? {};
  const intents = Number.isFinite(execution.inFlightIntents) ? execution.inFlightIntents : null;
  const exits = Number.isFinite(execution.inFlightExits) ? execution.inFlightExits : null;
  setText(
    "in-flight",
    intents === null || exits === null ? DASH : `${count(intents)}/${count(exits)}`,
  );

  const flightEl = field("in-flight")?.closest(".tick");
  if (flightEl) {
    flightEl.title =
      intents === null
        ? "The engine has not reported its execution state."
        : `${count(intents)} intents and ${count(exits)} exits on the network\n` +
          `${count(execution.unobserved)} dispatched and never seen again`;
  }
}

async function pollMetrics() {
  if (!invoke || !metricsSupported) return;
  try {
    renderMetrics(await invoke("get_metrics"));
    markBridge(true);
  } catch (err) {
    // A build with no `get_metrics` is a different thing from an engine that
    // has stopped answering, and only the second one is a dead bridge.
    if (isMissingCommand(err)) {
      metricsSupported = false;
      console.info("[sts] no get_metrics in this build; the queue reads as unknown");
    } else {
      markBridge(false);
    }
  }
}

// ---------------------------------------------------------------------------
// the bundle deck
// ---------------------------------------------------------------------------

// `get_bundle_telemetry` takes the deck's lock for the length of one snapshot
// and does no IO under it, so this is safe on the repaint cadence. It is asked
// on the slow one anyway, for the same reason `get_metrics` is: these are
// totals over a run and percentiles over one, and neither says anything new ten
// times a second.

// Set false the first time the call comes back saying the command is not there.
let bundlesSupported = true;

/// A meter's fill and band, from a ratio in millionths.
///
/// The band is the only colour in this block and it is on the bar rather than
/// on the number, because a colour that means something has to be attached to
/// the thing it means something about. A land rate is the one number here where
/// low is bad, so its band runs the other way — which is why the threshold is a
/// parameter rather than a constant.
function setMeter(name, micros, { warnBelow = null, warnAbove = null } = {}) {
  const meter = field(name);
  if (!meter) return;
  const fill = meter.firstElementChild;

  if (!Number.isFinite(micros)) {
    if (fill) fill.style.setProperty("--pct", "0%");
    meter.classList.remove("is-warn", "is-halt");
    return;
  }

  const pct = Math.max(0, Math.min(100, micros / 10_000));
  if (fill) fill.style.setProperty("--pct", `${pct.toFixed(1)}%`);

  meter.classList.remove("is-warn", "is-halt");
  if (warnBelow !== null && micros < warnBelow) meter.classList.add("is-warn");
  if (warnAbove !== null && micros > warnAbove) meter.classList.add("is-warn");
}

/// Writes the tip floor, what moved it, and how bundles are ending.
///
/// Every cell goes back to an em dash when the field behind it is missing
/// rather than to a zero, the same rule the status bar follows: a land rate of
/// `0%` and "nothing has resolved yet" are opposite facts, and the engine sends
/// `null` for the second precisely so the window does not have to guess.
function renderBundleDeck(telemetry) {
  if (!telemetry) return;

  const floor = telemetry.floor ?? {};
  const counts = telemetry.counts ?? {};
  const land = telemetry.land ?? {};
  const tip = telemetry.tip ?? {};
  const latency = telemetry.latency ?? {};

  // --- where the deck's clock is ------------------------------------------
  setText("bundle-slot", Number.isFinite(floor.headSlot) && floor.headSlot > 0
    ? `slot ${count(floor.headSlot)}`
    : "no slots observed");

  // --- the floor -----------------------------------------------------------
  // In lamports, which is the unit a tip is decided in. A tip is four to seven
  // figures of lamports and would be five leading zeroes as SOL.
  const lamports = Number.isFinite(floor.lamports) ? floor.lamports : null;
  setText("tip-floor", lamports === null ? DASH : `${count(lamports)} lam`);

  const floorEl = field("tip-floor")?.closest(".gov-row");
  if (floorEl) {
    floorEl.title = lamports === null
      ? "The engine has not priced a floor."
      : `${count(lamports)} lamports · ${sol(lamports, 6)} SOL\n` +
        `${count(floor.observedLamports)} observed over ${count(floor.slotsObserved)} slots\n` +
        `x ${micropct(floor.multiplierMicros, 2)}% multiplier\n` +
        CLAMP_NOTE[floor.clamp ?? ""];
  }

  // --- what moved it -------------------------------------------------------
  const saturation = Number.isFinite(floor.saturationMicros) ? floor.saturationMicros : null;
  setText("congestion", saturation === null ? DASH : `${micropct(saturation)}%`);
  // Over four fifths full is where blocks start refusing what they are handed.
  setMeter("congestion-meter", saturation, { warnAbove: 800_000 });

  // `null` is not zero here and the difference is the whole reason the field is
  // nullable: every schedule in this build answers "unknown", and rendering
  // that as 0% would read as "no leader is near" — a measurement nobody made.
  const proximity = floor.proximityMicros;
  setText(
    "leader-proximity",
    proximity === null || proximity === undefined ? "unknown" : `${micropct(proximity)}%`,
  );
  const leaderEl = field("leader-proximity")?.closest(".gov-row");
  if (leaderEl) {
    leaderEl.title =
      proximity === null || proximity === undefined
        ? "No leader schedule is fitted, so proximity is unmeasured rather than zero.\n" +
          "The floor carries no proximity term at all while this reads unknown."
        : `${micropct(proximity)}% of the full proximity term\n` +
          "100% is a connected leader in this slot; it decays over the slots to the next one.";
  }

  // --- how bundles are ending ---------------------------------------------
  const overall = land.overallMicros;
  const rateKnown = Number.isFinite(overall);
  setText("land-rate", rateKnown ? `${micropct(overall)}%` : DASH);
  // Low is the bad direction for this one, so the band runs the other way.
  setMeter("land-rate-meter", rateKnown ? overall : null, { warnBelow: 700_000 });

  const landEl = field("land-rate")?.closest(".gov-row");
  if (landEl) {
    landEl.title = rateKnown
      ? `${count(counts.landed)} landed of ${count(resolvedCount(counts))} resolved\n` +
        `${micropct(land.firstAttemptMicros)}% landed first attempt\n` +
        `market: ${Number.isFinite(land.windowMicros) ? `${micropct(land.windowMicros)}%` : "unobserved"}`
      : "Nothing has resolved yet, so there is no rate — which is not a rate of zero.";
  }

  setText(
    "bundle-states",
    Number.isFinite(counts.live)
      ? `${count(counts.live)} live · ${count(counts.inFlight)} sent`
      : DASH,
  );
  const statesEl = field("bundle-states")?.closest(".gov-row");
  if (statesEl) {
    statesEl.title = Number.isFinite(counts.opened)
      ? `${count(counts.opened)} opened · ${count(counts.retried)} retried\n` +
        `${count(counts.landed)} landed · ${count(counts.rejected)} rejected\n` +
        `${count(counts.evictedRetention)} aged out · ${count(counts.evictedLeaderBoundary)} lost a leader\n` +
        `${count(tip.paidLamports)} lamports paid · ${count(tip.forfeitedLamports)} forfeited`
      : "The deck has not opened a bundle.";
  }

  // --- where the time went -------------------------------------------------
  const settle = latency.priceToLand ?? {};
  setText("bundle-settle", micros(settle.p50Us));
  const settleEl = field("bundle-settle")?.closest(".gov-row");
  if (settleEl) {
    const build = latency.priceToSubmit ?? {};
    const flight = latency.submitToLand ?? {};
    settleEl.title = Number.isFinite(settle.p50Us)
      ? `pricing to landing, p50 ${micros(settle.p50Us)} · p95 ${micros(settle.p95Us)} · p99 ${micros(settle.p99Us)}\n` +
        `ours: ${micros(build.p50Us)} to sign and send\n` +
        `theirs: ${micros(flight.p50Us)} waiting on a block`
      : "Nothing has landed, so nothing has been timed.";
  }
}

/// What the two bounds mean, in the words the tooltip uses.
const CLAMP_NOTE = {
  unclamped: "The window's own number, inside both bounds.",
  lifted: "Lifted to the configured minimum — the window priced below it.",
  cut: "Cut to the configured maximum — the market is asking more than the ceiling.",
  "": "",
};

/// Everything that reached a terminal state, which the engine reports as parts
/// rather than a total because the parts are the useful thing.
function resolvedCount(counts) {
  return (
    (counts.landed ?? 0) +
    (counts.evictedRetention ?? 0) +
    (counts.evictedLeaderBoundary ?? 0) +
    (counts.rejected ?? 0)
  );
}

async function pollBundleTelemetry() {
  if (!invoke || !bundlesSupported) return;
  try {
    renderBundleDeck(await invoke("get_bundle_telemetry"));
    markBridge(true);
  } catch (err) {
    if (isMissingCommand(err)) {
      bundlesSupported = false;
      console.info("[sts] no get_bundle_telemetry in this build; the deck reads as unknown");
    } else {
      markBridge(false);
    }
  }
}

// ---------------------------------------------------------------------------
// what SOL is worth
// ---------------------------------------------------------------------------

// The one control on this window that sends a number into the engine.
//
// It is write-only from here. `get_ingestion_metrics` does not carry the price
// and no other command reports it, so the only thing the window knows about it
// is what `set_sol_price` handed back the last time somebody set it. Before
// that the field says `unset` and is marked as a warning — not a zero and not a
// plausible-looking guess, because a wrong price does not fail loudly. It moves
// every market cap threshold the engine is filtering on and the only visible
// symptom is that nothing ever qualifies.

const SOL_PRICE_COMMAND = "set_sol_price";
/// Micro-dollars per SOL, which is the unit `SolPrice` reports in.
const MICRO_USD_PER_USD = 1_000_000;

function solPriceInput() {
  return document.querySelector('[data-action="sol-price"]');
}

/// Draws whatever the engine last said the price was.
///
/// Written from the command's answer and never from what was typed, for the
/// same reason the replay switch is: a control that reports its own intent
/// rather than the engine's state is one that can read "set" over an engine
/// that refused it.
function renderSolPrice(price) {
  const stat = field("sol-price-stat");
  const input = solPriceInput();
  const micro = Number.isFinite(price?.microUsdPerSol) ? price.microUsdPerSol : null;
  const known = micro !== null && micro > 0;

  if (stat) stat.setAttribute("data-state", known ? "set" : "unset");
  if (!input) return;

  input.setAttribute("aria-invalid", "false");
  // Not while it is being typed into: overwriting the field under the cursor
  // is how a half-entered price gets committed.
  if (known && document.activeElement !== input) {
    input.value = (micro / MICRO_USD_PER_USD).toFixed(2);
  }
  input.title = known
    ? `One SOL is $${(micro / MICRO_USD_PER_USD).toFixed(2)} to the engine. ` +
      `Every market cap threshold is measured against it.`
    : "The engine has no SOL price, so every candidate reads as too small to " +
      "trade. Nothing will be entered until this is set.";
}

async function submitSolPrice(text) {
  const input = solPriceInput();
  if (!invoke || !input) return;

  // Stripped rather than rejected: somebody typing a price into a trading
  // window types `$142.50` about as often as `142.50`, and refusing the first
  // teaches nothing.
  const dollars = Number.parseFloat(String(text).replace(/[$,\s]/g, ""));
  // The command takes whole cents. Rounded rather than truncated, because a
  // price entered as 142.999 is 143.00 to the person who typed it.
  const cents = Number.isFinite(dollars) ? Math.round(dollars * 100) : NaN;

  // Refused here as well as in Rust, and the two refusals are different
  // sentences: this one is "that is not a price", the engine's is "a price of
  // zero would make every candidate look too small to trade".
  if (!Number.isFinite(cents) || cents <= 0) {
    input.setAttribute("aria-invalid", "true");
    return;
  }

  try {
    renderSolPrice(await invoke(SOL_PRICE_COMMAND, { centsPerSol: cents }));
    markBridge(true);
  } catch (err) {
    input.setAttribute("aria-invalid", "true");
    console.warn("[sts] the engine refused that SOL price", err);
  }
}

function wireSolPrice() {
  const input = solPriceInput();
  if (!input) return;

  // Disabled rather than merely inert without an engine, so it looks like what
  // it is instead of like a control that swallows what you type.
  input.disabled = !invoke;
  if (!invoke) {
    input.title = "There is no engine attached to this window.";
    return;
  }

  // `change` and not `input`: this sends a command, and one per keystroke would
  // set the price to $1, then $14, then $142 on the way to $1425. It fires on
  // Enter and on blur, which are the two moments somebody means it.
  input.addEventListener("change", () => submitSolPrice(input.value));
  renderSolPrice(null);
}

// ---------------------------------------------------------------------------
// 2. the radar
// ---------------------------------------------------------------------------

// How many rows are kept. Past this the oldest is dropped: the radar is a view
// of what is live, and an unbounded list of everything ever seen is a memory
// leak that also happens to be unreadable.
const MAX_RADAR_ROWS = 200;

// What counts as graduating, in basis points of curve progress. A coin this far
// along is close to migrating off the curve, which is a different trade from a
// fresh launch and is why it gets its own filter.
const GRADUATING_BPS = 8000;

// Keyed by curve account, which is the only stable identity ingestion has.
// Not by mint: the curve account is a PDA of the mint, the mapping only runs
// one way, and `CandidateView` carries no mint at all.
const radar = new Map();
let radarFilter = "all";
let selectedAccount = null;
let highestSlot = 0;

function candidateMatchesFilter(entry) {
  if (!candidateMatchesSearch(entry)) return false;
  if (radarFilter === "fast path") return entry.route === "fastPath";
  if (radarFilter === "graduating") {
    return entry.view.curveComplete || entry.view.curveProgressBps >= GRADUATING_BPS;
  }
  return true;
}

function buildRadarRow(entry) {
  const row = document.createElement("div");
  row.className = "radar-grid row";
  row.setAttribute("role", "row");
  row.setAttribute("aria-selected", "false");
  row.dataset.account = entry.view.account;
  row.tabIndex = 0;

  // Token column: account key + resolved name
  const tokenCell = document.createElement("span");
  tokenCell.className = "sym";
  tokenCell.textContent = shortKey(entry.view.account);
  const nameSpan = document.createElement("span");
  nameSpan.className = "token-name";
  nameSpan.dataset.field = `token-name-${entry.view.account}`;
  tokenCell.append(nameSpan);
  row.append(tokenCell);

  const curve = document.createElement("span");
  curve.className = "curve num";
  curve.append(document.createElement("span"));
  row.append(curve);

  row.append(cell("num", ""));
  row.append(cell("num dim", ""));

  row.addEventListener("click", () => selectCandidate(entry.view.account));
  row.addEventListener("keydown", (event) => {
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      selectCandidate(entry.view.account);
    }
  });

  return row;
}

function paintRadarRow(row, entry) {
  const view = entry.view;
  const [, curve, liquidity, age] = row.children;

  const progress = bps(view.curveProgressBps);
  const pct = `${Math.min(100, (view.curveProgressBps ?? 0) / 100).toFixed(1)}%`;
  if (curve.style.getPropertyValue("--pct") !== pct) {
    curve.style.setProperty("--pct", pct);
  }
  if (curve.firstChild.textContent !== progress) {
    curve.firstChild.textContent = progress;
  }

  // Pool lamports, not market cap: this is real SOL in the curve, which is what
  // could actually be sold back into. Market cap is a number the curve implies.
  const liq = sol(view.poolLamports);
  if (liquidity.textContent !== liq) liquidity.textContent = liq;

  // Age is measured in slots since this process first saw the account, not
  // since the create instruction — the frame does not carry one. After a
  // restart everything already alive looks new, which is the safe direction.
  const seen = duration((view.slotsSinceLaunch ?? 0) * MS_PER_SLOT);
  if (age.textContent !== seen) age.textContent = seen;

  row.title =
    `curve ${view.account}\n` +
    `creator ${view.creator}\n` +
    `${entry.route === "fastPath" ? "fast path" : "standard"} · slot ${view.slot} · ` +
    `${entry.provider ?? view.provider}\n` +
    `pool ${sol(view.poolLamports)} SOL · dispatch ${micros(entry.dispatchLatencyUs)}`;
}

/// Folds one candidate event into the radar.
///
/// The same curve account is reported again on every update it receives, so
/// this updates in place rather than appending. Appending would fill the pane
/// with one token repeated, and — worse — would make a busy coin look like a
/// busy market.
function onCandidate(event) {
  const view = event?.view;
  if (!view?.account) return;

  if (typeof view.slot === "number" && view.slot > highestSlot) {
    highestSlot = view.slot;
    setText("slot", count(highestSlot));
  }

  const container = region("radar-rows");
  if (!container) return;

  let entry = radar.get(view.account);
  const isNew = !entry;
  if (entry) {
    entry.view = view;
    entry.route = event.route;
    entry.marketCapUsdCents = event.marketCapUsdCents;
    entry.dispatchLatencyUs = event.dispatchLatencyUs;
  } else {
    entry = {
      view,
      route: event.route,
      provider: view.provider,
      marketCapUsdCents: event.marketCapUsdCents,
      dispatchLatencyUs: event.dispatchLatencyUs,
      firstSeenAtMs: event.receivedAtMs,
      row: null,
    };
    entry.row = buildRadarRow(entry);
    radar.set(view.account, entry);
    container.prepend(entry.row);
    evictOldestRows(container);

    // Resolve token name from pump.fun API
    resolveTokenName(view.account);

    // Update stats for new candidates only
    updateRadarStats(entry);
  }

  paintRadarRow(entry.row, entry);
  applyRadarFilter();

  // Update token name display if now resolved
  const nameDisplay = getTokenDisplay(view.account);
  if (nameDisplay) {
    const nameEl = entry.row.querySelector(".token-name");
    if (nameEl && !nameEl.textContent) nameEl.textContent = nameDisplay;
  }

  // Check for toast-worthy events: high curve progress or graduation
  if (isNew) {
    const progress = view.curveProgressBps ?? 0;
    if (view.curveComplete) {
      showToast(`${shortKey(view.account)} graduated!`, "live");
    } else if (progress >= 8000 && progress < 8500) {
      showToast(`${shortKey(view.account)} at ${bps(progress)}% — nearing graduation`, "warn", 4000);
    }
  }

  onTick(event);

  if (view.account === selectedAccount) renderCurveModule(view);
}

function evictOldestRows(container) {
  while (radar.size > MAX_RADAR_ROWS) {
    const last = container.lastElementChild;
    if (!last) break;
    radar.delete(last.dataset.account);
    last.remove();
  }
}

function applyRadarFilter() {
  let shown = 0;
  for (const entry of radar.values()) {
    const visible = candidateMatchesFilter(entry);
    entry.row.hidden = !visible;
    if (visible) shown += 1;
  }
  setText("candidate-count", String(shown));
  setPopulated("radar", shown > 0);
}

function selectCandidate(account) {
  selectedAccount = account;
  activeList = "radar";
  for (const [key, entry] of radar) {
    entry.row.setAttribute("aria-selected", key === account ? "true" : "false");
  }

  // The subject filter is a question about the selection, so changing the
  // selection changes its answer. Re-applied whether or not the filter is on:
  // the count in the head is "shown of held" either way.
  applyTickFilter();

  const entry = radar.get(account);
  if (!entry) {
    clearCurveModule();
    clearClusterModule();
    return;
  }
  const view = entry.view;

  renderCurveModule(view);

  // The subject line takes what the frame actually carries. The mint stays an
  // em dash because resolving it needs the create instruction, which ingestion
  // never sees — an em dash is the true answer, not a missing one.
  setText("subject-symbol", shortKey(view.account));
  field("subject-symbol")?.classList.remove("faint");
  // Show resolved token name if available
  const tokenInfo = tokenNameCache.get(view.account);
  if (tokenInfo?.symbol) {
    const symEl = field("subject-symbol");
    if (symEl) {
      symEl.textContent = `${tokenInfo.symbol}`;
      if (tokenInfo.name && tokenInfo.name !== tokenInfo.symbol) {
        symEl.textContent = `${tokenInfo.symbol} · ${tokenInfo.name}`;
      }
    }
  }
  setText("subject-mint", DASH);
  setText("subject-creator", view.creator);
  setText("subject-curve-account", view.account);
  setText("subject-slot", count(view.slot));

  // The empty state was written for "nothing is selected", and that stops being
  // true the moment a row is clicked — leaving it would have the pane deny a
  // selection it is already displaying the creator of. It now says the thing
  // that *is* true of a selected subject with no report: nobody has run the
  // analysis. Rewritten before the fetch so the pane is never briefly claiming
  // no subject is selected while one is.
  const clusterEmpty = region("cluster-empty");
  if (clusterEmpty) {
    clusterEmpty.querySelector(".empty-title").textContent = "not analysed";
    clusterEmpty.querySelector(".empty-note").textContent =
      "This candidate is selected and no forensic report has been recorded " +
      "for it. The scores above are blank rather than zero — a zero here " +
      "would read as a token that had been looked at and cleared.";
  }

  loadClusterReport(view.account);
}

function wireRadarFilters() {
  for (const chip of document.querySelectorAll(".pane-tools .chip")) {
    chip.addEventListener("click", () => {
      for (const other of document.querySelectorAll(".pane-tools .chip")) {
        other.setAttribute("aria-pressed", other === chip ? "true" : "false");
      }
      radarFilter = chip.textContent.trim();
      applyRadarFilter();
    });
  }
}

// ---------------------------------------------------------------------------
// 2b. the bonding curve and its migration
// ---------------------------------------------------------------------------

// Everything in this block is read straight off the `CandidateView` of the
// selected subject. Three of the four facts are the raw reserves and the fourth
// is one closed form over one of them; nothing is modelled, smoothed, or held
// over from an earlier slot. With no subject, or with a backend too old to send
// the virtual reserves, the cells stay em dashes rather than becoming zeroes —
// a reserve ratio rendered as 0% says the curve holds nothing, which is a very
// different claim from "this build was not told".

const CURVE_FIELDS = [
  "migration-pct",
  "migration-remaining",
  "curve-real-sol",
  "curve-virtual-sol",
  "curve-reserve-ratio",
  "sandwich-floor",
];

function clearCurveModule() {
  for (const name of CURVE_FIELDS) setText(name, DASH);
  const fill = field("migration-fill");
  if (fill) {
    fill.style.setProperty("--pct", "0%");
    fill.removeAttribute("data-complete");
    fill.removeAttribute("data-risk");
  }
  setBadge("unknown", DASH, "No subject is selected.");
}

// ---------------------------------------------------------------------------
// 2b-ii. the wallet cluster, and whether its evidence survived being checked
// ---------------------------------------------------------------------------

// Set false the first time `get_cluster_report` comes back saying the command is
// not there, the same way every other optional command on this window is
// handled: asked once, then left alone.
let clusterSupported = true;

// The report on screen, so a poll that returns the same one does not rebuild
// rows the operator may be selecting text in.
let clusterShown = null;

const CLUSTER_FIELDS = [
  "cluster-hhi",
  "cluster-temporal",
  "cluster-entropy",
  "cluster-separation",
];

function setClusterBadge(risk, text, title) {
  const badge = field("cluster-evidence");
  if (!badge) return;
  if (badge.dataset.risk !== risk) badge.dataset.risk = risk;
  if (badge.textContent !== text) badge.textContent = text;
  if (badge.title !== title) badge.title = title;
}

/// What the chain said about the edges the report rests on.
///
/// Four states and they are deliberately not collapsed into two. "Nobody
/// checked" is not "checked and clean", and "one provider agreed" is not "two
/// providers agreed" — `chainproof.rs` keeps all three apart and a badge that
/// merged them would be undoing the distinction at the last step.
function clusterEvidence(report) {
  const proof = report?.proof;
  if (!proof) {
    return [
      "unknown",
      "unverified",
      "No witness was supplied with this analysis, so its funding edges are " +
        "what the request asserted rather than what the chain shows. That is " +
        "not a pass: nothing was checked.",
    ];
  }
  if (proof.contradicted > 0) {
    return [
      "contradicted",
      `${count(proof.contradicted)} contradicted`,
      `The chain contradicts ${count(proof.contradicted)} of ` +
        `${count(proof.claimed)} asserted funding edges. Those edges were ` +
        "dropped before the graph was built, so the numbers above are over " +
        "what survived.",
    ];
  }
  if (proof.complete) {
    return [
      "verified",
      "chain-verified",
      `All ${count(proof.claimed)} funding edges confirmed by the provider ` +
        "quorum. This is the only state in which a report may clear a launch " +
        "rather than only block one.",
    ];
  }
  const unknown = (proof.singleSource ?? 0) + (proof.unverified ?? 0);
  return [
    "unconfirmed",
    `${count(unknown)} unconfirmed`,
    `${count(proof.confirmed)} of ${count(proof.claimed)} funding edges ` +
      `confirmed by the quorum; ${count(unknown)} are UNKNOWN and are carried ` +
      "at a discount. A lineage with an unconfirmed edge in it may block an " +
      "entry and may never clear one.",
  ];
}

/// The kinds a trail may end at but never pass through.
///
/// `tracer.rs` makes these absorbing because paying out to everybody links
/// everybody: transit one and the graph collapses into a blob in which every
/// customer of an exchange is related to every other. So a creator whose trail
/// ends at one of these has an **exit node**, not a funder, and the only thing
/// that has been learned is which venue the money came out of. Naming it as a
/// funder would be the single most misleading sentence this pane could print.
const EXIT_KINDS = {
  EXCHANGE: "an exchange",
  BRIDGE: "a bridge",
  MIXER: "a mixer",
  // Deliberately not "an exchange". The graph observed a fan-out; it has not
  // identified a venue, and `NodeKind::Router` says so in as many words.
  ROUTER: "an address the graph inferred was a router",
};

/// `InsiderReason` in the words an operator reads, keyed by the spelling
/// `clustering.rs` serialises.
///
/// A reason this map does not know is shown as the engine spelled it rather
/// than dropped. A window that silently omits a reason it has not been taught
/// is a window that under-reports exactly when the engine has learned something
/// new, which is the wrong direction for this pane to fail in.
const INSIDER_REASONS = {
  SHARED_FUNDER: "one origin paid for a majority of this launch's buying",
  SYNCHRONISED_OPEN: "the first buys landed inside one synchrony kernel",
  CONCENTRATED_OWNERSHIP: "the cluster holds a material share of the supply",
  UNIFORM_FUNDING: "the wallets were funded in near-identical amounts",
  PRE_MIGRATION_ACCUMULATION: "the buying landed before the curve migrated",
  COSTUME_RING: "the holdings have the one-wallet-and-costumes shape",
  DEV_SHARES_ORIGIN: "the creator traces back to the cluster's own origin",
  DEV_FUNDED_CLUSTER: "the creator is that origin: it paid for this book",
};

function reasonWords(reason) {
  return INSIDER_REASONS[reason] ?? String(reason);
}

/// The strongest claim the dev trace supports, as a sentence.
///
/// Ordered rather than summarised, and the order is the point. A launch can be
/// true of several of these at once — a creator that funded three buyers *and*
/// came out of a router — and printing all of them would bury the finding in
/// the context. So the list below runs from "this is the operator" down to "we
/// looked and learned nothing", and the first one that fires is what the strip
/// says. The rest are on the title.
///
/// Two of them are read in a deliberate order. `fundedBuyers` is checked before
/// the exit-node test because it is a claim about the **dev wallet paying**,
/// which stays true whatever funded the dev; the exit-node test is checked
/// before `siblings` because it is a claim about the dev's **origin**, and
/// "twelve buyers share the creator's funder" is a sentence that must never be
/// printed about Binance.
///
/// Returns `[text, title, strong]`. `strong` un-faints the line: it marks the
/// cases where something was found, as opposed to the cases where the honest
/// answer is that nothing was.
function devClaim(report, note) {
  if (!report) {
    // Two different silences, and the strip must not read them the same way.
    // "Nothing is selected" is the window having nothing to show; "nothing was
    // recorded" is the window having asked and been told nobody looked. The
    // caller already distinguishes them for the badge, so the wording comes
    // from there rather than being guessed at again here.
    return [
      note ? "No analysis has been recorded for this subject." : "No subject is selected.",
      note ??
        "Pick a candidate on the left to see who deployed it, who paid them, " +
          "and whether the same hand paid the opening buyers.",
      false,
    ];
  }

  const dev = report.dev ?? null;
  if (!dev) {
    return [
      "This report names no creator.",
      "The analysis ran without a dev wallet, so nothing here has been checked " +
        "about whoever deployed this launch. UNKNOWN, and not a clean result.",
      false,
    ];
  }

  const funded = Array.isArray(dev.fundedBuyers) ? dev.fundedBuyers : [];
  const siblings = Array.isArray(dev.siblings) ? dev.siblings : [];
  const truncation = dev.trace?.truncated
    ? "\nA budget bound the traversal behind this trail, so what it found is a " +
      "lower bound: more search could only find more funding, never less."
    : "";

  if (funded.length > 0 || dev.fundsCluster) {
    const detail = funded.length
      ? `${count(funded.length)} opening ${funded.length === 1 ? "buyer was" : "buyers were"} ` +
        `funded out of the deploy wallet itself, for ${sol(dev.fundedBuyLamports)} SOL:\n` +
        funded.map(shortKey).join(", ")
      : "The deploy wallet is itself a cluster root in this report.";
    return [
      funded.length
        ? `The creator paid for ${count(funded.length)} of the opening ` +
          `${funded.length === 1 ? "buyer" : "buyers"} itself.`
        : "The creator paid for part of the opening book itself.",
      `${detail}\n\nThis is the strongest shape in the report and it is kept ` +
        "apart from sharing an origin on purpose: a dev and a buyer out of one " +
        "exchange share an origin too. These wallets were paid by the deployer." +
        truncation,
      true,
    ];
  }

  const exit = EXIT_KINDS[dev.originKind];
  if (exit && dev.origin) {
    return [
      `The creator came out of ${exit}.`,
      `The trail ends at ${dev.origin} after ${count(dev.hops)} ` +
        `${dev.hops === 1 ? "hop" : "hops"}, and it ends there rather than ` +
        "passing through: an exit node pays out to everybody, so transiting " +
        "one would link every one of its customers to every other.\n\n" +
        "This links the creator to nobody. It is neither a finding nor a " +
        "clearance — it is the venue the money came out of, and no more." +
        truncation,
      false,
    ];
  }

  if (dev.clusterRoot) {
    return [
      "The creator shares an origin with the loudest cluster.",
      `Whoever paid the creator also paid the wallets clustered behind ` +
        `${dev.clusterRoot}. The creator is ${count(dev.hops)} ` +
        `${dev.hops === 1 ? "hop" : "hops"} from that origin.\n\n` +
        "Sharing an origin is weaker than being one: this says the creator " +
        "and those buyers were funded by the same hand, not that the creator " +
        "was that hand." +
        truncation,
      true,
    ];
  }

  if (siblings.length > 0) {
    return [
      `${count(siblings.length)} opening ${siblings.length === 1 ? "buyer was" : "buyers were"} ` +
        "paid by whoever paid the creator.",
      `${siblings.map(shortKey).join(", ")}\n\n` +
        `They trace back to ${dev.origin ?? "the creator's own origin"}, the ` +
        `same address the creator traces back to, ${count(dev.hops)} ` +
        `${dev.hops === 1 ? "hop" : "hops"} away. Together they bought ` +
        `${sol(dev.siblingBuyLamports)} SOL of this launch.` +
        truncation,
      true,
    ];
  }

  if (dev.origin) {
    return [
      "The creator's funder paid no other opening buyer.",
      `The trail reaches ${dev.origin} in ${count(dev.hops)} ` +
        `${dev.hops === 1 ? "hop" : "hops"}, and no other wallet that bought ` +
        "the open traces back to it.\n\nThat is a real answer and a narrow " +
        "one. It says this particular link was looked for and not found; it " +
        "does not say the launch is clean." +
        truncation,
      false,
    ];
  }

  return [
    "Nobody funded the creator inside the lookback.",
    "UNKNOWN. The traversal found no origin for the deploy wallet within the " +
      "window it was given, and `tracer.rs` is explicit that this is neither " +
      "'self-funded' nor 'clean' — a zero here would read as 'we looked and " +
      "nobody funded this', which is a claim nothing has made." +
      truncation,
    false,
  ];
}

/// Writes one strip cell, its tooltip and whether it knows anything.
function setDevField(name, text, title, known) {
  const el = field(name);
  setText(name, text);
  if (!el) return;
  if (el.title !== title) el.title = title;
  el.classList.toggle("faint", !known);
}

/// The creator strip: who deployed this, who paid them, and what it scored.
///
/// Rendered from the same report as everything else in the pane and never from
/// a second call, so the strip and the list below it can never be describing
/// two different analyses.
function renderDevTrace(report, note) {
  const dev = report?.dev ?? null;
  const insider = report?.insider ?? null;

  setDevField(
    "cluster-creator",
    dev?.wallet ? shortKey(dev.wallet) : DASH,
    dev?.wallet
      ? `${dev.wallet}\n\nThe wallet that deployed this launch.`
      : "No creator is named in this report. UNKNOWN, not clean.",
    Boolean(dev?.wallet),
  );

  // An absorbing origin still shows its address — which venue the money came
  // out of is worth knowing — but it does not get the styling a person's
  // address gets, because the two are the same nine characters and opposite
  // amounts of evidence. The cell withholds the treatment, the title names the
  // kind, and the sentence underneath says it in words. Putting "(exchange)"
  // in the cell itself was the other option and it does not fit: this column
  // is a third of the strip and would ellipsis the distinction away.
  const exit = dev ? EXIT_KINDS[dev.originKind] : null;
  setDevField(
    "cluster-dev-origin",
    dev?.origin ? shortKey(dev.origin) : DASH,
    dev?.origin
      ? `${dev.origin}\n\n${exit ? `An exit node — ${exit}. ` : ""}` +
        `${count(dev.hops)} ${dev.hops === 1 ? "hop" : "hops"} back from the creator.`
      : "The creator's origin is UNKNOWN. That is neither 'self-funded' nor " +
        "'clean': the traversal looked and did not reach one.",
    Boolean(dev?.origin) && !exit,
  );

  const score = insider?.scoreMicros;
  const known = typeof score === "number" && Number.isFinite(score);
  const partial =
    typeof insider?.measuredWeightBps === "number" && insider.measuredWeightBps < 10_000;
  const reasons = Array.isArray(insider?.reasons) ? insider.reasons : [];
  setDevField(
    "cluster-insider",
    known ? micropct(score, 1) : DASH,
    known
      ? `${reasons.map((r) => `• ${reasonWords(r)}`).join("\n") || "No reason fired."}` +
        (partial
          ? `\n\nMeasured over ${bps(insider.measuredWeightBps, 0)}% of the ` +
            "evidence: a component was UNKNOWN and left out of the mean rather " +
            "than scored as zero. A missing test is not a passed test, and a " +
            "score resting on half the evidence should be read as one."
          : "\n\nEvery component was measurable.") +
        (insider?.truncated
          ? "\n\nBudget-bound, so this is a lower bound. It may block an entry " +
            "and may never clear one."
          : "")
      : "No cluster in this report was judged an accumulation. That is not a " +
        "clean reading: §3.5 refuses a finding outright when either of its two " +
        "primary components is UNKNOWN, so this is also what a half-traced " +
        "launch looks like.",
    known,
  );

  const [text, title, strong] = devClaim(report, note);
  setDevField("cluster-dev-claim", text, title, strong);
}

function clearClusterModule(note) {
  clusterShown = null;
  for (const name of CLUSTER_FIELDS) {
    setText(name, DASH);
    field(name)?.classList.add("faint");
  }
  const rows = region("cluster-rows");
  if (rows) rows.replaceChildren();
  setPopulated("cluster", false);
  setClusterBadge("unknown", DASH, note ?? "No subject is selected.");
  renderDevTrace(null, note);
}

/// One row per hand, not one per wallet.
///
/// The grid's first column is headed `wallet` and a cluster root is one — the
/// address that paid for the group. Per-wallet shares are deliberately not
/// invented here: `FundingCluster` reports its members sorted by address and
/// their funding sorted by amount, and those two lists are not index-aligned,
/// so pairing them off would be manufacturing a number the engine did not send.
/// The members are on the row's tooltip, which is where this pane already puts
/// detail that does not fit a cell.
function clusterRow(cluster) {
  const row = document.createElement("div");
  row.className = "cluster-grid";
  row.setAttribute("role", "row");

  const sybil = cluster.temporalInfluenceMicros;
  const reading =
    typeof sybil === "number" ? micropct(sybil, 0) : DASH;

  row.append(
    cell("mono", shortKey(cluster.root)),
    cell("num", bps(cluster.flowShareBps, 0)),
    cell("num", clock(cluster.firstBuyMs)),
    cell("num", reading),
  );

  const members = Array.isArray(cluster.wallets) ? cluster.wallets : [];
  const lines = [
    `${cluster.root}`,
    `${count(cluster.walletCount)} wallets, ` +
      `${bps(cluster.flowShareBps, 1)}% of the launch's buying`,
    members.map(shortKey).join(", "),
  ];
  if (cluster.sharedHub) {
    lines.push(
      "Rooted at an exchange, bridge, mixer or inferred router. Reported and " +
        "never scored: sharing an exit node is evidence of a popular exit " +
        "node, not of common ownership.",
    );
  }
  if (cluster.costumeRing) {
    lines.push("Holdings have the one-wallet-and-costumes shape.");
  }
  if (cluster.truncated) {
    lines.push("A budget bound the traversal behind this row, so its numbers are lower bounds.");
  }
  if (sybil === null || sybil === undefined) {
    lines.push(
      "Sybil reading is UNKNOWN — one of its two halves could not be " +
        "measured, and a zero here would read as 'these wallets are " +
        "unrelated', which is the opposite of what was learned.",
    );
  }
  row.title = lines.join("\n");
  return row;
}

/// Fills the four scores and the cluster list from one stored report.
function renderClusterModule(report) {
  if (!report) {
    clearClusterModule(
      "No analysis has been recorded for this subject. That is an absence of " +
        "evidence, not a clean result.",
    );
    return;
  }

  // Identity, so two polls that return the same report leave the rows and any
  // text being selected in them alone.
  //
  // The whole report rather than a handful of fields off it. A digest of
  // "mint, cluster count, insider score, proof counts" is cheaper and it is
  // wrong: a re-analysis that moves a cluster's root, its members or its
  // `sharedHub` flag lands on the same digest and the pane keeps showing the
  // previous answer. Serialising a few kilobytes once per slow tick is not the
  // cost worth optimising here; showing a stale forensic reading is not a cost
  // at all, it is a defect.
  const stamp = JSON.stringify(report);
  if (clusterShown === stamp) return;
  clusterShown = stamp;

  const top = report.clusters?.[0] ?? null;

  // The three the engine actually computes. Each is left an em dash when its
  // own answer is UNKNOWN rather than filled with a zero — `clustering.rs`
  // returns `null` exactly where "no measurement" is the honest answer, and
  // rendering that as 0.0 would turn every one of those into a clean reading.
  setClusterScore("cluster-hhi", top?.holdingHhiBps, (v) => bps(v, 0));
  setClusterScore("cluster-temporal", top?.temporalInfluenceMicros, (v) => micropct(v, 1));
  setClusterScore("cluster-entropy", top?.fundingRing?.uniformityMicros, (v) => micropct(v, 1));
  // `separation` stays UNKNOWN on purpose. Spectral separation is section 4 of
  // RISK_AND_SYBIL_SPEC.md and nothing in this build computes it; a number here
  // would be the window inventing one.
  setClusterScore("cluster-separation", null, () => DASH);

  const rows = region("cluster-rows");
  if (rows) {
    rows.replaceChildren(...(report.clusters ?? []).map(clusterRow));
  }
  setPopulated("cluster", (report.clusters?.length ?? 0) > 0);

  if ((report.clusters?.length ?? 0) === 0) {
    const empty = region("cluster-empty");
    if (empty) {
      empty.querySelector(".empty-title").textContent = "no cluster resolved";
      empty.querySelector(".empty-note").textContent =
        `${count(report.participants)} opening buyers were traced and none of ` +
        "them grouped behind a shared origin. " +
        `${count(report.unclusteredWallets)} are UNKNOWN and counted in every ` +
        "denominator rather than assumed independent.";
    }
  }

  const [risk, text, title] = clusterEvidence(report);
  setClusterBadge(risk, text, title);

  // Last, and from the same report. The strip above the list is the only part
  // of this pane that can speak when the list is empty — a launch with two
  // opening buyers resolves no cluster and can still have a creator that paid
  // for both of them — so it is rendered on every path, not only the populated
  // one.
  renderDevTrace(report);
}

function setClusterScore(name, value, format) {
  const el = field(name);
  const known = typeof value === "number" && Number.isFinite(value);
  setText(name, known ? format(value) : DASH);
  if (el) el.classList.toggle("faint", !known);
}

/// Asks the engine for the report filed under this subject.
///
/// **The key is the bonding curve account, which is the only identity this
/// window has.** `clustering.rs` files a report by mint; the curve is a PDA of
/// the mint, so the mapping runs one way and resolving it needs the create
/// instruction, which ingestion never sees — `CandidateView::account` says so
/// in as many words, and `subject-mint` is an em dash for the same reason. A
/// report filed under a mint is therefore not found by this, and the pane says
/// "not analysed" rather than implying the launch came back clean.
///
/// `None` from the command is a real answer and means nobody has run the
/// analysis, which the empty state says in those words. It is emphatically not
/// the same as the analysis having found nothing, and the two are rendered
/// differently for that reason.
async function loadClusterReport(mint) {
  if (!invoke || !clusterSupported || !mint) {
    renderClusterModule(null);
    return;
  }
  try {
    renderClusterModule(await invoke("get_cluster_report", { mint }));
    markBridge(true);
  } catch (err) {
    if (isMissingCommand(err)) {
      clusterSupported = false;
      console.info("[sts] no get_cluster_report in this build; the cluster pane reads as unknown");
      renderClusterModule(null);
    } else {
      markBridge(false);
    }
  }
}

function setBadge(risk, text, title) {
  const badge = field("sandwich-badge");
  if (!badge) return;
  if (badge.dataset.risk !== risk) badge.dataset.risk = risk;
  if (badge.textContent !== text) badge.textContent = text;
  if (badge.title !== title) badge.title = title;
}

/// Draws the curve module for one observation.
///
/// Called on selection and again on every later observation of the same
/// account, so the bar is the curve as of the newest slot the window has seen
/// rather than as of the click.
function renderCurveModule(view) {
  if (!view) {
    clearCurveModule();
    return;
  }

  // --- migration ----------------------------------------------------------
  // `curveProgressBps` is measured against the 85 SOL of *real* reserves the
  // curve graduates at, and it is capped there by the engine. `curveComplete`
  // is not "100%" — §17 of the replay specification makes graduation a hard
  // branch, and the two are drawn differently for that reason.
  const progressBps = Number.isFinite(view.curveProgressBps) ? view.curveProgressBps : null;
  const complete = view.curveComplete === true;
  const fill = field("migration-fill");

  if (progressBps === null) {
    setText("migration-pct", DASH);
    setText("migration-remaining", DASH);
    if (fill) fill.style.setProperty("--pct", "0%");
  } else {
    const pct = Math.min(100, progressBps / 100);
    setText("migration-pct", `${bps(progressBps)}%`);
    if (fill) fill.style.setProperty("--pct", `${pct.toFixed(1)}%`);

    const remaining = Math.max(0, PUMP_GRADUATION_LAMPORTS - (view.poolLamports ?? 0));
    setText(
      "migration-remaining",
      complete
        ? "migrated"
        : `${sol(remaining)} of ${sol(PUMP_GRADUATION_LAMPORTS)} SOL left`,
    );
  }

  if (fill) {
    if (complete) fill.dataset.complete = "true";
    else fill.removeAttribute("data-complete");
  }

  // --- the two reserves ---------------------------------------------------
  const realSol = Number.isFinite(view.poolLamports) ? view.poolLamports : null;
  const virtualSol = Number.isFinite(view.virtualSolReserves)
    ? view.virtualSolReserves
    : null;

  setText("curve-real-sol", realSol === null ? DASH : sol(realSol));
  setText("curve-virtual-sol", virtualSol === null ? DASH : sol(virtualSol));

  // The ratio is what fraction of the price-setting reserve is actually there
  // to be sold into. It rises along the curve — a fresh launch prices off 30
  // virtual SOL with none of it real — and it is the number that says how much
  // of a quoted market cap an exit could realise.
  if (realSol === null || virtualSol === null || virtualSol <= 0) {
    setText("curve-reserve-ratio", DASH);
  } else {
    setText("curve-reserve-ratio", `${((realSol / virtualSol) * 100).toFixed(1)}%`);
  }

  // --- the sandwich reading ----------------------------------------------
  renderSandwichBadge(virtualSol, complete);
}

/// The sandwich floor, and what it means for a buy of ordinary size.
///
/// The badge grades the curve, not any order this window intends to send. The
/// engine has not been asked for a size and inventing one to colour a badge
/// with would be a claim about what it is about to do.
function renderSandwichBadge(virtualSol, complete) {
  if (complete) {
    setText("sandwich-floor", DASH);
    setBadge(
      "graduated",
      "graduated",
      "The curve has migrated. There is no curve quote to sandwich; whatever " +
        "this token now trades on is a different pool with its own book.",
    );
    return;
  }

  const floor = sandwichFloorLamports(virtualSol);
  if (floor === null) {
    setText("sandwich-floor", DASH);
    setBadge(
      "unknown",
      "unknown",
      "The virtual SOL reserve has not been reported for this account, and the " +
        "threshold is written in terms of it. Nothing is assumed in its place.",
    );
    return;
  }

  setText("sandwich-floor", `${sol(floor)} SOL`);

  // Below the floor no front-run of any size clears fees. At or above it, a buy
  // the size of the corpus median already pays one.
  const exposed = floor <= CORPUS_MEDIAN_FIRST_BUY_LAMPORTS;
  const explanation =
    `Front-running pays only on buys above ${sol(floor)} SOL here ` +
    `(b > φy/(1−φ)², φ = ${CURVE_FEE_BPS} bps, ` +
    `y = ${sol(virtualSol)} SOL virtual).\n` +
    `The median first buy in the corpus is ${sol(CORPUS_MEDIAN_FIRST_BUY_LAMPORTS)} SOL, ` +
    `which is ${exposed ? "above" : "below"} that floor.`;

  setBadge(
    exposed ? "exposed" : "guarded",
    exposed ? "sandwich pays" : "below floor",
    explanation,
  );

  const fill = field("migration-fill");
  if (fill) {
    if (exposed) fill.dataset.risk = "exposed";
    else fill.removeAttribute("data-risk");
  }
}

// ---------------------------------------------------------------------------
// 2c. the tick stream forensic inspector
// ---------------------------------------------------------------------------

// A tick is one observation of one curve account — exactly what an `ingestion`
// telemetry line already carries. The list adds the two things an absolute
// number cannot show on its own: what the price did since the last observation
// of the *same* account, and how the SOL that moved compares with how much
// usually moves there.
//
// Both are derived from pairs of real observations. Neither is interpolated
// across a gap, and neither is reported at all until there is a pair.

const MAX_TICKS = 500;

/// How far outside an account's usual movement counts as an anomaly.
///
/// A screening threshold, not a test. The multiple itself is in the column
/// beside the flag precisely so the operator can disagree with this number
/// without having to know what it is.
const ANOMALY_MULTIPLE = 4;

/// How many earlier moves an account needs before the ratio says anything.
///
/// Under this the column is an em dash and the row is not flagged, because a
/// "4x the usual" computed from one earlier observation is a statement about
/// nothing.
const ANOMALY_MIN_SAMPLES = 3;

/// How many moves per account are kept for the median. Bounded so a busy
/// account cannot grow this without limit; long enough that the median is not
/// dominated by the last few seconds.
const ANOMALY_WINDOW = 64;

const ticks = [];
const tickHistory = new Map();
let tickCursor = -1;

/// The order the rows are drawn in.
///
/// `ticks` is arrival order and stays that way whatever the operator sorts by:
/// it is what the ring buffer drops the oldest from and what the cursor indexes
/// into, and both of those mean "the observation that came in first", not "the
/// row nearest the bottom". Sorting reorders this array and the DOM to match,
/// and touches nothing else. With no sort chosen the two are the same list.
const tickOrder = [];

/// A monotonic arrival number, for breaking ties in a sort.
///
/// Two rows with the same δ have to land in *some* order, and it has to be the
/// same order every time or the list reshuffles under the reader on the next
/// tick. Arrival is that order, newest first, which is what the list does when
/// nothing is sorted at all.
let tickSeq = 0;

const tickFilter = {
  fromSlot: null,
  minAbsDeltaBps: null,
  minVolumeMultiple: null,
  anomalyOnly: false,
  sandwichOnly: false,
  subjectOnly: false,
};

/// What the stream is sorted by, and which way.
///
/// `key` of `null` is arrival order — not a sort of the slot column, which is a
/// different claim: slots can arrive out of order and the difference between
/// "what the chain says happened" and "what this pipeline saw" is one of the
/// things this pane exists to show. So arrival is a state the operator can get
/// back to, and the header cycles descending → ascending → arrival rather than
/// toggling between two.
const tickSort = { key: null, dir: "desc" };

/// The sortable columns, each as the number it sorts on.
///
/// `null` means the row has nothing to sort on — a first observation has no
/// delta, an account with no earlier moves has no multiple — and those rows go
/// last in both directions. An em dash is not a small number and sorting it as
/// one would put every row that says nothing at the top of an ascending sort.
const TICK_SORT_KEYS = {
  slot: (tick) => tick.slot,
  mcap: (tick) => tick.mcapLamports,
  delta: (tick) => tick.priceDeltaBps,
  flow: (tick) => tick.flowLamports,
  beta: (tick) => tick.betaBps,
  vol: (tick) => tick.volumeMultiple,
};

/// Folds one candidate observation into the stream.
function onTick(event) {
  const view = event?.view;
  if (!view?.account) return;
  const container = region("tick-rows");
  if (!container) return;

  const account = view.account;
  const mcap = Number.isFinite(view.marketCapLamports) ? view.marketCapLamports : null;
  const realSol = Number.isFinite(view.poolLamports) ? view.poolLamports : null;

  const prior = tickHistory.get(account);
  const priceDeltaBps =
    prior && prior.mcap !== null && mcap !== null ? deltaBps(mcap, prior.mcap) : null;
  const flowLamports =
    prior && prior.realSol !== null && realSol !== null ? realSol - prior.realSol : null;

  // The reserve the buy arrived at, which is the one it has not moved yet.
  const priorVirtualSol = prior && Number.isFinite(prior.virtualSol) ? prior.virtualSol : null;

  // The baseline is the account's earlier moves only. Including this one would
  // let a single large move raise the bar it is being measured against.
  const baseline = prior ? median(prior.moves) : null;
  let volumeMultiple = null;
  let anomaly = false;
  let volumeNote;

  if (flowLamports === null) {
    volumeNote = "First observation of this account: there is no earlier one to compare with.";
  } else if (!prior || prior.moves.length < ANOMALY_MIN_SAMPLES) {
    const seen = prior ? prior.moves.length : 0;
    volumeNote =
      `Only ${seen} earlier move${seen === 1 ? "" : "s"} recorded on this account. ` +
      `A multiple needs ${ANOMALY_MIN_SAMPLES}, so none is claimed.`;
  } else if (baseline === 0) {
    // Every earlier observation moved no SOL at all and this one did. There is
    // no ratio to report and it is unambiguously the account's first movement,
    // which is the strongest reading this column has rather than the weakest.
    anomaly = Math.abs(flowLamports) > 0;
    volumeNote = anomaly
      ? "Every earlier observation of this account moved no SOL. This one did."
      : "This account has never moved any SOL.";
  } else {
    volumeMultiple = Math.abs(flowLamports) / baseline;
    anomaly = volumeMultiple >= ANOMALY_MULTIPLE;
    volumeNote =
      `${signedSol(flowLamports)} SOL against a median move of ` +
      `${sol(baseline)} SOL over ${prior.moves.length} earlier observations.`;
  }

  // --- the sandwich reading ------------------------------------------------
  //
  // Retrospective, and about this one buy: the flow is the net SOL a buy put
  // into the curve, and the question is whether a front-runner sitting in front
  // of it would have cleared the fees on the three swaps a sandwich costs.
  // Nothing here is a claim that anybody did — STS does not take the public
  // path this prices — it is the adverse selection a public buy of this size on
  // this curve was exposed to, which is the number §15.4 says a private bundle's
  // tip has to be justified against.
  const sandwichBeta = sandwichBetaBps(flowLamports, priorVirtualSol);
  const sandwichAbove = sandwichAboveThreshold(flowLamports, priorVirtualSol);
  const sandwichFloor = priorVirtualSol === null ? null : sandwichFloorLamports(priorVirtualSol);
  let sandwichNote;
  if (priorVirtualSol === null) {
    sandwichNote =
      "First observation of this account: there is no earlier reserve for a buy " +
      "to be measured against.";
  } else if (flowLamports === null || flowLamports <= 0) {
    sandwichNote =
      "No SOL entered the curve on this observation. The threshold is about the " +
      "size of a buy, and there is no buy here to size.";
  } else {
    sandwichNote =
      `${sandwichBeta} bps of the ${sol(priorVirtualSol)} SOL virtual reserve this buy ` +
      `arrived at. The threshold is ${SANDWICH_THRESHOLD_BPS} bps — \u03c6/(1\u2212\u03c6) at a ` +
      `${bps(CURVE_FEE_BPS)}% fee — and this is ${sandwichAbove ? "above" : "below"} it: a ` +
      `front-run ${sandwichAbove ? "clears fees before any landing cost" : "cannot clear fees at any size"}.`;
  }

  const tick = {
    seq: (tickSeq += 1),
    slot: view.slot,
    account,
    provider: event.provider ?? view.provider,
    route: event.route,
    mcapLamports: mcap,
    realSolLamports: realSol,
    virtualSolLamports: Number.isFinite(view.virtualSolReserves)
      ? view.virtualSolReserves
      : null,
    curveProgressBps: view.curveProgressBps,
    curveComplete: view.curveComplete === true,
    priorVirtualSolLamports: priorVirtualSol,
    priceDeltaBps,
    flowLamports,
    baselineLamports: baseline,
    volumeMultiple,
    anomaly,
    volumeNote,
    betaBps: sandwichBeta,
    sandwichAbove,
    sandwichFloor,
    sandwichNote,
    atMs: event.receivedAtMs,
    dispatchLatencyUs: event.dispatchLatencyUs,
    row: null,
  };

  tick.row = buildTickRow(tick);
  ticks.unshift(tick);
  placeTickRow(tick, container);

  while (ticks.length > MAX_TICKS) {
    const dropped = ticks.pop();
    const at = tickOrder.indexOf(dropped);
    if (at >= 0) tickOrder.splice(at, 1);
    dropped.row?.remove();
  }

  // The history is updated after the tick is built, so a tick is never measured
  // against itself.
  const moves = prior ? prior.moves : [];
  if (flowLamports !== null) {
    moves.push(Math.abs(flowLamports));
    while (moves.length > ANOMALY_WINDOW) moves.shift();
  }
  tickHistory.set(account, { mcap, realSol, virtualSol: tick.virtualSolLamports, moves });

  applyTickFilter();
}

function buildTickRow(tick) {
  const row = document.createElement("div");
  row.className = "stream-grid row";
  row.setAttribute("role", "row");
  row.setAttribute("aria-selected", "false");
  row.tabIndex = -1;
  row.dataset.account = tick.account;

  row.append(gridCell("num", count(tick.slot)));
  row.append(gridCell("key", shortKey(tick.account)));
  row.append(gridCell("num", tick.mcapLamports === null ? DASH : sol(tick.mcapLamports)));

  const delta = gridCell(
    `num ${tick.priceDeltaBps === null ? "faint" : tick.priceDeltaBps > 0 ? "live" : tick.priceDeltaBps < 0 ? "halt" : "dim"}`,
    tick.priceDeltaBps === null ? DASH : signedInt(tick.priceDeltaBps),
  );
  row.append(delta);

  row.append(
    gridCell(
      "num dim",
      tick.flowLamports === null ? DASH : signedSol(tick.flowLamports),
    ),
  );

  const risk = gridCell("risk", betaText(tick));
  risk.dataset.sandwich = tick.sandwichAbove ? "true" : "false";
  risk.title = tick.sandwichNote;
  row.append(risk);

  const volume = gridCell("vol", volumeText(tick));
  volume.dataset.anomaly = tick.anomaly ? "true" : "false";
  volume.title = tick.volumeNote;
  row.append(volume);

  row.title = tickTitle(tick);
  row.addEventListener("click", () => focusTick(ticks.indexOf(tick)));
  row.addEventListener("dblclick", () => openTickModal(tick));
  row.addEventListener("focus", () => {
    activeList = "tick";
  });

  return row;
}

/// The volume column, as a word or a multiple.
///
/// Three distinct readings and three distinct renderings: no baseline yet is an
/// em dash, an account whose every earlier observation moved nothing is `new`,
/// and everything else is the multiple itself.
function volumeText(tick) {
  if (tick.volumeMultiple !== null) {
    return tick.volumeMultiple >= 99.5 ? "99+x" : `${tick.volumeMultiple.toFixed(1)}x`;
  }
  if (tick.anomaly) return "new";
  return DASH;
}

/// The sandwich column: how large this buy was against the reserve it hit.
///
/// In basis points, which is the unit the threshold beside it is written in, and
/// clamped rather than allowed to grow a digit and shift the column — a buy at
/// four figures of basis points is already an order of magnitude past the line
/// and the exact figure is in the detail. Only a buy has one: a sell and a
/// quiet observation are both an em dash, because the threshold is a statement
/// about the size of an incoming buy and neither of those is one.
function betaText(tick) {
  if (tick.betaBps === null) return DASH;
  return tick.betaBps > 9999 ? "9999+" : String(tick.betaBps);
}

function tickTitle(tick) {
  return (
    `curve ${tick.account}\n` +
    `slot ${tick.slot} · ${tick.provider ?? "unknown provider"} · ` +
    `${tick.route === "fastPath" ? "fast path" : "standard"}\n` +
    `mcap ${tick.mcapLamports === null ? DASH : `${sol(tick.mcapLamports)} SOL`} · ` +
    `real ${tick.realSolLamports === null ? DASH : `${sol(tick.realSolLamports)} SOL`}\n` +
    `${tick.volumeNote}\n` +
    tick.sandwichNote
  );
}

function gridCell(className, text) {
  const span = cell(className, text);
  span.setAttribute("role", "gridcell");
  return span;
}

// --- the order the rows are drawn in ---------------------------------------

/// Two ticks, in the order the current sort puts them.
///
/// Three rules, and each of them is about a list that is being appended to
/// while somebody reads it:
///
///   * a row with nothing to sort on goes last in *both* directions, because an
///     em dash is not a small number;
///   * ties break by arrival, newest first, so the order within a run of equal
///     values is the order the list has when nothing is sorted;
///   * and the comparison is total, so a row inserted now lands where the same
///     row would have landed had it been there from the start.
function compareTicks(a, b) {
  const value = TICK_SORT_KEYS[tickSort.key];
  if (!value) return b.seq - a.seq;

  const left = value(a);
  const right = value(b);
  const leftMissing = left === null || left === undefined || !Number.isFinite(left);
  const rightMissing = right === null || right === undefined || !Number.isFinite(right);
  if (leftMissing || rightMissing) {
    if (leftMissing && rightMissing) return b.seq - a.seq;
    return leftMissing ? 1 : -1;
  }
  if (left !== right) return tickSort.dir === "desc" ? right - left : left - right;
  return b.seq - a.seq;
}

/// Where a new row belongs in the current order.
///
/// A binary search rather than a re-sort: the stream is appended to ten times a
/// second against five hundred held rows, and re-sorting the whole list on each
/// arrival is five hundred comparisons and five hundred DOM moves for one new
/// row. The list is already in order, so the search is nine comparisons and the
/// insert is one.
function tickInsertionIndex(tick) {
  let low = 0;
  let high = tickOrder.length;
  while (low < high) {
    const mid = (low + high) >> 1;
    if (compareTicks(tickOrder[mid], tick) <= 0) low = mid + 1;
    else high = mid;
  }
  return low;
}

/// Puts one new row into the list and into the DOM at the same position.
function placeTickRow(tick, container) {
  if (tickSort.key === null) {
    tickOrder.unshift(tick);
    container.prepend(tick.row);
    return;
  }
  const at = tickInsertionIndex(tick);
  tickOrder.splice(at, 0, tick);
  container.insertBefore(tick.row, tickOrder[at + 1]?.row ?? null);
}

/// Redraws the whole list in the current order.
///
/// Only ever called from an interaction — choosing a sort — which is why it is
/// allowed to move every row at once. The rows are moved rather than rebuilt,
/// so the cursor still points at the row the operator chose and every title and
/// handler on it survives; the focus is the one thing a move does not carry,
/// and it is put back below.
function resortTicks() {
  const container = region("tick-rows");
  if (!container) return;

  // Collecting the rows into a fragment detaches them, and a detached element
  // is not the focused one any more. The cursor is a row rather than a
  // position, so it has to come back with it — otherwise sorting silently puts
  // the focus on the body and the next `j` starts again from the top.
  const refocus = ticks[tickCursor]?.row === document.activeElement;

  tickOrder.length = 0;
  tickOrder.push(...ticks);
  if (tickSort.key !== null) tickOrder.sort(compareTicks);

  const fragment = document.createDocumentFragment();
  for (const tick of tickOrder) fragment.append(tick.row);
  container.append(fragment);

  renderSortHeaders();
  applyTickFilter();

  if (refocus && ticks[tickCursor]) ticks[tickCursor].row.focus({ preventScroll: true });
}

/// The headings, saying what the list is sorted by.
///
/// `aria-sort` is the reading of it, and the arrow beside the heading is the
/// same fact for whoever is looking rather than listening. Only the sorted
/// column carries either, because "none" on five headings at once is five
/// statements that say nothing.
function renderSortHeaders() {
  for (const header of document.querySelectorAll(".col-head [data-sort]")) {
    const column = header.closest('[role="columnheader"]') ?? header;
    const active = header.dataset.sort === tickSort.key;
    column.setAttribute("aria-sort", active ? sortDirectionWord(tickSort.dir) : "none");
    header.dataset.active = active ? "true" : "false";
  }
}

function sortDirectionWord(dir) {
  return dir === "asc" ? "ascending" : "descending";
}

/// Cycles one column: descending, then ascending, then back to arrival order.
function cycleTickSort(key) {
  if (tickSort.key !== key) {
    tickSort.key = key;
    tickSort.dir = "desc";
  } else if (tickSort.dir === "desc") {
    tickSort.dir = "asc";
  } else {
    tickSort.key = null;
    tickSort.dir = "desc";
  }
  resortTicks();
}

function wireTickSort() {
  for (const header of document.querySelectorAll(".col-head [data-sort]")) {
    header.addEventListener("click", () => cycleTickSort(header.dataset.sort));
  }
  renderSortHeaders();
}

/// Whether one tick survives the current filters.
///
/// Filters narrow the view. Nothing here removes a tick from `ticks`, so
/// clearing a filter brings every row back exactly as it was.
function tickMatchesFilter(tick) {
  if (tickFilter.fromSlot !== null && !(tick.slot >= tickFilter.fromSlot)) return false;
  if (tickFilter.minAbsDeltaBps !== null) {
    if (tick.priceDeltaBps === null) return false;
    if (Math.abs(tick.priceDeltaBps) < tickFilter.minAbsDeltaBps) return false;
  }
  if (tickFilter.minVolumeMultiple !== null) {
    // A row with no multiple has not cleared the threshold; it has failed to
    // make a claim about one. Both are reasons to hide it under a filter that
    // asks for a multiple, and neither is a reason to call it large.
    if (tick.volumeMultiple === null) return false;
    if (tick.volumeMultiple < tickFilter.minVolumeMultiple) return false;
  }
  if (tickFilter.anomalyOnly && !tick.anomaly) return false;
  if (tickFilter.sandwichOnly && !tick.sandwichAbove) return false;
  if (tickFilter.subjectOnly && tick.account !== selectedAccount) return false;
  return true;
}

function activeFilterCount() {
  let active = 0;
  if (tickFilter.fromSlot !== null) active += 1;
  if (tickFilter.minAbsDeltaBps !== null) active += 1;
  if (tickFilter.minVolumeMultiple !== null) active += 1;
  if (tickFilter.anomalyOnly) active += 1;
  if (tickFilter.sandwichOnly) active += 1;
  if (tickFilter.subjectOnly) active += 1;
  return active;
}

function applyTickFilter() {
  let shown = 0;
  for (const tick of ticks) {
    const visible = tickMatchesFilter(tick);
    tick.row.hidden = !visible;
    if (visible) shown += 1;
  }

  // Shown of held, always both. A filtered pane that reports only what survived
  // reads as a quiet feed, and a quiet feed and a narrow filter are the two
  // things this window exists to keep apart.
  setText("tick-count", `${shown} / ${ticks.length}`);

  const active = activeFilterCount();
  setText(
    "tick-filter-state",
    active === 0 ? "no filter" : `${active} filter${active === 1 ? "" : "s"} active`,
  );

  const rows = region("tick-rows");
  const empty = region("tick-empty");
  const filtered = region("tick-filtered");
  const held = ticks.length > 0;
  if (rows) rows.hidden = shown === 0;
  if (empty) empty.hidden = held;
  if (filtered) filtered.hidden = !(held && shown === 0);

  // A cursor pointing at a row the filter just hid is a cursor pointing at
  // nothing. It is moved to the nearest visible row rather than dropped, so a
  // filter change does not throw away where the operator was.
  if (tickCursor >= 0) {
    const current = ticks[tickCursor];
    if (!current || current.row.hidden) {
      // The nearest visible row as *drawn*, which under a sort is not the
      // nearest one as held.
      const next = tickOrder.find((tick) => !tick.row.hidden);
      setTickCursor(next ? ticks.indexOf(next) : -1, false);
    }
  }
}

function setTickCursor(index, moveFocus = true) {
  if (tickCursor >= 0 && ticks[tickCursor]) {
    ticks[tickCursor].row.setAttribute("aria-selected", "false");
  }
  tickCursor = index;
  if (index < 0 || !ticks[index]) return;
  const row = ticks[index].row;
  row.setAttribute("aria-selected", "true");
  if (moveFocus) row.focus({ preventScroll: false });
}

function focusTick(index) {
  if (index < 0) return;
  activeList = "tick";
  setTickCursor(index);
}

/// Moves the cursor `step` visible rows, skipping everything a filter hid.
///
/// Down the list as it is drawn. `j` means "the row under this one", and under
/// a sort the row under this one is not the next one to have arrived.
function moveTickCursor(step) {
  const visible = tickOrder.filter((tick) => !tick.row.hidden);
  if (visible.length === 0) return;
  const current = tickCursor >= 0 ? ticks[tickCursor] : null;
  const position = current ? visible.indexOf(current) : -1;
  const next =
    position === -1
      ? visible[step > 0 ? 0 : visible.length - 1]
      : visible[Math.min(visible.length - 1, Math.max(0, position + step))];
  setTickCursor(ticks.indexOf(next));
}

/// Which state each filter field and chip writes to.
///
/// Named in one place so a control added to the strip is a line here rather
/// than a branch in the middle of an event handler.
const TICK_FILTER_FIELDS = {
  slot: { key: "fromSlot", whole: true },
  delta: { key: "minAbsDeltaBps", whole: true },
  vol: { key: "minVolumeMultiple", whole: false },
};

const TICK_FILTER_CHIPS = {
  anomaly: "anomalyOnly",
  sandwich: "sandwichOnly",
  subject: "subjectOnly",
};

function wireTickFilters() {
  for (const input of document.querySelectorAll(".stream-tools .filter-input")) {
    const field = TICK_FILTER_FIELDS[input.dataset.filter];
    if (!field) continue;
    input.addEventListener("input", () => {
      const raw = input.value.trim();
      const parsed = raw === "" ? null : Number(raw);
      // A slot and a count of basis points are whole numbers; a volume multiple
      // is the thing the column reports to one decimal, so 2.5 is a threshold
      // somebody will actually want and rejecting it would be pedantry.
      const valid =
        raw === "" ||
        (Number.isFinite(parsed) && parsed >= 0 && (!field.whole || Number.isInteger(parsed)));

      // Rejected input is shown as rejected. A filter silently not applied is a
      // row count that is wrong in a way nothing on screen admits to.
      input.setAttribute("aria-invalid", valid ? "false" : "true");
      if (!valid) return;

      tickFilter[field.key] = parsed;
      applyTickFilter();
    });
  }

  for (const [name, key] of Object.entries(TICK_FILTER_CHIPS)) {
    const chip = document.querySelector(`.stream-tools [data-filter="${name}"]`);
    chip?.addEventListener("click", () => {
      tickFilter[key] = !tickFilter[key];
      chip.setAttribute("aria-pressed", tickFilter[key] ? "true" : "false");
      applyTickFilter();
    });
  }
}

// ---------------------------------------------------------------------------
// the tick detail
// ---------------------------------------------------------------------------

let tickReturnFocus = null;

function tickModal() {
  return region("tick-modal");
}

function isTickModalOpen() {
  return tickModal()?.dataset.open === "true";
}

function openTickModal(tick) {
  const modal = tickModal();
  if (!modal || !tick) return;
  tickReturnFocus = document.activeElement;

  setText("tick-detail-slot", count(tick.slot));
  setText(
    "tick-detail-summary",
    tick.priceDeltaBps === null
      ? "The first observation of this account. There is nothing earlier to measure it against."
      : `${signedInt(tick.priceDeltaBps)} bps on the curve price, ` +
        `${tick.flowLamports === null ? "no measured flow" : `${signedSol(tick.flowLamports)} SOL through the pool`}` +
        `, between slot ${count(tick.slot)} and the previous observation of this account.`,
  );

  const fields = region("tick-detail-fields");
  if (fields) {
    fields.replaceChildren(
      ...detailPairs(tick).flatMap(([term, value]) => {
        const dt = document.createElement("dt");
        dt.className = "label";
        dt.textContent = term;
        const dd = document.createElement("dd");
        dd.textContent = value;
        return [dt, dd];
      }),
    );
  }

  setText(
    "tick-detail-raw",
    JSON.stringify(
      {
        slot: tick.slot,
        account: tick.account,
        provider: tick.provider ?? null,
        route: tick.route ?? null,
        marketCapLamports: tick.mcapLamports,
        poolLamports: tick.realSolLamports,
        virtualSolReserves: tick.virtualSolLamports,
        curveProgressBps: tick.curveProgressBps,
        curveComplete: tick.curveComplete,
        receivedAtMs: tick.atMs,
        dispatchLatencyUs: tick.dispatchLatencyUs,
      },
      null,
      2,
    ),
  );

  modal.dataset.open = "true";
  modal.hidden = false;
  modal.querySelector('[data-action="tick-close"]')?.focus();
}

/// The pair a derived column came from, beside the derived number itself.
function detailPairs(tick) {
  const pairs = [
    ["slot", count(tick.slot)],
    ["curve account", tick.account],
    ["provider", tick.provider ?? DASH],
    ["market cap", tick.mcapLamports === null ? DASH : `${sol(tick.mcapLamports)} SOL`],
    ["price delta", tick.priceDeltaBps === null ? DASH : `${signedInt(tick.priceDeltaBps)} bps`],
    ["real sol", tick.realSolLamports === null ? DASH : `${sol(tick.realSolLamports)} SOL`],
    ["virtual sol", tick.virtualSolLamports === null ? DASH : `${sol(tick.virtualSolLamports)} SOL`],
    ["flow", tick.flowLamports === null ? DASH : `${signedSol(tick.flowLamports)} SOL`],
    [
      "median move",
      tick.baselineLamports === null ? DASH : `${sol(tick.baselineLamports)} SOL`,
    ],
    ["volume multiple", volumeText(tick)],
    ["anomaly", tick.anomaly ? "flagged" : "not flagged"],
    [
      "victim buy",
      tick.flowLamports === null || tick.flowLamports <= 0
        ? DASH
        : `${sol(victimGrossLamports(tick.flowLamports))} SOL gross`,
    ],
    [
      "sandwich floor",
      tick.sandwichFloor === null ? DASH : `${sol(tick.sandwichFloor)} SOL`,
    ],
    [
      "reserve at buy",
      tick.priorVirtualSolLamports === null
        ? DASH
        : `${sol(tick.priorVirtualSolLamports)} SOL`,
    ],
    ["beta", tick.betaBps === null ? DASH : `${count(tick.betaBps)} bps`],
    ["beta threshold", `${SANDWICH_THRESHOLD_BPS} bps`],
    ["sandwich", tick.sandwichAbove ? "above threshold" : "below threshold"],
    ["curve progress", `${bps(tick.curveProgressBps)}%`],
    ["observed at", clock(tick.atMs)],
    ["dispatch", micros(tick.dispatchLatencyUs)],
  ];
  return pairs;
}

function closeTickModal() {
  const modal = tickModal();
  if (!modal) return;
  modal.dataset.open = "false";
  modal.hidden = true;
  if (tickReturnFocus && document.contains(tickReturnFocus)) tickReturnFocus.focus();
  tickReturnFocus = null;
}

function wireTickModal() {
  document
    .querySelector('[data-action="tick-close"]')
    ?.addEventListener("click", closeTickModal);

  tickModal()?.addEventListener("mousedown", (event) => {
    if (event.target === tickModal()) closeTickModal();
  });
}

// ---------------------------------------------------------------------------
// 2d. replay
// ---------------------------------------------------------------------------

// The two command names, in one place each.
//
// Both are registered in `src-tauri/src/lib.rs` and both answer off a real
// `ReplaySession`: the fixture, the playhead and the multiplier the engine is
// actually holding. These constants and `renderReplay` below are the whole
// binding surface — nothing else in this file knows what they are called or
// what they take.
//
// The missing-command path below is still live and still matters. An older
// build has neither command, and against one of those the toggle reports that
// the engine has no replay control rather than pretending to have flipped
// something: a switch that says "replay" over a live engine is the one mistake
// in this window that costs real money.
//
// Four commands, and the split between the last three is the point.
//
// `set_replay_playback` carries the switch. `set_replay_speed` and
// `set_replay_transport` are narrow halves of the same thing and cannot start
// or stop a fixture whatever they are handed, which is why the chips and the
// transport buttons go through them: a control that can reach the `active`
// field is one bad payload away from stopping a recording that somebody is
// reading numbers off. The switch stays the only thing in this window that can
// turn replay on or off.
//
// All of them answer with the session's own `ReplayStatus`, so the bar is drawn
// from the engine's answer whichever one was pressed and no two of them can
// disagree about what is running.
const REPLAY_STATUS_COMMAND = "get_replay_status";
const REPLAY_CONTROL_COMMAND = "set_replay_playback";
const REPLAY_SPEED_COMMAND = "set_replay_speed";
const REPLAY_TRANSPORT_COMMAND = "set_replay_transport";

// Set false on a build that has the wide command but not the narrow one, so the
// chips fall back to `set_replay_playback` instead of going dead.
let speedCommandSupported = true;

// Set false on a build with no transport command. There is no fallback for this
// one and there must not be: play, pause, step and faster have no spelling in
// terms of `set_replay_playback`, and the nearest thing — stopping the fixture
// and starting it again — is the one thing the transport is defined by not
// doing. A build without it gets four dead buttons that say why.
let transportSupported = true;

// Whether the engine has a replay control at all. Set false the first time a
// call comes back saying the command does not exist, so the window asks once
// rather than ten times a second forever.
let replaySupported = true;
// The last status the engine reported, and whether it has reported one. `null`
// with `replayKnown` true means the engine answered and said it is not in
// replay; `null` with it false means nobody has answered.
let replayStatus = null;
let replayKnown = false;

function isReplayActive() {
  return replayKnown && replayStatus?.active === true;
}

/// Which of the four the transport is in, as `PlaybackState` names them.
///
/// `stopped`, `playing`, `paused`, `ended`. Read straight off the status rather
/// than reduced to a boolean, because `ended` is the state a boolean cannot
/// hold: a fixture played to its last record is not moving, and it is also not
/// held — there is nothing left to resume into, and offering a resume that
/// silently does nothing is how an operator concludes the transport is broken.
///
/// `null` when the engine has not answered. Not `stopped`: "nothing is playing"
/// and "nobody has said" are different claims and only one of them is evidence.
function replayPlaybackState() {
  if (!replayKnown) return null;
  const state = replayStatus?.state;
  return typeof state === "string" ? state : null;
}

/// Whether the playhead of an active run is being held.
///
/// Only ever true inside replay. A window that has lost the engine reports
/// neither held nor moving, because it does not know which — the same reason
/// the switch has a third state.
function isReplayHeld() {
  return isReplayActive() && replayPlaybackState() === "paused";
}

/// Whether the playhead is past the last record.
///
/// The engine's own word for it, not a comparison of two counters. `Ended` is
/// set by `ReplaySession` when the cursor is exhausted, and a window that
/// derived it from `recordsPlayed >= recordCount` would be a second opinion
/// about when a fixture is finished.
function isReplayEnded() {
  return isReplayActive() && replayPlaybackState() === "ended";
}

/// Draws the toggle, the bar, and every fact on it.
///
/// The switch is written from the engine's answer and never from the click that
/// caused it. `mixed` is a real third state here and means the window has lost
/// the engine while it was in replay — neither "on" nor "off" is a true answer
/// at that point and rendering either one is a guess about whether a fixture is
/// still driving the numbers below.
function renderReplay() {
  const toggle = document.querySelector('[data-action="replay-toggle"]');
  const bar = region("replay-bar");
  const active = isReplayActive();

  if (toggle) {
    const checked = !replaySupported
      ? "false"
      : !replayKnown
        ? "mixed"
        : active
          ? "true"
          : "false";
    if (toggle.getAttribute("aria-checked") !== checked) {
      toggle.setAttribute("aria-checked", checked);
    }
    toggle.disabled = !replaySupported || !invoke;
    toggle.title = !invoke
      ? "There is no engine attached to this window."
      : !replaySupported
        ? `This build has no ${REPLAY_CONTROL_COMMAND} command, so it cannot run a fixture.`
        : !replayKnown
          ? "The engine has not answered. Whether a fixture is driving these numbers is unknown."
          : active
            ? "Running a recorded fixture. Nothing below is live."
            : "Ask the engine to run a recorded fixture instead of the live feeds.";
  }

  if (bar) bar.hidden = !(active || (replayKnown === false && replayStatus !== null));

  if (!active) {
    if (replayStatus === null) return;
  }

  const status = replayStatus ?? {};

  // --- the clock ----------------------------------------------------------
  // §2 of the replay specification: all three clocks are virtualised and wall
  // time is derived from slot rather than the other way round. The counters
  // beside it are the clamps — records whose timestamp was behind the clock —
  // which are counted rather than hidden precisely so a fixture recorded
  // against a provider with a broken clock is visible as one.
  const clamped = Number.isFinite(status.clamped) ? status.clamped : null;
  const regressions = Number.isFinite(status.slotRegressions) ? status.slotRegressions : null;
  const clockNote =
    clamped === null && regressions === null
      ? "virtualised"
      : `virtualised · ${count(clamped ?? 0)} clamped`;
  setText("replay-clock", replayKnown ? clockNote : DASH);
  const clockEl = field("replay-clock");
  if (clockEl) {
    clockEl.title =
      "Wall time, timers and the slot clock are all driven by the fixture. " +
      `${count(clamped ?? 0)} record${(clamped ?? 0) === 1 ? "" : "s"} arrived with a timestamp behind the clock; ` +
      `${count(regressions ?? 0)} arrived with a slot behind it.`;
  }

  // --- where the playhead is ---------------------------------------------
  setText("replay-slot", Number.isFinite(status.slot) ? count(status.slot) : DASH);
  const slotEl = field("replay-slot");
  if (slotEl) {
    slotEl.title =
      Number.isFinite(status.firstSlot) && Number.isFinite(status.lastSlot)
        ? `Fixture covers slots ${count(status.firstSlot)} to ${count(status.lastSlot)}.`
        : "The fixture did not report the slot range it covers.";
  }

  setText(
    "replay-progress",
    Number.isFinite(status.recordsPlayed) && Number.isFinite(status.recordCount)
      ? `${count(status.recordsPlayed)} / ${count(status.recordCount)}`
      : DASH,
  );

  // --- which fixture ------------------------------------------------------
  setText("replay-fixture", status.streamId ? shortKey(status.streamId) : DASH);
  const fixtureEl = field("replay-fixture");
  if (fixtureEl) fixtureEl.title = status.streamId ?? "No fixture reported.";

  setText("replay-chain-head", status.chainHead ? shortHash(status.chainHead) : DASH);
  const headEl = field("replay-chain-head");
  if (headEl) headEl.title = status.chainHead ?? "No chain head reported.";

  renderReplayIntegrity(status);
  renderReplayTransport(status);
  renderReplaySpeed(status);
  // The engine has just said whether the playhead is held. If it is not, and
  // the ticker was holding rows back while it was, they go out now rather than
  // waiting for whatever happens to arrive next.
  releaseTicker();
}

/// Whether the fixture's hash chain was checked, and what the answer was.
///
/// Four states, in the order that matters. A broken chain outranks everything;
/// a recording that was cut short is a hole in the evidence even when every
/// link in what survived is sound; verified is only ever shown when the engine
/// says it verified; and not having been told is its own state and is never
/// drawn as either answer.
function renderReplayIntegrity(status) {
  const el = field("replay-integrity");
  if (!el) return;

  let state;
  let text;
  let title;

  if (status.chainVerified === false) {
    state = "broken";
    text = "broken";
    title =
      "The fixture's hash chain does not verify. Some record has been altered, " +
      "reordered or lost, and nothing replayed from it is evidence of anything.";
  } else if (status.fixtureComplete === false) {
    state = "partial";
    text = "partial";
    title =
      "Every link that exists verifies, but the recording was stopped by an " +
      "error rather than finishing. There is a hole in it, and a result " +
      "computed across the hole is a claim about data that was never recorded.";
  } else if (status.chainVerified === true) {
    state = "verified";
    text = "verified";
    title =
      "Every record's hash chains to the one before it and the head matches " +
      "the manifest. The fixture is byte-for-byte what was recorded.";
  } else {
    state = "unverified";
    text = "unverified";
    title =
      "The engine has not said whether it checked the chain. Unverified is not " +
      "a failure and it is not a pass; it is the absence of the check.";
  }

  if (el.dataset.state !== state) el.dataset.state = state;
  if (el.textContent.trim() !== text) el.textContent = text;
  if (el.title !== title) el.title = title;
}

/// What each button says about itself once there is a run to say it about.
///
/// Written out per state rather than as one sentence with the state left out,
/// because the two questions an operator has here — why is this button dead,
/// and what will it do if I press it — have different answers for each of them.
const TRANSPORT_TITLES = {
  play: (state) =>
    state === "playing"
      ? "The playhead is already moving."
      : "Let the playhead move again from where it is. It does not rewind — the switch is what starts a fixture from the beginning.",
  pause: (state) =>
    state === "paused"
      ? "The playhead is already held."
      : "Hold the playhead. The fixture is still what is driving these numbers, so this bar stays up.",
  step: () =>
    "Play exactly one more record, whatever the multiplier says, and hold there.",
  fastForward: () =>
    `Play the next ${FAST_FORWARD_RECORDS} records now, without spending wall clock on them, and hold. ` +
    "They are played and not skipped: every one goes past the strategy in order, so the ledger afterwards is the ledger of having watched them.",
};

/// The four transport buttons: what each one may do, and which one is lit.
///
/// Everything here is decided from the engine's reported status and nothing
/// from what was clicked, for the reason the switch and the chips are. A
/// playhead the operator asked to hold and the engine did not hold is a
/// playhead that is still moving, and a button drawn from the click would be
/// the only thing on screen saying otherwise.
///
/// Two of the four disable themselves, and both of those are refusals the
/// engine makes on its own:
///
///   `step` is a control for a held playhead. Stepping one that is moving
///   would advance it by one record *and* by whatever the tick in flight was
///   worth, with nothing afterwards able to tell those apart. It is also off at
///   the end of the fixture, where there is no next record to play.
///
///   `faster` is off at `max`, which is the top of the ladder. Nothing is
///   hidden by that — the pressed speed chip says where on the ladder the run
///   is, and this button only ever walks it upwards.
///
/// `play` and `pause` stay enabled whichever is lit. Pressing the one that is
/// already true is a no-op the engine accepts, and a button that disabled
/// itself as it was pressed would drop the keyboard focus that pressed it.
function renderReplayTransport(status) {
  const buttons = document.querySelectorAll(".transport .chip");
  if (buttons.length === 0) return;

  const active = isReplayActive();
  const state = replayPlaybackState();
  const ended = isReplayEnded();
  const available = active && replaySupported && transportSupported && !!invoke;

  for (const button of buttons) {
    const action = button.dataset.transport;

    // Which one is lit is the engine's answer and not the click's. A playhead
    // the operator asked to hold and the engine did not hold is a playhead that
    // is still moving, and a button drawn from the press would be the only
    // thing on screen saying otherwise.
    if (action === "play" || action === "pause") {
      const pressed = String(
        available && state === (action === "pause" ? "paused" : "playing"),
      );
      if (button.getAttribute("aria-pressed") !== pressed) {
        button.setAttribute("aria-pressed", pressed);
      }
    }

    // Only `ended` disables anything, and it disables everything: there is no
    // next record to step to, none to fast-forward through, and `resume` is a
    // no-op the session makes on its own from there.
    //
    // Play and pause stay enabled otherwise, whichever is lit. Pressing the one
    // that is already true is a no-op `ReplaySession` accepts — `pause` does
    // nothing from anywhere but `Playing`, `resume` from `Playing` sets
    // `Playing` again — and a button that disabled itself as it was pressed
    // would drop the keyboard focus that pressed it.
    //
    // `step` is deliberately *not* disabled on a moving playhead. This build's
    // session permits it and leaves the transport held afterwards, and drawing
    // a refusal the engine does not make is drawing a rule that is not there.
    button.disabled = !available || ended;

    const title = !invoke
      ? "There is no engine attached to this window."
      : !replaySupported || !transportSupported
        ? `This build has no ${REPLAY_TRANSPORT_COMMAND} command, so the playhead cannot be steered from here.`
        : !replayKnown
          ? "The engine has not answered. Whether the playhead is moving is unknown."
          : !active
            ? "Nothing is playing. The replay switch is what starts a fixture."
            : ended
              ? "The fixture has been played to its last record. Nothing here has anywhere left to move it."
              : (TRANSPORT_TITLES[action]?.(state) ?? "");
    if (button.title !== title) button.title = title;
  }
}

/// The four multipliers, with the engine's own answer pressed.
///
/// `aria-pressed` is set from the reported speed rather than the click for the
/// same reason the switch is. A speed the engine refused has to read as
/// refused.
function renderReplaySpeed(status) {
  const reported = status.speed === undefined || status.speed === null
    ? null
    : String(status.speed);
  for (const chip of document.querySelectorAll(".speeds .chip")) {
    const pressed = reported !== null && chip.dataset.speed === reported;
    const value = pressed ? "true" : "false";
    if (chip.getAttribute("aria-pressed") !== value) chip.setAttribute("aria-pressed", value);
    chip.disabled = !isReplayActive();
  }
}

// The newest edition of the replay state this window has drawn, or null if it
// has not drawn one, or if this engine does not number them. See
// `applyReplayStatus`.
let replayRevision = null;

/// Everything on the bar, as one string.
///
/// The ordering fallback for an engine that does not number its editions. Every
/// field the bar draws is in it, so "the same digest" means "nothing on screen
/// would change", which is exactly the question being asked. A digest over a
/// subset would let a status through that changed only the part left out.
function replayDigest(status) {
  if (!status) return "none";
  return [
    status.active,
    status.state,
    status.speed,
    status.slot,
    status.recordsPlayed,
    status.recordCount,
    status.streamId,
    status.chainHead,
    status.chainVerified,
    status.fixtureComplete,
    status.clamped,
    status.slotRegressions,
    status.firstSlot,
    status.lastSlot,
    status.atMs,
    status.ledger ? JSON.stringify(status.ledger) : "",
  ].join("\u0001");
}

/// Folds a reported status in and redraws.
///
/// Three things call this and they do not arrive in order. The command the
/// operator just pressed answers on its own schedule; the once-a-second poll
/// may have been issued before the press and answer after it; and the engine's
/// ticker pushes a telemetry line carrying whatever was true when the tick ran.
/// A status is a snapshot with no timestamp on its face, so "the last one to
/// arrive" is not the same question as "the current one" — and the difference
/// is a window that draws `playing` over a fixture the engine is holding, with
/// the transport drawn for the wrong state, until the next poll lands a second
/// later. The operator paused in order to step.
///
/// Two tiers, and which one is in use depends on the engine rather than on a
/// build flag here.
///
///   **If the engine numbers its editions** — a `revision` on the status — that
///   is the order, and anything older than what is already on screen is
///   dropped. Equal is not dropped: two statuses at one revision describe one
///   state, and the second is folded in and counted as the duplicate it is.
///
///   **If it does not**, and this build's `ReplayStatus` does not, there is no
///   way to order two statuses and the last one to arrive wins — which is what
///   the bar did before any of this existed. What the window can still do is
///   refuse to *count* a redraw that changes nothing, and that is what the
///   digest is for: the revision below is the window's own, it goes up once per
///   status that actually changed something, and a poll that returns the same
///   answer a second later moves the duplicate counter instead.
///
/// Either way `feeds.replay.revision` is a number that goes up when the bar
/// changed and never otherwise, which is the property the pane's readout and
/// the suite are both written against.
function applyReplayStatus(status) {
  const revision = Number.isFinite(status?.revision) ? status.revision : null;
  if (revision !== null && replayRevision !== null && revision < replayRevision) {
    feeds.replay.stale += 1;
    return;
  }
  if (revision !== null) replayRevision = revision;

  const changed = feeds.replay.changed(replayDigest(status));

  replayStatus = status ?? null;
  replayKnown = true;
  if (changed) feeds.replay.apply({ seq: revision, digest: replayDigest(status) });
  renderReplay();
}

/// Called when the window has lost the engine.
///
/// The bar is not hidden and the switch is not set to off. The last thing the
/// window knew was that a fixture was driving these numbers, and the engine
/// going quiet is not evidence that it stopped.
function markReplayUnknown() {
  if (!replayKnown) return;
  replayKnown = false;
  // The ordering above holds inside one uninterrupted conversation with one
  // engine, and this is the window noticing that conversation has broken. An
  // engine that comes back is counting from wherever it is counting from, and a
  // watermark held over from before would drop everything it said. So the next
  // status to arrive re-seeds instead of being compared against a number from a
  // run that may no longer exist.
  replayRevision = null;
  feeds.replay.digest = null;
  renderReplay();
}

/// Telemetry with `source: "replay"`. Carries the same status the command
/// returns, so one renderer serves both paths.
function onReplayStatusEvent(data) {
  if (!data || typeof data !== "object") return;
  applyReplayStatus(data);
}

async function pollReplayStatus() {
  if (!invoke || !replaySupported) return;
  try {
    applyReplayStatus(await invoke(REPLAY_STATUS_COMMAND));
  } catch (err) {
    if (isMissingCommand(err)) {
      replaySupported = false;
      replayKnown = true;
      replayStatus = null;
      renderReplay();
      console.info(`[sts] no ${REPLAY_STATUS_COMMAND} in this build; replay control is off`);
    } else {
      markReplayUnknown();
    }
  }
}

/// Whether an error means the command is not there, as opposed to the engine
/// having refused. The two need different answers: one disables the control,
/// the other is a reason worth showing.
function isMissingCommand(err) {
  const message = typeof err === "string" ? err : err?.message ?? String(err);
  return /not found|unknown command|not allowed|no such/i.test(message);
}

async function requestReplay(payload) {
  if (!invoke || !replaySupported) return;
  try {
    applyReplayStatus(await invoke(REPLAY_CONTROL_COMMAND, payload));
  } catch (err) {
    if (isMissingCommand(err)) {
      replaySupported = false;
      renderReplay();
    } else {
      // A refusal is the engine's answer and the switch has to keep showing the
      // engine's answer, so the status is re-read rather than assumed.
      console.warn("[sts] replay control refused", err);
      await pollReplayStatus();
    }
  }
}

/// The multiplier on its own.
///
/// Falls back to the wide command on a build that never registered this one, so
/// a chip on an older engine keeps working rather than going quiet — and it
/// sends only `speed` there too. Nothing in this function names `active`.
async function requestReplaySpeed(speed) {
  if (!invoke || !replaySupported) return;
  if (!speedCommandSupported) return requestReplay({ speed });

  try {
    applyReplayStatus(await invoke(REPLAY_SPEED_COMMAND, { speed }));
  } catch (err) {
    if (isMissingCommand(err)) {
      speedCommandSupported = false;
      console.info(
        `[sts] no ${REPLAY_SPEED_COMMAND} in this build; speed goes through ${REPLAY_CONTROL_COMMAND}`,
      );
      await requestReplay({ speed });
    } else {
      // A refusal is the engine's answer and the chips are drawn from the
      // engine's answer, so the status is re-read rather than assumed.
      console.warn("[sts] replay speed refused", err);
      await pollReplayStatus();
    }
  }
}

/// How many records one press of fast-forward plays.
///
/// Bounded, and bounded here rather than left to the command's own default. A
/// `fastForward` with no count plays *every record that is left* — which is the
/// right default for the backtest runner that shares this call, and the wrong
/// one for a button beside a pause: a single press would run a ninety-thousand
/// record fixture to its end, and the transport's whole reason to exist is that
/// the operator is looking at something and wants a little more of it.
const FAST_FORWARD_RECORDS = 250;

/// What each button sends, in `ReplayControl`'s own words.
///
/// `play` sends `resume`, not `play`, and that is the important line in this
/// file. `ReplayControl::Play` opens the fixture, **rewinds it**, and plays;
/// `Resume` carries on from where the playhead is. A transport whose play
/// button silently rewound would lose the paused position somebody stopped on
/// to look at, which is the one thing a pause is for.
///
/// Nothing here sends `stop`, and nothing here can reach `active`. Starting and
/// stopping a fixture stays the switch's job alone — that split is what lets
/// four more controls sit on this bar without becoming a second way into
/// replay.
const TRANSPORT_PAYLOADS = {
  play: { control: "resume" },
  pause: { control: "pause" },
  step: { control: "step", records: 1 },
  fastForward: { control: "fastForward", records: FAST_FORWARD_RECORDS },
};

/// One press of one transport button.
///
/// Sends the control and its record count and nothing else. No speed: the
/// multiplier is the chips' business, and a transport press that also moved the
/// speed would be a control with two effects and one label.
///
/// There is no fallback command. `set_replay_playback` cannot express any of
/// these four, and the nearest thing it can express — stop, then start — would
/// rewind the fixture somebody paused to look at.
async function requestReplayTransport(action) {
  if (!invoke || !replaySupported || !transportSupported) return;
  const payload = TRANSPORT_PAYLOADS[action];
  if (!payload) return;

  try {
    applyReplayStatus(await invoke(REPLAY_TRANSPORT_COMMAND, payload));
  } catch (err) {
    if (isMissingCommand(err)) {
      transportSupported = false;
      renderReplay();
      console.info(
        `[sts] no ${REPLAY_TRANSPORT_COMMAND} in this build; the playhead cannot be steered from here`,
      );
    } else {
      // A refusal is the engine's answer — the playhead did not move, or it
      // moved less than was asked for — and the buttons are drawn from the
      // engine's answer, so the status is re-read rather than assumed. The
      // live-feed guard in `refuse_over_a_live_feed` is the refusal this is
      // most often carrying.
      console.warn("[sts] replay transport refused", err);
      await pollReplayStatus();
    }
  }
}

function wireReplay() {
  document
    .querySelector('[data-action="replay-toggle"]')
    ?.addEventListener("click", () => requestReplay({ active: !isReplayActive() }));

  for (const chip of document.querySelectorAll(".speeds .chip")) {
    chip.addEventListener("click", () => requestReplaySpeed(chip.dataset.speed));
  }

  for (const button of document.querySelectorAll(".transport .chip")) {
    button.addEventListener("click", () => requestReplayTransport(button.dataset.transport));
  }
}

// ---------------------------------------------------------------------------
// keyboard navigation
// ---------------------------------------------------------------------------

// Which list `j` and `k` move. Set by whatever was last focused or clicked, so
// the keys act on the pane the operator is looking at rather than on a fixed
// one.
let activeList = "radar";

function anyModalOpen() {
  return isUnwindModalOpen() || isTickModalOpen() || isGeyserOpen();
}

/// Whether a key belongs to the element under it rather than to the window.
/// Without this, `j` in the slot filter filters on nothing and moves the cursor.
function isTypingTarget(target) {
  return (
    target instanceof HTMLInputElement ||
    target instanceof HTMLTextAreaElement ||
    target?.isContentEditable === true
  );
}

function visibleRadarRows() {
  return [...radar.values()].filter((entry) => !entry.row.hidden);
}

function moveRadarCursor(step) {
  const rows = visibleRadarRows();
  if (rows.length === 0) return;
  const current = rows.findIndex((entry) => entry.view.account === selectedAccount);
  const next =
    current === -1
      ? step > 0
        ? 0
        : rows.length - 1
      : Math.min(rows.length - 1, Math.max(0, current + step));
  const entry = rows[next];
  entry.row.focus({ preventScroll: false });
  selectCandidate(entry.view.account);
}

function wireKeyboard() {
  document.addEventListener("keydown", (event) => {
    // Escape closes the tick detail. The unwind confirmation owns its own
    // Escape — it is a question about money and its handler traps focus with
    // it — so this one stays out of the way while that is up.
    if (event.key === "Escape") {
      if (isUnwindModalOpen()) return;
      if (isGeyserOpen()) {
        event.preventDefault();
        closeGeyserView();
        return;
      }
      if (isTickModalOpen()) {
        event.preventDefault();
        closeTickModal();
      }
      return;
    }

    if (event.metaKey || event.ctrlKey || event.altKey) return;
    if (isTypingTarget(event.target)) return;

    if (isTickModalOpen() || isGeyserOpen()) {
      // The detail has one control. Tab has nowhere else to go, and holding it
      // there is what stops the key walking into the covered surface behind.
      if (event.key === "Tab") event.preventDefault();
      return;
    }
    if (isUnwindModalOpen()) return;

    switch (event.key) {
      case "j":
        event.preventDefault();
        if (activeList === "tick") moveTickCursor(1);
        else moveRadarCursor(1);
        break;
      case "k":
        event.preventDefault();
        if (activeList === "tick") moveTickCursor(-1);
        else moveRadarCursor(-1);
        break;
      case "Enter":
        if (activeList === "tick" && tickCursor >= 0 && ticks[tickCursor]) {
          event.preventDefault();
          openTickModal(ticks[tickCursor]);
        }
        break;
      // The one view in this window with no home in the three panes. `g` for
      // Geyser, and it is a key rather than only a status-bar cell because the
      // cell is eleven pixels tall at the bottom of the screen and this is the
      // thing somebody reaches for when the feed feels wrong.
      case "g":
        event.preventDefault();
        openGeyserView();
        break;
      default:
        break;
    }
  });
}

// ---------------------------------------------------------------------------
// 3. the execution deck, the unwind obligations, and the flatten control
// ---------------------------------------------------------------------------

// The one place the emergency unwind command is named.
//
// This constant and `unwindArgs` below are the whole binding surface — no other
// line in this file knows what the command is called or what it takes.
//
// The name follows the convention the other registered commands use:
// `trigger_kill_switch` is the other thing in this window that changes the
// world rather than reporting on it, and both read as an operator pulling
// something rather than the UI performing it.
const UNWIND_COMMAND = "trigger_emergency_unwind";

// What the receipt means, and which fields say it.
//
// `trigger_emergency_unwind` arms the kill switch and stops the engine managing
// these positions. Whether it also *sold* them depends on an execution backend
// the shipped application does not install, so on this build `exitsSent` comes
// back zero and every entry in `stranded` is still on chain. `UnwindReceipt`
// says so itself: a caller that renders this as "positions closed" is reporting
// a lie about money.
//
// Nothing in this file treats a successful call as a closed position, and
// nothing in it decides a position's fate from a count. The counts are
// aggregates over sets that stop being the same set the moment there has been
// more than one press:
//
//   exitsSent        what THIS press put on the network
//   exitsAlreadyOut  what an EARLIER press left out there, found rather than sent
//   exitsConfirmed   what actually closed — these are in `flattened`, not here
//
// The per-position answer is `stranded[].exit.onNetwork`, and that is what
// every branch below reads. A second press over a position whose exit is still
// flying reports `exitsSent: 0` and `exitsAlreadyOut: 1`, and a window that
// branched on the first of those alone would tell the operator nothing was ever
// sold while a transaction of theirs was in the air.
const UNWIND_SOLD_NOTHING = 0;

/// The argument object sent with it.
///
/// Keyed by `intent_id` because that is the correlation ID the schema, the audit
/// NDJSON and the telemetry event all share — see SCHEMA.md on `execution_logs`.
/// Sending the intent rather than a mint or a size means the backend resolves
/// the obligation against its own newest row instead of trusting a number this
/// window happened to be showing when the button was pressed.
function unwindArgs(obligations) {
  return {
    intentIds: obligations.map((o) => o.intentId),
    reason: "flattened from the UI",
  };
}

// `ExecutionState` reaches this window in two encodings and they are the same
// enum. Serde writes camelCase — `intentCreated` — because the type carries
// `rename_all = "camelCase"`. `ExecutionState::as_str` writes snake_case —
// `intent_created` — and that is what `execution_logs.state` stores, so anything
// read back out of the database arrives in that form. A row from the database
// and an event from the hub must not render as two different states.
function normaliseState(state) {
  if (typeof state !== "string") return null;
  return state.trim().replace(/[_\s]+(.)/g, (_, c) => c.toUpperCase());
}

/// The same value as a phrase, for the `state` column.
function stateWords(state) {
  return String(state).replace(/([A-Z])/g, " $1").toLowerCase().trim();
}

// `ExecutionState::has_money_at_risk`: true exactly in these two. Everything
// about obligations follows from this set, so it is written once.
const MONEY_AT_RISK = new Set(["sent", "confirmed"]);
const TERMINAL = new Set(["completed", "aborted"]);

const executions = [];
const MAX_EXEC_ROWS = 300;

// Obligations resolved by the engine, by intent id. Kept separately from the
// log because the log is history: RISK_AND_SYBIL_SPEC U2 says the row is never
// edited, and a resolved obligation is a new intent and new rows rather than an
// update to the old one. So resolution is recorded beside the log, not in it.
const resolvedObligations = new Set();
// Obligations the engine has confirmed it abandoned with nothing out for them.
// It has stopped managing them and has written the obligation down, and no exit
// transaction exists: either nothing could be built — no signer — or one was
// tried and never reached the network. They stay on the banner until the engine
// reports them resolved.
const haltedObligations = new Set();

// Obligations with an exit transaction on the network right now, whether this
// window's press put it there or an earlier one did. Kept separate from the set
// above precisely so it cannot quietly become a synonym for it: these are not
// halted-and-untouched, and they are not closed either — a transaction in the
// air has decided the position's fate without anybody yet knowing what it
// decided, which is the one state where selling again by hand is the wrong
// move.
const sentObligations = new Set();

// The last receipt, so the modal can show what is still out there after the
// call rather than closing on top of it.
let lastUnwindReceipt = null;

/// Whether this obligation has already been through a call.
///
/// Either way it is not offered again: an exit is already in the air for it, or
/// the engine has already halted on it and pressing again would only re-arm a
/// switch that is armed. Neither is a reason to hide it from the banner.
function isSpokenFor(intentId) {
  return haltedObligations.has(intentId) || sentObligations.has(intentId);
}

/// Whether an exit transaction is on the network for this stranded position.
///
/// `StrandedExit.onNetwork` is the engine's own per-position answer, and per
/// position is the only level this question has an answer at. `exitsSent`,
/// `exitsAlreadyOut` and `exitsInFlight` are counts over the whole call; none
/// of them says which of the rows in front of the operator it is about.
function exitIsOnNetwork(position) {
  return position?.exit?.onNetwork === true;
}

/// `n` positions, pluralised.
function positions(n) {
  return `${n} position${n === 1 ? "" : "s"}`;
}

/// One state transition, appended.
///
/// The payload is `execution_logs` as a row: `intentId`, `seq`, `mint`, `state`,
/// `prevState`, `side`, `sizeLamports`, `signature`, `needsUnwind`,
/// `abortReason`, `mode`, `atMs`. Nothing publishes it yet; the shape is taken
/// from SCHEMA.md rather than invented here so that when the engine starts
/// writing telemetry from the same rows it writes to SQLite, this reads it
/// without a translation layer.
function onExecution(data, atMs) {
  const state = normaliseState(data?.state);
  if (!state) return;
  const container = region("exec-rows");
  if (!container) return;

  const intentId = data.intentId ?? data.id ?? null;
  const record = {
    intentId,
    seq: data.seq,
    mint: data.mint ?? null,
    state,
    prevState: normaliseState(data.prevState),
    side: (data.side ?? "").toLowerCase(),
    sizeLamports: data.sizeLamports,
    signature: data.signature ?? null,
    needsUnwind: !!data.needsUnwind,
    abortReason: data.abortReason ?? null,
    atMs: data.atMs ?? atMs,
  };

  const row = document.createElement("div");
  row.className = "exec-grid row";
  row.setAttribute("role", "row");

  const sideCell = cell("side", record.side || DASH);
  if (record.side === "buy" || record.side === "sell") sideCell.dataset.side = record.side;
  row.append(sideCell);

  row.append(cell("sym", data.symbol ?? (record.mint ? shortKey(record.mint) : DASH)));
  row.append(cell("num", sol(record.sizeLamports)));

  const stateCell = cell("state", stateWords(state));
  stateCell.dataset.state = state;
  row.append(stateCell);

  row.title =
    `${record.intentId ?? "execution"} · seq ${record.seq ?? "?"} · ${clock(record.atMs)}` +
    (record.prevState ? `\nfrom ${stateWords(record.prevState)}` : "") +
    (record.abortReason ? `\naborted: ${record.abortReason}` : "") +
    (record.signature ? `\n${record.signature}` : "");

  // Appended, never rewritten. One row per transition means the pane is the
  // order's history rather than its current state, which is the thing you need
  // when working out how a position was reached rather than what it is now.
  container.prepend(row);
  executions.unshift(record);
  while (executions.length > MAX_EXEC_ROWS) {
    executions.pop();
    container.lastElementChild?.remove();
  }

  setPopulated("exec", true);
  renderOpenPositions();
  renderUnwind();
}

/// The newest row per intent. Where an order is now is the newest row for its
/// `intent_id` — SCHEMA.md says so about the table, and it is true of this list
/// for the same reason.
function latestByIntent() {
  const latest = new Map();
  for (const record of executions) {
    if (record.intentId != null && !latest.has(record.intentId)) {
      latest.set(record.intentId, record);
    }
  }
  return [...latest.values()];
}

/// Every obligation that is still open.
///
/// An obligation is a `needs_unwind` row whose intent has not been resolved.
/// Aborting does not sell anything, and `needs_unwind` is never cleared on the
/// row itself, so "still open" is a question about resolution, not about state.
function openObligations() {
  return latestByIntent().filter(
    (record) => record.needsUnwind && !resolvedObligations.has(record.intentId),
  );
}

/// Whether an obligation is safe to act on yet.
///
/// RISK_AND_SYBIL_SPEC 13.1: an abort from `Confirmed` leaves a position, and an
/// abort from `Sent` leaves a transaction with an unknown outcome. The second is
/// conditional and has to be reconciled — the signature followed until it lands
/// or its blockhash expires — before anything is sold against it. Selling a
/// position that does not exist because an abort assumed the worst is its own
/// incident, so an unknown provenance is treated as unreconciled rather than
/// assumed to be a position.
function isReconciled(record) {
  return record.prevState === "confirmed";
}

/// How many positions are open.
///
/// RISK_AND_SYBIL_SPEC U4: this counts rows in `sent` or `confirmed` **plus**
/// unresolved unwind obligations. Counting only the managed ones lets the engine
/// look emptier than it is while it has orphans it has forgotten about, which is
/// how a one-position limit becomes a three-position limit.
function renderOpenPositions() {
  const managed = latestByIntent().filter((record) => MONEY_AT_RISK.has(record.state)).length;
  const orphans = openObligations().length;
  const open = managed + orphans;
  setText("open-positions", `${open} open`);
  const el = field("open-positions");
  el?.classList.toggle("halt", orphans > 0);
  if (el) {
    el.title = orphans > 0
      ? `${managed} managed, ${orphans} awaiting unwind`
      : `${managed} managed`;
  }
}

/// The unwind banner and the control that opens the confirmation.
///
/// The banner is a status surface: it appears when an execution was abandoned
/// with something still on chain, and it does not clear itself, because
/// `needs_unwind` is never cleared on the row — the obligation ends when the
/// engine reports it resolved, not when somebody has looked at the banner.
///
/// The button beside it opens a confirmation rather than acting. There is no
/// path from one press to a sent transaction anywhere in this file.
function renderUnwind() {
  const banner = document.querySelector(".unwind");
  if (!banner) return;

  const obligations = openObligations();
  banner.dataset.open = obligations.length > 0 ? "true" : "false";

  const halted = obligations.filter((o) => haltedObligations.has(o.intentId)).length;
  const sent = obligations.filter((o) => sentObligations.has(o.intentId)).length;
  const blocked = obligations.filter((o) => !isReconciled(o)).length;

  let text;
  if (obligations.length === 0) {
    text = "no positions awaiting unwind";
  } else {
    const noun = positions(obligations.length);
    if (sent === obligations.length && sent > 0) {
      text = `${noun} — exit${sent === 1 ? "" : "s"} on the network, not confirmed`;
    } else if (sent > 0 && halted > 0) {
      // Two different things to do, and neither is the other one. Collapsing
      // them into "nothing sold" would tell somebody to go and flatten a
      // position that already has a transaction flying at it.
      text = `${noun} still on chain — ${sent} with an exit out, ${halted} with nothing sold`;
    } else if (halted > 0) {
      // The truthful sentence when nothing reached the network, and it wins
      // over every other reading as soon as even one obligation is in it. The
      // engine has stopped, nothing was sold, and a person has to go and close
      // these — that stays the headline whether it applies to one or all.
      text = `${noun} still on chain — engine halted, nothing sold, flatten by hand`;
    } else if (blocked === obligations.length) {
      text = `${noun} left on chain — not reconciled yet`;
    } else {
      text = `${noun} left on chain`;
    }
  }
  setText("unwind-text", text);

  const action = document.querySelector('[data-action="unwind"]');
  if (action) {
    // Nothing to act on and nothing to confirm. Disabled rather than hidden, so
    // the control does not move around as obligations come and go.
    const actionable = obligations.filter((o) => isReconciled(o) && !isSpokenFor(o.intentId));
    action.disabled = actionable.length === 0;
    action.title = actionable.length === 0
      ? sent > 0
        ? "An exit is already on the network for these. Nothing is closed until it confirms; follow the signature rather than selling again."
        : halted > 0
          ? "The engine has already halted on these. Nothing was sold; they have to be closed by hand."
          : blocked > 0
            ? "Nothing can be flattened yet: every obligation is still being reconciled."
            : "Nothing to flatten."
      : `Review ${actionable.length} obligation${actionable.length === 1 ? "" : "s"} before flattening.`;
  }

  if (isUnwindModalOpen()) renderUnwindModal();
}

/// The engine's word that an obligation is closed.
///
/// Arrives as a telemetry line with `source: "unwind"` and a payload of
/// `{ intentId, resolved, outcome }`. `outcome` is what actually happened —
/// a position was flattened, or the transaction never landed and there was
/// nothing out there after all. Both close the obligation; only one of them
/// sold anything, which is why the outcome is shown rather than assumed.
function onUnwindResolution(data) {
  const intentId = data?.intentId ?? data?.id;
  if (intentId == null) return;
  if (data.resolved === false) {
    // An explicit failure to resolve. The obligation stays open and can be
    // acted on again; silently dropping it here is how something gets left on
    // chain with nothing on screen saying so.
    haltedObligations.delete(intentId);
    sentObligations.delete(intentId);
  } else {
    resolvedObligations.add(intentId);
    haltedObligations.delete(intentId);
    sentObligations.delete(intentId);
  }
  renderOpenPositions();
  renderUnwind();
}

// ---------------------------------------------------------------------------
// the unwind confirmation
// ---------------------------------------------------------------------------

// The element the focus came from, so it can be given back on close. A modal
// that drops focus on the body leaves a keyboard user at the top of the
// document with no idea where they were.
let unwindReturnFocus = null;

function unwindModal() {
  return document.querySelector('[data-region="unwind-modal"]');
}

function isUnwindModalOpen() {
  return unwindModal()?.dataset.open === "true";
}

function openUnwindModal() {
  const modal = unwindModal();
  if (!modal || isUnwindModalOpen()) return;
  unwindReturnFocus = document.activeElement;
  setUnwindError("");
  resetUnwindModal();
  renderUnwindModal();
  modal.dataset.open = "true";
  modal.hidden = false;
  // Focus lands on cancel, not on confirm. The dangerous button is never the
  // one a stray return key finds.
  modal.querySelector('[data-action="unwind-cancel"]')?.focus();
}

function closeUnwindModal() {
  const modal = unwindModal();
  if (!modal) return;
  modal.dataset.open = "false";
  modal.hidden = true;
  if (unwindReturnFocus && document.contains(unwindReturnFocus)) {
    unwindReturnFocus.focus();
  }
  unwindReturnFocus = null;
}

function setUnwindError(message) {
  const el = document.querySelector('[data-field="unwind-error"]');
  if (!el) return;
  el.textContent = message;
  el.hidden = !message;
}

/// Draws the obligations the confirmation is about.
///
/// Every number a person is about to act on is shown as the engine reported it:
/// the mint, the side, the size and the signature. The two groups are drawn
/// separately because they are different decisions — one is a position that
/// exists, the other is a transaction whose outcome nobody knows yet.
function renderUnwindModal() {
  // While the result is showing, the modal is reporting rather than asking and
  // must not be redrawn as a question by a telemetry event arriving behind it.
  if (region("unwind-result") && !region("unwind-result").hidden) return;
  const list = document.querySelector('[data-region="unwind-list"]');
  const blockedList = document.querySelector('[data-region="unwind-blocked"]');
  const blockedNote = document.querySelector('[data-region="unwind-blocked-note"]');
  if (!list || !blockedList) return;

  const obligations = openObligations();
  const actionable = obligations.filter((o) => isReconciled(o) && !isSpokenFor(o.intentId));
  const blocked = obligations.filter((o) => !isReconciled(o));

  list.replaceChildren(...actionable.map(obligationRow));
  blockedList.replaceChildren(...blocked.map(obligationRow));
  if (blockedNote) blockedNote.hidden = blocked.length === 0;

  const summary = document.querySelector('[data-field="unwind-summary"]');
  if (summary) {
    summary.textContent = actionable.length === 0
      ? "Nothing here can be flattened yet."
      : `Sell ${actionable.length} position${actionable.length === 1 ? "" : "s"} at market, now.`;
  }

  const confirm = document.querySelector('[data-action="unwind-confirm"]');
  if (confirm) {
    confirm.disabled = actionable.length === 0;
    confirm.textContent = actionable.length === 0
      ? "nothing to flatten"
      : `flatten ${actionable.length}`;
  }
}

function obligationRow(record) {
  const row = document.createElement("div");
  row.className = "oblig-grid row";
  row.setAttribute("role", "row");
  row.append(cell("sym key", record.mint ? shortKey(record.mint) : DASH));
  const side = cell("side", record.side || DASH);
  if (record.side === "buy" || record.side === "sell") side.dataset.side = record.side;
  row.append(side);
  row.append(cell("num", sol(record.sizeLamports)));
  row.append(cell("num dim", record.prevState ? stateWords(record.prevState) : "unknown"));
  row.title =
    `${record.intentId}\n` +
    (record.mint ? `mint ${record.mint}\n` : "") +
    (record.signature ? `signature ${record.signature}\n` : "no signature recorded\n") +
    (record.abortReason ? `aborted: ${record.abortReason}` : "");
  return row;
}

/// Sends the unwind.
///
/// The only call site of `UNWIND_COMMAND` in this file. Three things it
/// deliberately does not do: it does not fall back to any other command if this
/// one is missing, it does not clear the banner on success, and it does not
/// treat a rejection as anything other than a failure. A window that says a
/// position was flattened when nothing was sold is worse than one that says
/// nothing at all.
async function confirmUnwind() {
  const confirm = document.querySelector('[data-action="unwind-confirm"]');
  const obligations = openObligations().filter(
    (o) => isReconciled(o) && !isSpokenFor(o.intentId),
  );
  if (obligations.length === 0 || !invoke) return;

  if (confirm) confirm.disabled = true;
  setUnwindError("");

  try {
    const receipt = await invoke(UNWIND_COMMAND, unwindArgs(obligations));
    console.info("[sts] unwind receipt", receipt);
    applyUnwindReceipt(receipt, obligations);
  } catch (err) {
    // The command may not be registered, or the engine may have refused. Both
    // are real answers and are shown as ones rather than swallowed.
    const message = typeof err === "string" ? err : err?.message ?? String(err);
    setUnwindError(
      /not found|unknown command|not allowed/i.test(message)
        ? `The engine has no ${UNWIND_COMMAND} command. Nothing was sent, and the position is still open.`
        : `Unwind failed: ${message}. Nothing was sent.`,
    );
    console.error("[sts] unwind failed", err);
  } finally {
    renderOpenPositions();
    renderUnwind();
    if (isUnwindModalOpen()) renderUnwindModal();
  }
}

/// Reads the receipt and says what actually happened, position by position.
///
/// The engine answers this at two levels and only one of them is usable here.
/// `exitsSent`, `exitsAlreadyOut` and `exitsConfirmed` are counts over the
/// whole call; `stranded[].exit.onNetwork` is the fate of one position. Marking
/// obligations from the counts — the old `exitsSent >= stranded.length` — is
/// right only while those two numbers happen to describe the same set, and a
/// second press is exactly when they stop: the exits are out, so `exitsSent` is
/// zero, and every row is still stranded.
///
/// `UnwindReceipt` is explicit that a window reading `exitsSent` and reporting
/// the position closed is telling the operator that money they still own is
/// gone. So there is no branch here that closes an obligation unless the engine
/// left it out of `stranded` altogether, which is the engine saying it is gone.
function applyUnwindReceipt(receipt, requested) {
  lastUnwindReceipt = receipt ?? null;

  const stranded = Array.isArray(receipt?.stranded) ? receipt.stranded : [];
  const known = receipt?.strandedKnown !== false;

  // The engine's own list beats this window's. It is rebuilt from
  // `execution_logs`, so it includes obligations from before this window was
  // open — which telemetry alone would never have shown.
  for (const position of stranded) adoptStranded(position);

  const inFlight = new Set(
    stranded.filter(exitIsOnNetwork).map((position) => position.intentId),
  );
  const stillOut = new Set(stranded.map((position) => position.intentId));

  // Everything asked for has been abandoned by the engine whatever else
  // happened, because the halt is armed before anything that can fail. What
  // differs is what is out there now.
  for (const obligation of requested) {
    if (inFlight.has(obligation.intentId)) {
      sentObligations.add(obligation.intentId);
      haltedObligations.delete(obligation.intentId);
    } else if (known && !stillOut.has(obligation.intentId)) {
      // Absent from a list the engine could read is the engine saying this one
      // is closed: it was sold and booked, or it turned out there was nothing
      // on chain behind it. `flattened` and `resolved` say which. An absence
      // from a list it could *not* read says nothing at all, which is why this
      // branch is gated on `known`.
      resolvedObligations.add(obligation.intentId);
      haltedObligations.delete(obligation.intentId);
      sentObligations.delete(obligation.intentId);
    } else {
      haltedObligations.add(obligation.intentId);
      sentObligations.delete(obligation.intentId);
    }
  }

  // The kill switch came with it — `UnwindReceipt` carries the halt's own
  // receipt — so the top bar is now wrong until it is asked again.
  pollEngineStatus();

  showUnwindResult(receipt, stranded, known);
}

/// Folds a `StrandedPosition` into the obligation list.
///
/// `atRiskIn` is the state the money was left at risk in, never `aborted` — the
/// engine reports it precisely because the table's own `aborted` says nothing
/// about what is out there. That maps onto the same reconciliation question the
/// deck already asks, and `conditional` is the engine's answer to it.
function adoptStranded(position) {
  if (!position?.intentId) return;
  const existing = executions.find((record) => record.intentId === position.intentId);
  const prevState = normaliseState(position.atRiskIn);
  if (existing) {
    existing.needsUnwind = true;
    existing.prevState = prevState ?? existing.prevState;
    existing.signature = position.signature ?? existing.signature;
    existing.mint = position.mint ?? existing.mint;
    return;
  }
  executions.push({
    intentId: position.intentId,
    seq: null,
    mint: position.mint ?? null,
    state: "aborted",
    prevState,
    side: String(position.side ?? "").toLowerCase(),
    sizeLamports: position.sizeLamports,
    signature: position.signature ?? null,
    needsUnwind: true,
    abortReason: null,
    atMs: null,
  });
}

/// Turns the modal into a list of what somebody now has to go and do.
///
/// Four readings, and which one is shown is decided per position rather than
/// from a count — see `applyUnwindReceipt`. The order matters: not knowing
/// outranks everything, then a transaction in the air, then a position sitting
/// there with nothing out for it. "Nothing was sold" is only ever said when the
/// receipt says nothing could have been.
function showUnwindResult(receipt, stranded, known) {
  const confirmBody = region("unwind-confirm-body");
  const result = region("unwind-result");
  const list = region("unwind-stranded");
  if (!result || !list) return;

  if (confirmBody) confirmBody.hidden = true;
  result.hidden = false;

  document.querySelector('[data-action="unwind-cancel"]').hidden = true;
  document.querySelector('[data-action="unwind-confirm"]').hidden = true;
  const done = document.querySelector('[data-action="unwind-done"]');
  if (done) {
    done.hidden = false;
    done.focus();
  }

  list.replaceChildren(...stranded.map(strandedRow));

  const sent = receipt?.exitsSent ?? UNWIND_SOLD_NOTHING;
  const alreadyOut = receipt?.exitsAlreadyOut ?? 0;
  const flattened = Array.isArray(receipt?.flattened) ? receipt.flattened.length : 0;
  const inFlight = stranded.filter(exitIsOnNetwork).length;
  // `signer` is `null` when no execution backend was installed, which is the
  // only circumstance in which "nothing could have been sold" is a true
  // sentence. A backend that was there and failed sold nothing either, and
  // saying it the same way would hide a broken signer behind a build note.
  const noSendPath = !receipt?.signer;

  let summary;
  if (!known) {
    // An empty list with `strandedKnown` false means the obligations could not
    // be read, which is not the same as there being none, and must not be shown
    // as though it were.
    summary =
      "The engine is halted. It could not read back what is still on chain, so " +
      "this list is not the answer — check the positions by hand before assuming " +
      "there are none.";
  } else if (stranded.length === 0) {
    summary = flattened > 0
      ? `The engine is halted. ${positions(flattened)} sold, landed and booked; nothing was left on chain.`
      : "The engine is halted. Nothing was left on chain.";
  } else if (inFlight === stranded.length) {
    const one = stranded.length === 1;
    summary =
      `The engine is halted and an exit is on the network for ${one ? "it" : `all ${stranded.length}`}` +
      `${sent === UNWIND_SOLD_NOTHING && alreadyOut > 0 ? ", sent by an earlier press" : ""}. ` +
      `Nothing here is closed until ${one ? "it confirms" : "each one confirms"} — follow the ` +
      `signature${one ? "" : "s"} rather than selling again.`;
  } else if (inFlight > 0) {
    const rest = stranded.length - inFlight;
    summary =
      `The engine is halted. ${inFlight} of ${stranded.length} ha${inFlight === 1 ? "s" : "ve"} an exit ` +
      `on the network and ${inFlight === 1 ? "is" : "are"} not closed until it confirms; the other ` +
      `${rest} ${rest === 1 ? "has" : "have"} nothing out and ${rest === 1 ? "has" : "have"} to be ` +
      `closed by hand.`;
  } else if (noSendPath) {
    summary =
      `The engine is halted and has stopped managing ${positions(stranded.length)}. Nothing was ` +
      `sold — there is no send path in this build — so ${stranded.length === 1 ? "it is" : "they are"} ` +
      `still on chain and have to be closed by hand.`;
  } else {
    summary =
      `The engine is halted and has stopped managing ${positions(stranded.length)}. ${receipt.signer} ` +
      `sold none of ${stranded.length === 1 ? "it" : "them"} and nothing reached the network, so ` +
      `${stranded.length === 1 ? "it is" : "they are"} still on chain — the reason is on each row.`;
  }
  setText("unwind-result-summary", summary);

  // The one instruction that changes with a transaction in the air: do not sell
  // it again. Kept off the summary sentence so it is still there when only some
  // of the rows are flying and the sentence has to be about both halves.
  const note = document.querySelector('[data-field="unwind-inflight"]');
  if (note) {
    note.hidden = inFlight === 0;
    note.textContent = inFlight === 0
      ? ""
      : `${inFlight} exit${inFlight === 1 ? "" : "s"} on the network. Selling one of these again ` +
        `by hand while its signature is unresolved opens a short if it lands. Follow the signature ` +
        `on the row first.`;
  }

  const problems = document.querySelector('[data-field="unwind-problems"]');
  if (problems) {
    const reported = receipt?.problems ?? [];
    problems.hidden = reported.length === 0;
    problems.textContent = reported.length === 0
      ? ""
      : `Problems on the way: ${reported.join("; ")}`;
  }
}

function strandedRow(position) {
  const row = document.createElement("div");
  row.className = "oblig-grid row";
  row.setAttribute("role", "row");
  row.append(cell("sym key", position.mint ? shortKey(position.mint) : DASH));
  const side = cell("side", String(position.side ?? "").toLowerCase() || DASH);
  if (side.textContent === "buy" || side.textContent === "sell") side.dataset.side = side.textContent;
  row.append(side);
  row.append(cell("num", sol(position.sizeLamports)));

  // The last column is what was tried on the way out when something was, and
  // the state the money was left at risk in when nothing was. A row with a
  // transaction in the air is not in the same situation as one nothing was ever
  // attempted for, and the column that used to say "confirmed" for both is the
  // column an operator reads before deciding whether to go and sell it again.
  const onNetwork = exitIsOnNetwork(position);
  const atRisk = position.atRiskIn ? stateWords(normaliseState(position.atRiskIn)) : "unknown";
  const last = cell("num dim", onNetwork ? "exit in flight" : atRisk);
  if (onNetwork) last.dataset.exit = "onNetwork";
  row.append(last);

  row.dataset.exit = onNetwork ? "onNetwork" : position.exit ? "failed" : "none";

  // The signature is the thing somebody flattening this by hand actually needs,
  // so it is on the row rather than only in a log line. The exit's own sentence
  // goes with it: when one was attempted, why it did not close this is the next
  // question after which position it was.
  row.title =
    `${position.intentId}\n` +
    (position.mint ? `mint ${position.mint}\n` : "") +
    `at risk in ${atRisk}\n` +
    (position.signature ? `signature ${position.signature}\n` : "no signature recorded\n") +
    (position.exit?.signature ? `exit signature ${position.exit.signature}\n` : "") +
    (position.exit?.detail ? `${position.exit.detail}\n` : "") +
    (position.conditional ? "conditional: the transaction may never have landed" : "");
  return row;
}

/// Puts the modal back into the state it asks questions in.
function resetUnwindModal() {
  const confirmBody = region("unwind-confirm-body");
  const result = region("unwind-result");
  if (confirmBody) confirmBody.hidden = false;
  if (result) result.hidden = true;
  // The notes belong to the receipt that produced them. Leaving one up over the
  // next question would tell somebody a transaction is flying that landed ten
  // minutes ago.
  for (const field of ["unwind-inflight", "unwind-problems"]) {
    const note = document.querySelector(`[data-field="${field}"]`);
    if (note) {
      note.hidden = true;
      note.textContent = "";
    }
  }
  const cancel = document.querySelector('[data-action="unwind-cancel"]');
  const confirm = document.querySelector('[data-action="unwind-confirm"]');
  const done = document.querySelector('[data-action="unwind-done"]');
  if (cancel) cancel.hidden = false;
  if (confirm) confirm.hidden = false;
  if (done) done.hidden = true;
}

function wireUnwind() {
  document.querySelector('[data-action="unwind"]')?.addEventListener("click", openUnwindModal);
  document.querySelector('[data-action="unwind-cancel"]')?.addEventListener("click", closeUnwindModal);
  document.querySelector('[data-action="unwind-done"]')?.addEventListener("click", closeUnwindModal);
  document.querySelector('[data-action="unwind-confirm"]')?.addEventListener("click", confirmUnwind);

  // Clicking the scrim is a cancel. Only the scrim itself, not a click that
  // started inside the panel and happened to end on it.
  unwindModal()?.addEventListener("mousedown", (event) => {
    if (event.target === unwindModal()) closeUnwindModal();
  });

  document.addEventListener("keydown", (event) => {
    if (!isUnwindModalOpen()) return;

    if (event.key === "Escape") {
      event.preventDefault();
      closeUnwindModal();
      return;
    }

    // A modal that does not hold focus is a dialog the tab key walks straight
    // out of, leaving the operator typing into a surface that is covered up.
    if (event.key !== "Tab") return;
    const focusable = [...unwindModal().querySelectorAll("button:not([disabled])")];
    if (focusable.length === 0) return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  });
}

/// The risk governor.
///
/// Fed from a telemetry line with `source: "risk"` carrying a `RiskSnapshot`.
/// Nothing publishes one yet, so every cell here holds its em dash; the shape is
/// wired so that the day the engine starts publishing, the pane fills without
/// another change.
function onRiskSnapshot(snapshot) {
  if (!snapshot) return;

  setText("equity", sol(snapshot.equityLamports, 4));

  const drawdown = snapshot.drawdownBps ?? 0;
  const limit = snapshot.maxDrawdownBps || 1;
  setText("drawdown", `${bps(drawdown)}%`);
  paintMeter("drawdown-meter", drawdown / limit);

  const positions = snapshot.openPositions ?? 0;
  const maxPositions = snapshot.maxOpenPositions || 1;
  setText("exposure", `${positions}/${maxPositions}`);
  paintMeter("exposure-meter", positions / maxPositions);

  if (snapshot.circuitBreaker) {
    const breaker = snapshot.circuitBreaker;
    setText("breaker-detail", breaker.trippedUntilMs ? `tripped until ${clock(breaker.trippedUntilMs)}` : "clear");
  }
}

/// A meter fill, as a fraction of its own limit.
///
/// Amber from three quarters, red at the limit. The colour changes before the
/// bar is full because the point of the bar is to be seen filling, not to
/// report that it already has.
function paintMeter(name, fraction) {
  const meter = field(name);
  if (!meter) return;
  const clamped = Math.max(0, Math.min(1, Number.isFinite(fraction) ? fraction : 0));
  meter.querySelector("span")?.style.setProperty("--pct", `${(clamped * 100).toFixed(1)}%`);
  meter.classList.toggle("is-warn", clamped >= 0.75 && clamped < 1);
  meter.classList.toggle("is-halt", clamped >= 1);
}

// ---------------------------------------------------------------------------
// the event trail
// ---------------------------------------------------------------------------

const MAX_TRAIL_ROWS = 400;

// A gap in `seq` is a dropped event rather than a quiet engine, and the two are
// indistinguishable on screen unless something says so. This is what says so.
let lastSeq = null;

function onTelemetryLine(event) {
  const container = region("trail-rows");
  if (!container) return;

  if (lastSeq !== null && event.seq > lastSeq + 1) {
    appendTrailRow(container, {
      atMs: event.atMs,
      level: "warn",
      source: "telemetry",
      message: `${event.seq - lastSeq - 1} events dropped before this one`,
    });
  }
  lastSeq = event.seq;

  appendTrailRow(container, event);
}

const LEVEL_CLASS = { debug: "faint", info: "dim", warn: "warn", error: "halt" };

function appendTrailRow(container, event) {
  const row = document.createElement("div");
  row.className = "trail-grid row";
  row.setAttribute("role", "row");
  row.append(cell("num faint", clock(event.atMs)));
  row.append(cell("label", event.source));
  row.append(cell(`trail-msg ${LEVEL_CLASS[event.level] ?? "dim"}`, event.message));

  // The correlation the pane's own note promises: the raw payload is on the row
  // so a decision can be traced back to what it was made on.
  if (event.data && Object.keys(event.data).length > 0) {
    row.title = JSON.stringify(event.data, null, 2);
  }

  container.prepend(row);
  while (container.childElementCount > MAX_TRAIL_ROWS) {
    container.lastElementChild.remove();
  }
  setPopulated("trail", true);
}

// ---------------------------------------------------------------------------
// the telemetry stream
// ---------------------------------------------------------------------------

/// Routes one telemetry event to whichever pane owns it.
///
/// Candidates arrive here rather than on an event of their own: `lib.rs` drains
/// both ingestion channels into the telemetry hub and publishes each one as a
/// line with `source: "ingestion"` and the serialised `CandidateEvent` as its
/// data. There is no separate candidate event to subscribe to.
function onTelemetry(event) {
  if (!event || typeof event.seq !== "number") return;

  switch (event.source) {
    case "ingestion":
      if (event.data?.view) onCandidate(event.data);
      break;
    case "execution":
      onExecution(event.data, event.atMs);
      break;
    case "risk":
      onRiskSnapshot(event.data);
      break;
    case "unwind":
      onUnwindResolution(event.data);
      break;
    case "replay":
      onReplayStatusEvent(event.data);
      break;
    // Every alert rides the hub as well as its own channel, so a window with
    // only this stream open still sees them. The pane is fed from the channel;
    // this arm is what keeps the two from being applied twice, by going through
    // the same sequence-gated admission the channel does.
    case "alert":
      if (event.data?.alert) onAlert(event.data.alert);
      break;
    // One released tick, carrying the `TickKey` the sub-slot jitter is measured
    // from. `geyser.rs` publishes counters through `get_geyser_telemetry`; the
    // per-arrival micro-timestamp exists only here.
    case "geyser":
      onGeyserTick(event.data);
      break;
    default:
      break;
  }

  onTelemetryLine(event);
}

async function openTelemetryStream() {
  if (!invoke || !ChannelCtor) return;
  try {
    const channel = new ChannelCtor(onTelemetry);
    const subscription = await invoke("stream_telemetry", { onEvent: channel });
    console.info(
      `[sts] telemetry subscriber ${subscription.subscriberId}, from seq ${subscription.fromSeq}`,
    );
    lastSeq = subscription.fromSeq > 0 ? subscription.fromSeq - 1 : null;
    markBridge(true);
  } catch (err) {
    markBridge(false);
    console.warn("[sts] telemetry stream unavailable", err);
  }
}

// ---------------------------------------------------------------------------
// 5. revisions, and the one ticker that repaints the feeds
// ---------------------------------------------------------------------------

// Everything below this line arrives faster than a person can read and slower
// than the engine can produce it, which is the situation every feed in this
// file is in and the reason they all go through the same two objects.
//
// `Feed` is the revision discipline. `paint` is the ticker.

/// One revisioned feed.
///
/// Three counters and one rule. The rule is that `revision` goes up by exactly
/// one when something was applied, and never for any other reason — not for a
/// repaint, not for an update that turned out to be the one already on screen,
/// and not for a tick that was skipped. A counter that moved when nothing did
/// would be a counter nobody could use to answer "has this changed since I last
/// looked", which is the only question it is for.
///
/// Two admission tests, because the two feeds behind it are different shapes.
/// The alert feed is a stream with a sequence number on every message, so it is
/// gated on the sequence: anything at or below the highest already applied is a
/// message this feed has seen, which is the normal case on a reconnect and not
/// an error. The journal is a query answered in full every time it is asked, so
/// it is gated on a digest of the answer: two identical answers describe one
/// state, and applying the second would append a page of rows already on
/// screen.
class Feed {
  constructor(name) {
    this.name = name;
    /// Goes up by one per applied change. Never down, never skipped.
    this.revision = 0;
    /// Changes actually applied. Equal to `revision` by construction, and kept
    /// separately so that the suite can assert they are — a `revision` that is
    /// written anywhere but `apply` would show up here as a divergence.
    this.applied = 0;
    /// Updates refused for being at or below the high-water mark.
    this.stale = 0;
    /// Updates refused for describing the state already on screen.
    this.duplicate = 0;
    /// Ticks that found this feed held rather than idle.
    this.skipped = 0;
    /// The highest sequence applied, for a stream.
    this.head = 0;
    /// The digest of the last answer applied, for a query.
    this.digest = null;
    /// What has arrived and not yet been drawn. Held rather than dropped while
    /// the transport is paused; see `paint`.
    this.pending = [];
  }

  /// Whether a streamed message is new. Counts a refusal rather than
  /// swallowing it, because a feed that is quietly discarding half its input
  /// looks exactly like a quiet feed.
  admits(seq) {
    if (!Number.isFinite(seq)) return false;
    if (seq <= this.head) {
      this.stale += 1;
      return false;
    }
    return true;
  }

  /// Whether an answer differs from the one already applied.
  changed(digest) {
    if (this.digest === digest) {
      this.duplicate += 1;
      return false;
    }
    return true;
  }

  /// The one place `revision` is written.
  apply({ seq = null, digest = null } = {}) {
    if (seq !== null) this.head = Math.max(this.head, seq);
    if (digest !== null) this.digest = digest;
    this.revision += 1;
    this.applied += 1;
    return this.revision;
  }
}

const feeds = {
  journal: new Feed("journal"),
  alerts: new Feed("alerts"),
  geyser: new Feed("geyser"),
  // The replay bar is a feed too. It has no rows, but it has exactly the same
  // problem — three unordered sources for one piece of state — and it is
  // counted here so that "how many times has the bar actually changed" is
  // answerable by the same number as every other pane's.
  replay: new Feed("replay"),
};

// --- the ticker ------------------------------------------------------------

/// Whether the repaint is already scheduled, and whether one is running.
///
/// The second is the lock. A feed that arrives *during* a paint must not start
/// a second one inside the first: the row it wants appending is appended by the
/// paint already in progress, and a re-entrant paint would bump the revision
/// twice for one change.
let paintScheduled = false;
let paintLock = false;

/// Ticks that took the lock, found the transport holding the playhead, and put
/// it back down without drawing anything.
///
/// This is the window's half of the rule `spawn_replay_ticker` keeps in
/// `lib.rs`: a tick with no work does not reach for the session. Here the work
/// is a repaint and the reason to skip it is that the playhead is held — every
/// number under the replay bar is the fixture's, the fixture is not advancing,
/// and appending rows to a frozen timeline would put movement on screen that
/// nothing in the engine is doing.
///
/// Nothing is dropped by a skip. The pending queues keep what arrived and the
/// count of what is waiting is drawn beside the feed, so a held feed says how
/// far behind it is rather than looking quiet. The first unheld tick after a
/// resume draws all of it, as one revision, because it is one change to what is
/// on screen however many messages it took to make.
let ticksSkipped = 0;
let ticksPainted = 0;

/// Whether the ticker should draw right now.
///
/// Deliberately the only condition. A window that also skipped when it was
/// hidden, or when nothing had focus, would be a window whose feed depth
/// depended on where the pointer was.
function tickerHeld() {
  return isReplayHeld();
}

/// Asks for a repaint on the next frame. Idempotent within a frame.
function schedulePaint() {
  if (paintScheduled) return;
  paintScheduled = true;
  requestAnimationFrame(paint);
}

/// One tick.
function paint() {
  paintScheduled = false;
  if (paintLock) return;
  paintLock = true;
  try {
    if (tickerHeld()) {
      ticksSkipped += 1;
      for (const feed of Object.values(feeds)) {
        if (feed.pending.length > 0) feed.skipped += 1;
      }
      // The counts still move, because "held, and eleven behind" is a different
      // state from "held" and the operator has to be able to see which.
      renderFeedCounts();
      return;
    }
    ticksPainted += 1;
    flushJournalFeed();
    flushAlertFeed();
    flushGeyser();
    renderFeedCounts();
  } finally {
    paintLock = false;
  }
}

/// Draws whatever the ticker was holding, now.
///
/// Called from `renderReplay` on every status the engine reports, which is what
/// makes a resume flush immediately rather than on whatever happens to arrive
/// next. A feed that only caught up when the next message came in would stay
/// visibly behind on a fixture that was paused at its last record.
function releaseTicker() {
  if (tickerHeld()) return;
  const waiting = Object.values(feeds).some((feed) => feed.pending.length > 0);
  if (waiting) schedulePaint();
}

// ---------------------------------------------------------------------------
// 6. the trade journal and the alert feed
// ---------------------------------------------------------------------------

// Both panes read `src-tauri/src/journal.rs` and `src-tauri/src/alerting.rs`,
// and the split between them is the split those two modules are built on. The
// journal is every trade this engine has settled, queried back out of `sts.db`
// in full on every poll. The alerts are the subset somebody has to act on,
// pushed as they are raised.
//
// Four commands and one channel. `query_journal` and `journal_totals` take the
// same filter and answer different questions about it — the page, and what the
// whole filter adds up to — which is why the totals are asked for separately
// rather than summed from the rows on screen: fifty rows of nine hundred sum to
// the wrong number, and a total that is wrong in the direction of "smaller
// loss" is the expensive direction.
const JOURNAL_QUERY_COMMAND = "query_journal";
const JOURNAL_TOTALS_COMMAND = "journal_totals";
const ALERT_STATUS_COMMAND = "get_alert_status";
const ALERT_STREAM_COMMAND = "stream_alerts";

/// Set false the first time the engine says it has no journal, so a build
/// without one is asked once rather than once a second forever. The same latch
/// every other optional command in this file uses.
let journalSupported = true;
let alertsSupported = true;

/// The most rows either feed keeps. The journal is bounded by the query's own
/// limit as well; this is the second bound, on what is in the document.
const MAX_JOURNAL_ROWS = 200;
const MAX_ALERT_ROWS = 200;

/// How many trades one poll asks for.
const JOURNAL_PAGE = 100;

/// Which of the two feeds is showing. The other one keeps its rows — switching
/// tabs is not a reason to throw away a feed and ask for it again.
let activeFeed = "journal";

/// What the filter chips are set to.
const journalFilter = { mode: "", onlyClosed: false, lossesOnly: false };

/// The last answer applied, held so a tab switch redraws without a round trip.
let journalRows = [];
let journalTotals = null;
let alertStatus = null;

/// The payload `query_journal` and `journal_totals` are both given.
///
/// `JournalFilter` is `#[serde(default)]` on the Rust side, so an absent field
/// is "do not filter on this" rather than a null that has to be spelled. The
/// losses chip sets two fields, not one: a realised loss is only a fact about a
/// trade that closed, and asking for `pnl < 0` without `onlyClosed` would sweep
/// in every open position, which has no realised number at all.
function journalFilterPayload() {
  const filter = { limit: JOURNAL_PAGE };
  if (journalFilter.mode) filter.mode = journalFilter.mode;
  if (journalFilter.onlyClosed || journalFilter.lossesOnly) filter.onlyClosed = true;
  if (journalFilter.lossesOnly) filter.maxRealizedPnlLamports = -1;
  return filter;
}

/// A stable summary of one answer, for the duplicate test.
///
/// Every field that can change without the trade id changing is in it — a trade
/// that closes keeps its id and gains a realised number — so "the same answer"
/// means the same trades in the same states, not merely the same trades.
function journalDigest(rows, totals) {
  return (
    rows
      .map(
        (row) =>
          `${row.tradeId}:${row.proceedsLamports ?? ""}:${row.realizedPnlLamports ?? ""}:${row.closedAtMs ?? ""}`,
      )
      .join("|") +
    "#" +
    (totals ? `${totals.trades}/${totals.closed}/${totals.realizedPnlLamports}` : "")
  );
}

async function pollJournal() {
  if (!invoke || !journalSupported) return;
  const filter = journalFilterPayload();
  try {
    const [rows, totals] = await Promise.all([
      invoke(JOURNAL_QUERY_COMMAND, { filter }),
      invoke(JOURNAL_TOTALS_COMMAND, { filter }),
    ]);
    const next = Array.isArray(rows) ? rows : [];
    const digest = journalDigest(next, totals);
    if (!feeds.journal.changed(digest)) {
      // The same answer as last time. The rows on screen are already this
      // answer, so there is nothing to draw and nothing to count.
      return;
    }
    feeds.journal.pending.push({ rows: next, totals, digest });
    schedulePaint();
  } catch (err) {
    if (isMissingCommand(err)) {
      journalSupported = false;
      renderJournalUnavailable();
      console.warn(`[sts] this build has no ${JOURNAL_QUERY_COMMAND}; the journal pane is empty`);
      return;
    }
    console.warn("[sts] the journal query failed", err);
  }
}

/// Draws whatever the last poll returned, and takes exactly one revision for
/// it however many polls were held.
function flushJournalFeed() {
  const queued = feeds.journal.pending;
  if (queued.length === 0) return;
  // Only the newest matters: each answer is the whole query, so an older one is
  // a page that has already been superseded. They are still all consumed, and
  // the ones passed over are counted as what they are.
  const latest = queued[queued.length - 1];
  feeds.journal.stale += queued.length - 1;
  feeds.journal.pending = [];

  journalRows = latest.rows;
  journalTotals = latest.totals;
  feeds.journal.apply({ digest: latest.digest });
  renderJournalRows();
}

function renderJournalRows() {
  const container = region("journal-rows");
  if (!container) return;
  const rows = journalRows.slice(0, MAX_JOURNAL_ROWS);
  container.replaceChildren(...rows.map(journalRow));
  setJournalState();
}

/// One trade, as `TradeRow` serialises it.
///
/// Nothing here is derived from anything else on the row. The realised number
/// is the engine's, the slippage is the fill's, and an open trade shows an em
/// dash where the realised number would be rather than a zero — a trade that
/// closed exactly flat is a real outcome and it has to look different from one
/// that has not closed at all.
function journalRow(trade) {
  const row = document.createElement("div");
  row.className = "journal-grid row";
  row.setAttribute("role", "row");
  const open = trade.closedAtMs === null || trade.closedAtMs === undefined;
  row.dataset.open = String(open);
  row.dataset.tradeId = trade.tradeId ?? "";

  const mint = gridCell("key", shortKey(trade.mint ?? ""));
  row.append(mint);

  const side = gridCell("side", trade.side ?? DASH);
  side.dataset.side = trade.side ?? "";
  row.append(side);

  row.append(gridCell("num", sol(trade.notionalLamports ?? 0)));

  const pnl = document.createElement("span");
  pnl.className = "num journal-pnl";
  pnl.setAttribute("role", "gridcell");
  const realized = trade.realizedPnlLamports;
  if (!Number.isFinite(realized)) {
    pnl.dataset.sign = "none";
    pnl.textContent = DASH;
  } else {
    pnl.dataset.sign = realized > 0 ? "up" : realized < 0 ? "down" : "flat";
    pnl.textContent = signedSol(realized);
  }
  row.append(pnl);

  row.append(
    gridCell("num", Number.isFinite(trade.slippageBps) ? count(trade.slippageBps) : DASH),
  );

  const state = gridCell("state", open ? "open" : "closed");
  state.dataset.state = open ? "sent" : "completed";
  row.append(state);

  row.title =
    `${trade.tradeId ?? "trade"} · ${trade.mode ?? "?"} · ${trade.venue ?? "no venue"}\n` +
    `${trade.mint ?? "no mint"}\n` +
    `cost ${sol(trade.costBasisLamports ?? 0)} SOL, ` +
    `fees ${sol(trade.feeLamports ?? 0)}, tips ${sol(trade.tipLamports ?? 0)}\n` +
    (open
      ? "still open — no realised number until it closes"
      : `proceeds ${sol(trade.proceedsLamports ?? 0)} SOL, closed ${clock(trade.closedAtMs)}`);

  return row;
}

// --- the alert feed --------------------------------------------------------

const alerts = [];

/// One alert off `stream_alerts`.
///
/// Gated on `seq` and not on arrival. The channel and the telemetry hub both
/// carry every alert, so a window listening to both sees each one twice by
/// design — the hub is what puts an alert in the audit trail beside whatever
/// else was happening, and this is what puts it in the pane. Applying it twice
/// would be the window's fault, not the engine's.
function onAlert(alert) {
  if (!alert || typeof alert.seq !== "number") return;
  if (!feeds.alerts.admits(alert.seq)) return;
  feeds.alerts.pending.push(alert);
  schedulePaint();
}

function flushAlertFeed() {
  const queued = feeds.alerts.pending;
  if (queued.length === 0) return;
  feeds.alerts.pending = [];

  // In sequence order, because the channel does not promise one and a feed
  // read top-down has to be in the order the engine raised them.
  queued.sort((a, b) => a.seq - b.seq);
  const highest = queued[queued.length - 1].seq;

  const container = region("alert-rows");
  if (container) {
    for (const alert of queued) {
      alerts.unshift(alert);
      container.prepend(alertRow(alert));
    }
    while (container.childElementCount > MAX_ALERT_ROWS) {
      container.lastElementChild.remove();
    }
    while (alerts.length > MAX_ALERT_ROWS) alerts.pop();
  }

  feeds.alerts.apply({ seq: highest });
  setJournalState();
}

/// The unit `observed` and `threshold` are counted in, as `AlertUnit` names it.
///
/// Carried on the alert rather than inferred from its kind, and printed rather
/// than assumed, because the two numbers on the row are meaningless without it
/// and a window that guessed would eventually print milliseconds as lamports.
const ALERT_UNITS = {
  basisPoints: (value) => `${count(value)} bps`,
  lamports: (value) => `${sol(value)} SOL`,
  // Not `duration`, which is the uptime formatter and rounds 94 seconds to
  // "1m". The two numbers on a confirmation-latency row are a reading and the
  // threshold it crossed, and a formatter that rounds them both to the same
  // minute is a row that says nothing happened.
  milliseconds: (value) =>
    value < 1_000 ? `${count(value)}ms` : `${(value / 1_000).toFixed(1)}s`,
  count: (value) => count(value),
  micros: (value) => micropct(value),
};

function alertAmount(value, unit) {
  if (!Number.isFinite(value)) return DASH;
  const format = ALERT_UNITS[unit];
  return format ? format(value) : count(value);
}

function alertRow(alert) {
  const row = document.createElement("div");
  row.className = "alert-grid row";
  row.setAttribute("role", "row");
  row.dataset.severity = alert.severity ?? "info";
  row.dataset.kind = alert.kind ?? "";
  row.dataset.seq = String(alert.seq);

  row.append(gridCell("num faint", clock(alert.atMs)));

  const kind = gridCell("alert-kind", alert.kind ?? DASH);
  row.append(kind);

  row.append(gridCell("key", alert.mint ? shortKey(alert.mint) : (alert.subject ?? DASH)));
  row.append(gridCell("num", alertAmount(alert.observed, alert.unit)));
  row.append(gridCell("num faint", alertAmount(alert.threshold, alert.unit)));

  row.title =
    `${alert.severity ?? "info"} · ${alert.kind ?? "alert"} · ${alert.mode ?? "?"}\n` +
    `${alert.message ?? ""}\n` +
    `observed ${alertAmount(alert.observed, alert.unit)} against ${alertAmount(alert.threshold, alert.unit)}` +
    (alert.subject ? `\nsubject ${alert.subject}` : "");

  return row;
}

async function pollAlertStatus() {
  if (!invoke || !alertsSupported) return;
  try {
    alertStatus = await invoke(ALERT_STATUS_COMMAND);
    setJournalState();
  } catch (err) {
    if (isMissingCommand(err)) {
      alertsSupported = false;
      console.warn(`[sts] this build has no ${ALERT_STATUS_COMMAND}; the alert pane is empty`);
      renderJournalUnavailable();
      return;
    }
    console.warn("[sts] the alert status query failed", err);
  }
}

async function openAlertStream() {
  if (!invoke || !ChannelCtor || !alertsSupported) return;
  try {
    const channel = new ChannelCtor(onAlert);
    const subscription = await invoke(ALERT_STREAM_COMMAND, { onAlert: channel });
    // Anything before this happened while nothing was listening. The feed's
    // high-water mark starts there so the first real alert is applied rather
    // than refused as stale.
    feeds.alerts.head = Math.max(0, (subscription?.fromSeq ?? 1) - 1);
    console.info(
      `[sts] alert subscriber ${subscription?.subscriberId}, from seq ${subscription?.fromSeq}`,
    );
  } catch (err) {
    if (isMissingCommand(err)) {
      alertsSupported = false;
      renderJournalUnavailable();
      return;
    }
    console.warn("[sts] the alert stream is unavailable", err);
  }
}

// --- what the pane says about itself ---------------------------------------

/// The count, the revision, and which of the four empty states is showing.
///
/// One function for both feeds because they share one box: whichever is
/// showing decides the count and the empty state, and a second function for the
/// other one is how the two end up disagreeing about which empty state is up.
function renderFeedCounts() {
  const journalRevision = feeds.journal.revision;
  const alertRevision = feeds.alerts.revision;
  const revision = activeFeed === "journal" ? journalRevision : alertRevision;
  const feed = activeFeed === "journal" ? feeds.journal : feeds.alerts;

  const held = feed.pending.length;
  setText(
    "journal-revision",
    `rev ${count(revision)}${held > 0 ? ` · ${count(held)} held` : ""}`,
  );
  const revisionEl = field("journal-revision");
  if (revisionEl) {
    revisionEl.title =
      `The ${activeFeed} feed has applied ${count(revision)} change${revision === 1 ? "" : "s"}. ` +
      `${count(feed.stale)} arrived already applied, ${count(feed.duplicate)} described the state already on screen.` +
      (held > 0
        ? `\n${count(held)} waiting: the playhead is held, so the feed is holding with it.`
        : "");
  }

  if (activeFeed === "journal") {
    const shown = Math.min(journalRows.length, MAX_JOURNAL_ROWS);
    const total = journalTotals?.trades ?? journalRows.length;
    setText("journal-count", `${count(shown)} / ${count(total)}`);
    const countEl = field("journal-count");
    if (countEl && journalTotals) {
      countEl.title =
        `${count(journalTotals.trades)} trade${journalTotals.trades === 1 ? "" : "s"} match this filter, ` +
        `${count(journalTotals.closed)} of them closed.\n` +
        `realised ${signedSol(journalTotals.realizedPnlLamports)} SOL, ` +
        `fees ${sol(journalTotals.feeLamports)}, tips ${sol(journalTotals.tipLamports)}.\n` +
        "The totals are of the filter, not of the rows on screen.";
    }
  } else {
    const shown = Math.min(alerts.length, MAX_ALERT_ROWS);
    // The larger of what the engine last said it had raised and what this
    // window has actually been handed. They disagree for up to a second at a
    // time — alerts are pushed and the counters are polled — and in that window
    // the engine's number is behind. Showing fewer raised than are on screen
    // would be the pane disagreeing with itself.
    const raised = Math.max(alertStatus?.raised ?? 0, alerts.length);
    setText("journal-count", `${count(shown)} / ${count(raised)}`);
    const countEl = field("journal-count");
    if (countEl) {
      countEl.title = alertStatus
        ? `${count(alertStatus.raised)} raised, ${count(alertStatus.suppressed)} held back by the cooldown.\n` +
          "A suppressed alert is one that fired again inside its own quiet window; it is counted, not shown."
        : "The engine has not reported what the alerting engine has done.";
    }
  }
}

/// Which of the two feeds and which of the four empty states is showing.
///
/// One function for both, because they share one box. Two functions — one per
/// feed — is how the two end up disagreeing about which empty state is up, and
/// a box showing an empty state over a populated list is a pane that says
/// nothing has happened while the rows saying otherwise are behind it.
///
/// The regions are set explicitly rather than through `setPopulated`: that
/// helper pairs one list with one empty state, and this box has two lists and
/// four, of which exactly one may ever be visible.
function setJournalState() {
  const available = journalSupported || alertsSupported;
  const onJournal = activeFeed === "journal";
  const journalHasRows = journalRows.length > 0;
  const alertsHaveRows = alerts.length > 0;
  const filtered =
    journalFilter.mode !== "" || journalFilter.onlyClosed || journalFilter.lossesOnly;

  const show = (name, visible) => {
    const el = region(name);
    if (el) el.hidden = !visible;
  };

  show("journal-feed", available && onJournal && journalHasRows);
  show("alert-feed", available && !onJournal && alertsHaveRows);
  show("journal-rows", journalHasRows);
  show("alert-rows", alertsHaveRows);

  // Exactly one empty state, and only when the list it stands in for is not
  // showing. `filtered` is what splits "nothing has happened" from "you asked
  // for something nothing matches", which look identical and mean opposite
  // things.
  show("journal-unavailable", !available);
  show("journal-empty", available && onJournal && !journalHasRows && !filtered);
  show("journal-filtered", available && onJournal && !journalHasRows && filtered);
  show("alert-empty", available && !onJournal && !alertsHaveRows);

  renderFeedCounts();
}

/// The state a build with no journal commands leaves the pane in.
function renderJournalUnavailable() {
  if (journalSupported || alertsSupported) return;
  const container = region("journal-rows");
  if (container) container.replaceChildren();
  journalRows = [];
  setJournalState();
}

function wireJournalFeed() {
  for (const button of document.querySelectorAll("[data-feed]")) {
    button.addEventListener("click", () => {
      activeFeed = button.dataset.feed;
      for (const other of document.querySelectorAll("[data-feed]")) {
        other.setAttribute("aria-pressed", String(other.dataset.feed === activeFeed));
      }
      setJournalState();
    });
  }

  const mode = document.querySelector('[data-filter="journal-mode"]');
  if (mode) {
    mode.addEventListener("change", () => {
      journalFilter.mode = mode.value;
      // The filter changed, so the answer on screen is an answer to a different
      // question. The digest is cleared rather than kept, or the first poll
      // under the new filter would be refused as "the state already showing".
      feeds.journal.digest = null;
      pollJournal();
    });
  }

  for (const [chip, key] of [
    ['[data-filter="journal-closed"]', "onlyClosed"],
    ['[data-filter="journal-losses"]', "lossesOnly"],
  ]) {
    const button = document.querySelector(chip);
    if (!button) continue;
    button.addEventListener("click", () => {
      journalFilter[key] = !journalFilter[key];
      button.setAttribute("aria-pressed", String(journalFilter[key]));
      feeds.journal.digest = null;
      pollJournal();
    });
  }
}

// ---------------------------------------------------------------------------
// 7. the 0x100 sub-slot telemetry view
// ---------------------------------------------------------------------------

// What `geyser.rs` and `subslot.rs` are doing, drawn as a shape.
//
// The window keeps its own ring of the last 0x100 sub-slot samples rather than
// asking the engine for one. The engine's snapshot is counters — `geyser.rs`
// says so in as many words, and it is right to: a derived number in a snapshot
// is one the reader cannot check against the counter it came from. But a
// counter cannot show a *pattern*, and the question this view answers — is the
// feed steady, or is it steady on average — is a question about a pattern.
//
// So the two halves come from two places, and neither is guessed from the
// other. The drift is read straight off `GeyserSnapshot`: the chain head, and
// the heads the cluster has confirmed and finalised behind it. The jitter is
// computed here from the `TickKey` on every event the pipeline releases, which
// is the only place the sub-slot micro-timestamp exists.

const GEYSER_COMMAND = "get_geyser_telemetry";

/// Set false the first time the engine says it has no Geyser feed.
let geyserSupported = true;

/// 0x100. The window, the grid, and the name of the view.
const GEYSER_WINDOW = 0x100;
const GEYSER_ROWS = 0x10;
const GEYSER_COLS = 0x10;

/// How long a slot is, in microseconds.
///
/// Used for exactly one thing: turning a slot difference into a time difference
/// when two arrivals are in different slots, so that the gap between them is
/// comparable with the gap between two in the same one. Everything else on this
/// view is measured rather than assumed — the jitter is a gap against the gap
/// before it, for the reason `SlotMetrics::record_tick_at` gives.
const SLOT_MICROS = MS_PER_SLOT * 1_000;

/// The jitter bands, quietest first, and the glyph each one draws.
///
/// Bands rather than a ramp. The eye cannot tell 300µs of grey from 340µs of
/// grey and can see a block of one glyph in a field of another, and "is there a
/// run of these" is the whole question the grid is here to answer. The
/// thresholds are on the boundaries a Solana feed actually crosses: inside a
/// millisecond is a stream keeping pace with the validator, ten is a stream
/// behind a busy relay, fifty is one whose ordering window is doing real work.
const GEYSER_BANDS = [
  { band: "steady", glyph: ".", underUs: 1_000, note: "under 1ms" },
  { band: "loose", glyph: ":", underUs: 10_000, note: "under 10ms" },
  { band: "wide", glyph: "+", underUs: 50_000, note: "under 50ms" },
  { band: "broken", glyph: "#", underUs: Infinity, note: "50ms and over" },
];

function geyserBand(jitterUs) {
  if (!Number.isFinite(jitterUs)) return GEYSER_BANDS[0];
  return GEYSER_BANDS.find((entry) => jitterUs < entry.underUs) ?? GEYSER_BANDS[3];
}

/// The ring. Fixed length from the first repaint, so the grid is 0x100 cells
/// whether or not 0x100 samples have arrived.
const geyserRing = new Array(GEYSER_WINDOW).fill(null);
let geyserCursor = -1;
let geyserCount = 0;
let geyserSnapshot = null;

/// The previous arrival and the previous gap, which is what a jitter is
/// measured against. `null` rather than zero: a first sample has nothing to
/// compare with and a zero would read as a perfectly steady one.
let lastArrival = null;
let lastGapUs = null;

/// One released tick, as `TickEvent` carries it.
///
/// The key is the whole contract: `{ slot, micros, writeVersion, seq }`, which
/// is `TickKey` in `src-tauri/src/subslot.rs`. Anything without one is a line
/// from the Geyser layer that is not an arrival — a connect, a rollback — and
/// is left to the audit trail.
function onGeyserTick(data) {
  const key = data?.key ?? data?.tick?.key ?? null;
  if (!key || !Number.isFinite(key.slot) || !Number.isFinite(key.micros)) return;
  if (data?.snapshot) geyserSnapshot = data.snapshot;

  const atUs = key.slot * SLOT_MICROS + key.micros;
  let jitterUs = null;
  if (lastArrival !== null && atUs >= lastArrival) {
    const gapUs = atUs - lastArrival;
    if (lastGapUs !== null) jitterUs = Math.abs(gapUs - lastGapUs);
    lastGapUs = gapUs;
  }
  lastArrival = atUs;

  feeds.geyser.pending.push({ slot: key.slot, micros: key.micros, jitterUs });
  schedulePaint();
}

/// Writes what arrived into the ring and redraws, as one revision.
function flushGeyser() {
  const queued = feeds.geyser.pending;
  if (queued.length === 0) return;
  feeds.geyser.pending = [];

  for (const sample of queued) {
    geyserCursor = (geyserCursor + 1) % GEYSER_WINDOW;
    geyserRing[geyserCursor] = sample;
    geyserCount = Math.min(geyserCount + 1, GEYSER_WINDOW);
  }

  feeds.geyser.apply({ seq: feeds.geyser.head + queued.length });
  renderGeyser();
}

/// Every sample in the ring, oldest first.
///
/// The grid is read left to right and top to bottom like anything else written
/// in hex, so `0x00` is the oldest sample the window still holds and `0xff` is
/// the newest. A ring drawn from its own cursor would put the seam in a
/// different column every time a sample landed.
function geyserSamples() {
  if (geyserCount === 0) return [];
  const out = [];
  const start = geyserCount < GEYSER_WINDOW ? 0 : (geyserCursor + 1) % GEYSER_WINDOW;
  for (let index = 0; index < geyserCount; index += 1) {
    out.push(geyserRing[(start + index) % GEYSER_WINDOW]);
  }
  return out;
}

/// The quantiles the stats block prints.
///
/// Computed over the window rather than since the process started, which is the
/// difference between this and `get_metrics`: that answers "what has this run
/// been like", and this answers "what is the feed doing now".
function geyserQuantiles() {
  const values = geyserSamples()
    .map((sample) => sample?.jitterUs)
    .filter((value) => Number.isFinite(value))
    .sort((a, b) => a - b);
  if (values.length === 0) return null;
  const at = (fraction) =>
    values[Math.min(values.length - 1, Math.floor(fraction * values.length))];
  return {
    count: values.length,
    min: values[0],
    p50: at(0.5),
    p95: at(0.95),
    max: values[values.length - 1],
  };
}

/// How far behind the chain head the cluster has actually agreed.
function geyserDrift() {
  if (!geyserSnapshot) return null;
  const head = geyserSnapshot.headSlot ?? 0;
  const confirmed = geyserSnapshot.confirmedHead ?? 0;
  const finalized = geyserSnapshot.finalizedHead ?? 0;
  return {
    head,
    confirmed,
    finalized,
    // Guarded rather than subtracted blind: a confirmed head above the chain
    // head is a provider disagreeing with itself, and a negative drift drawn as
    // a number would read as the feed being ahead of the chain.
    drift: head >= confirmed ? head - confirmed : null,
    finality: confirmed >= finalized ? confirmed - finalized : null,
    reorgs: geyserSnapshot.reorgs ?? 0,
  };
}

async function pollGeyser() {
  if (!invoke || !geyserSupported) return;
  try {
    geyserSnapshot = await invoke(GEYSER_COMMAND);
    renderGeyserSummary();
    if (isGeyserOpen()) renderGeyser();
  } catch (err) {
    if (isMissingCommand(err)) {
      geyserSupported = false;
      renderGeyserSummary();
      console.warn(`[sts] this build has no ${GEYSER_COMMAND}; the subslot view has no feed`);
      return;
    }
    console.warn("[sts] the geyser snapshot failed", err);
  }
}

/// A duration bounded to six characters, for the title beside the status cell.
///
/// `micros` is the general formatter and it grows: two decimals and no ceiling,
/// so a wobble of a second reads as `1000.00ms` and is three characters wider
/// than the same cell was a moment earlier. Everywhere else that is fine — the
/// number is in a cell of its own. In the status bar it is not: the cells sit
/// in a row and a value that gets wider moves every cell to its right while
/// somebody is reading one of them.
///
/// So this trades precision for a bound, in the direction that costs nothing: a
/// tenth of a millisecond under a hundred, whole milliseconds over it, and a
/// ceiling past which the exact number is not the point any more. The view
/// behind the cell prints the unrounded one.
function compactMicros(us) {
  if (!Number.isFinite(us)) return DASH;
  const ms = us / 1_000;
  if (ms >= 9_999) return "9999ms";
  if (ms >= 100) return `${Math.round(ms)}ms`;
  return `${ms.toFixed(1)}ms`;
}

/// The same bound, for a slot count.
function compactSlots(slots) {
  if (!Number.isFinite(slots)) return DASH;
  return slots > 9_999 ? "9999+sl" : `${slots}sl`;
}

/// The status-bar cell: the jitter's median and the drift, and nothing else.
function renderGeyserSummary() {
  const cell = document.querySelector('[data-action="geyser"]');
  const dot = cell?.querySelector(".dot");
  const quantiles = geyserQuantiles();
  const drift = geyserDrift();

  if (!geyserSupported) {
    setText("geyser-summary", DASH);
    setText("geyser-state", "no geyser feed in this build");
    if (dot) dot.className = "dot";
    return;
  }

  if (!quantiles && !drift) {
    setText("geyser-summary", DASH);
    setText("geyser-state", "state unknown");
    if (dot) dot.className = "dot";
    return;
  }

  // One number and a dot, which is the shape every endpoint cell beside this
  // one already has: the dot carries the band and the number carries the
  // reading. Two numbers would need a cell twice as wide as its neighbours, and
  // this row is already wider than the window it sits in — so the jitter goes
  // to the dot, the title and the view, and the drift keeps the digits.
  const behind = drift?.drift ?? null;
  setText("geyser-summary", behind === null ? DASH : compactSlots(behind));

  const cellValue = field("geyser-summary");
  if (cellValue) {
    cellValue.title = quantiles
      ? `Sub-slot arrival jitter: p50 ${micros(quantiles.p50)}, p95 ${micros(quantiles.p95)}, ` +
        `worst ${micros(quantiles.max)} over ${count(quantiles.count)} arrivals.` +
        (behind === null
          ? ""
          : `\nThe cluster has agreed on everything up to ${count(behind)} slot${behind === 1 ? "" : "s"} behind the chain head.`)
      : "No sub-slot arrivals have been released yet.";
  }

  // The dot is the jitter's band and nothing else. Drift is a number the reader
  // has to interpret against what they are doing; a wobbling feed is not.
  const band = quantiles ? geyserBand(quantiles.p50).band : null;
  if (dot) {
    dot.className =
      band === "broken" ? "dot is-halt" : band === "wide" ? "dot is-warn" : "dot is-live";
  }
  setText(
    "geyser-state",
    band === null
      ? "state unknown"
      : `subslot jitter ${band}${behind === null ? "" : `, ${behind} slots behind the head`}`,
  );
}

/// Draws the grid, the legend, and the two stats blocks.
function renderGeyser() {
  renderGeyserSummary();

  const grid = region("geyser-grid");
  if (!grid) return;

  const samples = geyserSamples();
  // Padded to the full window, so the grid is 0x100 cells from the first
  // repaint and the addresses down the gutter always mean the same thing.
  const cells = new Array(GEYSER_WINDOW).fill(null);
  const offset = GEYSER_WINDOW - samples.length;
  for (let index = 0; index < samples.length; index += 1) {
    cells[offset + index] = samples[index];
  }

  const children = [];
  for (let row = 0; row < GEYSER_ROWS; row += 1) {
    const address = document.createElement("span");
    address.className = "addr";
    address.textContent = `0x${(row * GEYSER_COLS).toString(16).padStart(2, "0")}`;
    children.push(address);

    for (let column = 0; column < GEYSER_COLS; column += 1) {
      const index = row * GEYSER_COLS + column;
      const sample = cells[index];
      const cell = document.createElement("span");
      cell.className = "cell";
      if (sample === null) {
        cell.dataset.band = "empty";
        cell.textContent = " ";
      } else {
        const band = geyserBand(sample.jitterUs);
        cell.dataset.band = Number.isFinite(sample.jitterUs) ? band.band : "empty";
        cell.textContent = Number.isFinite(sample.jitterUs) ? band.glyph : " ";
        cell.title =
          `0x${index.toString(16).padStart(2, "0")} · slot ${count(sample.slot)}\n` +
          `+${micros(sample.micros)} into the slot\n` +
          (Number.isFinite(sample.jitterUs)
            ? `jitter ${micros(sample.jitterUs)} against the gap before it`
            : "the first arrival — nothing to measure it against");
      }
      if (index === GEYSER_WINDOW - 1 && samples.length > 0) cell.dataset.head = "true";
      children.push(cell);
    }
  }
  grid.replaceChildren(...children);

  setText(
    "geyser-window",
    `0x${samples.length.toString(16).padStart(3, "0")} / 0x100`,
  );

  const quantiles = geyserQuantiles();
  setText(
    "geyser-grid-scale",
    quantiles ? `p50 ${micros(quantiles.p50)} · max ${micros(quantiles.max)}` : DASH,
  );
  setText(
    "geyser-grid-alt",
    samples.length === 0
      ? "No sub-slot samples have been received."
      : `${samples.length} of 256 sub-slot samples. ` +
        GEYSER_BANDS.map(
          (entry) =>
            `${samples.filter((sample) => Number.isFinite(sample?.jitterUs) && geyserBand(sample.jitterUs).band === entry.band).length} ${entry.band}`,
        ).join(", ") +
        ".",
  );

  renderGeyserLegend();
  renderGeyserStats(quantiles);
}

function renderGeyserLegend() {
  const legend = region("geyser-legend");
  if (!legend) return;
  legend.replaceChildren(
    ...GEYSER_BANDS.map((entry) => {
      const swatch = document.createElement("span");
      swatch.className = "swatch";
      const glyph = document.createElement("span");
      glyph.className = "glyph cell";
      glyph.dataset.band = entry.band;
      glyph.textContent = entry.glyph;
      swatch.append(glyph, document.createTextNode(`${entry.band} ${entry.note}`));
      return swatch;
    }),
  );
}

function renderGeyserStats(quantiles) {
  const stats = region("geyser-stats");
  const ring = region("geyser-ring");
  const drift = geyserDrift();

  if (stats) {
    stats.replaceChildren(
      ...pairs([
        ["samples", quantiles ? `${count(quantiles.count)} of 256` : DASH],
        ["jitter min", quantiles ? micros(quantiles.min) : DASH],
        ["jitter p50", quantiles ? micros(quantiles.p50) : DASH],
        ["jitter p95", quantiles ? micros(quantiles.p95) : DASH],
        ["jitter max", quantiles ? micros(quantiles.max) : DASH],
        ["chain head", drift ? count(drift.head) : DASH],
        ["confirmed", drift ? count(drift.confirmed) : DASH],
        ["finalized", drift ? count(drift.finalized) : DASH],
        ["slot drift", drift?.drift === null || !drift ? DASH : `${count(drift.drift)} slots`],
        [
          "finality lag",
          drift?.finality === null || !drift ? DASH : `${count(drift.finality)} slots`,
        ],
        ["reorgs", drift ? count(drift.reorgs) : DASH],
      ]),
    );
  }

  if (ring) {
    const metrics = geyserSnapshot?.ring ?? null;
    ring.replaceChildren(
      ...pairs([
        ["buffered", metrics ? count(metrics.buffered) : DASH],
        ["released", metrics ? count(metrics.released) : DASH],
        ["late", metrics ? count(metrics.late) : DASH],
        ["shed", metrics ? count(metrics.shed) : DASH],
        ["forced releases", metrics ? count(metrics.forcedReleases) : DASH],
        ["rolled back", metrics ? count(metrics.rolledBack) : DASH],
        ["unrecoverable", metrics ? count(metrics.unrecoverableReorgs) : DASH],
        ["out of order", metrics ? count(metrics.outOfOrderArrivals) : DASH],
      ]),
    );
  }

  setText(
    "geyser-summary-line",
    !geyserSupported
      ? "This build has no Geyser stream. The pipeline is compiled in; nothing is dialling it, so there is nothing to plot."
      : quantiles === null
        ? "No sub-slot samples yet. Every cell below is an arrival, and none have been released."
        : `${count(quantiles.count)} arrivals in the window. ` +
          `Half of them sat within ${micros(quantiles.p50)} of the cadence before them` +
          (drift?.drift === null || !drift
            ? "."
            : `, and the cluster has agreed on everything up to ${count(drift.drift)} slot${drift.drift === 1 ? "" : "s"} behind the head.`),
  );
}

/// A `dt`/`dd` pair per entry, which is what `.detail-grid` draws.
function pairs(entries) {
  const out = [];
  for (const [label, value] of entries) {
    const dt = document.createElement("dt");
    dt.className = "label";
    dt.textContent = label;
    const dd = document.createElement("dd");
    dd.textContent = value;
    out.push(dt, dd);
  }
  return out;
}

// --- opening and closing it ------------------------------------------------

let geyserReturnFocus = null;

function geyserModal() {
  return region("geyser-modal");
}

function isGeyserOpen() {
  return geyserModal()?.dataset.open === "true";
}

function openGeyserView() {
  const modal = geyserModal();
  if (!modal) return;
  geyserReturnFocus = document.activeElement;
  modal.hidden = false;
  modal.dataset.open = "true";
  renderGeyser();
  modal.querySelector('[data-action="geyser-close"]')?.focus();
}

function closeGeyserView() {
  const modal = geyserModal();
  if (!modal) return;
  modal.dataset.open = "false";
  modal.hidden = true;
  if (geyserReturnFocus instanceof HTMLElement) geyserReturnFocus.focus();
  geyserReturnFocus = null;
}

function wireGeyserView() {
  document
    .querySelector('[data-action="geyser"]')
    ?.addEventListener("click", () => openGeyserView());
  document
    .querySelector('[data-action="geyser-close"]')
    ?.addEventListener("click", () => closeGeyserView());
  // The wash closes it, the panel does not. Same as the tick detail.
  geyserModal()?.addEventListener("click", (event) => {
    if (event.target === geyserModal()) closeGeyserView();
  });
}

// ---------------------------------------------------------------------------
// what the suite reads
// ---------------------------------------------------------------------------

/// The window's own counters, for the headless suite.
///
/// Read-only and derived: every number here is read off the same variable the
/// pane is drawn from, never a copy kept beside it. That is the point. The
/// assertion this exists for is that the revision drawn in the pane and the
/// revision the feed actually applied are the same number, and a hook that
/// answered from its own bookkeeping could not fail that assertion however
/// broken the pane was.
Object.defineProperty(window, "__STS_UI__", {
  value: {
    get feeds() {
      const out = {};
      for (const [name, feed] of Object.entries(feeds)) {
        out[name] = {
          revision: feed.revision,
          applied: feed.applied,
          stale: feed.stale,
          duplicate: feed.duplicate,
          skipped: feed.skipped,
          head: feed.head,
          pending: feed.pending.length,
        };
      }
      return out;
    },
    get ticker() {
      return {
        painted: ticksPainted,
        skipped: ticksSkipped,
        held: tickerHeld(),
        scheduled: paintScheduled,
        locked: paintLock,
      };
    },
    get geyser() {
      return {
        supported: geyserSupported,
        window: GEYSER_WINDOW,
        samples: geyserCount,
        quantiles: geyserQuantiles(),
        drift: geyserDrift(),
      };
    },
    get journal() {
      return {
        supported: journalSupported,
        alertsSupported,
        activeFeed,
        rows: journalRows.length,
        alerts: alerts.length,
        filter: { ...journalFilter },
      };
    },
  },
  writable: false,
  configurable: false,
});

// ---------------------------------------------------------------------------
// the kill switch
// ---------------------------------------------------------------------------

function wireKillSwitch() {
  const button = document.querySelector('[data-action="kill-switch"]');
  if (!button || !invoke) return;

  button.addEventListener("click", async () => {
    // No confirmation dialog. The switch is meant to be reachable in one press
    // by someone who has already decided, and pulling it twice is explicitly
    // not an error on the Rust side — `already_armed` comes back in the
    // receipt. A confirm step here would cost seconds at the only moment they
    // are worth anything.
    button.disabled = true;
    try {
      const receipt = await invoke("trigger_kill_switch", {
        reason: "pulled from the UI",
      });
      console.info(
        receipt.alreadyArmed
          ? `[sts] kill switch was already armed at ${clock(receipt.atMs)}`
          : `[sts] kill switch armed at ${clock(receipt.atMs)}`,
        receipt,
      );
      await pollEngineStatus();
    } catch (err) {
      console.error("[sts] kill switch failed", err);
    } finally {
      button.disabled = false;
    }
  });
}

// ---------------------------------------------------------------------------
// start
// ---------------------------------------------------------------------------

function start() {
  wireRadarFilters();
  wireRadarSearch();
  wireKillSwitch();
  wireUnwind();
  wireTickFilters();
  wireTickSort();
  wireTickModal();
  wireReplay();
  wireSolPrice();
  wireJournalFeed();
  wireGeyserView();
  wireKeyboard();
  renderUnwind();
  clearCurveModule();
  clearClusterModule();
  applyTickFilter();
  renderReplay();
  // Drawn once before anything is polled, so the two feeds and the sub-slot
  // cell start at the empty state that says which kind of empty they are
  // rather than blank.
  setJournalState();
  renderGeyserSummary();

  if (!invoke) {
    console.warn("[sts] no Tauri bridge in this window; nothing will be polled");
    markBridge(false, "no engine");
    return;
  }

  openTelemetryStream();
  openAlertStream();
  pollEngineStatus();
  pollMetrics();
  pollBundleTelemetry();
  pollReplayStatus();
  pollJournal();
  pollAlertStatus();
  pollGeyser();
  pollIngestion();
  scheduleIngestionPoll();
  scheduleStatusPoll();
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", start, { once: true });
} else {
  start();
}
