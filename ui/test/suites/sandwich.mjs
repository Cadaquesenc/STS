// The sandwich column: β against the reserve a buy arrived at, and the
// threshold §15.2 derives.
//
// Every expected number here is worked out by hand from the observations below
// rather than computed by the code under test. The arithmetic is small enough
// to do that with, and an assertion whose expected value comes out of the same
// function as the actual one asserts that the function equals itself.
//
//   φ = 100 bps, so the threshold φ/(1−φ) is 100·10⁴/9900 = 101.01 bps, which
//   the strip reports rounded up as 102. A buy clears it when
//
//       net · (10⁴ − 100) > 100 · y      ⟺      net · 99 > y
//
//   with `net` the SOL the buy added to the pool and `y` the virtual reserve of
//   the observation *before* it.

import { LAMPORTS, observation, push, account } from "../seed.mjs";

const rows = () =>
  [...document.querySelectorAll('[data-region="tick-rows"] .row')].map((row) => ({
    hidden: row.hidden,
    account: row.dataset.account,
    slot: row.children[0].textContent.trim(),
    beta: row.children[5].textContent.trim(),
    sandwich: row.children[5].dataset.sandwich,
  }));

export default {
  name: "sandwich",
  async run(t, page) {
    const walk = [
      // --- account 0: a first observation, a small buy, a large one, a sell,
      //     and one that moved nothing --------------------------------------
      observation({ index: 0, slot: 400_000_000, realSol: 10 * LAMPORTS, virtualSol: 40 * LAMPORTS, mcap: 40 * LAMPORTS }),
      // +0.3 SOL against a 40 SOL reserve. β = 0.3·10⁴/40 = 75 bps, and
      // 0.3·99 = 29.7 is not above 40, so no front-run pays.
      observation({ index: 0, slot: 400_000_010, realSol: 10.3 * LAMPORTS, virtualSol: 40.3 * LAMPORTS, mcap: 41 * LAMPORTS }),
      // +0.5 SOL against 40.3. β = 0.5·10⁴/40.3 = 124.06 → 124 bps, and
      // 0.5·99 = 49.5 is above 40.3, so one does.
      observation({ index: 0, slot: 400_000_020, realSol: 10.8 * LAMPORTS, virtualSol: 40.8 * LAMPORTS, mcap: 42 * LAMPORTS }),
      // −0.8 SOL. A sell is not a victim buy.
      observation({ index: 0, slot: 400_000_030, realSol: 10 * LAMPORTS, virtualSol: 40 * LAMPORTS, mcap: 39 * LAMPORTS }),
      // Nothing moved at all.
      observation({ index: 0, slot: 400_000_040, realSol: 10 * LAMPORTS, virtualSol: 40 * LAMPORTS, mcap: 39 * LAMPORTS }),

      // --- account 1: inside the band where the rounding and the inequality
      //     disagree ----------------------------------------------------------
      observation({ index: 1, slot: 400_000_100, realSol: 0, virtualSol: 100 * LAMPORTS, mcap: 100 * LAMPORTS }),
      // +1.015 SOL against 100. β = 1.015·10⁴/100 = 101.5 → 101, which is below
      // the 102 the strip prints; 1.015·99 = 100.485 is above 100, so the buy is
      // above the threshold anyway. The flag comes from the inequality.
      observation({ index: 1, slot: 400_000_110, realSol: 1.015 * LAMPORTS, virtualSol: 101.015 * LAMPORTS, mcap: 102 * LAMPORTS }),

      // --- account 2: exactly on the line ----------------------------------
      observation({ index: 2, slot: 400_000_200, realSol: 0, virtualSol: 99 * LAMPORTS, mcap: 99 * LAMPORTS }),
      // +1 SOL against 99. 1·99 = 99 is equal to y, not above it.
      observation({ index: 2, slot: 400_000_210, realSol: 1 * LAMPORTS, virtualSol: 100 * LAMPORTS, mcap: 100 * LAMPORTS }),
    ];

    await push(page, walk);

    const held = await page.evaluate(rows);
    t.eq("one row per observation", held.length, walk.length);
    const bySlot = (slot) => held.find((row) => row.slot === slot.toLocaleString("en-US"));

    // --- the column ---------------------------------------------------------
    const first = bySlot(400_000_000);
    t.eq("a first observation has no reserve to measure a buy against", first.beta, "—");
    t.eq("and is not flagged", first.sandwich, "false");

    const small = bySlot(400_000_010);
    t.eq("β is the buy over the reserve it arrived at, in basis points", small.beta, "75");
    t.eq("and a buy below the threshold is not flagged", small.sandwich, "false");

    const large = bySlot(400_000_020);
    t.eq("β is floored, not rounded", large.beta, "124");
    t.eq("and a buy above the threshold is flagged", large.sandwich, "true");

    const sell = bySlot(400_000_030);
    t.eq("a sell has no β: the threshold is about the size of a buy", sell.beta, "—");
    t.eq("and is not flagged", sell.sandwich, "false");

    const quiet = bySlot(400_000_040);
    t.eq("an observation that moved nothing has no β either", quiet.beta, "—");
    t.eq("and is not flagged", quiet.sandwich, "false");

    // --- the two numbers and the one that decides ---------------------------
    const band = bySlot(400_000_110);
    t.eq("β can read below the printed threshold", band.beta, "101");
    t.eq(
      "and still be flagged, because the flag is the exact inequality rather than the two rounded numbers",
      band.sandwich,
      "true",
    );

    const online = bySlot(400_000_210);
    t.eq("a buy exactly on the line has its β reported", online.beta, "101");
    t.eq("and is not flagged, because the inequality is strict", online.sandwich, "false");

    // --- the filter ---------------------------------------------------------
    const filtered = await page.evaluate(() => {
      document.querySelector('[data-filter="sandwich"]').click();
      const visible = [...document.querySelectorAll('[data-region="tick-rows"] .row')].filter((r) => !r.hidden);
      return {
        shown: visible.length,
        allFlagged: visible.every((row) => row.children[5].dataset.sandwich === "true"),
        pressed: document.querySelector('[data-filter="sandwich"]').getAttribute("aria-pressed"),
        state: document.querySelector('[data-field="tick-filter-state"]').textContent.trim(),
        count: document.querySelector('[data-field="tick-count"]').textContent.trim(),
      };
    });
    t.eq("the sandwich filter keeps the two buys above the threshold", filtered.shown, 2);
    t.ok("and only those", filtered.allFlagged);
    t.eq("the chip says it is on", filtered.pressed, "true");
    t.eq("the strip counts it", filtered.state, "1 filter active");
    t.eq("and the rows are hidden, not dropped", filtered.count, `2 / ${walk.length}`);

    const cleared = await page.evaluate(() => {
      document.querySelector('[data-filter="sandwich"]').click();
      return [...document.querySelectorAll('[data-region="tick-rows"] .row')].filter((r) => !r.hidden).length;
    });
    t.eq("clearing it brings every row back", cleared, walk.length);

    // --- the detail carries the pair the verdict came from ------------------
    const detail = await page.evaluate((slot) => {
      const row = [...document.querySelectorAll('[data-region="tick-rows"] .row')].find(
        (candidate) => candidate.children[0].textContent.trim() === slot,
      );
      row.dispatchEvent(new MouseEvent("dblclick", { bubbles: true }));
      const pairs = {};
      const fields = document.querySelector('[data-region="tick-detail-fields"]');
      const terms = [...fields.querySelectorAll("dt")];
      const values = [...fields.querySelectorAll("dd")];
      terms.forEach((term, index) => {
        pairs[term.textContent.trim()] = values[index].textContent.trim();
      });
      document.querySelector('[data-action="tick-close"]').click();
      return pairs;
    }, (400_000_020).toLocaleString("en-US"));

    t.eq("the detail names the reserve β was measured against", detail["reserve at buy"], "40.300 SOL");
    t.ok(
      "which is not the one this observation carries",
      detail["virtual sol"] === "40.800 SOL",
      detail["virtual sol"],
    );
    t.eq("the detail states β", detail["beta"], "124 bps");
    t.eq("and the threshold it was compared against", detail["beta threshold"], "102 bps");
    // 0.5 SOL net at a 100 bps fee is 0.5·10⁴/9900 = 0.50505 SOL gross, and the
    // floor on a 40.3 SOL reserve is 100·40.3·10⁴/9900² = 0.41118 SOL. Both are
    // gross, which is the only way the two can be read side by side.
    t.eq("and the buy that produced it, grossed up past the fee", detail["victim buy"], "0.505 SOL gross");
    t.eq("and the floor that buy cleared", detail["sandwich floor"], "0.411 SOL");
    t.eq("and states the verdict as a phrase", detail["sandwich"], "above threshold");

    const belowDetail = await page.evaluate((slot) => {
      const row = [...document.querySelectorAll('[data-region="tick-rows"] .row')].find(
        (candidate) => candidate.children[0].textContent.trim() === slot,
      );
      row.dispatchEvent(new MouseEvent("dblclick", { bubbles: true }));
      const fields = document.querySelector('[data-region="tick-detail-fields"]');
      const terms = [...fields.querySelectorAll("dt")].map((dt) => dt.textContent.trim());
      const values = [...fields.querySelectorAll("dd")].map((dd) => dd.textContent.trim());
      const pairs = Object.fromEntries(terms.map((term, index) => [term, values[index]]));
      document.querySelector('[data-action="tick-close"]').click();
      return pairs;
    }, (400_000_030).toLocaleString("en-US"));

    t.eq("a sell reports no victim buy rather than a negative one", belowDetail["victim buy"], "—");
    t.eq("and says the verdict is below the threshold", belowDetail["sandwich"], "below threshold");
  },
};
