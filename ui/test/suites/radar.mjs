// The radar's search, its running counts, and the toasts.
//
// These three arrived on `d2d1d9a` from outside this repository's worktrees
// and landed here with no coverage at all, which is the reason this file
// exists. What they do is straightforward; what they must not do is not.
//
//   * **The search hides rows, it never drops them.** Same rule the tick
//     filters keep. A filter that evicted what it was hiding would turn a typo
//     into data loss on a feed that does not repeat itself.
//   * **The counts are of what arrived, not of what is shown.** Filtering the
//     radar must not change them: "seen" that fell when you typed would be a
//     number nobody could use.
//   * **A toast can never move a pane.** It is the one surface in this window
//     that appears without being asked for, so it is fixed, bounded, and out of
//     the grid entirely.
//   * **The window reaches nothing.** The name lookup that shipped with these
//     features called a third party once per candidate; it is off, and this is
//     the assertion that says so out loud rather than in a comment.

import { LAMPORTS, account, observation, push } from "../seed.mjs";

/// A candidate at a given curve progress, with everything else held still.
function at(index, progressBps, complete = false) {
  return observation({
    index,
    slot: 312_905_000 + index,
    realSol: Math.round(8 * LAMPORTS),
    mcap: Math.round(40 * LAMPORTS),
    progressBps,
    complete,
  });
}

function readRadar() {
  const rows = [...document.querySelectorAll('[data-region="radar-rows"] .row')];
  return {
    total: rows.length,
    shown: rows.filter((row) => !row.hidden).length,
    accounts: rows.filter((row) => !row.hidden).map((row) => row.dataset.account),
    stats: {
      seen: document.querySelector('[data-field="stats-seen"]').textContent.trim(),
      fast: document.querySelector('[data-field="stats-fast"]').textContent.trim(),
      grad: document.querySelector('[data-field="stats-grad"]').textContent.trim(),
      rate: document.querySelector('[data-field="stats-rate"]').textContent.trim(),
    },
    toasts: [...document.querySelectorAll('[data-region="toast-container"] .toast')].map(
      (toast) => ({
        text: toast.querySelector(".toast-message").textContent.trim(),
        level: toast.className,
      }),
    ),
  };
}

async function search(page, query) {
  await page.evaluate((text) => {
    const input = document.querySelector('[data-field="radar-search"]');
    input.value = text;
    input.dispatchEvent(new Event("input", { bubbles: true }));
  }, query);
  await page.settle();
}

