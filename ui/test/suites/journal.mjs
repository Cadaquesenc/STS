// The trade journal and the alert feed.
//
// Two feeds in one box, reading `src-tauri/src/journal.rs` and
// `src-tauri/src/alerting.rs`. What is being asserted is mostly not what they
// render but what they refuse to:
//
//   * An open trade has **no realised number**, and an em dash is not a zero.
//     A trade that closed exactly flat is a real outcome; a trade that has not
//     closed has no outcome at all, and a pane that drew both as `0.000` would
//     be inviting somebody to read a position off a blank.
//   * The count is the **filter's** total and not the page's. Fifty rows of
//     nine hundred sum to the wrong number, and wrong in the direction of
//     "smaller loss" is the expensive direction.
//   * The filters are **sent**, not applied on screen. A window that filtered
//     the page it already had would answer a different question from the one
//     the chip asks.
//   * The same alert arriving twice — replayed on the channel, or mirrored on
//     the telemetry hub — is applied **once**.

import { settleFeeds } from "../seed.mjs";

/// Everything the two feeds are showing right now.
function readFeed() {
  const rows = (region) =>
    [...document.querySelectorAll(`[data-region="${region}"] .row`)].map((row) => ({
      cells: [...row.children].map((cell) => cell.textContent.trim()),
      severity: row.dataset.severity ?? null,
      open: row.dataset.open ?? null,
      sign: row.querySelector(".journal-pnl")?.dataset.sign ?? null,
    }));
  const hidden = (region) => document.querySelector(`[data-region="${region}"]`).hidden;
  return {
    journal: rows("journal-rows"),
    alerts: rows("alert-rows"),
    count: document.querySelector('[data-field="journal-count"]').textContent.trim(),
    countTitle: document.querySelector('[data-field="journal-count"]').title,
    revision: document.querySelector('[data-field="journal-revision"]').textContent.trim(),
    tabs: [...document.querySelectorAll("[data-feed]")].map((chip) => ({
      feed: chip.dataset.feed,
      pressed: chip.getAttribute("aria-pressed"),
    })),
    empty: {
      journal: hidden("journal-empty"),
      filtered: hidden("journal-filtered"),
      alerts: hidden("alert-empty"),
      unavailable: hidden("journal-unavailable"),
    },
    feeds: {
      journal: hidden("journal-feed"),
      alerts: hidden("alert-feed"),
    },
    internal: window.__STS_UI__.feeds,
  };
}

/// The filter the window last sent, off the invocation log.
function lastFilter(page, command = "query_journal") {
  return page.evaluate((which) => {
    const calls = window.__STS_TEST__.invocations.filter((c) => c.command === which);
    return calls.length ? calls[calls.length - 1].payload.filter : null;
  }, command);
}

async function clickAndSettle(page, selector) {
  await page.evaluate((sel) => document.querySelector(sel)?.click(), selector);
  await settleFeeds(page);
}

