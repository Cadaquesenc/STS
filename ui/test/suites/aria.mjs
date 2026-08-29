// What the window says to something that is not a pair of eyes.
//
// The rule this suite exists to hold: a state that is drawn is a state that is
// announced, and neither one may claim something the other does not. A chip
// that is pressed, a row that is selected, a switch that is on, a dialog that
// is modal, a field that was rejected — each of those is a fact the window
// already draws, and each one is checked here against the attribute that
// carries it.

import { LAMPORTS, observation, push, selectFirst, enableReplay, account } from "../seed.mjs";

export default {
  name: "aria",
  async run(t, page) {
    // --- the switch, against a build with no replay control ------------------
    // First, because entering replay reloads the window and this is the only
    // state a build without the command can be in.
    const before = await page.evaluate(() => {
      const toggle = document.querySelector('[data-action="replay-toggle"]');
      return {
        role: toggle.getAttribute("role"),
        checked: toggle.getAttribute("aria-checked"),
        disabled: toggle.disabled,
      };
    });
    t.eq("the replay control is a switch", before.role, "switch");
    t.eq("a build with no replay command reports the switch as off", before.checked, "false");
    t.ok("and disables it rather than offering a control that does nothing", before.disabled);

    await enableReplay(page);

    await push(page, [
      observation({ index: 0, slot: 312_905_100, realSol: 10 * LAMPORTS, mcap: 40 * LAMPORTS }),
      observation({ index: 0, slot: 312_905_120, realSol: 12 * LAMPORTS, mcap: 43 * LAMPORTS }),
      observation({ index: 1, slot: 312_905_140, realSol: 70 * LAMPORTS, mcap: 100 * LAMPORTS }),
      observation({ index: 1, slot: 312_905_160, realSol: 72 * LAMPORTS, mcap: 103 * LAMPORTS }),
      observation({ index: 2, slot: 312_905_180, realSol: 3 * LAMPORTS, mcap: 33 * LAMPORTS }),
    ]);

    // --- landmarks and names -----------------------------------------------
    const names = await page.evaluate(() => ({
      panes: [...document.querySelectorAll(".pane")].map((p) => p.getAttribute("aria-label")),
      groups: [...document.querySelectorAll('[role="group"]')].map((g) => g.getAttribute("aria-label")),
      grid: document.querySelector('[role="grid"]')?.getAttribute("aria-label") ?? null,
      inputsWithoutName: [...document.querySelectorAll("input")]
        .filter((input) => !input.getAttribute("aria-label") && !input.labels?.length)
        .map((input) => input.dataset.filter ?? input.outerHTML.slice(0, 40)),
      buttonsWithoutName: [...document.querySelectorAll("button")]
        .filter((button) => !button.textContent.trim() && !button.getAttribute("aria-label"))
        .map((button) => button.outerHTML.slice(0, 60)),
    }));

    t.every("every pane is named", names.panes, (name) => typeof name === "string" && name.length > 3);
    t.every("every control group is named", names.groups, (name) => typeof name === "string" && name.length > 3);
    t.eq("the tick stream is a named grid", names.grid, "Tick stream");
    t.every("every input has an accessible name", names.inputsWithoutName, () => false, (x) => x);
    t.every("every button has an accessible name", names.buttonsWithoutName, () => false, (x) => x);

    // --- the tick grid ------------------------------------------------------
    const grid = await page.evaluate(() => {
      const grid = document.querySelector('[role="grid"]');
      const headers = [...grid.querySelectorAll('[role="columnheader"]')];
      const rows = [...grid.querySelectorAll('[role="row"]:not(.col-head)')];
      return {
        headers: headers.map((h) => h.textContent.trim()),
        rowgroup: !!grid.querySelector('[role="rowgroup"]'),
        cellCounts: rows.map((row) => row.querySelectorAll('[role="gridcell"]').length),
        rowCount: rows.length,
        selectedCount: rows.filter((row) => row.getAttribute("aria-selected") === "true").length,
        allHaveSelected: rows.every((row) => row.hasAttribute("aria-selected")),
        focusable: rows.every((row) => row.tabIndex === -1),
      };
    });

    t.eq("the grid declares seven columns", grid.headers.length, 7);
    t.ok("the data rows sit in a rowgroup", grid.rowgroup);
    t.every(
      "every row has one cell per column header",
      grid.cellCounts.map((count, index) => ({ count, index })),
      (row) => row.count === 7,
      (row) => `row ${row.index} has ${row.count}`,
    );
    t.ok("every row carries a selection state", grid.allHaveSelected);
    t.eq("nothing is selected before the cursor moves", grid.selectedCount, 0);
    t.ok("rows are reachable by script but not by tab", grid.focusable);

    // --- the cursor ---------------------------------------------------------
    await page.evaluate(() => {
      document.querySelector('[data-region="tick-rows"] .row').click();
    });
    await page.settle();
    await page.evaluate(() => {
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "j", bubbles: true }));
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "j", bubbles: true }));
    });
    await page.settle();

    const cursor = await page.evaluate(() => {
      const rows = [...document.querySelectorAll('[data-region="tick-rows"] .row')];
      const selected = rows.filter((row) => row.getAttribute("aria-selected") === "true");
      return {
        selectedCount: selected.length,
        selectedIndex: rows.indexOf(selected[0]),
        focusedIsSelected: selected[0] === document.activeElement,
      };
    });
    t.eq("exactly one row is selected after j j", cursor.selectedCount, 1);
    t.eq("j moved the cursor down two rows", cursor.selectedIndex, 2);
    t.ok("the selected row is the focused one", cursor.focusedIsSelected);

    // --- the radar's filter chips -------------------------------------------
    // Scoped to the radar's own group. `.pane-tools .chip` also catches the
    // tick filters and the journal/alert tabs, which are a different kind of
    // control: those are independent toggles and several of them may be on at
    // once. The claim being made here is about a radio group — exactly one of
    // these three is true at a time — and it has to be made about the three.
    const chips = await page.evaluate(() => {
      const group = '[aria-label="Candidate filter"] .chip';
      const read = () =>
        [...document.querySelectorAll(group)].map((chip) => ({
          text: chip.textContent.trim(),
          pressed: chip.getAttribute("aria-pressed"),
        }));
      const before = read();
      [...document.querySelectorAll(group)].find((c) => c.textContent.trim() === "graduating")?.click();
      return { before, after: read(), shown: document.querySelector('[data-field="candidate-count"]').textContent };
    });
    t.eq(
      "exactly one radar filter is pressed to start",
      chips.before.filter((c) => c.pressed === "true").length,
      1,
    );
    t.eq(
      "exactly one radar filter is pressed after a change",
      chips.after.filter((c) => c.pressed === "true").length,
      1,
    );
    t.ok(
      "the pressed chip is the one that was clicked",
      chips.after.find((c) => c.pressed === "true")?.text === "graduating",
      JSON.stringify(chips.after),
    );

    // --- rejected filter input ----------------------------------------------
    const invalid = await page.evaluate(() => {
      const input = document.querySelector('[data-filter="slot"]');
      const set = (value) => {
        input.value = value;
        input.dispatchEvent(new Event("input", { bubbles: true }));
        return input.getAttribute("aria-invalid");
      };
      return { garbage: set("abc"), negative: set("-4"), fractional: set("1.5"), good: set("312905140"), empty: set("") };
    });
    t.eq("a non-numeric slot filter is marked invalid", invalid.garbage, "true");
    t.eq("a negative slot filter is marked invalid", invalid.negative, "true");
    t.eq("a fractional slot filter is marked invalid", invalid.fractional, "true");
    t.eq("a valid slot filter is marked valid", invalid.good, "false");
    t.eq("an empty slot filter is valid, not invalid", invalid.empty, "false");

    // --- both modals ---------------------------------------------------------
    const dialogs = await page.evaluate(() => {
      return [...document.querySelectorAll('[role="dialog"]')].map((dialog) => {
        const labelId = dialog.getAttribute("aria-labelledby");
        const label = labelId ? document.getElementById(labelId) : null;
        return {
          region: dialog.dataset.region,
          modal: dialog.getAttribute("aria-modal"),
          hidden: dialog.hidden,
          open: dialog.dataset.open,
          labelId,
          labelText: label?.textContent.trim() ?? null,
        };
      });
    });
    // Named rather than counted. A fourth dialog appearing is a thing somebody
    // should have to write down here, and a count on its own does not say which
    // one went missing when it drops to two.
    t.eq(
      "the window has three dialogs, and they are the three it should have",
      dialogs
        .map((dialog) => dialog.region)
        .sort()
        .join(","),
      "geyser-modal,tick-modal,unwind-modal",
    );
    t.every("every dialog is modal", dialogs, (d) => d.modal === "true", (d) => d.region);
    t.every(
      "every dialog names itself with an element that exists",
      dialogs,
      (d) => typeof d.labelText === "string" && d.labelText.length > 0,
      (d) => `${d.region} → ${d.labelId}`,
    );
    t.every(
      "a closed dialog is hidden and says so twice",
      dialogs,
      (d) => d.hidden === true && d.open === "false",
      (d) => `${d.region} hidden=${d.hidden} open=${d.open}`,
    );

    const opened = await page.evaluate(() => {
      const row = document.querySelector('[data-region="tick-rows"] .row');
      row.dispatchEvent(new MouseEvent("dblclick", { bubbles: true }));
      const dialog = document.querySelector('[data-region="tick-modal"]');
      return {
        hidden: dialog.hidden,
        open: dialog.dataset.open,
        focusInside: dialog.contains(document.activeElement),
        focusIsButton: document.activeElement?.dataset?.action === "tick-close",
      };
    });
    t.eq("the tick detail is no longer hidden when open", opened.hidden, false);
    t.eq("the tick detail says it is open", opened.open, "true");
    t.ok("focus moves into the dialog", opened.focusInside);
    t.ok("focus lands on its one control", opened.focusIsButton);

    const closed = await page.evaluate(() => {
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
      const dialog = document.querySelector('[data-region="tick-modal"]');
      return {
        hidden: dialog.hidden,
        open: dialog.dataset.open,
        focusReturned: document.activeElement?.classList?.contains("row") === true,
      };
    });
    t.eq("Escape hides the tick detail", closed.hidden, true);
    t.eq("Escape marks it closed", closed.open, "false");
    t.ok("focus goes back where it came from", closed.focusReturned);

    // --- live regions ---------------------------------------------------------
    const live = await page.evaluate(() => ({
      unwind: document.querySelector('[data-field="unwind-text"]')?.getAttribute("role"),
      unwindError: document.querySelector('[data-field="unwind-error"]')?.getAttribute("role"),
      replayBar: document.querySelector('[data-region="replay-bar"]')?.getAttribute("role"),
      decorativeDots: [...document.querySelectorAll(".dot")].every(
        (dot) => dot.getAttribute("aria-hidden") === "true",
      ),
      meters: [...document.querySelectorAll(".meter")].every(
        (meter) => meter.getAttribute("aria-hidden") === "true",
      ),
    }));
    t.eq("the unwind sentence is a status region", live.unwind, "status");
    t.eq("the unwind error is an alert", live.unwindError, "alert");
    t.eq("the replay bar is a status region", live.replayBar, "status");
    t.ok("every status dot is hidden from the reader", live.decorativeDots);
    t.ok("every meter is hidden from the reader, its number is not", live.meters);

    // --- the switch, in replay ------------------------------------------------
    const after = await page.evaluate(() => {
      const toggle = document.querySelector('[data-action="replay-toggle"]');
      const pressed = [...document.querySelectorAll(".speeds .chip")].filter(
        (chip) => chip.getAttribute("aria-pressed") === "true",
      );
      const transport = [...document.querySelectorAll(".transport .chip")];
      return {
        checked: toggle.getAttribute("aria-checked"),
        disabled: toggle.disabled,
        pressedSpeeds: pressed.length,
        pressedValue: pressed[0]?.dataset.speed ?? null,
        barHidden: document.querySelector('[data-region="replay-bar"]').hidden,
        // play and pause are a pair and exactly one of them is true. step and
        // faster are presses rather than states and carry no pressed attribute
        // at all — an unpressed button and a button that is not that kind of
        // control read the same to a screen reader otherwise.
        transportPressed: transport
          .filter((b) => b.getAttribute("aria-pressed") === "true")
          .map((b) => b.dataset.transport),
        transportStateful: transport
          .filter((b) => b.hasAttribute("aria-pressed"))
          .map((b) => b.dataset.transport),
      };
    });
    t.eq("the switch reads on once the engine says it is", after.checked, "true");
    t.ok("and is enabled", !after.disabled);
    t.eq("exactly one speed is pressed", after.pressedSpeeds, 1);
    t.eq("and it is the speed the engine reported", after.pressedValue, "1");
    t.eq("the replay bar is showing", after.barHidden, false);
    t.eq(
      "only the two transport buttons that are a state carry one",
      after.transportStateful.join(","),
      "play,pause",
    );
    t.eq(
      "and the one that is true is the one the engine reported",
      after.transportPressed.join(","),
      "play",
    );

    // Losing the engine is not the same as leaving replay, and the switch has
    // a third state precisely so it does not have to claim either.
    const lost = await page.evaluate(async () => {
      window.__STS_TEST__.bridgeLive = false;
      await new Promise((resolve) => setTimeout(resolve, 1_400));
      const toggle = document.querySelector('[data-action="replay-toggle"]');
      const transport = [...document.querySelectorAll(".transport .chip")];
      return {
        checked: toggle.getAttribute("aria-checked"),
        barHidden: document.querySelector('[data-region="replay-bar"]').hidden,
        transportDisabled: transport.every((b) => b.disabled),
        transportPressed: transport.filter((b) => b.getAttribute("aria-pressed") === "true").length,
        transportTitle: transport[0].title,
      };
    });
    t.eq("a lost engine puts the switch in its third state", lost.checked, "mixed");
    t.eq("and does not take the replay bar down", lost.barHidden, false);
    t.ok(
      "and takes the transport away, because a playhead nobody can see is not one to steer",
      lost.transportDisabled,
    );
    t.eq(
      "and neither play nor pause claims to be the state it is in",
      lost.transportPressed,
      0,
      "the window does not know whether the playhead is moving",
    );
    t.ok(
      "and the buttons say that is what they do not know",
      lost.transportTitle.includes("has not answered"),
      lost.transportTitle,
    );
  },
};
