// The playback transport.
//
// Four buttons that move a playhead inside a run the switch has already
// started, drawn from `ReplayStatus.state` — `stopped`, `playing`, `paused`,
// `ended` — and never from the press that caused it.
//
// Three properties hold under everything here, and most of the assertions are
// one of the three:
//
//   1. **No button here can start or stop a fixture.** `set_replay_transport`
//      has no way to reach `active` in Rust, nothing in the window sends
//      `stop`, and `play` sends `resume` — which carries on rather than
//      rewinding. The switch stays the only way in and the only way out.
//   2. **Every button is drawn from the engine's answer.** A playhead the
//      operator asked to hold and the engine did not hold is a playhead that is
//      still moving, and the transport must say so.
//   3. **A refusal is an answer.** `refuse_over_a_live_feed` is what keeps the
//      bar's claim true, and a window that drew a press as having worked when
//      the engine refused it would be the one thing on screen disagreeing with
//      the engine about whether a recording is behind the clock.

import { enableReplay, setReplay } from "../seed.mjs";

/// What every transport button is doing right now.
function readTransport() {
  return [...document.querySelectorAll(".transport .chip")].map((button) => ({
    action: button.dataset.transport,
    label: button.textContent.trim(),
    disabled: button.disabled,
    pressed: button.getAttribute("aria-pressed"),
    title: button.title,
  }));
}

/// Presses one button and lets the answer come back.
async function press(page, action) {
  await page.evaluate(async (which) => {
    document.querySelector(`.transport .chip[data-transport="${which}"]`)?.click();
    await new Promise((resolve) => setTimeout(resolve, 60));
  }, action);
  await page.settle();
}

/// Every call the window has made to the transport command, with its payload.
function transportCalls(page) {
  return page.evaluate(() =>
    window.__STS_TEST__.invocations
      .filter((call) => call.command === "set_replay_transport")
      .map((call) => call.payload),
  );
}

