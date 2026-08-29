// The order the stream is drawn in.
//
// Sorting a list that is being appended to is two claims, and they are checked
// separately: that the held rows end up in order, and that a row arriving after
// the sort was chosen lands where it belongs rather than on top. The second is
// the one that makes it a live sort rather than a snapshot of one.
//
// Arrival order is a state of its own here, not "sorted by slot". Slots can
// arrive out of order, and the difference between what the chain says happened
// and what this pipeline saw is one of the things this pane exists to show.

import { LAMPORTS, observation, push } from "../seed.mjs";

/// Every row as drawn, in the order it is drawn.
const drawn = () =>
  [...document.querySelectorAll('[data-region="tick-rows"] .row')].map((row) => ({
    hidden: row.hidden,
    slot: Number(row.children[0].textContent.replace(/,/g, "")),
    delta: row.children[3].textContent.trim(),
    beta: row.children[5].textContent.trim(),
    vol: row.children[6].textContent.trim(),
  }));

/// A signed delta as a number. "—" is not one.
const asNumber = (text) =>
  text === "—" ? null : Number(text.replace("−", "-").replace("+", ""));

export default {
  name: "sorting",
  async run(t, page) {
    // Four rows with a delta and three without. The deltas are worked out from
    // the market caps: +1000, +500 and +300 basis points, and one fall of 1000.
    const walk = [
      observation({ index: 0, slot: 500_000_000, realSol: 10 * LAMPORTS, mcap: 40 * LAMPORTS }),
      observation({ index: 0, slot: 500_000_010, realSol: 11 * LAMPORTS, mcap: 44 * LAMPORTS }),
      observation({ index: 0, slot: 500_000_020, realSol: 11.5 * LAMPORTS, mcap: 46.2 * LAMPORTS }),
      observation({ index: 1, slot: 500_000_030, realSol: 5 * LAMPORTS, mcap: 20 * LAMPORTS }),
      observation({ index: 1, slot: 500_000_040, realSol: 5.2 * LAMPORTS, mcap: 20.6 * LAMPORTS }),
      observation({ index: 2, slot: 500_000_050, realSol: 8 * LAMPORTS, mcap: 30 * LAMPORTS }),
      observation({ index: 2, slot: 500_000_060, realSol: 7.5 * LAMPORTS, mcap: 27 * LAMPORTS }),
    ];
    await push(page, walk);

    // --- arrival order, before anything is sorted --------------------------
    const arrival = await page.evaluate(drawn);
    t.eq("every observation is held", arrival.length, walk.length);
    t.eq("the newest is on top before anything is sorted", arrival[0].slot, 500_000_060);
    t.eq("and the oldest is at the bottom", arrival.at(-1).slot, 500_000_000);
    t.every(
      "no heading claims a sort yet",
      await page.evaluate(() =>
        [...document.querySelectorAll('.col-head [role="columnheader"][aria-sort]')].map((h) =>
          h.getAttribute("aria-sort"),
        ),
      ),
      (value) => value === "none",
      (value) => value,
    );

    // --- descending ---------------------------------------------------------
    const desc = await page.evaluate(() => {
      document.querySelector('.col-head [data-sort="delta"]').click();
      return {
        rows: [...document.querySelectorAll('[data-region="tick-rows"] .row')].map((row) =>
          row.children[3].textContent.trim(),
        ),
        sorted: document
          .querySelector('.col-head [data-sort="delta"]')
          .closest('[role="columnheader"]')
          .getAttribute("aria-sort"),
        active: document.querySelector('.col-head [data-sort="delta"]').dataset.active,
        others: [...document.querySelectorAll('.col-head [role="columnheader"][aria-sort]')].filter(
          (h) => h.getAttribute("aria-sort") !== "none",
        ).length,
      };
    });

    const descValues = desc.rows.map(asNumber);
    t.eq("the column says it is sorted descending", desc.sorted, "descending");
    t.eq("and the heading is marked as the active one", desc.active, "true");
    t.eq("and it is the only column claiming a sort", desc.others, 1);
    t.eq(
      "the deltas come out in order",
      descValues.filter((value) => value !== null).join(","),
      "1000,500,300,-1000",
    );
    t.ok(
      "and every row without a delta is below every row with one",
      descValues.findIndex((value) => value === null) === 4,
      descValues.join(","),
    );

    // --- a row that arrives after the sort was chosen -----------------------
    //
    // 20.6 SOL to 21.424 is exactly 400 basis points, which belongs between the
    // 500 and the 300 rather than on top of the list.
    await push(page, [
      observation({ index: 1, slot: 500_000_070, realSol: 5.6 * LAMPORTS, mcap: 21.424 * LAMPORTS }),
    ]);

    const live = await page.evaluate(drawn);
    t.eq("the arriving row is held", live.length, walk.length + 1);
    t.eq(
      "and lands in its place in the sort rather than on top of it",
      live.map((row) => row.delta).filter((value) => value !== "—").join(","),
      "+1000,+500,+400,+300,−1000",
    );
    t.eq("the newest row is not the first one drawn", live[0].slot, 500_000_010);

    // --- ascending ----------------------------------------------------------
    const asc = await page.evaluate(() => {
      document.querySelector('.col-head [data-sort="delta"]').click();
      return {
        rows: [...document.querySelectorAll('[data-region="tick-rows"] .row')].map((row) =>
          row.children[3].textContent.trim(),
        ),
        sorted: document
          .querySelector('.col-head [data-sort="delta"]')
          .closest('[role="columnheader"]')
          .getAttribute("aria-sort"),
      };
    });
    const ascValues = asc.rows.map(asNumber);
    t.eq("clicking again reverses it", asc.sorted, "ascending");
    t.eq(
      "and the deltas come out the other way",
      ascValues.filter((value) => value !== null).join(","),
      "-1000,300,400,500,1000",
    );
    t.ok(
      "with the rows that have no delta still last",
      ascValues.findIndex((value) => value === null) === 5,
      ascValues.join(","),
    );

    // --- back to arrival ----------------------------------------------------
    const back = await page.evaluate(() => {
      document.querySelector('.col-head [data-sort="delta"]').click();
      return {
        slots: [...document.querySelectorAll('[data-region="tick-rows"] .row')].map((row) =>
          Number(row.children[0].textContent.replace(/,/g, "")),
        ),
        sorted: document
          .querySelector('.col-head [data-sort="delta"]')
          .closest('[role="columnheader"]')
          .getAttribute("aria-sort"),
        active: document.querySelector('.col-head [data-sort="delta"]').dataset.active,
      };
    });
    t.eq("a third click gives arrival order back", back.sorted, "none");
    t.eq("and the heading stops claiming to be the sorted one", back.active, "false");
    t.eq("the newest observation is on top again", back.slots[0], 500_000_070);
    t.eq("and the oldest is at the bottom again", back.slots.at(-1), 500_000_000);

    // --- the other columns --------------------------------------------------
    const byBeta = await page.evaluate(() => {
      document.querySelector('.col-head [data-sort="beta"]').click();
      return [...document.querySelectorAll('[data-region="tick-rows"] .row')].map((row) =>
        row.children[5].textContent.trim(),
      );
    });
    const betas = byBeta.map((text) => (text === "—" ? null : Number(text)));
    t.every(
      "β sorts descending",
      betas.filter((value) => value !== null).map((value, index, list) => ({ value, prev: list[index - 1] })),
      (pair) => pair.prev === undefined || pair.prev >= pair.value,
      (pair) => `${pair.prev} then ${pair.value}`,
    );
    t.ok(
      "and the rows with no buy to measure are last",
      betas.filter((value) => value !== null).length > 0 &&
        betas.slice(betas.filter((value) => value !== null).length).every((value) => value === null),
      betas.join(","),
    );

    const bySlot = await page.evaluate(() => {
      document.querySelector('.col-head [data-sort="slot"]').click();
      return [...document.querySelectorAll('[data-region="tick-rows"] .row')].map((row) =>
        Number(row.children[0].textContent.replace(/,/g, "")),
      );
    });
    t.every(
      "the slot column sorts too",
      bySlot.map((slot, index, list) => ({ slot, prev: list[index - 1] })),
      (pair) => pair.prev === undefined || pair.prev >= pair.slot,
      (pair) => `${pair.prev} then ${pair.slot}`,
    );

    // --- the cursor walks the list as drawn ---------------------------------
    const keys = await page.evaluate(() => {
      document.querySelector('.col-head [data-sort="delta"]').click(); // descending
      const list = [...document.querySelectorAll('[data-region="tick-rows"] .row')];
      list[0].click();
      const press = (key) =>
        document.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true }));
      press("j");
      press("j");
      const after = list.indexOf(document.activeElement);
      const selected = list.findIndex((row) => row.getAttribute("aria-selected") === "true");
      return { after, selected, deltas: list.map((row) => row.children[3].textContent.trim()) };
    });
    t.eq("j walks down the sorted list, not the arrival one", keys.after, 2);
    t.eq("and the cursor is on the row it landed on", keys.selected, 2);
    t.eq("which is the third row of the sort", keys.deltas[2], "+400");

    // --- a filter and a sort at once ----------------------------------------
    const both = await page.evaluate(() => {
      const input = document.querySelector('[data-filter="delta"]');
      input.value = "400";
      input.dispatchEvent(new Event("input", { bubbles: true }));
      const visible = [...document.querySelectorAll('[data-region="tick-rows"] .row')].filter(
        (row) => !row.hidden,
      );
      return {
        deltas: visible.map((row) => row.children[3].textContent.trim()),
        count: document.querySelector('[data-field="tick-count"]').textContent.trim(),
      };
    });
    t.eq(
      "a filter narrows the sorted list without reordering it",
      both.deltas.join(","),
      "+1000,+500,+400,−1000",
    );
    t.eq("and the count still reports what is held", both.count, "4 / 8");

    // --- the cursor is a row, not a position --------------------------------
    //
    // Re-sorting moves every row at once. The one the operator was on has to
    // come with them: a cursor that stays at "the third row" after the third
    // row became a different observation is a cursor pointing at something
    // nobody chose.
    const kept = await page.evaluate(() => {
      const input = document.querySelector('[data-filter="delta"]');
      input.value = "";
      input.dispatchEvent(new Event("input", { bubbles: true }));

      const rows = [...document.querySelectorAll('[data-region="tick-rows"] .row')];
      const cursor = rows[1];
      cursor.click();
      const wasAt = 1;
      document.querySelector('.col-head [data-sort="slot"]').click();
      const after = [...document.querySelectorAll('[data-region="tick-rows"] .row')];
      return {
        selected: cursor.getAttribute("aria-selected"),
        focused: document.activeElement === cursor,
        selectedCount: after.filter((row) => row.getAttribute("aria-selected") === "true").length,
        movedTo: after.indexOf(cursor),
        wasAt,
        active: document.activeElement?.className ?? "none",
      };
    });
    t.eq("the cursor stays on its own row across a re-sort", kept.selected, "true");
    t.eq("and only on that one", kept.selectedCount, 1);
    t.ok("and the row keeps the focus", kept.focused, `focus is on ${kept.active}`);

    // --- the ring buffer, while sorted --------------------------------------
    //
    // The buffer drops the oldest *arrival*, which under a sort is not the last
    // row drawn. Getting that wrong leaves a row on screen that nothing holds
    // any more, or takes one off that everything does.
    const bounded = await page.evaluate((from) => {
      for (let i = 0; i < 520; i += 1) {
        window.__STS_TEST__.pushCandidate({
          slot: from + i,
          account: "RingBufferSortAccount11111111111111111111111",
          marketCapLamports: 40_000_000_000 + (i % 37) * 1_000_000_000,
          poolLamports: 10_000_000_000 + (i % 23) * 1_000_000_000,
          virtualSolReserves: 40_000_000_000 + (i % 23) * 1_000_000_000,
          curveProgressBps: 1_176,
        });
      }
      const rows = [...document.querySelectorAll('[data-region="tick-rows"] .row')];
      return {
        drawn: rows.length,
        count: document.querySelector('[data-field="tick-count"]').textContent.trim(),
        slots: rows.map((row) => Number(row.children[0].textContent.replace(/,/g, ""))),
      };
    }, 600_000_000);

    t.eq("the stream is still bounded at 500 rows", bounded.drawn, 500);
    t.eq("and nothing is drawn that is not held", bounded.count, "500 / 500");
    t.every(
      "and what is left is still in order",
      bounded.slots.map((slot, index, list) => ({ slot, prev: list[index - 1] })),
      (pair) => pair.prev === undefined || pair.prev >= pair.slot,
      (pair) => `${pair.prev} then ${pair.slot}`,
    );
  },
};
