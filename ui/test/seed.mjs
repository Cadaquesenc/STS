// What the suites put into the window before they measure it.
//
// Every observation here is written out in full rather than generated, because
// half the assertions are arithmetic — a delta in basis points, a sandwich
// floor, a volume multiple — and an assertion whose expected value is computed
// by the same code that computed the actual one is not an assertion.

export const LAMPORTS = 1_000_000_000;

/// A curve account, as a plausible base58 string. Fixed per index so a run is
/// reproducible and two accounts are told apart by eye in a failure message.
export function account(index) {
  const alphabet = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
  let key = "";
  for (let i = 0; i < 44; i += 1) {
    key += alphabet[(index * 7 + i * 13) % alphabet.length];
  }
  return key;
}

/// One observation, in the shape `CandidateView` serialises to.
///
/// `virtualSolReserves` defaults to the launch reserve plus the real one, which
/// is what a pump.fun curve actually holds — every buy adds its net input to
/// both — but every caller that cares states it outright.
export function observation({
  index = 0,
  slot,
  realSol,
  virtualSol,
  mcap,
  progressBps,
  complete = false,
}) {
  const real = realSol;
  const virt = virtualSol ?? 30 * LAMPORTS + real;
  return {
    provider: "helius",
    slot,
    account: account(index),
    program: "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",
    creator: "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM",
    marketCapLamports: mcap,
    poolLamports: real,
    virtualSolReserves: virt,
    virtualTokenReserves: 347_400_000_000_000,
    curveProgressBps:
      progressBps ?? Math.min(10_000, Math.floor((real * 10_000) / (85 * LAMPORTS))),
    curveComplete: complete,
    slotsSinceLaunch: 40,
  };
}

/// Pushes observations through the telemetry channel, in order.
export async function push(page, views) {
  await page.evaluate((batch) => {
    for (const view of batch) window.__STS_TEST__.pushCandidate(view);
  }, views);
  await page.settle();
}

/// Selects the first radar row, which is what fills the forensic pane.
export async function selectFirst(page) {
  await page.evaluate(() => {
    const row = document.querySelector('[data-region="radar-rows"] .row:not([hidden])');
    row?.click();
  });
  await page.settle();
}

/// Reloads the window against a build that has a replay control, then enters it.
///
/// The reload is not incidental. `app.js` stops asking for a command the engine
/// has said it does not have, so a build that grows one halfway through a
/// session is a state the real thing cannot be in — and a helper that faked it
/// would be testing a code path that does not exist.
///
/// Anything pushed into the window before this is gone afterwards. Suites that
/// need both a populated pane and the replay bar call this first.
///
/// `transport: false` is a build that has the replay bar and not the four
/// buttons on it, which is the state every build before this one was in. It
/// has to be chosen before the load for the same reason the replay control
/// itself does.
export async function enableReplay(page, status = null, { transport = true } = {}) {
  await page.goto(`${page.origin}?replay=1${transport ? "" : "&transport=0"}`);
  await page.settle();

  if (status) {
    await page.evaluate((patch) => {
      Object.assign(window.__STS_TEST__.replay, patch);
    }, status);
  }

  await page.evaluate(() => document.querySelector('[data-action="replay-toggle"]').click());
  await page.settle();
}

/// Puts the engine and the window at the same replay status.
///
/// `pushReplay` alone only tells the window something; the fake engine goes on
/// answering whatever it answered before, which is a real state — a window that
/// heard a telemetry line the command has not caught up with — and several
/// assertions are about exactly that. It is the wrong state to measure a
/// *command* from, though: a press would come back with the engine's older
/// answer and the difference would look like the press having done nothing.
/// This sets both.
export async function setReplay(page, patch) {
  await page.evaluate((fields) => {
    const test = window.__STS_TEST__;
    Object.assign(test.replay, fields);
    // `active` is derived from `state` and never set beside it, exactly as
    // `PlaybackState::is_active` derives it in Rust. A helper that let a caller
    // write both would let a suite construct a status the engine cannot
    // produce — `active: false, state: "playing"` — and then assert the window
    // handles it.
    if ("state" in fields) test.replay.active = test.replay.state !== "stopped";
    // A new edition, the way a real session stamps one under the lock on the
    // way out of a change. Without this the window is entitled to treat the
    // push as a status it has already applied.
    if (test.numbersEditions) test.replay.revision = (test.replay.revision ?? 0) + 1;
    test.pushReplay({ ...test.replay });
  }, patch);
  await page.settle();
}

/// The computed value of one field, by its `data-field` name.
export function fieldText(page, name) {
  return page.evaluate(
    (field) => document.querySelector(`[data-field="${field}"]`)?.textContent?.trim() ?? null,
    name,
  );
}

/// Waits for the feeds to have been asked, answered and drawn.
///
/// The journal is polled on the slow cadence, but every filter press asks for
/// it again straight away and alerts are pushed rather than polled — so what
/// this actually waits for is a promise to resolve and a frame to be painted,
/// not a second to go by. Two rounds of it, because the poll's answer schedules
/// the paint and the paint is what writes the rows.
export async function settleFeeds(page) {
  for (let round = 0; round < 2; round += 1) {
    await page.evaluate(() => new Promise((resolve) => setTimeout(resolve, 90)));
    await page.settle();
  }
}
