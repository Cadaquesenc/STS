// The jito bundle deck: the tip floor, what moved it, and how sends are ending.
//
// The deck reads one command, `get_bundle_telemetry`, and every cell on it is a
// number the engine already computed. That is what most of this suite is really
// checking — not that the arithmetic is right, which `bundle.rs` proves against
// its own integers, but that the window renders the engine's answer rather than
// deriving its own from parts.
//
// The assertions that matter most are the null ones. A proximity of `null` and
// a proximity of zero are opposite facts: one is "no leader schedule is fitted"
// and the other is "no leader is anywhere near". Every schedule in this build
// answers the first, so a window that drew it as `0.0%` would be reporting a
// measurement nobody made, on the row that moves the tip most.

import { fieldText } from "../seed.mjs";

/// How long to wait for the slow poll to come round again. Must clear one full
/// round trip past `STATUS_POLL_MS`, which is one second in app.js.
const AFTER_A_POLL = 1300;

/// Overwrites part of the telemetry and waits for the deck to be redrawn.
async function patchDeck(page, patch) {
  await page.evaluate((update) => {
    const deck = window.__STS_TEST__.bundles;
    for (const [section, fields] of Object.entries(update)) {
      if (fields === null) {
        deck[section] = null;
      } else if (typeof fields === "object" && !Array.isArray(fields)) {
        Object.assign(deck[section], fields);
      } else {
        deck[section] = fields;
      }
    }
  }, patch);
  await new Promise((resolve) => setTimeout(resolve, AFTER_A_POLL));
}

/// A row's value, its tooltip, and the band on its meter if it has one.
function readRow(page, name) {
  return page.evaluate((field) => {
    const value = document.querySelector(`[data-field="${field}"]`);
    const row = value?.closest(".gov-row");
    const meter = row?.querySelector(".meter");
    const fill = meter?.firstElementChild;
    return {
      text: value?.textContent?.trim() ?? null,
      title: row?.getAttribute("title") ?? null,
      band: meter ? [...meter.classList].filter((c) => c.startsWith("is-")).join(" ") : null,
      pct: fill ? fill.style.getPropertyValue("--pct").trim() : null,
    };
  }, name);
}

