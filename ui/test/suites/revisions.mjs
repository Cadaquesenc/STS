// Revision discipline, and the ticker that keeps it.
//
// Every feed in this window carries a revision. The rule is narrow and the
// whole suite is about the narrowness: **it goes up by exactly one when
// something was applied, and it does not move for any other reason.** Not for a
// repaint. Not for an update that turned out to be the state already on screen.
// Not for a message the feed has already seen. Not for a tick that was skipped.
//
// A counter that moved when nothing did could not answer the only question it
// exists for — has this changed since I last looked — and a counter that failed
// to move when something did is worse, because the pane would be showing rows
// it claims are not there.
//
// The second half is the ticker. It coalesces: a hundred alerts in one frame
// are one repaint and one revision, because they are one change to what is on
// screen. And while the replay transport is holding the playhead, it **skips**
// — takes its lock, finds the run held, puts it down without drawing. That is
// the window's half of the rule `spawn_replay_ticker` keeps in `lib.rs`, and it
// is the reason a paused fixture does not have rows appearing under it.
//
// Nothing is dropped by a skip. What arrived is held, the count of what is
// waiting is on screen, and the first unheld tick draws all of it as one
// revision.

import { enableReplay, setReplay, settleFeeds } from "../seed.mjs";

/// The revision each feed has applied, what the pane says it has applied, and
/// the ticker's own counters.
function readRevisions() {
  const feeds = window.__STS_UI__.feeds;
  return {
    feeds,
    ticker: window.__STS_UI__.ticker,
    shown: document.querySelector('[data-field="journal-revision"]').textContent.trim(),
    activeFeed: window.__STS_UI__.journal.activeFeed,
    alertRows: document.querySelectorAll('[data-region="alert-rows"] .row').length,
    journalRows: document.querySelectorAll('[data-region="journal-rows"] .row').length,
  };
}

/// The revision the pane is *displaying*, parsed back out of the text.
///
/// Read from the DOM rather than from the counter, because the failure this
/// exists to catch is the two disagreeing. A helper that asked the counter
/// would agree with itself no matter how broken the pane was.
function shownRevision(text) {
  const match = /^rev ([\d,]+)/.exec(text);
  return match ? Number(match[1].replace(/,/g, "")) : null;
}

