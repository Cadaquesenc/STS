// The 0x100 sub-slot telemetry view.
//
// The one surface in this window that shows a shape rather than a number, and
// the assertions are mostly about the shape being a fixed one:
//
//   * The grid is **0x10 by 0x10, always**. Sixteen rows of sixteen from the
//     first repaint, whether four samples have arrived or four thousand. A
//     grid that grew as samples landed would change size under the eye of
//     whoever was reading it, which is the rule the rest of this window is
//     built on and the reason this is a fixed window rather than a list.
//   * The newest sample is always at **0xff**. A ring drawn from its own cursor
//     would put the seam in a different column every time something arrived,
//     and the address down the gutter would stop meaning anything.
//   * The jitter is **measured against the feed's own cadence**, not against an
//     assumed four hundred milliseconds. `metrics.rs` gives the reason: the
//     chain's cadence moves, and a wobble measured against a constant is mostly
//     a measurement of the constant.
//   * The drift is **read off the snapshot**, not derived from the jitter. They
//     are two different questions — how steady is the feed, and how much of
//     what is on screen has the cluster not agreed on yet — and a view that
//     computed one from the other would be answering neither.

import { settleFeeds } from "../seed.mjs";

/// Everything the view is showing.
function readView() {
  const grid = document.querySelector('[data-region="geyser-grid"]');
  const cells = [...grid.querySelectorAll(".cell")];
  const pairs = (region) => {
    const dl = document.querySelector(`[data-region="${region}"]`);
    const out = {};
    const kids = [...dl.children];
    for (let index = 0; index + 1 < kids.length; index += 2) {
      out[kids[index].textContent.trim()] = kids[index + 1].textContent.trim();
    }
    return out;
  };
  return {
    open: document.querySelector('[data-region="geyser-modal"]').dataset.open,
    hidden: document.querySelector('[data-region="geyser-modal"]').hidden,
    addresses: [...grid.querySelectorAll(".addr")].map((el) => el.textContent.trim()),
    cellCount: cells.length,
    bands: cells.map((cell) => cell.dataset.band),
    glyphs: cells.map((cell) => cell.textContent),
    head: cells.findIndex((cell) => cell.dataset.head === "true"),
    window: document.querySelector('[data-field="geyser-window"]').textContent.trim(),
    scale: document.querySelector('[data-field="geyser-grid-scale"]').textContent.trim(),
    summary: document.querySelector('[data-field="geyser-summary-line"]').textContent.trim(),
    alt: document.querySelector('[data-field="geyser-grid-alt"]').textContent.trim(),
    legend: [...document.querySelectorAll('[data-region="geyser-legend"] .swatch')].map((s) => ({
      band: s.querySelector(".glyph").dataset.band,
      glyph: s.querySelector(".glyph").textContent,
      text: s.textContent.trim(),
    })),
    stats: pairs("geyser-stats"),
    ring: pairs("geyser-ring"),
    cell: document.querySelector('[data-field="geyser-summary"]').textContent.trim(),
    cellState: document.querySelector('[data-field="geyser-state"]').textContent.trim(),
    dot: document.querySelector('[data-action="geyser"] .dot').className,
    cellTitle: document.querySelector('[data-field="geyser-summary"]').title,
    internal: window.__STS_UI__.geyser,
  };
}

/// Puts `readView` into the page so the evaluates below can call it.
///
/// Re-installed after every navigation, because a reload takes the window's
/// whole global with it.
async function install(page) {
  await page.evaluate((source) => {
    window.readViewInPage = new Function(`return (${source})`)();
  }, readView.toString());
}