export default {
  name: "journal",
  async run(t, page) {
    await settleFeeds(page);

    // --- the journal ---------------------------------------------------------
    const first = await page.evaluate(readFeed);

    t.eq("the journal is the tab that starts showing", first.tabs[0].pressed, "true");
    t.eq("and the alert feed is not", first.tabs[1].pressed, "false");
    t.eq("the journal list is up", first.feeds.journal, false);
    t.eq("and the alert list is not", first.feeds.alerts, true);
    t.every(
      "no empty state is showing over a populated list",
      Object.entries(first.empty),
      ([, hidden]) => hidden === true,
      ([name]) => name,
    );

    t.eq("every trade the engine recorded is on screen", first.journal.length, 4);
    t.eq(
      "newest first",
      first.journal.map((row) => row.cells[0]).join(","),
      "So11…1112,4hRt…1111,9WzD…1111,7xKX…1111",
    );

    // The open trade, which is the one this pane is most able to lie about.
    const open = first.journal[0];
    t.eq("an open trade is marked open", open.open, "true");
    t.eq("and says so in its state column", open.cells[5], "open");
    t.eq(
      "and has no realised number at all",
      open.cells[3],
      "—",
      "a zero here would read as a trade that closed exactly flat, which is a different outcome",
    );
    t.eq("which is carried as a sign of its own", open.sign, "none");

    // The two closed ones, whose realised numbers are the engine's and not a
    // sum this window computed out of the columns beside them.
    const loss = first.journal[1];
    t.eq("a closed loss shows the engine's realised number", loss.cells[3], "−0.084");
    t.eq("signed down", loss.sign, "down");
    t.eq("and its slippage in basis points", loss.cells[4], "1,620");
    t.eq("and is marked closed", loss.cells[5], "closed");

    const win = first.journal[2];
    t.eq("a closed win shows its realised number", win.cells[3], "+0.018");
    t.eq("signed up", win.sign, "up");

    t.eq("the size column is the notional in SOL", first.journal[0].cells[2], "0.250");
    t.eq("and the side is the trade's own", first.journal[0].cells[1], "buy");

    // --- the count is the filter's, not the page's ---------------------------
    t.eq("the count is shown over held", first.count, "4 / 4");
    t.ok(
      "and says the totals are of the filter",
      first.countTitle.includes("not of the rows on screen"),
      first.countTitle,
    );
    t.ok(
      "and carries the realised total for the whole filter",
      first.countTitle.includes("realised"),
      first.countTitle,
    );

    // A limit smaller than the match is where the two answers differ, and the
    // head has to keep showing the larger one. Asked of the fake directly:
    // the window never sends a limit that small, and the point being pinned is
    // that the two commands answer different questions rather than that this
    // window happens to ask them the same way.
    const paged = await page.evaluate(() => {
      const test = window.__STS_TEST__;
      return {
        page: test.filteredJournal({ limit: 2 }).length,
        total: test.journalTotals(test.filteredJournal({})).trades,
      };
    });
    t.eq("a page can be smaller than the filter it came from", paged.page, 2);
    t.eq("and the filter still matches four", paged.total, 4);

    // --- the filters are sent -----------------------------------------------
    await clickAndSettle(page, '[data-filter="journal-closed"]');
    const closedFilter = await lastFilter(page);
    t.eq("the closed chip sends onlyClosed", closedFilter.onlyClosed, true);
    t.ok(
      "and nothing about a realised bound",
      !("maxRealizedPnlLamports" in closedFilter),
      JSON.stringify(closedFilter),
    );

    const closed = await page.evaluate(readFeed);
    t.eq("and the open trade is gone", closed.journal.length, 3);
    t.every(
      "so every row left is closed",
      closed.journal,
      (row) => row.open === "false",
      (row) => row.cells[0],
    );

    await clickAndSettle(page, '[data-filter="journal-losses"]');
    const lossFilter = await lastFilter(page);
    t.eq("the losses chip bounds the realised number", lossFilter.maxRealizedPnlLamports, -1);
    t.eq(
      "and asks for closed trades as well",
      lossFilter.onlyClosed,
      true,
      "an open trade has no realised number, so a loss filter without this would sweep them in",
    );

    const losses = await page.evaluate(readFeed);
    t.eq("only the loss is left", losses.journal.length, 1);
    t.eq("and it is the one that lost", losses.journal[0].cells[3], "−0.084");

    // The same filter asked for the totals, so the head cannot describe a
    // different question from the rows.
    const totalsFilter = await lastFilter(page, "journal_totals");
    t.eq(
      "the totals are asked for under the same filter as the page",
      JSON.stringify(totalsFilter),
      JSON.stringify(lossFilter),
    );

    // --- a filter that matches nothing --------------------------------------
    await page.evaluate(() => {
      document.querySelector('[data-filter="journal-mode"]').value = "live";
      document
        .querySelector('[data-filter="journal-mode"]')
        .dispatchEvent(new Event("change", { bubbles: true }));
    });
    await settleFeeds(page);

    const none = await page.evaluate(readFeed);
    t.eq("a filter matching nothing empties the list", none.journal.length, 0);
    t.eq("and says it is the filter", none.empty.filtered, false);
    t.eq(
      "rather than saying nothing has happened",
      none.empty.journal,
      true,
      "the two look identical on a blank pane and mean opposite things",
    );

    // Back to everything.
    await page.evaluate(() => {
      document.querySelector('[data-filter="journal-mode"]').value = "";
      document
        .querySelector('[data-filter="journal-mode"]')
        .dispatchEvent(new Event("change", { bubbles: true }));
    });
    await clickAndSettle(page, '[data-filter="journal-closed"]');
    await clickAndSettle(page, '[data-filter="journal-losses"]');
    const restored = await page.evaluate(readFeed);
    t.eq("clearing the filters brings every trade back", restored.journal.length, 4);

    // --- the alert feed ------------------------------------------------------
    await clickAndSettle(page, '[data-feed="alerts"]');

    const emptyAlerts = await page.evaluate(readFeed);
    t.eq("switching tabs hides the journal", emptyAlerts.feeds.journal, true);
    t.eq(
      "and with nothing raised there is no alert list either",
      emptyAlerts.feeds.alerts,
      true,
      "a list with no rows and a heading over it reads as a feed that has gone quiet",
    );
    t.eq("with nothing raised, it says so", emptyAlerts.empty.alerts, false);
    t.eq(
      "and the journal's empty state stays down",
      emptyAlerts.empty.journal,
      true,
      "one box, four empty states, and exactly one of them may ever be up",
    );

    await page.evaluate(() => {
      window.__STS_TEST__.pushAlert({});
      window.__STS_TEST__.pushAlert({
        kind: "confirmationLate",
        severity: "critical",
        observed: 94_000,
        threshold: 90_000,
        unit: "milliseconds",
        subject: "trade-0002",
        mint: null,
      });
    });
    await settleFeeds(page);

    const raised = await page.evaluate(readFeed);
    t.eq("two alerts arrive as two rows", raised.alerts.length, 2);
    t.eq("and now the alert list is up", raised.feeds.alerts, false);
    t.eq("and its empty state is down", raised.empty.alerts, true);
    t.eq("newest first", raised.alerts[0].cells[1], "confirmationLate");
    t.eq("and carries its severity", raised.alerts[0].severity, "critical");
    t.eq("the older one is below it", raised.alerts[1].cells[1], "slippageSpike");
    t.eq("at its own severity", raised.alerts[1].severity, "warn");

    // The unit is on the alert and printed from it. A window that guessed from
    // the kind would eventually print milliseconds as lamports.
    t.eq(
      "a milliseconds alert prints its observed value in seconds",
      raised.alerts[0].cells[3],
      "94.0s",
    );
    t.eq("and its threshold in the same unit", raised.alerts[0].cells[4], "90.0s");
    t.eq(
      "a basis-points alert says it is basis points",
      raised.alerts[1].cells[3],
      "1,620 bps",
    );
    t.eq("and its threshold too", raised.alerts[1].cells[4], "500 bps");

    t.eq(
      "an alert with a mint shows the mint",
      raised.alerts[1].cells[2],
      "4hRt…1111",
    );
    t.eq(
      "and one without falls back to its subject",
      raised.alerts[0].cells[2],
      "trade-0002",
    );

    t.eq("the count is shown over raised", raised.count, "2 / 2");

    // --- the same alert twice ------------------------------------------------
    //
    // Both are states a real window is in. A reconnected channel replays what
    // the subscriber missed and overlaps what it did not; and every alert also
    // rides the telemetry hub, so a window listening to both sees each one on
    // two paths by design.
    const before = raised.internal.alerts;
    await page.evaluate(() => {
      const test = window.__STS_TEST__;
      const seen = test.lastAlert;
      test.replayAlert(seen);
      test.pushAlertOnHub(seen);
    });
    await settleFeeds(page);

    const twice = await page.evaluate(readFeed);
    t.eq("an alert replayed on the channel is not a second row", twice.alerts.length, 2);
    t.eq(
      "and the same alert mirrored on the telemetry hub is not either",
      twice.internal.alerts.applied,
      before.applied,
    );
    t.eq(
      "both are counted as what they are",
      twice.internal.alerts.stale - before.stale,
      2,
      "a message the feed has already applied is not an error and not a silence",
    );

    // --- a build with neither ------------------------------------------------
    await page.goto(`${page.origin}?journal=0`);
    await settleFeeds(page);

    const absent = await page.evaluate(readFeed);
    t.eq("a build with no journal says so", absent.empty.unavailable, false);
    t.eq("rather than showing an empty journal", absent.empty.journal, true);
    t.eq("and no list is up", absent.feeds.journal, true);
    t.eq("nor the alert one", absent.feeds.alerts, true);

    const asked = await page.evaluate(
      () =>
        window.__STS_TEST__.invocations.filter((c) => c.command === "query_journal").length,
    );
    t.ok(
      "and it is asked once rather than once a second forever",
      asked <= 2,
      `${asked} calls after the engine said it has no such command`,
    );
  },
};