export default {
  name: "transport",
  async run(t, page) {
    // --- outside replay -----------------------------------------------------
    //
    // The buttons exist on a build that has never been in replay and they do
    // nothing there. Drawn rather than hidden: a control that appears when a
    // fixture starts is a control nobody knows is available.
    await page.goto(`${page.origin}?replay=1`);
    await page.settle();

    const idle = await page.evaluate(readTransport);
    t.eq("the transport is four buttons", idle.length, 4);
    t.every(
      "and outside replay every one of them is unavailable",
      idle,
      (button) => button.disabled === true,
      (button) => button.action,
    );
    t.every(
      "and each says the switch is what starts a fixture",
      idle,
      (button) => button.title.includes("replay switch"),
      (button) => `${button.action}: ${button.title}`,
    );
    t.eq(
      "the four are play, pause, step and ff",
      idle.map((button) => button.action).join(","),
      "play,pause,step,fastForward",
    );

    // --- in replay, playing -------------------------------------------------
    await enableReplay(page);
    await page.evaluate(() => window.__STS_TEST__.reset());

    const playing = await page.evaluate(readTransport);
    t.every(
      "a moving playhead leaves every button available",
      playing,
      (button) => button.disabled === false,
      (button) => button.action,
    );
    t.eq(
      "play is lit while the playhead is moving",
      playing.find((button) => button.action === "play").pressed,
      "true",
    );
    t.eq(
      "and pause is not",
      playing.find((button) => button.action === "pause").pressed,
      "false",
    );
    t.ok(
      "and play says the playhead is already moving",
      playing.find((button) => button.action === "play").title.includes("already moving"),
      playing.find((button) => button.action === "play").title,
    );

    // --- pause --------------------------------------------------------------
    await press(page, "pause");

    const held = await page.evaluate(() => ({
      buttons: [...document.querySelectorAll(".transport .chip")].map((b) => ({
        action: b.dataset.transport,
        pressed: b.getAttribute("aria-pressed"),
        disabled: b.disabled,
      })),
      engineState: window.__STS_TEST__.replay.state,
      engineActive: window.__STS_TEST__.replay.active,
      barHidden: document.querySelector('[data-region="replay-bar"]').hidden,
      checked: document.querySelector('[data-action="replay-toggle"]').getAttribute("aria-checked"),
    }));

    t.eq("pausing puts the engine in paused", held.engineState, "paused");
    t.eq(
      "and pause is now the lit one",
      held.buttons.find((b) => b.action === "pause").pressed,
      "true",
    );
    t.eq(
      "and play is not",
      held.buttons.find((b) => b.action === "play").pressed,
      "false",
    );
    t.ok(
      "a held playhead is still an active run",
      held.engineActive === true,
      "pause took the fixture off the clock, which is the one thing it must not do",
    );
    t.eq("so the bar stays up", held.barHidden, false);
    t.eq("and the switch still reads on", held.checked, "true");
    t.every(
      "and nothing is disabled by a hold",
      held.buttons,
      (b) => b.disabled === false,
      (b) => b.action,
    );

    // The payload, which is the half of this a screenshot cannot see.
    const pausePayloads = await transportCalls(page);
    t.eq("pause sent exactly one command", pausePayloads.length, 1);
    t.eq("and it sent the pause control", pausePayloads[0].control, "pause");
    t.ok(
      "and nothing about whether a fixture is running",
      !("active" in pausePayloads[0]),
      JSON.stringify(pausePayloads[0]),
    );

    const heldTitles = await page.evaluate(readTransport);
    t.ok(
      "and now play promises not to rewind",
      heldTitles.find((button) => button.action === "play").title.includes("does not rewind"),
      heldTitles.find((button) => button.action === "play").title,
    );
    t.ok(
      "which is the promise the switch above it does not make",
      heldTitles
        .find((button) => button.action === "play")
        .title.includes("switch is what starts a fixture"),
      heldTitles.find((button) => button.action === "play").title,
    );

    // --- play sends resume, not play ---------------------------------------
    //
    // The single most consequential line in the binding. `ReplayControl::Play`
    // opens the fixture, rewinds it and plays; `Resume` carries on from where
    // the playhead is. A play button that sent `play` would silently throw away
    // the position somebody paused to look at.
    await page.evaluate(() => window.__STS_TEST__.reset());
    const beforeResume = await page.evaluate(() => window.__STS_TEST__.replay.recordsPlayed);
    await press(page, "play");

    const resumed = await page.evaluate(() => ({
      payloads: window.__STS_TEST__.invocations
        .filter((c) => c.command === "set_replay_transport")
        .map((c) => c.payload),
      state: window.__STS_TEST__.replay.state,
      recordsPlayed: window.__STS_TEST__.replay.recordsPlayed,
      pressed: document
        .querySelector('.transport .chip[data-transport="play"]')
        .getAttribute("aria-pressed"),
    }));

    t.eq("the play button sends resume", resumed.payloads[0].control, "resume");
    t.ok(
      "it is not the control that rewinds",
      resumed.payloads[0].control !== "play",
      "play would open the fixture, rewind it and play it from the first record",
    );
    t.eq("the engine is moving again", resumed.state, "playing");
    t.eq(
      "and the playhead did not go back to the start",
      resumed.recordsPlayed,
      beforeResume,
    );
    t.eq("and play is lit again", resumed.pressed, "true");

    // --- step ---------------------------------------------------------------
    //
    // A frame advance. One record, whatever the multiplier says, and a hold
    // afterwards — which is the session's own behaviour and not the window's.
    await setReplay(page, { state: "paused", speed: "10" });
    await page.evaluate(() => window.__STS_TEST__.reset());
    const beforeStep = await page.evaluate(() => ({
      played: window.__STS_TEST__.replay.recordsPlayed,
      slot: window.__STS_TEST__.replay.slot,
    }));
    await press(page, "step");

    const stepped = await page.evaluate(() => ({
      payload: window.__STS_TEST__.invocations
        .filter((c) => c.command === "set_replay_transport")
        .map((c) => c.payload)[0],
      played: window.__STS_TEST__.replay.recordsPlayed,
      slot: window.__STS_TEST__.replay.slot,
      state: window.__STS_TEST__.replay.state,
      speed: window.__STS_TEST__.replay.speed,
      shown: document.querySelector('[data-field="replay-progress"]').textContent.trim(),
    }));

    t.eq("step sends the step control", stepped.payload.control, "step");
    t.eq("and asks for exactly one record", stepped.payload.records, 1);
    t.eq("one record was played", stepped.played, beforeStep.played + 1);
    t.eq("and the playhead moved one slot with it", stepped.slot, beforeStep.slot + 1);
    t.eq("a step leaves the playhead held", stepped.state, "paused");
    t.eq("and does not touch the multiplier", stepped.speed, "10");
    t.ok(
      "and the bar shows the record it moved to",
      stepped.shown.startsWith(`${(beforeStep.played + 1).toLocaleString("en-US")} /`),
      stepped.shown,
    );

    // Stepping a *moving* playhead is permitted, because this build's session
    // permits it. The window draws no refusal the engine does not make.
    await setReplay(page, { state: "playing" });
    const stepWhileMoving = await page.evaluate(
      () => document.querySelector('.transport .chip[data-transport="step"]').disabled,
    );
    t.eq("step is available on a moving playhead too", stepWhileMoving, false);

    // --- fast forward -------------------------------------------------------
    //
    // Records, not a multiplier. It plays them now instead of over the next
    // several seconds, and it plays them rather than skipping them.
    await setReplay(page, { state: "paused", speed: "1" });
    await page.evaluate(() => window.__STS_TEST__.reset());
    const beforeFf = await page.evaluate(() => window.__STS_TEST__.replay.recordsPlayed);
    await press(page, "fastForward");

    const forwarded = await page.evaluate(() => ({
      payload: window.__STS_TEST__.invocations
        .filter((c) => c.command === "set_replay_transport")
        .map((c) => c.payload)[0],
      played: window.__STS_TEST__.replay.recordsPlayed,
      speed: window.__STS_TEST__.replay.speed,
      state: window.__STS_TEST__.replay.state,
      speedPressed: [...document.querySelectorAll(".speeds .chip")].find(
        (chip) => chip.getAttribute("aria-pressed") === "true",
      )?.dataset.speed,
    }));

    t.eq("ff sends the fastForward control", forwarded.payload.control, "fastForward");
    t.ok(
      "and it is bounded",
      Number.isFinite(forwarded.payload.records) && forwarded.payload.records > 0,
      "a fastForward with no count plays every record that is left, which is a whole fixture per press",
    );
    t.eq(
      "it played the records it asked for",
      forwarded.played,
      beforeFf + forwarded.payload.records,
    );
    t.eq("and left the playhead held", forwarded.state, "paused");
    t.eq("a fast-forward is not a speed change", forwarded.speed, "1");
    t.eq("so the multiplier chip does not move", forwarded.speedPressed, "1");

    // --- the end of a fixture ----------------------------------------------
    //
    // `Ended` is the state a boolean cannot hold: not moving, and not held
    // either — there is nothing left to resume into. Every button goes
    // unavailable, because none of them has anywhere to move the playhead.
    await setReplay(page, {
      state: "ended",
      recordsPlayed: 91_244,
      slot: 313_000_000,
    });

    const ended = await page.evaluate(() => ({
      buttons: [...document.querySelectorAll(".transport .chip")].map((b) => ({
          action: b.dataset.transport,
          label: b.textContent.trim(),
          disabled: b.disabled,
          pressed: b.getAttribute("aria-pressed"),
          title: b.title,
        })),
      barHidden: document.querySelector('[data-region="replay-bar"]').hidden,
      checked: document.querySelector('[data-action="replay-toggle"]').getAttribute("aria-checked"),
    }));

    t.every(
      "at the end of a fixture every transport button is unavailable",
      ended.buttons,
      (button) => button.disabled === true,
      (button) => button.action,
    );
    t.every(
      "and each says why",
      ended.buttons,
      (button) => button.title.includes("last record"),
      (button) => `${button.action}: ${button.title}`,
    );
    t.eq(
      "an ended fixture is still not live, so the bar stays up",
      ended.barHidden,
      false,
    );
    t.eq("and the switch still reads on", ended.checked, "true");

    // --- the live-feed refusal ---------------------------------------------
    //
    // `refuse_over_a_live_feed` is what makes the bar's claim true. Pause and
    // stop are not gated on it — gating them would let a connected feed trap a
    // session in replay, which is the failure backwards — and everything else
    // is.
    await setReplay(page, { state: "playing", recordsPlayed: 4_812, slot: 312_905_118 });
    await page.evaluate(() => {
      window.__STS_TEST__.reset();
      window.__STS_TEST__.feedIsLive = true;
    });
    await press(page, "step");

    const refused = await page.evaluate(() => ({
      state: window.__STS_TEST__.replay.state,
      played: window.__STS_TEST__.replay.recordsPlayed,
      // The window re-reads the status after a refusal rather than assuming
      // the press worked.
      reread: window.__STS_TEST__.invocations.filter((c) => c.command === "get_replay_status")
        .length,
      pressed: document
        .querySelector('.transport .chip[data-transport="play"]')
        .getAttribute("aria-pressed"),
    }));

    t.eq("a refused step does not move the playhead", refused.played, 4_812);
    t.eq("and does not change the state", refused.state, "playing");
    t.ok(
      "the window re-reads the engine rather than believing the press",
      refused.reread >= 1,
      `${refused.reread} status reads after the refusal`,
    );
    t.eq("and the buttons still show what the engine says", refused.pressed, "true");

    // Pause is not gated, so it still works over a live feed.
    await page.evaluate(() => window.__STS_TEST__.reset());
    await press(page, "pause");
    const pausedOverLiveFeed = await page.evaluate(() => window.__STS_TEST__.replay.state);
    t.eq(
      "pause is not refused over a live feed",
      pausedOverLiveFeed,
      "paused",
      "gating the hold would let a connected feed trap a session in replay",
    );

    await page.evaluate(() => {
      window.__STS_TEST__.feedIsLive = false;
    });

    // --- drawn from the engine, not from the press --------------------------
    //
    // The engine holds a playhead nobody asked to hold — a breaker tripped, a
    // fixture ran out of budget — and the transport has to follow it.
    await setReplay(page, { state: "playing" });
    await page.evaluate(() =>
      window.__STS_TEST__.pushReplay({
        ...window.__STS_TEST__.replay,
        state: "paused",
        active: true,
        revision: (window.__STS_TEST__.replay.revision ?? 0) + 1,
      }),
    );
    await page.settle();

    const told = await page.evaluate(() => [...document.querySelectorAll(".transport .chip")].map((b) => ({
          action: b.dataset.transport,
          label: b.textContent.trim(),
          disabled: b.disabled,
          pressed: b.getAttribute("aria-pressed"),
          title: b.title,
        })));
    t.eq(
      "a hold the window was told about lights pause",
      told.find((b) => b.action === "pause").pressed,
      "true",
      "the operator pressed nothing; the engine said it held",
    );

    // --- a build with a replay bar and no transport --------------------------
    await page.goto(`${page.origin}?replay=1&transport=0`);
    await page.settle();
    await page.evaluate(() => document.querySelector('[data-action="replay-toggle"]').click());
    await page.settle();
    await page.evaluate(
      () => new Promise((resolve) => setTimeout(resolve, 60)),
    );
    await page.evaluate(() =>
      document.querySelector('.transport .chip[data-transport="pause"]').click(),
    );
    await page.evaluate(() => new Promise((resolve) => setTimeout(resolve, 80)));
    await page.settle();

    const without = await page.evaluate(() => {
      const test = window.__STS_TEST__;
      const asked = test.invocations.filter((c) => c.command === "set_replay_transport").length;
      document.querySelector('.transport .chip[data-transport="play"]')?.click();
      return {
        asked,
        buttons: [...document.querySelectorAll(".transport .chip")].map((b) => ({
          action: b.dataset.transport,
          label: b.textContent.trim(),
          disabled: b.disabled,
          pressed: b.getAttribute("aria-pressed"),
          title: b.title,
        })),
        barHidden: document.querySelector('[data-region="replay-bar"]').hidden,
        checked: document
          .querySelector('[data-action="replay-toggle"]')
          .getAttribute("aria-checked"),
        speedsDisabled: [...document.querySelectorAll(".speeds .chip")].every((c) => c.disabled),
      };
    });

    t.ok(
      "a build with no transport is asked at most once",
      without.asked <= 1,
      `${without.asked} calls`,
    );
    t.every(
      "and every transport button goes unavailable",
      without.buttons,
      (button) => button.disabled === true,
      (button) => button.action,
    );
    t.every(
      "and each says why in the words of the missing command",
      without.buttons,
      (button) => button.title.includes("set_replay_transport"),
      (button) => `${button.action}: ${button.title}`,
    );
    t.eq("the fixture it cannot steer is still playing", without.barHidden, false);
    t.eq("and the switch still says so", without.checked, "true");
    t.ok(
      "and the controls that do exist on that build still work",
      without.speedsDisabled === false,
      "a missing transport disabled the speed chips",
    );
  },
};
