// The bonding curve module, checked against the arithmetic it claims.
//
// The three numbers on the sandwich row are not a rendering of something the
// engine sent — they are a closed form this window evaluates itself — so they
// are checked against the table in REPLAY_AND_SIMULATION_SPEC.md §15.2 rather
// than against themselves.

import { LAMPORTS, observation, push, selectFirst, fieldText } from "../seed.mjs";

/// §15.2's own worked examples: the smallest victim buy a front-run pays on, at
/// three points along the curve. These are the specification's numbers, typed
/// in by hand from the table, and nothing in `app.js` produced them.
const SPEC_TABLE = [
  { virtualSol: 30, minimumVictimBuy: "0.306", where: "launch" },
  { virtualSol: 75, minimumVictimBuy: "0.765", where: "y_r = 45" },
  { virtualSol: 115, minimumVictimBuy: "1.173", where: "graduation" },
];

async function showCurve(page, view) {
  await push(page, [view]);
  await page.evaluate((wanted) => {
    const row = [...document.querySelectorAll('[data-region="radar-rows"] .row')].find(
      (candidate) => candidate.dataset.account === wanted,
    );
    row?.click();
  }, view.account);
  await page.settle();
}

export default {
  name: "curve",
  async run(t, page) {
    // --- nothing selected ---------------------------------------------------
    const blank = await page.evaluate(() =>
      [
        "migration-pct",
        "migration-remaining",
        "curve-real-sol",
        "curve-virtual-sol",
        "curve-reserve-ratio",
        "sandwich-floor",
      ].map((name) => document.querySelector(`[data-field="${name}"]`).textContent.trim()),
    );
    t.every(
      "every curve cell is an em dash before anything is selected",
      blank,
      (text) => text === "—",
      (text) => JSON.stringify(text),
    );
    t.eq(
      "and the sandwich badge says it does not know",
      await page.evaluate(() => document.querySelector('[data-field="sandwich-badge"]').dataset.risk),
      "unknown",
    );

    // --- the progress bar ---------------------------------------------------
    await showCurve(
      page,
      observation({ index: 3, slot: 312_905_200, realSol: 62.65 * LAMPORTS, mcap: 92.65 * LAMPORTS }),
    );

    t.eq("the migration percentage is the engine's own bps", await fieldText(page, "migration-pct"), "73.7%");
    t.near(
      "the bar is filled to that percentage and no other",
      await page.evaluate(() =>
        Number.parseFloat(
          document.querySelector('[data-field="migration-fill"]').style.getPropertyValue("--pct"),
        ),
      ),
      73.7,
      0.05,
    );
    t.eq(
      "the remainder is stated against the 85 SOL threshold",
      await fieldText(page, "migration-remaining"),
      "22.350 of 85.000 SOL left",
    );
    t.eq("real SOL is the executable reserve", await fieldText(page, "curve-real-sol"), "62.650");
    t.eq("virtual SOL is the price-setting one", await fieldText(page, "curve-virtual-sol"), "92.650");
    t.eq(
      "the reserve ratio is real over virtual",
      await fieldText(page, "curve-reserve-ratio"),
      `${((62.65 / 92.65) * 100).toFixed(1)}%`,
    );

    // --- the sandwich floor, against the specification's table --------------
    for (const row of SPEC_TABLE) {
      await showCurve(
        page,
        observation({
          index: 10 + row.virtualSol,
          slot: 312_906_000 + row.virtualSol,
          realSol: Math.max(0, (row.virtualSol - 30) * LAMPORTS),
          virtualSol: row.virtualSol * LAMPORTS,
          mcap: row.virtualSol * LAMPORTS,
        }),
      );
      t.eq(
        `the floor at ${row.where} (y = ${row.virtualSol} SOL) matches §15.2`,
        await fieldText(page, "sandwich-floor"),
        `${row.minimumVictimBuy} SOL`,
      );
    }

    // --- the badge ----------------------------------------------------------
    // The corpus median first buy is 0.52 SOL. A curve whose floor is at or
    // below that is one where a median-sized buy already pays a front-runner.
    const badgeAt = async (virtualSol, index) => {
      await showCurve(
        page,
        observation({
          index,
          slot: 312_907_000 + index,
          realSol: Math.max(0, virtualSol - 30 * LAMPORTS),
          virtualSol,
          mcap: virtualSol,
        }),
      );
      return page.evaluate(() => {
        const badge = document.querySelector('[data-field="sandwich-badge"]');
        return { risk: badge.dataset.risk, text: badge.textContent.trim(), title: badge.title };
      });
    };

    const atLaunch = await badgeAt(30 * LAMPORTS, 200);
    t.eq("a launch curve reads as exposed", atLaunch.risk, "exposed");
    t.eq("and says so in a word", atLaunch.text, "sandwich pays");
    t.ok(
      "and shows the arithmetic behind it",
      atLaunch.title.includes("φy/(1−φ)²") && atLaunch.title.includes("0.520 SOL"),
      atLaunch.title,
    );

    const atGraduation = await badgeAt(115 * LAMPORTS, 201);
    t.eq("a curve near graduation reads as guarded", atGraduation.risk, "guarded");
    t.eq("and says so in a word", atGraduation.text, "below floor");

    // The exact edge: y = 50.9652 SOL puts the floor at exactly the corpus
    // median, and "at the threshold" is the exposed side of it.
    const atEdge = await badgeAt(50_965_200_000, 202);
    t.eq("a floor exactly at the corpus median is exposed", atEdge.risk, "exposed");
    const justAbove = await badgeAt(50_965_300_000, 203);
    t.eq("one lamport of reserve above it is not", justAbove.risk, "guarded");

    // --- a graduated curve --------------------------------------------------
    await showCurve(
      page,
      observation({
        index: 210,
        slot: 312_908_000,
        realSol: 85 * LAMPORTS,
        virtualSol: 115 * LAMPORTS,
        mcap: 115 * LAMPORTS,
        progressBps: 10_000,
        complete: true,
      }),
    );
    const graduated = await page.evaluate(() => ({
      risk: document.querySelector('[data-field="sandwich-badge"]').dataset.risk,
      floor: document.querySelector('[data-field="sandwich-floor"]').textContent.trim(),
      remaining: document.querySelector('[data-field="migration-remaining"]').textContent.trim(),
      fillComplete: document.querySelector('[data-field="migration-fill"]').dataset.complete,
    }));
    t.eq("a migrated curve is its own state, not 100%", graduated.risk, "graduated");
    t.eq("and quotes no sandwich floor, because there is no curve to quote", graduated.floor, "—");
    t.eq("and says it has migrated rather than how far it has left", graduated.remaining, "migrated");
    t.eq("and the bar is drawn as a finished curve", graduated.fillComplete, "true");

    // --- a backend that does not send the virtual reserve --------------------
    await page.evaluate(() => {
      window.__STS_TEST__.pushCandidate({
        slot: 312_909_000,
        account: "OldBackendCurveAccountWithNoVirtualReserves11",
        marketCapLamports: 50 * 1_000_000_000,
        poolLamports: 20 * 1_000_000_000,
        curveProgressBps: 2_352,
        virtualSolReserves: undefined,
      });
    });
    await page.settle();
    await page.evaluate(() => {
      [...document.querySelectorAll('[data-region="radar-rows"] .row')]
        .find((row) => row.dataset.account === "OldBackendCurveAccountWithNoVirtualReserves11")
        ?.click();
    });
    await page.settle();

    const old = await page.evaluate(() => ({
      risk: document.querySelector('[data-field="sandwich-badge"]').dataset.risk,
      virtual: document.querySelector('[data-field="curve-virtual-sol"]').textContent.trim(),
      ratio: document.querySelector('[data-field="curve-reserve-ratio"]').textContent.trim(),
      floor: document.querySelector('[data-field="sandwich-floor"]').textContent.trim(),
      real: document.querySelector('[data-field="curve-real-sol"]').textContent.trim(),
      pct: document.querySelector('[data-field="migration-pct"]').textContent.trim(),
    }));
    t.eq("an unreported virtual reserve is an em dash, not a zero", old.virtual, "—");
    t.eq("so is the ratio that would have been computed from it", old.ratio, "—");
    t.eq("and the floor", old.floor, "—");
    t.eq("and the badge says unknown rather than guessing", old.risk, "unknown");
    t.eq("everything the backend did send is still shown", old.real, "20.000");
    t.eq("including the migration percentage", old.pct, "23.5%");

    // --- the module tracks the subject in real time --------------------------
    const account = await page.evaluate(
      () => document.querySelector('[data-field="subject-curve-account"]').textContent.trim(),
    );
    await page.evaluate((wanted) => {
      window.__STS_TEST__.pushCandidate({
        slot: 312_909_500,
        account: wanted,
        marketCapLamports: 60 * 1_000_000_000,
        poolLamports: 30 * 1_000_000_000,
        virtualSolReserves: 60 * 1_000_000_000,
        curveProgressBps: 3_529,
      });
    }, account);
    await page.settle();

    t.eq(
      "a later observation of the subject redraws the bar without a click",
      await fieldText(page, "migration-pct"),
      "35.3%",
    );
    t.eq(
      "and fills in what the earlier frame did not carry",
      await fieldText(page, "curve-virtual-sol"),
      "60.000",
    );
  },
};