export default {
  name: "geyser",
  async run(t, page) {
    await install(page);
    await settleFeeds(page);

    // --- before anything has arrived ----------------------------------------
    //
    // The counters are there from the first poll — they are counters, and the
    // engine has been running — but nothing has been *released* yet, so there
    // is no cadence to have wobbled.
    const quiet = await page.evaluate(() => {
      document.querySelector('[data-action="geyser"]').click();
      return readViewInPage();
    });

    t.eq("the cell opens the view", quiet.open, "true");
    t.eq("and the dialog is no longer hidden", quiet.hidden, false);
    t.eq("the grid is sixteen addressed rows", quiet.addresses.length, 16);
    t.eq(
      "addressed in hex, sixteen apart",
      quiet.addresses.join(","),
      "0x00,0x10,0x20,0x30,0x40,0x50,0x60,0x70,0x80,0x90,0xa0,0xb0,0xc0,0xd0,0xe0,0xf0",
    );
    t.eq("of sixteen cells each", quiet.cellCount, 256);
    t.eq(
      "and with nothing received every one of them is empty",
      quiet.bands.filter((band) => band === "empty").length,
      256,
    );
    t.eq("the window says how full it is", quiet.window, "0x000 / 0x100");
    t.eq("there is no head marker on an empty ring", quiet.head, -1);
    t.ok(
      "and it says no samples have arrived",
      quiet.summary.includes("No sub-slot samples yet"),
      quiet.summary,
    );
    t.eq(
      "the screen-reader description says the same",
      quiet.alt,
      "No sub-slot samples have been received.",
    );

    // The drift half is answerable from the snapshot alone, and it is answered.
    t.eq("the chain head is the snapshot's", quiet.stats["chain head"], "312,905,150");
    t.eq("and the confirmed head", quiet.stats.confirmed, "312,905,118");
    t.eq("and the finalized one", quiet.stats.finalized, "312,905,087");
    t.eq(
      "the drift is the gap between the head and what is confirmed",
      quiet.stats["slot drift"],
      "32 slots",
    );
    t.eq(
      "and the finality lag is the gap below that",
      quiet.stats["finality lag"],
      "31 slots",
    );
    t.eq("reorgs are the ledger's own count", quiet.stats.reorgs, "2");

    // The ordering ring, straight off `RingMetrics`.
    t.eq("the ring reports what it is holding", quiet.ring.buffered, "61");
    t.eq("what it has released", quiet.ring.released, "902,118");
    t.eq("what arrived too late to order", quiet.ring.late, "18");
    t.eq("what it shed under backpressure", quiet.ring.shed, "4");
    t.eq("what it let go early to keep flowing", quiet.ring["forced releases"], "91");
    t.eq("what a re-org took back", quiet.ring["rolled back"], "26");
    t.eq("and what it could not take back", quiet.ring.unrecoverable, "1");
    t.eq("and how much of the feed was out of order", quiet.ring["out of order"], "3,140");

    // The legend is the key to the glyphs, drawn from the same thresholds the
    // cells are banded by.
    t.eq("the legend names four bands", quiet.legend.length, 4);
    t.eq(
      "in order of how much wobble they mean",
      quiet.legend.map((entry) => entry.band).join(","),
      "steady,loose,wide,broken",
    );
    t.eq(
      "each with the glyph its cells draw",
      quiet.legend.map((entry) => entry.glyph).join(""),
      ".:+#",
    );
    t.ok(
      "and the threshold it stands for",
      quiet.legend.every((entry) => /ms/.test(entry.text)),
      quiet.legend.map((entry) => entry.text).join(" | "),
    );

    // --- a steady feed -------------------------------------------------------
    //
    // Three hundred arrivals at a fixed cadence. More than the window holds, so
    // this is also the assertion that the ring wraps rather than growing.
    await page.evaluate(() => {
      window.__STS_TEST__.pushGeyserRun(300, { stepUs: 40_000, wobbleUs: 0 });
    });
    await settleFeeds(page);

    const steady = await page.evaluate(() => readViewInPage());

    t.eq("the grid is still 0x100 cells", steady.cellCount, 256);
    t.eq("and still sixteen addresses", steady.addresses.length, 16);
    t.eq("the window fills to 0x100 and stops", steady.window, "0x100 / 0x100");
    t.eq("which is what the ring holds", steady.internal.samples, 256);
    t.eq(
      "a cadence that never varies is steady in every cell",
      steady.bands.filter((band) => band === "steady").length,
      256,
    );
    t.eq(
      "drawn with the quietest glyph there is",
      steady.glyphs.filter((glyph) => glyph === ".").length,
      256,
    );
    t.eq("the newest sample is at 0xff", steady.head, 255);
    t.eq("the median wobble is nothing", steady.stats["jitter p50"], "0.00ms");
    t.eq("and so is the worst of it", steady.stats["jitter max"], "0.00ms");

    // The status-bar cell carries the same two numbers.
    t.eq(
      "the status cell shows the drift as its number",
      steady.cell,
      "32sl",
      "one number and a dot, the same shape every endpoint cell beside it has",
    );
    t.eq("and reads as live", steady.dot, "dot is-live");
    t.ok(
      "and says what it is for a reader",
      steady.cellState.includes("subslot jitter steady"),
      steady.cellState,
    );
    t.ok(
      "and the jitter it dropped is still one hover away",
      steady.cellTitle.includes("p50 0.00ms"),
      steady.cellTitle,
    );
    t.ok(
      "at every quantile, unrounded",
      steady.cellTitle.includes("p95") && steady.cellTitle.includes("worst"),
      steady.cellTitle,
    );

    // Three hundred arrivals, one repaint, one revision. The ticker coalesces
    // — the alternative is three hundred repaints of a grid nobody read in
    // between, and a revision counter that counts frames rather than changes.
    const coalesced = await page.evaluate(() => window.__STS_UI__.feeds.geyser);
    t.ok(
      "three hundred arrivals cost a handful of revisions, not three hundred",
      coalesced.revision <= 4,
      `${coalesced.revision} revisions for 300 samples`,
    );
    t.eq("and nothing is left waiting", coalesced.pending, 0);

    // --- a feed that wobbles -------------------------------------------------
    //
    // The gap alternates between 40ms and 65ms, so every jitter after the first
    // two is exactly 25ms — which is the number the stats block has to print.
    // Stated rather than computed: an expected value produced by the same
    // arithmetic as the actual one is not an assertion.
    await page.goto(page.origin);
    await install(page);
    await settleFeeds(page);
    await page.evaluate(() => {
      window.__STS_TEST__.pushGeyserRun(64, { stepUs: 40_000, wobbleUs: 25_000 });
      document.querySelector('[data-action="geyser"]').click();
    });
    await settleFeeds(page);

    const wobbly = await page.evaluate(() => readViewInPage());

    t.eq("a 25ms wobble reads as 25ms", wobbly.stats["jitter p50"], "25.00ms");
    t.eq("at the top of the window too", wobbly.stats["jitter max"], "25.00ms");
    t.eq(
      "and at the bottom, because the wobble is the same every time",
      wobbly.stats["jitter min"],
      "25.00ms",
    );
    t.eq(
      "25ms is the wide band",
      wobbly.bands.filter((band) => band === "wide").length,
      62,
      "sixty-four arrivals give sixty-two jitters: the first has nothing before it, the second no gap before that",
    );
    t.eq(
      "drawn with the wide band's glyph",
      wobbly.glyphs.filter((glyph) => glyph === "+").length,
      62,
    );
    t.eq(
      "the two that cannot be measured are drawn as unmeasured",
      wobbly.bands.filter((band) => band === "empty").length,
      256 - 62,
    );
    t.eq("the window is 0x040 of 0x100 full", wobbly.window, "0x040 / 0x100");
    t.eq("and the newest is still at 0xff", wobbly.head, 255);
    t.eq(
      "the status cell keeps showing the drift",
      wobbly.cell,
      "32sl",
      "the wobble is the dot's job; the digits are the drift's",
    );
    t.eq(
      "at full precision inside the view",
      wobbly.stats["jitter p50"],
      "25.00ms",
      "the cell is bounded so it cannot move its neighbours; the view has a column to itself",
    );
    t.eq("and the dot warns rather than reading live", wobbly.dot, "dot is-warn");
    t.ok(
      "the summary says how many arrivals it is over",
      wobbly.summary.includes("62 arrivals in the window"),
      wobbly.summary,
    );
    t.ok(
      "and the screen-reader description counts the bands",
      wobbly.alt.includes("62 wide"),
      wobbly.alt,
    );

    // --- closing -------------------------------------------------------------
    const closed = await page.evaluate(() => {
      document.querySelector('[data-action="geyser-close"]').click();
      return {
        open: document.querySelector('[data-region="geyser-modal"]').dataset.open,
        hidden: document.querySelector('[data-region="geyser-modal"]').hidden,
      };
    });
    t.eq("the close button closes it", closed.open, "false");
    t.eq("and hides it", closed.hidden, true);

    // `g` opens it, and Escape closes it, which is the pair every other dialog
    // in this window keeps.
    const byKey = await page.evaluate(() => {
      document.body.dispatchEvent(
        new KeyboardEvent("keydown", { key: "g", bubbles: true }),
      );
      const opened = document.querySelector('[data-region="geyser-modal"]').dataset.open;
      document.body.dispatchEvent(
        new KeyboardEvent("keydown", { key: "Escape", bubbles: true }),
      );
      return {
        opened,
        closed: document.querySelector('[data-region="geyser-modal"]').dataset.open,
      };
    });
    t.eq("g opens the view", byKey.opened, "true");
    t.eq("and Escape closes it", byKey.closed, "false");

    // --- a build with the pipeline and no feed -------------------------------
    //
    // The shipped state until something dials a Geyser endpoint: `geyser.rs` is
    // compiled in and nothing is behind it.
    await page.goto(`${page.origin}?geyser=0`);
    await install(page);
    await settleFeeds(page);

    const absent = await page.evaluate(() => {
      document.querySelector('[data-action="geyser"]').click();
      return readViewInPage();
    });

    t.eq("with no feed the status cell is an em dash", absent.cell, "—");
    t.ok(
      "and says which kind of nothing it is",
      absent.cellState.includes("no geyser feed"),
      absent.cellState,
    );
    t.eq("the dot claims nothing", absent.dot, "dot");
    t.eq("the view still opens", absent.open, "true");
    t.ok(
      "and says there is no feed rather than drawing a steady one",
      absent.summary.includes("no Geyser stream"),
      absent.summary,
    );
    t.eq(
      "every cell is empty",
      absent.bands.filter((band) => band === "empty").length,
      256,
      "a grid of zeroes would be a claim that the feed is perfectly steady",
    );
    t.eq("and every drift number is an em dash", absent.stats["slot drift"], "—");
    t.eq("including the ring's", absent.ring.released, "—");

    const askedTwice = await page.evaluate(
      () =>
        window.__STS_TEST__.invocations.filter((c) => c.command === "get_geyser_telemetry")
          .length,
    );
    t.ok(
      "and the window asks once rather than once a second forever",
      askedTwice <= 2,
      `${askedTwice} calls after the engine said it has no such command`,
    );
  },
};
