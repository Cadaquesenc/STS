// Two properties the window is supposed to have and one it is supposed not to.
//
//   1. Nothing on screen moves because a number changed. The panes update ten
//      times a second and a column that reflows when a value gets a digit wider
//      is a column somebody misreads while they are looking at it.
//   2. No heading is clipped at any width the window can be at. A column head
//      that renders as "ABORTED FR…" reads as a bug; one that renders as "MCAP"
//      when it means "MCAP SOL" reads as a different number.
//   3. The window itself never scrolls sideways. Panes scroll; the frame does
//      not.

import { LAMPORTS, observation, push, selectFirst, enableReplay, setReplay } from "../seed.mjs";

/// The width at which the replay bar stops fitting on one line.
///
/// Found rather than written down. Where the bar wraps depends on what is on
/// it — how long the fixture's name is, how many notches the multiplier ladder
/// has, how much room each fact reserved — and a number in this file would be
/// a number that quietly stopped being true the first time any of that changed,
/// in the one suite whose job is to notice exactly that.
///
/// Returns the narrowest width that still fits on one line, so the wrap happens
/// one pixel below what comes back. Null if the bar never wraps in the range,
/// which is not a failure: it is a bar that got smaller.
async function findWrapBoundary(page, from = 1600, to = 960) {
  const heightAt = async (width) => {
    await page.setViewport(width, 800);
    await page.settle();
    return page.evaluate(() =>
      Math.round(
        document.querySelector('[data-region="replay-bar"]').getBoundingClientRect().height,
      ),
    );
  };

  const oneLine = await heightAt(from);
  if ((await heightAt(to)) === oneLine) return null;

  // Invariant through the loop: `fits` is a width the bar is one line at, and
  // `wraps` is one it is taller at.
  let fits = from;
  let wraps = to;
  while (fits - wraps > 1) {
    const mid = Math.floor((fits + wraps) / 2);
    if ((await heightAt(mid)) === oneLine) fits = mid;
    else wraps = mid;
  }
  return fits;
}

/// The bar's varying facts at their narrowest and their widest.
///
/// Both are states one run passes through: a fixture is opened at the first
/// record and played to the last, and the clamp counter beside the clock counts
/// up as it goes. Nothing here is a resize and nothing here is a press — it is
/// the same fixture a few seconds later.
const PLAYHEAD_AT_THE_START = { slot: 312_900_001, recordsPlayed: 9, clamped: 0 };
const PLAYHEAD_AT_THE_END = {
  slot: 313_000_000,
  recordsPlayed: 91_244,
  clamped: 91_244,
  slotRegressions: 91_244,
};

/// The widths the window is checked at.
///
/// 960 is the configured minimum in `tauri.conf.json`; 1180 is the breakpoint
/// itself and is checked on both sides of, because a rule that only applies
/// below a width is a rule that has never been tested above it.
const WIDTHS = [1600, 1440, 1200, 1181, 1180, 1024, 960];

/// The narrowest window the replay bar still fits one line in.
///
/// Found by `findWrapBoundary` rather than trusted, and pinned here so that a
/// change to what is on the bar is a change somebody has to write down. The bar
/// is one line at 1589 and wraps to two at 1588.
///
/// It is a pinned number only because the bar is built so that it can be: the
/// facts reserve their cells and `.transport` reserves its cluster, so the
/// point is a property of the bar's design rather than of its current wording.
/// Before that reserve existed, shortening one button's label moved this by
/// twenty-eight pixels — and with it the whole band section 3 sweeps.
const WRAP_BOUNDARY = 1589;

/// Everything that has to be readable in full at every one of those widths.
const HEADINGS = [
  ".pane-head .label",
  ".section-head .label",
  ".col-head > *",
  ".replay-word",
  ".replaybar .label",
  ".integrity",
  ".badge",
  ".filter .label",
  ".chip",
  ".curve-fact .label",
  ".metric .label",
  ".tick-name",
  ".brand",
  ".kill",
  ".btn",
  // The two bars whose cells were given a reserved width. A cell wide enough
  // to stop a number shifting its neighbours and too narrow to hold the number
  // has traded one wrong reading for another.
  ".topbar-stats .stat > *",
  ".statusbar .tick > *",
];

