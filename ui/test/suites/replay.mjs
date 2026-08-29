// The replay badge.
//
// The whole point of the bar is that it is the one thing on screen that says
// nothing else on screen is live. So the assertions are less about what it
// renders than about when it refuses to: it never claims a fixture was verified
// unless the engine said it verified one, it does not go away when the engine
// goes quiet, and the switch above it reports the engine's answer rather than
// the operator's click.

import { enableReplay, fieldText, setReplay } from "../seed.mjs";

const CHAIN_HEAD = "9f2c1ab74e0d5c8831bb0e6f4a27d9c05e1f3a7b6c8d90e1f2a3b4c5d6e7f809";

function integrity() {
  const el = document.querySelector('[data-field="replay-integrity"]');
  return { state: el.dataset.state, text: el.textContent.trim(), title: el.title };
}

export default {
  name: "replay",
  async run(t, page) {
    // --- a build with no replay control -------------------------------------
    const absent = await page.evaluate(async () => {
      await new Promise((resolve) => setTimeout(resolve, 1_300));
      const toggle = document.querySelector('[data-action="replay-toggle"]');
      return {
        barHidden: document.querySelector('[data-region="replay-bar"]').hidden,
        checked: toggle.getAttribute("aria-checked"),
        disabled: toggle.disabled,
        title: toggle.title,
        asked: window.__STS_TEST__.invocations.filter((i) => i.command === "get_replay_status").length,
      };
    });
    t.ok("the bar is down when there is no replay", absent.barHidden);
    t.eq("the switch is off rather than unknown", absent.checked, "false");
    t.ok("and disabled", absent.disabled);
    t.ok(
      "and says why in the words of the missing command",
      absent.title.includes("set_replay_playback"),
      absent.title,
    );
    t.ok(
      "and the window stops asking after being told once",
      absent.asked <= 2,
      `${absent.asked} calls to get_replay_status`,
    );

    // --- in replay ------------------------------------------------------------
    await enableReplay(page);

    const bar = await page.evaluate(() => ({
      hidden: document.querySelector('[data-region="replay-bar"]').hidden,
      role: document.querySelector('[data-region="replay-bar"]').getAttribute("role"),
      outsidePanes: !document.querySelector(".panes").contains(document.querySelector('[data-region="replay-bar"]')),
      aboveThePanes:
        document.querySelector('[data-region="replay-bar"]').getBoundingClientRect().bottom <=
        document.querySelector(".panes").getBoundingClientRect().top + 1,
    }));
    t.eq("the bar comes up", bar.hidden, false);
    t.eq("as a status region", bar.role, "status");
    t.ok("outside the panes, so no pane can scroll it away", bar.outsidePanes);
    t.ok("and above them", bar.aboveThePanes);

    t.eq("it says the slot clock is virtualised", await fieldText(page, "replay-clock"), "virtualised · 0 clamped");
    // The switch sends `set_replay_playback(active: true)`, which is
    // `ReplaySession::start` — open, **rewind**, play. So a fixture that had a
    // playhead four thousand records in is at its first record here, and the
    // bar says so. That is the difference between the switch and the
    // transport's play button, which resumes and does not rewind, and it is
    // worth pinning on the bar rather than only in the transport suite: this is
    // the number an operator reads to find out which of the two just happened.
    t.eq("starting a fixture rewinds it", await fieldText(page, "replay-progress"), "0 / 91,244");
    t.eq("so the slot is the fixture's first", await fieldText(page, "replay-slot"), "312,900,000");
    t.eq("and which fixture", await fieldText(page, "replay-fixture"), "phas…8-14");
    t.eq(
      "and its chain head",
      await fieldText(page, "replay-chain-head"),
      `${CHAIN_HEAD.slice(0, 8)}…${CHAIN_HEAD.slice(-8)}`,
    );
    t.eq(
      "with the whole hash available rather than only the ends",
      await page.evaluate(() => document.querySelector('[data-field="replay-chain-head"]').title),
      CHAIN_HEAD,
    );

    // --- the four integrity states ---------------------------------------------
    const verified = await page.evaluate(integrity);
    t.eq("a verified chain reads as verified", verified.state, "verified");
    t.eq("and says so", verified.text, "verified");

    const unverified = await page.evaluate((source) => {
      window.__STS_TEST__.pushReplay({
        ...window.__STS_TEST__.replay,
        chainVerified: undefined,
      });
      return new Function(`return (${source})()`)();
    }, integrity.toString());
    t.eq("a chain nobody checked is not a chain that passed", unverified.state, "unverified");
    t.ok(
      "and the reader is told it is the absence of the check",
      unverified.title.includes("absence of the check"),
      unverified.title,
    );

    const partial = await page.evaluate((source) => {
      window.__STS_TEST__.pushReplay({
        ...window.__STS_TEST__.replay,
        chainVerified: true,
        fixtureComplete: false,
      });
      return new Function(`return (${source})()`)();
    }, integrity.toString());
    t.eq("a recording that was cut short is its own state", partial.state, "partial");
    t.ok(
      "even though every link in it verifies",
      partial.title.includes("Every link that exists verifies"),
      partial.title,
    );

    const broken = await page.evaluate((source) => {
      window.__STS_TEST__.pushReplay({
        ...window.__STS_TEST__.replay,
        chainVerified: false,
        fixtureComplete: true,
      });
      return new Function(`return (${source})()`)();
    }, integrity.toString());
    t.eq("a chain that does not verify reads as broken", broken.state, "broken");
    t.ok(
      "and says what a broken chain means for anything replayed from it",
      broken.title.includes("altered, reordered or lost") &&
        broken.title.includes("evidence of anything"),
      broken.title,
    );

    // A broken chain outranks a truncated recording: both are wrong and only
    // one of them means the bytes were altered.
    const both = await page.evaluate((source) => {
      window.__STS_TEST__.pushReplay({
        ...window.__STS_TEST__.replay,
        chainVerified: false,
        fixtureComplete: false,
      });
      return new Function(`return (${source})()`)();
    }, integrity.toString());
    t.eq("broken outranks partial", both.state, "broken");

    // --- the clamp counters -----------------------------------------------------
    const clamped = await page.evaluate(() => {
      window.__STS_TEST__.pushReplay({
        ...window.__STS_TEST__.replay,
        chainVerified: true,
        fixtureComplete: true,
        clamped: 17,
        slotRegressions: 4,
      });
      const el = document.querySelector('[data-field="replay-clock"]');
      return { text: el.textContent.trim(), title: el.title };
    });
    t.eq("records whose timestamp was behind the clock are counted on the bar", clamped.text, "virtualised · 17 clamped");
    t.ok(
      "and the slot regressions are there too",
      clamped.title.includes("4 arrived with a slot behind it"),
      clamped.title,
    );

    // --- the playhead moves on telemetry alone ----------------------------------
    const moved = await page.evaluate(() => {
      window.__STS_TEST__.pushReplay({
        ...window.__STS_TEST__.replay,
        slot: 312_950_500,
        recordsPlayed: 40_000,
      });
      return {
        slot: document.querySelector('[data-field="replay-slot"]').textContent.trim(),
        progress: document.querySelector('[data-field="replay-progress"]').textContent.trim(),
        polls: window.__STS_TEST__.invocations.filter((i) => i.command === "get_replay_status").length,
      };
    });
    t.eq("the playhead follows the stream", moved.slot, "312,950,500");
    t.eq("and so does the record count", moved.progress, "40,000 / 91,244");

    // --- speed --------------------------------------------------------------------
    const speeds = await page.evaluate(() => {
      const chips = [...document.querySelectorAll(".speeds .chip")];
      return {
        offered: chips.map((chip) => chip.dataset.speed),
        labels: chips.map((chip) => chip.textContent.trim()),
      };
    });
    t.eq("four multipliers are offered", speeds.offered.length, 4);
    t.eq("and they are the four in the brief", speeds.labels.join(","), "1x,5x,10x,max");

    for (const wanted of ["5", "10", "max", "1"]) {
      const result = await page.evaluate((speed) => {
        // Only what this click sends. The switch legitimately named `active`
        // on the way into replay, and counting that would make the assertion
        // below pass for the wrong reason.
        const from = window.__STS_TEST__.invocations.length;
        document.querySelector(`.speeds .chip[data-speed="${speed}"]`).click();
        return new Promise((resolve) =>
          setTimeout(() => {
            const pressed = [...document.querySelectorAll(".speeds .chip")].filter(
              (chip) => chip.getAttribute("aria-pressed") === "true",
            );
            resolve({
              count: pressed.length,
              value: pressed[0]?.dataset.speed ?? null,
              // The chips go through the narrow command. It is the half that
              // cannot start or stop a fixture, which is the whole reason the
              // split exists.
              sent: window.__STS_TEST__.invocations
                .filter((i) => i.command === "set_replay_speed")
                .at(-1)?.payload,
              // Nothing a chip sends may name the switch, on either command.
              touchedActive: window.__STS_TEST__.invocations
                .slice(from)
                .some((i) => i.payload && "active" in i.payload),
            });
          }, 30),
        );
      }, wanted);
      t.eq(`asking for ${wanted}x sends it to the engine`, result.sent?.speed, wanted);
      t.eq(`and exactly one chip is pressed at ${wanted}x`, result.count, 1);
      t.eq(`and it is ${wanted}x`, result.value, wanted);
      t.ok(
        `and asking for ${wanted}x cannot stop the fixture`,
        result.touchedActive === false,
        "a speed chip sent a payload naming `active`",
      );
    }

    // --- an engine that refuses --------------------------------------------------
    const refused = await page.evaluate(async () => {
      const test = window.__STS_TEST__;
      const before = test.replay.speed;
      // The engine says no and keeps the speed it had. The window must show the
      // engine's answer, not the operator's request.
      const original = window.__TAURI_INTERNALS__.invoke;
      window.__TAURI_INTERNALS__.invoke = (command, payload) =>
        command === "set_replay_speed"
          ? Promise.reject(new Error("the fixture cannot be played faster than it was recorded"))
          : original(command, payload);
      document.querySelector('.speeds .chip[data-speed="max"]').click();
      await new Promise((resolve) => setTimeout(resolve, 80));
      window.__TAURI_INTERNALS__.invoke = original;
      const pressed = [...document.querySelectorAll(".speeds .chip")].find(
        (chip) => chip.getAttribute("aria-pressed") === "true",
      );
      return { before, pressed: pressed?.dataset.speed ?? null };
    });
    t.eq("a refused speed change leaves the chip where the engine left it", refused.pressed, refused.before);

    // The transport is a suite of its own: it is four controls, a four-state
    // machine and a live-feed refusal, and folding it in here would make this
    // file about the transport rather than about the badge.
  },
};
