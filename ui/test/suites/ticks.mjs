// The tick stream inspector: the two derived columns, the three filters, and
// the promise that a filter narrows the view rather than dropping data.

import { LAMPORTS, observation, push, account } from "../seed.mjs";

const rows = () =>
  [...document.querySelectorAll('[data-region="tick-rows"] .row')].map((row) => ({
    hidden: row.hidden,
    slot: row.children[0].textContent.trim(),
    curve: row.children[1].textContent.trim(),
    mcap: row.children[2].textContent.trim(),
    delta: row.children[3].textContent.trim(),
    flow: row.children[4].textContent.trim(),
    beta: row.children[5].textContent.trim(),
    sandwich: row.children[5].dataset.sandwich,
    volume: row.children[6].textContent.trim(),
    anomaly: row.children[6].dataset.anomaly,
  }));

export default {
  name: "ticks",
  async run(t, page) {
    // Account 0 moves a steady 1 SOL per observation and then jumps 8; account
    // 1 never moves at all and then moves once. Between them they exercise
    // every branch the volume column has.
    const walk = [];
    let real = 10 * LAMPORTS;
    let mcap = 40 * LAMPORTS;
    walk.push(observation({ index: 0, slot: 312_905_100, realSol: real, mcap }));
    // The second observation is the one every delta assertion below is about.
    real = 12 * LAMPORTS;
    mcap = 43 * LAMPORTS;
    walk.push(observation({ index: 0, slot: 312_905_110, realSol: real, mcap }));
    for (const step of [1, 1, 1]) {
      real += step * LAMPORTS;
      mcap += LAMPORTS;
      walk.push(observation({ index: 0, slot: 312_905_110 + walk.length * 10, realSol: real, mcap }));
    }
    real += 8 * LAMPORTS;
    mcap += 8 * LAMPORTS;
    walk.push(observation({ index: 0, slot: 312_905_200, realSol: real, mcap }));

    for (let i = 0; i < 4; i += 1) {
      walk.push(
        observation({ index: 1, slot: 312_905_300 + i, realSol: 5 * LAMPORTS, mcap: 35 * LAMPORTS }),
      );
    }
    walk.push(observation({ index: 1, slot: 312_905_400, realSol: 9 * LAMPORTS, mcap: 39 * LAMPORTS }));

    await push(page, walk);

    const held = await page.evaluate(rows);
    t.eq("one row per observation", held.length, walk.length);

    // Rows are newest first, so the walk reads backwards.
    const bySlot = (slot) => held.find((row) => row.slot === slot.toLocaleString("en-US"));

    // --- the two derived columns -------------------------------------------
    const first = bySlot(312_905_100);
    t.eq("the first observation of an account has no price delta", first.delta, "—");
    t.eq("and no flow", first.flow, "—");
    t.eq("and claims no volume multiple", first.volume, "—");
    t.eq("and is not flagged", first.anomaly, "false");

    const second = bySlot(312_905_110);
    // 40 SOL → 43 SOL is 750 basis points, exactly.
    t.eq("the price delta is basis points against the previous observation", second.delta, "+750");
    t.eq("the flow is the change in real SOL, signed", second.flow, "+2.000");
    t.eq("one earlier observation is not enough for a multiple", second.volume, "—");
    t.eq("so nothing is flagged on it", second.anomaly, "false");

    const fifth = bySlot(312_905_150);
    t.eq(
      "a multiple appears once there are three earlier moves",
      fifth.volume,
      "1.0x",
    );
    t.eq("and a move at the median is not an anomaly", fifth.anomaly, "false");

    const jump = bySlot(312_905_200);
    t.eq("an eightfold move is reported as one", jump.volume, "8.0x");
    t.eq("and is flagged", jump.anomaly, "true");

    const firstMove = bySlot(312_905_400);
    t.eq(
      "an account whose every earlier observation moved nothing has no ratio to report",
      firstMove.volume,
      "new",
    );
    t.eq("but its first movement is still an anomaly", firstMove.anomaly, "true");

    const nonMover = bySlot(312_905_302);
    t.eq("an account that has never moved reports a zero flow, not an em dash", nonMover.flow, "0.000");
    t.eq("and is not flagged for it", nonMover.anomaly, "false");

    // --- the filters --------------------------------------------------------
    const count = () => document.querySelector('[data-field="tick-count"]').textContent.trim();
    const state = () => document.querySelector('[data-field="tick-filter-state"]').textContent.trim();
    const shown = () =>
      [...document.querySelectorAll('[data-region="tick-rows"] .row')].filter((row) => !row.hidden).length;

    const unfiltered = await page.evaluate(
      (fns) => {
        const [countFn, stateFn, shownFn] = fns.map((source) => new Function(`return (${source})()`));
        return { count: countFn(), state: stateFn(), shown: shownFn() };
      },
      [count.toString(), state.toString(), shown.toString()],
    );
    t.eq("the count is shown of held", unfiltered.count, `${walk.length} / ${walk.length}`);
    t.eq("and says there is no filter", unfiltered.state, "no filter");

    const bySlotFilter = await page.evaluate((slot) => {
      const input = document.querySelector('[data-filter="slot"]');
      input.value = String(slot);
      input.dispatchEvent(new Event("input", { bubbles: true }));
      return {
        count: document.querySelector('[data-field="tick-count"]').textContent.trim(),
        state: document.querySelector('[data-field="tick-filter-state"]').textContent.trim(),
        held: document.querySelectorAll('[data-region="tick-rows"] .row').length,
        shown: [...document.querySelectorAll('[data-region="tick-rows"] .row')].filter((r) => !r.hidden).length,
        minShownSlot: Math.min(
          ...[...document.querySelectorAll('[data-region="tick-rows"] .row')]
            .filter((r) => !r.hidden)
            .map((r) => Number(r.children[0].textContent.replace(/,/g, ""))),
        ),
      };
    }, 312_905_300);

    t.eq("a slot filter hides everything before it", bySlotFilter.shown, 5);
    t.ok(
      "and nothing it kept is below the slot",
      bySlotFilter.minShownSlot >= 312_905_300,
      String(bySlotFilter.minShownSlot),
    );
    t.eq("the rows are still held, not deleted", bySlotFilter.held, walk.length);
    t.eq("the count still says shown of held", bySlotFilter.count, `5 / ${walk.length}`);
    t.eq("and the strip says a filter is on", bySlotFilter.state, "1 filter active");

    const cleared = await page.evaluate(() => {
      const input = document.querySelector('[data-filter="slot"]');
      input.value = "";
      input.dispatchEvent(new Event("input", { bubbles: true }));
      return {
        shown: [...document.querySelectorAll('[data-region="tick-rows"] .row')].filter((r) => !r.hidden).length,
        state: document.querySelector('[data-field="tick-filter-state"]').textContent.trim(),
      };
    });
    t.eq("clearing the filter brings every row back", cleared.shown, walk.length);
    t.eq("and the strip says so", cleared.state, "no filter");

    const byDelta = await page.evaluate(() => {
      const input = document.querySelector('[data-filter="delta"]');
      input.value = "700";
      input.dispatchEvent(new Event("input", { bubbles: true }));
      const visible = [...document.querySelectorAll('[data-region="tick-rows"] .row')].filter((r) => !r.hidden);
      return {
        shown: visible.length,
        deltas: visible.map((row) => row.children[3].textContent.trim()),
      };
    });
    t.ok("a delta filter keeps only what clears it", byDelta.shown > 0, `${byDelta.shown} rows`);
    t.every(
      "and every row it kept is at or above the threshold",
      byDelta.deltas,
      (delta) => Math.abs(Number(delta.replace("−", "-").replace("+", ""))) >= 700,
      (delta) => delta,
    );
    t.every(
      "and no row without a delta survives a delta filter",
      byDelta.deltas,
      (delta) => delta !== "—",
      (delta) => delta,
    );

    // The volume filter asks for a multiple, which is a different question from
    // the one the anomaly chip asks. An account whose every earlier observation
    // moved nothing is flagged and has no multiple, so it answers the chip and
    // not this.
    const byVolume = await page.evaluate(() => {
      const delta = document.querySelector('[data-filter="delta"]');
      delta.value = "";
      delta.dispatchEvent(new Event("input", { bubbles: true }));

      const input = document.querySelector('[data-filter="vol"]');
      input.value = "4";
      input.dispatchEvent(new Event("input", { bubbles: true }));
      const visible = [...document.querySelectorAll('[data-region="tick-rows"] .row')].filter((r) => !r.hidden);
      return {
        shown: visible.length,
        multiples: visible.map((row) => row.children[6].textContent.trim()),
        state: document.querySelector('[data-field="tick-filter-state"]').textContent.trim(),
      };
    });
    t.eq("a volume filter keeps only what clears the multiple", byVolume.shown, 1);
    t.eq("and it is the row that claims one", byVolume.multiples.join(","), "8.0x");
    t.eq("and the strip counts it", byVolume.state, "1 filter active");

    const fractional = await page.evaluate(() => {
      const input = document.querySelector('[data-filter="vol"]');
      input.value = "1.5";
      input.dispatchEvent(new Event("input", { bubbles: true }));
      const shown = [...document.querySelectorAll('[data-region="tick-rows"] .row')].filter((r) => !r.hidden);
      const invalid = input.getAttribute("aria-invalid");
      input.value = "";
      input.dispatchEvent(new Event("input", { bubbles: true }));
      return { invalid, shown: shown.length, restored: [...document.querySelectorAll('[data-region="tick-rows"] .row')].filter((r) => !r.hidden).length };
    });
    t.eq(
      "a multiple may have a decimal in it, unlike a slot or a count of basis points",
      fractional.invalid,
      "false",
    );
    t.eq("and a threshold under the 8x row still keeps only it", fractional.shown, 1);
    t.eq("clearing it brings every row back", fractional.restored, walk.length);

    const anomalies = await page.evaluate(() => {
      document.querySelector('[data-filter="delta"]').value = "";
      document.querySelector('[data-filter="delta"]').dispatchEvent(new Event("input", { bubbles: true }));
      document.querySelector('[data-filter="anomaly"]').click();
      const visible = [...document.querySelectorAll('[data-region="tick-rows"] .row')].filter((r) => !r.hidden);
      return {
        shown: visible.length,
        allFlagged: visible.every((row) => row.children[6].dataset.anomaly === "true"),
        pressed: document.querySelector('[data-filter="anomaly"]').getAttribute("aria-pressed"),
        state: document.querySelector('[data-field="tick-filter-state"]').textContent.trim(),
      };
    });
    t.eq("the anomaly filter keeps the two flagged ticks", anomalies.shown, 2);
    t.ok("and only flagged ticks", anomalies.allFlagged);
    t.eq("and the chip says it is on", anomalies.pressed, "true");
    t.eq("and the strip counts it", anomalies.state, "1 filter active");

    // --- two filters at once ------------------------------------------------
    const both = await page.evaluate(() => {
      const input = document.querySelector('[data-filter="slot"]');
      input.value = "312905350";
      input.dispatchEvent(new Event("input", { bubbles: true }));
      return {
        shown: [...document.querySelectorAll('[data-region="tick-rows"] .row')].filter((r) => !r.hidden).length,
        state: document.querySelector('[data-field="tick-filter-state"]').textContent.trim(),
        filteredEmptyHidden: document.querySelector('[data-region="tick-filtered"]').hidden,
        emptyHidden: document.querySelector('[data-region="tick-empty"]').hidden,
      };
    });
    t.eq("two filters compose", both.shown, 1);
    t.eq("and are counted", both.state, "2 filters active");
    t.ok("the no-ticks empty state stays down while ticks are held", both.emptyHidden);

    const noneLeft = await page.evaluate(() => {
      const input = document.querySelector('[data-filter="delta"]');
      input.value = "99999";
      input.dispatchEvent(new Event("input", { bubbles: true }));
      return {
        shown: [...document.querySelectorAll('[data-region="tick-rows"] .row')].filter((r) => !r.hidden).length,
        filteredEmptyHidden: document.querySelector('[data-region="tick-filtered"]').hidden,
        emptyHidden: document.querySelector('[data-region="tick-empty"]').hidden,
        count: document.querySelector('[data-field="tick-count"]').textContent.trim(),
      };
    });
    t.eq("a filter that matches nothing shows nothing", noneLeft.shown, 0);
    t.eq(
      "and says the filter is why, not that the feed is quiet",
      noneLeft.filteredEmptyHidden,
      false,
    );
    t.ok("the quiet-feed empty state stays down", noneLeft.emptyHidden);
    t.eq("and the count still reports what is held", noneLeft.count, `0 / ${walk.length}`);

    // --- the subject filter --------------------------------------------------
    const subject = await page.evaluate((wanted) => {
      for (const name of ["slot", "delta"]) {
        const input = document.querySelector(`[data-filter="${name}"]`);
        input.value = "";
        input.dispatchEvent(new Event("input", { bubbles: true }));
      }
      document.querySelector('[data-filter="anomaly"]').click();
      [...document.querySelectorAll('[data-region="radar-rows"] .row')]
        .find((row) => row.dataset.account === wanted)
        ?.click();
      document.querySelector('[data-filter="subject"]').click();
      const visible = [...document.querySelectorAll('[data-region="tick-rows"] .row')].filter((r) => !r.hidden);
      return {
        shown: visible.length,
        allSubject: visible.every((row) => row.dataset.account === wanted),
      };
    }, account(1));
    t.eq("the subject filter keeps only the selected account", subject.shown, 5);
    t.ok("and nothing else", subject.allSubject);

    // --- keyboard -------------------------------------------------------------
    const keys = await page.evaluate(() => {
      document.querySelector('[data-filter="subject"]').click();
      const list = [...document.querySelectorAll('[data-region="tick-rows"] .row')];
      list[0].click();
      const press = (key) =>
        document.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true }));
      press("j");
      press("j");
      press("j");
      const afterDown = list.indexOf(document.activeElement);
      press("k");
      const afterUp = list.indexOf(document.activeElement);
      press("Enter");
      const open = document.querySelector('[data-region="tick-modal"]').dataset.open;
      press("Escape");
      const closed = document.querySelector('[data-region="tick-modal"]').dataset.open;
      return { afterDown, afterUp, open, closed };
    });
    t.eq("j walks down the list", keys.afterDown, 3);
    t.eq("k walks back up it", keys.afterUp, 2);
    t.eq("Enter opens the detail for the row under the cursor", keys.open, "true");
    t.eq("Escape closes it", keys.closed, "false");

    // j and k are navigation everywhere except inside a text field, where they
    // are the letters j and k.
    const typing = await page.evaluate(() => {
      const list = [...document.querySelectorAll('[data-region="tick-rows"] .row')];
      const before = list.indexOf(document.activeElement);
      const input = document.querySelector('[data-filter="slot"]');
      input.focus();
      input.dispatchEvent(new KeyboardEvent("keydown", { key: "j", bubbles: true }));
      return { before, after: list.indexOf(document.activeElement), focused: document.activeElement === input };
    });
    t.ok("j in a filter field does not move the cursor", typing.focused && typing.after === -1);

    // --- the detail carries the pair the derived number came from ------------
    const detail = await page.evaluate(() => {
      const row = [...document.querySelectorAll('[data-region="tick-rows"] .row')].find(
        (candidate) => candidate.children[6].dataset.anomaly === "true",
      );
      row.dispatchEvent(new MouseEvent("dblclick", { bubbles: true }));
      const pairs = {};
      const fields = document.querySelector('[data-region="tick-detail-fields"]');
      const terms = [...fields.querySelectorAll("dt")];
      const values = [...fields.querySelectorAll("dd")];
      terms.forEach((term, index) => {
        pairs[term.textContent.trim()] = values[index].textContent.trim();
      });
      const raw = document.querySelector('[data-field="tick-detail-raw"]').textContent;
      document.querySelector('[data-action="tick-close"]').click();
      return { pairs, raw: JSON.parse(raw) };
    });
    t.ok("the detail shows the flow", "flow" in detail.pairs, JSON.stringify(Object.keys(detail.pairs)));
    t.ok("and the median it was compared against", "median move" in detail.pairs);
    t.ok("and the multiple those two produce", "volume multiple" in detail.pairs);
    t.eq("and states the flag as a word", detail.pairs.anomaly, "flagged");
    t.ok("and carries the raw frame", typeof detail.raw.account === "string" && detail.raw.slot > 0);

    // --- the ring buffer -------------------------------------------------------
    const bounded = await page.evaluate((from) => {
      for (let i = 0; i < 520; i += 1) {
        window.__STS_TEST__.pushCandidate({
          slot: from + i,
          account: "RingBufferCurveAccount1111111111111111111111",
          marketCapLamports: 40_000_000_000 + i * 1_000_000,
          poolLamports: 10_000_000_000 + i * 1_000_000,
          virtualSolReserves: 40_000_000_000 + i * 1_000_000,
          curveProgressBps: 1_176,
        });
      }
      return {
        held: document.querySelectorAll('[data-region="tick-rows"] .row').length,
        count: document.querySelector('[data-field="tick-count"]').textContent.trim(),
      };
    }, 312_910_000);
    t.eq("the stream is bounded at 500 rows", bounded.held, 500);
    t.eq("and the count says so", bounded.count, "500 / 500");
  },
};