export default {
  name: "revisions",
  async run(t, page) {
    // Every sample taken along the way, tagged with which window it came from.
    //
    // A reload is a new window and its counters start again from zero, which is
    // correct and is asserted below — so the monotonicity claim is about one
    // window's lifetime and the samples are grouped accordingly. Comparing
    // across a reload would be comparing two different windows.
    const seen = [];
    let generation = 0;
    const sample = async (label) => {
      const state = await page.evaluate(readRevisions);
      seen.push({ label, generation, ...state });
      return state;
    };

    await settleFeeds(page);
    const first = await sample("first poll");

    // --- one per applied change ---------------------------------------------
    t.eq(
      "the journal took one revision for the answer it drew",
      first.feeds.journal.revision,
      1,
    );
    t.eq(
      "and applied exactly that many",
      first.feeds.journal.applied,
      first.feeds.journal.revision,
    );
    t.eq(
      "the pane shows the revision the feed actually applied",
      shownRevision(first.shown),
      first.feeds.journal.revision,
    );
    t.eq("nothing has arrived on the alert feed", first.feeds.alerts.revision, 0);
    t.eq("nor the geyser one", first.feeds.geyser.revision, 0);

    // --- an answer that says nothing new ------------------------------------
    //
    // The journal is a query answered in full on every poll, so the ordinary
    // case is that two polls in a row return the same trades. Redrawing that is
    // free; *counting* it would make the revision a count of polls.
    await page.evaluate(() => new Promise((resolve) => setTimeout(resolve, 1_150)));
    await settleFeeds(page);
    const repolled = await sample("second poll, same answer");

    t.ok(
      "the journal really was asked again",
      repolled.feeds.journal.duplicate > first.feeds.journal.duplicate,
      `${repolled.feeds.journal.duplicate} duplicates`,
    );
    t.eq(
      "and an answer describing the state on screen does not move the revision",
      repolled.feeds.journal.revision,
      first.feeds.journal.revision,
    );
    t.eq(
      "and the pane still agrees with it",
      shownRevision(repolled.shown),
      repolled.feeds.journal.revision,
    );

    // A real change does move it.
    await page.evaluate(() => {
      const test = window.__STS_TEST__;
      test.journal[0] = Object.assign({}, test.journal[0], {
        proceedsLamports: 260_000_000,
        realizedPnlLamports: 7_351_500,
        closedAtMs: 1_700_000_004_900,
      });
    });
    await page.evaluate(() => new Promise((resolve) => setTimeout(resolve, 1_150)));
    await settleFeeds(page);
    const changed = await sample("a trade closed");

    t.eq(
      "a trade closing is one change and one revision",
      changed.feeds.journal.revision,
      repolled.feeds.journal.revision + 1,
    );
    t.eq(
      "and the pane moves with it",
      shownRevision(changed.shown),
      changed.feeds.journal.revision,
    );

    // --- a message the feed has already seen --------------------------------
    await page.evaluate(() => {
      window.__STS_TEST__.pushAlert({});
    });
    await settleFeeds(page);
    const oneAlert = await sample("one alert");

    t.eq("an alert is one revision", oneAlert.feeds.alerts.revision, 1);
    t.eq("and one row", oneAlert.alertRows, 1);

    await page.evaluate(() => {
      const test = window.__STS_TEST__;
      test.replayAlert(test.lastAlert);
      test.replayAlert(test.lastAlert);
    });
    await settleFeeds(page);
    const replayed = await sample("the same alert twice more");

    t.eq(
      "the same alert again is not a revision",
      replayed.feeds.alerts.revision,
      oneAlert.feeds.alerts.revision,
    );
    t.eq("and not a row", replayed.alertRows, 1);
    t.eq(
      "it is counted as already applied",
      replayed.feeds.alerts.stale,
      oneAlert.feeds.alerts.stale + 2,
    );

    // --- many arrivals, one repaint -----------------------------------------
    await page.evaluate(() => {
      for (let index = 0; index < 40; index += 1) {
        window.__STS_TEST__.pushAlert({ kind: "tipOverrun", severity: "info" });
      }
    });
    await settleFeeds(page);
    const burst = await sample("forty alerts at once");

    t.eq("forty alerts are forty rows", burst.alertRows, 41);
    t.ok(
      "and cost a handful of revisions rather than forty",
      burst.feeds.alerts.revision - replayed.feeds.alerts.revision <= 3,
      `${burst.feeds.alerts.revision - replayed.feeds.alerts.revision} revisions for 40 alerts`,
    );
    t.eq(
      "the pane's revision is still the feed's",
      shownRevision(
        await page.evaluate(() => {
          document.querySelector('[data-feed="alerts"]').click();
          return document.querySelector('[data-field="journal-revision"]').textContent.trim();
        }),
      ),
      burst.feeds.alerts.revision,
    );

    // --- the ticker holds while the playhead is held -------------------------
    //
    // Everything under the replay bar is the fixture's, and a held fixture is
    // not advancing. Rows appearing under it would be movement nothing in the
    // engine is making.
    // A reload, which is where the second generation starts.
    await enableReplay(page);
    generation += 1;
    const reloaded = await sample("after the reload into replay");
    t.eq(
      "a reloaded window counts from zero again",
      reloaded.feeds.alerts.revision,
      0,
      "the counters describe what this window has drawn, not what the engine has done",
    );
    t.eq("and has drawn no alert rows yet", reloaded.alertRows, 0);

    await page.evaluate(() => document.querySelector('[data-feed="alerts"]').click());
    await settleFeeds(page);
    await setReplay(page, { state: "paused" });
    await settleFeeds(page);

    const beforeHold = await sample("held");
    t.eq(
      "the window agrees the playhead is held",
      beforeHold.ticker.held,
      true,
    );

    await page.evaluate(() => {
      for (let index = 0; index < 5; index += 1) {
        window.__STS_TEST__.pushAlert({ kind: "exitFailed", severity: "critical" });
      }
      window.__STS_TEST__.pushGeyserRun(12, { stepUs: 40_000, wobbleUs: 3_000 });
    });
    await settleFeeds(page);

    const held = await sample("five alerts and twelve ticks, while held");

    t.ok(
      "the ticker ran and skipped rather than not running",
      held.ticker.skipped > beforeHold.ticker.skipped,
      `${held.ticker.skipped} skips`,
    );
    t.eq(
      "a skipped tick does not move the alert revision",
      held.feeds.alerts.revision,
      beforeHold.feeds.alerts.revision,
    );
    t.eq(
      "nor the geyser one",
      held.feeds.geyser.revision,
      beforeHold.feeds.geyser.revision,
    );
    t.eq(
      "and no row appears under a held fixture",
      held.alertRows,
      beforeHold.alertRows,
    );

    // Held, not dropped. This is the difference between a transport that
    // pauses a view and one that loses the events it was paused through.
    t.eq("the five alerts are waiting", held.feeds.alerts.pending, 5);
    t.eq("and so are the twelve ticks", held.feeds.geyser.pending, 12);
    t.ok(
      "and the pane says how far behind it is",
      held.shown.includes("5 held"),
      held.shown,
    );
    t.eq(
      "the revision it shows is still the one it applied",
      shownRevision(held.shown),
      held.feeds.alerts.revision,
    );
    t.ok(
      "and the ticker never keeps its lock across a skip",
      held.ticker.locked === false,
      "a lock held across a skip is a resume that can never draw",
    );

    // --- and releases everything on the first unheld tick --------------------
    await setReplay(page, { state: "playing" });
    await settleFeeds(page);

    const released = await sample("resumed");

    t.eq("nothing is left waiting", released.feeds.alerts.pending, 0);
    t.eq("nor on the geyser feed", released.feeds.geyser.pending, 0);
    t.eq(
      "every alert held through the pause is on screen",
      released.alertRows,
      held.alertRows + 5,
      "a pause that dropped what arrived through it would be a pause nobody could trust",
    );
    t.eq(
      "and the twelve ticks landed in the ring",
      await page.evaluate(() => window.__STS_UI__.geyser.samples),
      12,
    );
    t.eq(
      "the whole hold cost one revision, because it was one change to the screen",
      released.feeds.alerts.revision,
      held.feeds.alerts.revision + 1,
    );
    t.eq(
      "and the pane no longer says anything is waiting",
      released.shown.includes("held"),
      false,
    );

    // --- the replay bar is a feed too ---------------------------------------
    //
    // Three unordered sources for one piece of state — the press, the poll and
    // the ticker's telemetry line — and `ReplayStatus.revision` is what orders
    // them.
    const ordered = await page.evaluate(async () => {
      const test = window.__STS_TEST__;
      const before = window.__STS_UI__.feeds.replay.revision;
      const current = { ...test.replay };

      // A status from before the one already drawn. It says `1x`; the engine is
      // at `5x`; and it must not be drawn.
      test.pushReplay({ ...current, speed: "5", revision: current.revision + 4 });
      await new Promise((resolve) => setTimeout(resolve, 30));
      const newer = [...document.querySelectorAll(".speeds .chip")].find(
        (chip) => chip.getAttribute("aria-pressed") === "true",
      )?.dataset.speed;

      test.pushReplay({ ...current, speed: "1", revision: current.revision + 1 });
      await new Promise((resolve) => setTimeout(resolve, 30));
      const afterStale = [...document.querySelectorAll(".speeds .chip")].find(
        (chip) => chip.getAttribute("aria-pressed") === "true",
      )?.dataset.speed;

      return { before, newer, afterStale, after: window.__STS_UI__.feeds.replay.revision };
    });

    t.eq("a newer status is drawn", ordered.newer, "5");
    t.eq(
      "and one taken before it is not, however late it arrives",
      ordered.afterStale,
      "5",
      "the operator would be reading a multiplier the engine is not at",
    );
    t.ok(
      "the newer one moved the bar's own revision",
      ordered.after > ordered.before,
      `${ordered.before} → ${ordered.after}`,
    );

    // --- a build whose status carries no revision ---------------------------
    //
    // Every build before the field existed. There is no way to order two
    // statuses, so the last one wins — but the window can still refuse to count
    // a redraw that changed nothing, and that is what the digest is for.
    await page.goto(`${page.origin}?replay=1&revision=0`);
    generation += 1;
    await settleFeeds(page);
    await page.evaluate(() => document.querySelector('[data-action="replay-toggle"]').click());
    await settleFeeds(page);

    const digested = await page.evaluate(async () => {
      const test = window.__STS_TEST__;
      const start = window.__STS_UI__.feeds.replay;
      const hasRevision = "revision" in test.replay;

      // The same status three times over. Nothing on the bar changes.
      for (let index = 0; index < 3; index += 1) {
        test.pushReplay({ ...test.replay });
        await new Promise((resolve) => setTimeout(resolve, 20));
      }
      const same = window.__STS_UI__.feeds.replay;

      // And one that does change something.
      test.pushReplay({ ...test.replay, speed: "10" });
      await new Promise((resolve) => setTimeout(resolve, 30));
      return {
        hasRevision,
        startRevision: start.revision,
        sameRevision: same.revision,
        sameDuplicates: same.duplicate - start.duplicate,
        changedRevision: window.__STS_UI__.feeds.replay.revision,
        speed: [...document.querySelectorAll(".speeds .chip")].find(
          (chip) => chip.getAttribute("aria-pressed") === "true",
        )?.dataset.speed,
      };
    });

    t.eq(
      "this build's status really does not carry one",
      digested.hasRevision,
      false,
      "the fallback under test would not be under test otherwise",
    );
    t.eq(
      "three identical statuses do not move the revision",
      digested.sameRevision,
      digested.startRevision,
    );
    t.eq("they are counted as the duplicates they are", digested.sameDuplicates, 3);
    t.eq(
      "and a status that changes something does move it",
      digested.changedRevision,
      digested.sameRevision + 1,
    );
    t.eq("and is drawn", digested.speed, "10");

    // --- across everything above --------------------------------------------
    //
    // The property the whole file is really about, checked over every sample
    // taken along the way rather than at one moment.
    for (const name of ["journal", "alerts", "geyser", "replay"]) {
      const steps = seen.map((entry, index) => {
        const previous = index > 0 && seen[index - 1].generation === entry.generation
          ? seen[index - 1].feeds[name].revision
          : entry.feeds[name].revision;
        return { label: entry.label, value: entry.feeds[name].revision, previous };
      });
      t.every(
        `the ${name} revision never goes backwards`,
        steps,
        (step) => step.value >= step.previous,
        (step) => `${step.label}: ${step.previous} → ${step.value}`,
      );
      t.every(
        `and the ${name} feed applied exactly as many changes as it counted`,
        seen.map((entry) => ({ label: entry.label, feed: entry.feeds[name] })),
        (entry) => entry.feed.applied === entry.feed.revision,
        (entry) => `${entry.label}: applied ${entry.feed.applied}, revision ${entry.feed.revision}`,
      );
    }
  },
};