export default {
  name: "bundles",
  async run(t, page) {
    // --- the window asks at all ---------------------------------------------
    const asked = await page.evaluate(
      () => window.__STS_TEST__.invocations.filter((i) => i.command === "get_bundle_telemetry").length,
    );
    t.ok("the window asks the engine for its bundle deck", asked >= 1, `${asked} calls`);

    t.eq("the deck says which slot it is priced at", await fieldText(page, "bundle-slot"), "slot 312,905,150");

    // --- the floor ----------------------------------------------------------
    // Lamports, not SOL. A tip is four to seven figures of lamports and would be
    // five leading zeroes in the other unit.
    const floor = await readRow(page, "tip-floor");
    t.eq("the floor is in the unit a tip is decided in", floor.text, "148,500 lam");
    t.ok(
      "and the working behind it is a tooltip away",
      /90,000 observed over 32 slots/.test(floor.title ?? ""),
      floor.title,
    );
    t.ok(
      "including the multiplier that got it there",
      /165.00% multiplier/.test(floor.title ?? ""),
      floor.title,
    );
    t.ok(
      "and which bound, if either, decided it",
      /inside both bounds/.test(floor.title ?? ""),
      floor.title,
    );

    // --- what moved it ------------------------------------------------------
    const congestion = await readRow(page, "congestion");
    t.eq("congestion is millionths rendered as a percentage", congestion.text, "82.0%");
    t.eq("and the meter is filled to the same number", congestion.pct, "82.0%");
    t.eq("a block over four fifths full is a warning band", congestion.band, "is-warn");

    const leader = await readRow(page, "leader-proximity");
    t.eq("a measured proximity is a percentage", leader.text, "32.0%");

    // --- the land rates -----------------------------------------------------
    const land = await readRow(page, "land-rate");
    t.eq("the land rate is the engine's own number", land.text, "75.8%");
    t.eq("and its meter agrees with it", land.pct, "75.8%");
    t.ok(
      "the first-attempt rate is separated from the overall one",
      /56.5% landed first attempt/.test(land.title ?? ""),
      land.title,
    );
    t.ok(
      "and the market's rate is there to compare against",
      /market: 61.0%/.test(land.title ?? ""),
      land.title,
    );
    t.ok(
      "the denominator is everything that resolved, not everything opened",
      /47 landed of 62 resolved/.test(land.title ?? ""),
      land.title,
    );

    // --- the counters -------------------------------------------------------
    const states = await readRow(page, "bundle-states");
    t.eq("live and sent are counted apart", states.text, "3 live · 2 sent");
    t.ok(
      "the two evictions are never added together",
      /9 aged out · 4 lost a leader/.test(states.title ?? ""),
      states.title,
    );
    t.ok(
      "and money that moved is separated from money that did not",
      /5,640,000 lamports paid · 1,800,000 forfeited/.test(states.title ?? ""),
      states.title,
    );

    // --- the latency breakdown ----------------------------------------------
    const settle = await readRow(page, "bundle-settle");
    t.eq("settle time is milliseconds, not micros", settle.text, "226.80ms");
    t.ok(
      "our time and the network's are split apart on the tooltip",
      /ours: 7.20ms to sign and send/.test(settle.title ?? ""),
      settle.title,
    );
    t.ok(
      "and the tail is there too",
      /p95 529.20ms · p99 856.80ms/.test(settle.title ?? ""),
      settle.title,
    );

    // --- down to the safety limit -------------------------------------------
    // 960 is the configured minimum in tauri.conf.json and the execution deck
    // is the pane that gives up the least width on the way there, so these rows
    // are the ones with the least room to lose. Checked with the deck fully
    // populated, since that is when its cells are at their widest.
    for (const width of [1600, 1200, 1180, 1024, 960]) {
      await page.setViewport(width, 800);
      await page.settle();

      const cramped = await page.evaluate(() => {
        const bad = [];
        const deck = document.querySelector('[data-region="bundle-deck"]');
        if (!deck) return ["the deck is not on screen"];

        for (const cell of deck.querySelectorAll(".label, .num, .mono")) {
          if (!cell.getClientRects().length) continue;
          if (cell.scrollWidth - cell.clientWidth > 1) {
            bad.push(
              `clipped "${cell.textContent.trim()}" ${cell.scrollWidth} in ${cell.clientWidth}`,
            );
          }
          // A label that wrapped is not clipped, and is still a label that has
          // run out of room: the row it is in stops reading as one line.
          const tops = new Set();
          const walker = document.createTreeWalker(cell, NodeFilter.SHOW_TEXT);
          for (let node = walker.nextNode(); node; node = walker.nextNode()) {
            const range = document.createRange();
            range.selectNodeContents(node);
            for (const rect of range.getClientRects()) tops.add(Math.round(rect.top));
          }
          if (tops.size > 1) bad.push(`wrapped "${cell.textContent.trim()}" onto ${tops.size} lines`);
        }

        // And the rows themselves stay one row tall.
        for (const row of deck.querySelectorAll(".gov-row")) {
          const height = row.getBoundingClientRect().height;
          if (Math.abs(height - 26) > 0.5) bad.push(`row is ${height}px, not 26`);
        }
        return bad;
      });

      t.every(`the deck holds together at ${width}px`, cramped, () => false, (x) => x);
    }
    await page.setViewport(1440, 900);
    await page.settle();

    // --- an unfitted schedule -----------------------------------------------
    // The state every build ships in today, and the one the row exists to say
    // honestly.
    await patchDeck(page, { floor: { proximityMicros: null } });
    const unknown = await readRow(page, "leader-proximity");
    t.eq("an unmeasured proximity says so rather than showing a zero", unknown.text, "unknown");
    t.ok(
      "and explains that the floor carries no proximity term at all",
      /unmeasured rather than zero/.test(unknown.title ?? ""),
      unknown.title,
    );
    t.ok(
      "which is not the same text a measured zero would get",
      !/0.0%/.test(unknown.text ?? ""),
      unknown.text,
    );

    // --- nothing has resolved yet -------------------------------------------
    await patchDeck(page, {
      land: { overallMicros: null, firstAttemptMicros: null, windowMicros: null },
    });
    const noRate = await readRow(page, "land-rate");
    t.eq("a rate nobody can compute yet is an em dash", noRate.text, "—");
    t.eq("and its meter is empty rather than full", noRate.pct, "0%");
    t.eq("and carries no band, since there is nothing to warn about", noRate.band, "");
    t.ok(
      "the tooltip says why, in as many words",
      /not a rate of zero/.test(noRate.title ?? ""),
      noRate.title,
    );

    // --- a rate that is real and bad ----------------------------------------
    await patchDeck(page, { land: { overallMicros: 210_000, firstAttemptMicros: 90_000 } });
    const poor = await readRow(page, "land-rate");
    t.eq("a low rate is a number, not an em dash", poor.text, "21.0%");
    t.eq("and low is the bad direction for this one", poor.band, "is-warn");

    // --- a floor the market pushed past the ceiling -------------------------
    await patchDeck(page, {
      floor: { lamports: 10_000_000, observedLamports: 24_000_000, clamp: "cut" },
    });
    const cut = await readRow(page, "tip-floor");
    t.eq("a cut floor still shows what is actually bid", cut.text, "10,000,000 lam");
    t.ok(
      "and says the market asked for more than the ceiling",
      /Cut to the configured maximum/.test(cut.title ?? ""),
      cut.title,
    );

    // --- an engine that has never priced anything ---------------------------
    await patchDeck(page, {
      floor: {
        lamports: 10_000,
        observedLamports: 0,
        multiplierMicros: 1_000_000,
        saturationMicros: 0,
        proximityMicros: null,
        landRateMicros: null,
        slotsObserved: 0,
        headSlot: 0,
        clamp: "lifted",
      },
      counts: {
        opened: 0, submitted: 0, retried: 0, landed: 0, evictedRetention: 0,
        evictedLeaderBoundary: 0, rejected: 0, live: 0, inFlight: 0,
      },
    });
    t.eq(
      "a deck with no slots says that rather than naming slot zero",
      await fieldText(page, "bundle-slot"),
      "no slots observed",
    );
    const lifted = await readRow(page, "tip-floor");
    t.eq("an unfitted window still prices the static floor", lifted.text, "10,000 lam");
    t.ok(
      "and says it was the minimum that decided it",
      /Lifted to the configured minimum/.test(lifted.title ?? ""),
      lifted.title,
    );
    t.eq("with nothing opened, the counters read zero honestly", await fieldText(page, "bundle-states"), "0 live · 0 sent");
    t.eq("and congestion is a measured zero, which it is", await fieldText(page, "congestion"), "0.0%");

    // --- a build without the command ----------------------------------------
    // A window talking to an older engine has to degrade to em dashes rather
    // than to zeroes, and it has to stop asking.
    await page.evaluate(() => {
      window.__STS_TEST__.unregistered.add("get_bundle_telemetry");
      window.__STS_TEST__.invocations.length = 0;
    });
    await new Promise((resolve) => setTimeout(resolve, AFTER_A_POLL * 2));
    const attempts = await page.evaluate(
      () => window.__STS_TEST__.invocations.filter((i) => i.command === "get_bundle_telemetry").length,
    );
    t.ok(
      "a build without the command is asked once and then left alone",
      attempts === 1,
      `${attempts} calls after it started refusing`,
    );
  },
};