export default {
  name: "layout",
  async run(t, page) {
    // --- 0. the cells that fill in do not change size when they do ---------
    //
    // Section 1 below resets the counter before it measures, because it is
    // asking what happens once the window is up. That reset also hides the one
    // shift every user is guaranteed to see: the one at start-up, when every
    // cell goes from an em dash to its first answer.
    //
    // Two of those transitions used to move things. The mode chip opens saying
    // `no engine` and `running` is two characters narrower, so the first status
    // slid the whole top-bar stat block twelve pixels left. The bundle deck's
    // rows opened at seven pixels wide and went to a hundred and four when
    // `get_bundle_telemetry` first answered — four of them in one frame.
    //
    // This does **not** measure that with the layout-shift observer, and the
    // reason is worth writing down. Reproducing it needs a cold page: it is a
    // race between the first paint and the first answer, and this suite's
    // `goto` is a warm reload against a primed cache, where the race does not
    // happen. An observer assertion here passed with the bug present, three
    // runs out of three, which is worse than no assertion at all.
    //
    // So this asserts the property the fix actually installs, which is not a
    // race and does not need one: **a reserved cell's box does not depend on
    // its content.** Each cell is measured with its real value, then with an em
    // dash put back into it, and the two have to agree. That is deterministic,
    // it fails the moment somebody removes a `min-width`, and it is true of the
    // cell whether or not the timing that exposed it ever recurs.
    const reserved = await page.evaluate(() => {
      const widthOf = (el) => Math.round(el.getBoundingClientRect().width);
      const cells = [];

      // The mode chip holds its word in a child, so the dot beside it survives.
      const mode = document.querySelector('[data-field="mode"]');
      const word = mode.querySelector(".label");
      const modeText = word.textContent;
      const modeFilled = widthOf(mode);
      // Every word `renderEngineStatus` can put in it, plus the one the markup
      // opens with. Measuring two of them would reserve for two of them.
      let modeShortest = modeFilled;
      let modeLongest = modeFilled;
      for (const state of [
        "no engine",
        "shadow",
        "halted",
        "stopping",
        "stopped",
        "paper",
        "live",
      ]) {
        word.textContent = state;
        const width = widthOf(mode);
        modeShortest = Math.min(modeShortest, width);
        modeLongest = Math.max(modeLongest, width);
      }
      word.textContent = modeText;
      cells.push({ field: "mode", filled: modeFilled, empty: modeShortest, longest: modeLongest });

      for (const field of [
        "tip-floor",
        "congestion",
        "leader-proximity",
        "land-rate",
        "bundle-states",
        "bundle-settle",
      ]) {
        const el = document.querySelector(`[data-field="${field}"]`);
        const text = el.textContent;
        const filled = widthOf(el);
        el.textContent = "\u2014";
        const empty = widthOf(el);
        el.textContent = text;
        cells.push({ field, filled, empty, longest: filled });
      }
      return cells;
    });

    t.ok(
      "the deck had filled in, so there was something to measure",
      reserved.some((cell) => cell.field === "bundle-states" && cell.filled > 20),
      `bundle-states is ${reserved.find((c) => c.field === "bundle-states")?.filled}px wide`,
    );
    t.every(
      "no cell that starts as an em dash changes width when it fills in",
      reserved,
      (cell) => cell.filled === cell.empty,
      (cell) => `${cell.field}: ${cell.empty}px empty, ${cell.filled}px filled`,
    );
    t.every(
      "and the widest word each of them can hold still fits the same box",
      reserved,
      (cell) => cell.longest === cell.filled,
      (cell) => `${cell.field}: ${cell.filled}px filled, ${cell.longest}px at its widest`,
    );

    // --- 1. no shift while the numbers move --------------------------------
    //
    // Fifty observations across four accounts, interleaved so the same account
    // is revisited and every derived column recomputes. The ingestion poll is
    // running underneath the whole time and every counter it returns changes on
    // every call.
    const batch = [];
    for (let step = 0; step < 50; step += 1) {
      const index = step % 4;
      const real = (1 + step * 1.37 + index * 9) * LAMPORTS;
      batch.push(
        observation({
          index,
          slot: 312_905_000 + step * 3,
          realSol: Math.round(real),
          mcap: Math.round(real * 1.48 + 30 * LAMPORTS),
        }),
      );
    }

    // Replay first: entering it reloads the window, and the whole point of the
    // measurement below is that nothing moved after the reload.
    await enableReplay(page);
    await page.evaluate(() => window.__STS_TEST__.reset());
    await push(page, batch);
    await selectFirst(page);

    // And the three feeds that were added after this measurement was written,
    // streaming into it at the same time. Each of them appends to a list inside
    // a box that never changes height — the journal and the alerts share one
    // fixed viewport, the sub-slot ring is a fixed 0x100 grid — and this is the
    // assertion that those boxes really are fixed. A list that grew its
    // container would push the event trail down a row per arrival, which is the
    // one thing this pane cannot do.
    await page.evaluate(() => {
      const test = window.__STS_TEST__;
      for (let index = 0; index < 30; index += 1) {
        test.pushAlert({ kind: index % 2 ? "tipOverrun" : "slippageSpike" });
      }
      test.pushGeyserRun(200, { stepUs: 40_000, wobbleUs: 30_000 });
      // A trade closing changes a row's width in every column that carries a
      // number, which is the journal's version of a counter gaining a digit.
      test.journal[0] = Object.assign({}, test.journal[0], {
        proceedsLamports: 260_000_000,
        realizedPnlLamports: 7_351_500,
        closedAtMs: 1_700_000_004_900,
        slippageBps: 9_999,
      });
    });

    // Let the pollers run long enough to repaint the status bar many times, and
    // long enough for the journal to be asked again and answer differently.
    await page.evaluate(
      () => new Promise((resolve) => setTimeout(resolve, 1_400)),
    );
    await page.settle();

    const streaming = await page.evaluate(() => ({
      cls: window.__STS_TEST__.cls,
      shifts: window.__STS_TEST__.shifts,
      observerError: window.__STS_TEST__.observerError ?? null,
      ticks: document.querySelectorAll('[data-region="tick-rows"] .row').length,
      polls: window.__STS_TEST__.invocations.filter((i) => i.command === "get_ingestion_metrics").length,
      alerts: document.querySelectorAll('[data-region="alert-rows"] .row').length,
      samples: window.__STS_UI__.geyser.samples,
      journalRevision: window.__STS_UI__.feeds.journal.revision,
    }));

    t.ok("the layout-shift observer attached", streaming.observerError === null, streaming.observerError ?? "");
    t.ok("the stream actually ran", streaming.ticks >= 50, `${streaming.ticks} tick rows`);
    t.ok("the ingestion poll actually ran", streaming.polls >= 5, `${streaming.polls} polls`);
    t.ok("the alert feed actually filled", streaming.alerts >= 30, `${streaming.alerts} alert rows`);
    t.ok("the sub-slot ring actually filled", streaming.samples >= 200, `${streaming.samples} samples`);
    t.ok(
      "and the journal actually changed under it",
      streaming.journalRevision >= 2,
      `${streaming.journalRevision} revisions`,
    );
    // **Green here does not mean the window never moves.** It means nothing
    // moved *after* boot, and the distinction is not academic — a real
    // start-up shift lived behind this assertion for as long as it existed.
    //
    // Two blind spots, both by construction. The counter is reset above, so
    // everything before the reset is outside the measurement. And the reset is
    // not the only reason: reproducing a start-up shift needs a *cold* page,
    // and every `goto` in this file is a warm reload against a primed cache,
    // where the race between first paint and first answer does not happen. An
    // observer assertion written here passed with a known shift present, three
    // runs out of three.
    //
    // What covers that class instead is section 0, which asserts the property
    // rather than the race: a cell that opens as an em dash does not change
    // width when it fills in. If you are reading this green and concluding the
    // window is shift-free, read section 0's result too — and if you are adding
    // a cell that starts empty, add it there.
    t.eq(
      "zero layout shift while the window streams",
      streaming.cls,
      0,
      streaming.shifts.map((s) => `${s.value.toFixed(4)} @${s.at}ms ${s.sources.join(",")}`).join(" | "),
    );

    // Selecting, filtering and unfiltering are interactions, so the metric
    // excludes them by design — which is exactly why they are checked
    // separately, by measuring the box of a row before and after.
    const stable = await page.evaluate(() => {
      const rows = [...document.querySelectorAll('[data-region="tick-rows"] .row')].slice(0, 12);
      const before = rows.map((row) => row.getBoundingClientRect().height);
      rows[3]?.focus();
      rows[7]?.click();
      const after = rows.map((row) => row.getBoundingClientRect().height);
      return { before, after };
    });
    t.every(
      "selecting a row does not change any row's height",
      stable.before.map((height, index) => ({ height, after: stable.after[index], index })),
      (pair) => Math.abs(pair.height - pair.after) < 0.01,
      (pair) => `row ${pair.index}: ${pair.height} → ${pair.after}`,
    );

    // --- 2. nothing clipped at any width -----------------------------------
    //
    // The widths above plus the two either side of the replay bar's wrap point,
    // which is where the bar is least like itself and was the one part of the
    // range nothing looked at.
    const boundary = await findWrapBoundary(page);
    t.ok(
      "the replay bar has a wrap point inside the window's width range",
      boundary !== null,
      "the bar fits on one line at 960px, so section 3 has nothing to stand on",
    );

    // And it is *this* wrap point.
    //
    // The search above is deliberately a search — where the bar breaks is a
    // function of what is on it, and a number retyped here would be a number
    // that quietly stopped being true. This is the other half of that: the bar
    // is finished, its contents are fixed, and the width it breaks at is now a
    // property of the window rather than an accident of it. 1589 is the widest
    // window the bar wraps in; at 1590 it is one line.
    //
    // A failure here is not necessarily a bug — it means somebody changed what
    // is on the replay bar, which is allowed. It means they have to say so, and
    // to re-measure the band section 3 sweeps, because every assertion below is
    // written against this number rather than against a width somebody picked.
    t.eq(
      "and it is at 1589px, where it is pinned",
      boundary,
      WRAP_BOUNDARY,
      "the replay bar's contents changed; re-measure, and move the reserve in styles.css or WRAP_BOUNDARY here",
    );

    // The widths above, plus the four pixels around the wrap point: two either
    // side, so both arrangements of the bar are measured and so is the pixel
    // the change happens at.
    const widths = [
      ...new Set(
        [
          ...WIDTHS,
          WRAP_BOUNDARY + 2,
          WRAP_BOUNDARY + 1,
          WRAP_BOUNDARY,
          WRAP_BOUNDARY - 1,
          boundary,
          boundary - 1,
        ].filter(Boolean),
      ),
    ].sort((a, b) => b - a);

    for (const width of widths) {
      await page.setViewport(width, 800);
      await page.settle();

      const report = await page.evaluate((selectors) => {
        const clipped = [];
        for (const selector of selectors) {
          for (const el of document.querySelectorAll(selector)) {
            // An element inside a hidden branch has no box to clip.
            if (!el.getClientRects().length) continue;
            // Nor does the screen-reader text beside a status dot: it is one
            // pixel square on purpose and its content is meant to overflow it.
            if (el.classList.contains("visually-hidden")) continue;
            const overflowsX = el.scrollWidth - el.clientWidth > 1;
            const overflowsY = el.scrollHeight - el.clientHeight > 1;
            if (overflowsX || overflowsY) {
              clipped.push(
                `${selector} "${el.textContent.trim().slice(0, 24)}" ` +
                  `${el.scrollWidth}×${el.scrollHeight} in ${el.clientWidth}×${el.clientHeight}`,
              );
            }
          }
        }
        // A heading that wraps is not clipped — the bar it sits in is tall
        // enough for two lines and nothing is lost — but it is a heading that
        // has run out of room, and the row of headings it belongs to no longer
        // reads as a row. It is checked separately for that reason.
        const wrapped = [];
        for (const selector of selectors) {
          for (const el of document.querySelectorAll(selector)) {
            if (!el.getClientRects().length) continue;
            if (el.classList.contains("visually-hidden")) continue;
            // The element's own height is no use here: a chip has a fixed
            // height and a badge has a border, and both are taller than their
            // line without wrapping. What is wanted is how many lines the text
            // occupies.
            //
            // Which is the text's rects, and only the text's. A range over an
            // element's *contents* also reports a box for every element inside
            // it, and an inline-block's border box sits a pixel above the text
            // in it — so a heading that is a sort control reads as two lines
            // while rendering as one. Walking the text nodes measures the thing
            // this is about and nothing else: a heading that genuinely wraps
            // still puts its words a line height apart, and that is still two.
            const tops = new Set();
            const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
            for (let node = walker.nextNode(); node; node = walker.nextNode()) {
              if (!node.textContent.trim()) continue;
              const range = document.createRange();
              range.selectNodeContents(node);
              for (const rect of range.getClientRects()) tops.add(Math.round(rect.top));
            }
            if (tops.size > 1) {
              wrapped.push(
                `${selector} "${el.textContent.trim().slice(0, 28)}" over ${tops.size} lines`,
              );
            }
          }
        }

        // The bar's own two rules, which the heading sweep above cannot see.
        // A fact whose value is ellipsised is not clipped by the measure used
        // above — the element reports the width it was given — so the values
        // are checked directly; and the transport and the multiplier are one
        // cluster, which a wrapped bar is free to break apart and must not.
        const truncated = [
          ...document.querySelectorAll(".replaybar .mono, .replaybar .key"),
        ]
          .filter((el) => el.getClientRects().length && el.scrollWidth - el.clientWidth > 1)
          .map((el) => `${el.dataset.field ?? "fact"} "${el.textContent.trim()}" ${el.scrollWidth} in ${el.clientWidth}`);

        const firstTransport = document.querySelector(".transport .chip");
        const firstSpeed = document.querySelector(".speeds .chip");

        return {
          clipped,
          wrapped,
          truncated,
          controlsTogether:
            Math.abs(
              firstTransport.getBoundingClientRect().top -
                firstSpeed.getBoundingClientRect().top,
            ) < 1,
          documentScrollWidth: document.documentElement.scrollWidth,
          innerWidth: window.innerWidth,
          bodyScrollHeight: document.body.scrollHeight,
          innerHeight: window.innerHeight,
          replayBarVisible: !document.querySelector('[data-region="replay-bar"]').hidden,
        };
      }, HEADINGS);

      t.every(
        `no heading is clipped at ${width}px`,
        report.clipped,
        () => false,
        (entry) => entry,
      );
      t.every(
        `no heading wraps to a second line at ${width}px`,
        report.wrapped,
        () => false,
        (entry) => entry,
      );
      t.ok(
        `the window does not scroll sideways at ${width}px`,
        report.documentScrollWidth <= report.innerWidth,
        `${report.documentScrollWidth} > ${report.innerWidth}`,
      );
      t.ok(
        `the window does not scroll down at ${width}px`,
        report.bodyScrollHeight <= report.innerHeight + 1,
        `${report.bodyScrollHeight} > ${report.innerHeight}`,
      );
      t.ok(
        `the replay bar is still up at ${width}px`,
        report.replayBarVisible === true,
        "the bar says nothing below it is live and must survive every width",
      );
      t.every(
        `no fact on the replay bar is truncated at ${width}px`,
        report.truncated,
        () => false,
        (entry) => entry,
      );
      t.ok(
        `the transport and the multiplier stay on one line at ${width}px`,
        report.controlsTogether === true,
        "`faster` walks the ladder the chips draw; split across lines they read as unrelated",
      );
    }

    // --- 3. no number pushes the bar across its own wrap point -------------
    //
    // The property section 1 measures, at the width it is hardest to hold. The
    // bar wraps rather than clipping a fact, and where it wraps is decided by
    // how wide its contents are — so in the band just below the wrap point, a
    // record count gaining a digit is enough to put the controls on a second
    // line. The bar grows by a line, every pane below it moves down by a line,
    // and nobody resized anything and nobody pressed anything: a counter ticked
    // over while somebody was reading the row underneath it.
    //
    // Which is why each of those facts is given a cell wide enough for the
    // widest thing it can say. This is the assertion that the cells are wide
    // enough, and it is written against the boundary rather than against a
    // width somebody picked, because the band is about thirty pixels wide and
    // the widths in section 2 happened to fall either side of it.
    if (boundary !== null) {
      const panesTop = () =>
        page.evaluate(() =>
          Math.round(document.querySelector(".panes").getBoundingClientRect().top),
        );

      for (const width of [boundary + 2, boundary, boundary - 1, boundary - 12, boundary - 30]) {
        await page.setViewport(width, 800);

        await setReplay(page, PLAYHEAD_AT_THE_START);
        const atStart = await panesTop();

        await setReplay(page, PLAYHEAD_AT_THE_END);
        const atEnd = await panesTop();

        t.eq(
          `playing the fixture out does not move the panes at ${width}px`,
          atEnd,
          atStart,
          "a counter gaining a digit crossed the bar's wrap point and took every pane with it",
        );

        // And the cells are reserved rather than merely wide: a value that
        // overflowed the room kept for it would be ellipsised, which is the
        // other way to hide a fact the bar exists to show.
        const overflowed = await page.evaluate(() =>
          [...document.querySelectorAll(".replaybar .mono, .replaybar .key")]
            .filter((el) => el.getClientRects().length && el.scrollWidth - el.clientWidth > 1)
            .map((el) => `${el.dataset.field ?? "fact"} "${el.textContent.trim()}"`),
        );
        t.every(
          `no fact is clipped by its reserved cell at ${width}px`,
          overflowed,
          () => false,
          (entry) => entry,
        );
      }
    }

    await page.setViewport(1440, 900);
    await page.settle();
  },
};