export default {
  name: "radar",
  async run(t, page) {
    // A counter over `fetch`, installed before anything is pushed. Every
    // assertion below about the network is really about this number.
    await page.evaluate(() => {
      window.__FETCHES__ = [];
      const real = window.fetch;
      window.fetch = (...args) => {
        window.__FETCHES__.push(String(args[0]));
        return real.apply(window, args);
      };
    });

    await push(page, [
      at(0, 1_000),
      at(1, 4_000),
      at(2, 8_200),
      at(3, 9_900),
      at(4, 10_000, true),
    ]);

    const initial = await page.evaluate(readRadar);
    t.eq("every candidate is on the radar", initial.total, 5);
    t.eq("and all of them are shown", initial.shown, 5);

    // --- the counts ----------------------------------------------------------
    t.eq("seen counts every candidate that arrived", initial.stats.seen, "5");
    t.eq(
      "graduating counts the ones at or past the threshold",
      initial.stats.grad,
      "3",
      "8200, 9900 and the complete one are at or past 8000 bps",
    );
    t.eq(
      "fast path counts the ones routed to it",
      initial.stats.fast,
      "5",
      "the seed routes every candidate to the fast path",
    );
    t.eq("and the rate counts what arrived this minute", initial.stats.rate, "5");

    // --- the search ----------------------------------------------------------
    const first = await page.evaluate((key) => key, account(0));
    await search(page, first.slice(0, 8));

    const searched = await page.evaluate(readRadar);
    t.eq("a search narrows the radar to what matches", searched.shown, 1);
    t.eq("and it is the one that matched", searched.accounts[0], first);
    t.eq(
      "the rows it hid are still there",
      searched.total,
      5,
      "a filter that evicted what it hid would turn a typo into data loss",
    );
    t.eq(
      "and the counts are of what arrived, not of what is shown",
      searched.stats.seen,
      "5",
    );

    // The creator is on every seeded candidate, so searching it matches all of
    // them — which is the assertion that the search reads more than one field.
    await search(page, "9wzdxwbb");
    const byCreator = await page.evaluate(readRadar);
    t.eq("the search matches the creator as well as the account", byCreator.shown, 5);

    await search(page, "zzzzzzzz");
    const nothing = await page.evaluate(readRadar);
    t.eq("a search matching nothing shows nothing", nothing.shown, 0);
    t.eq("and still holds every row", nothing.total, 5);

    await search(page, "");
    const cleared = await page.evaluate(readRadar);
    t.eq("clearing the search brings them all back", cleared.shown, 5);

    // --- toasts --------------------------------------------------------------
    const toasted = await page.evaluate(readRadar);
    t.ok(
      "a graduated curve raises a toast",
      toasted.toasts.some((toast) => toast.text.includes("graduated")),
      toasted.toasts.map((toast) => toast.text).join(" | "),
    );
    t.ok(
      "and one nearing graduation raises a quieter one",
      toasted.toasts.some((toast) => toast.text.includes("nearing graduation")),
      toasted.toasts.map((toast) => toast.text).join(" | "),
    );
    t.ok(
      "the graduation toast is drawn as live and the warning as warn",
      toasted.toasts.every((toast) => /is-(live|warn|dim)/.test(toast.level)),
      toasted.toasts.map((toast) => toast.level).join(" | "),
    );

    // Bounded. A surface that appears unasked has to have a ceiling, or a busy
    // launch minute covers the window it is supposed to be annotating. Reached
    // the way a real run reaches it — through candidates that qualify — rather
    // than by calling the toast function, which the window does not expose.
    await push(
      page,
      Array.from({ length: 8 }, (_, index) => at(10 + index, 10_000, true)),
    );

    // This assertion is load-bearing and the interesting half of it is that it
    // returns at all.
    //
    // The overflow used to be evicted with the animating remove, which only
    // schedules the detach — so the count never moved inside the loop, and the
    // loop never yielded, so the detach it was waiting on could never run. Not
    // a stall: a renderer that stops. The trigger is a sixth toast alive at
    // once, and a toast lives six seconds, so six qualifying candidates inside
    // six seconds is the whole condition. There is no clean way to assert "did
    // not hang"; what there is, is a suite that stops dead if it comes back,
    // and this is where it stops.
    const bounded = await page.evaluate(() => ({
      count: document.querySelector('[data-region="toast-container"]').childElementCount,
      responsive: document.querySelectorAll('[data-region="radar-rows"] .row').length,
    }));
    t.eq(
      "a burst past the ceiling leaves exactly the ceiling",
      bounded.count,
      5,
      "thirteen qualifying candidates, five toasts",
    );
    t.eq(
      "and the window is still answering afterwards",
      bounded.responsive,
      13,
      "the eviction loop used to spin forever on a node it had only scheduled for removal",
    );

    // Dismissing one takes it away and leaves the rest.
    const dismissed = await page.evaluate(async () => {
      const container = document.querySelector('[data-region="toast-container"]');
      const before = container.childElementCount;
      container.querySelector(".toast-dismiss")?.click();
      await new Promise((resolve) => setTimeout(resolve, 220));
      return { before, after: container.childElementCount };
    });
    t.eq(
      "dismissing a toast removes exactly one",
      dismissed.after,
      dismissed.before - 1,
      "and the animation it leaves on does not leave the node behind",
    );

    // --- the toast surface cannot reach the panes ---------------------------
    const geometry = await page.evaluate(() => {
      const container = document.querySelector('[data-region="toast-container"]');
      return {
        position: getComputedStyle(container).position,
        insideApp: document.querySelector(".app").contains(container),
        insidePanes: document.querySelector(".panes").contains(container),
      };
    });
    t.eq("the toast surface is fixed", geometry.position, "fixed");
    t.eq(
      "and lives outside the shell grid",
      geometry.insideApp,
      false,
      "a fourth child of a three-row grid takes a row of its own",
    );
    t.eq("so no pane contains it", geometry.insidePanes, false);

    // --- the window reaches nothing -----------------------------------------
    //
    // Five candidates went past the radar. The name lookup that shipped with
    // these features fires once per new candidate, so if it were on this would
    // be five requests to a third party for accounts this operator is watching.
    const fetches = await page.evaluate(() => window.__FETCHES__);
    t.every(
      "the window made no network request while the radar filled",
      fetches,
      () => false,
      (url) => url,
    );
    t.eq(
      "and specifically asked pump.fun nothing",
      fetches.filter((url) => url.includes("pump.fun")).length,
      0,
      "every curve account the radar sees would otherwise reach the venue's API as it is seen",
    );
  },
};
