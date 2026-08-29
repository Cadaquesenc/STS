// The two commands the window reads the engine's own state through, and the one
// it writes a number back with.
//
// `get_metrics` fills the right-hand end of the status bar: the queue band, how
// long a tick takes, and what is on the network. `set_sol_price` is the only
// control in the window that sends a number into the engine.
//
// The assertions that matter most here are the negative ones. A queue that has
// not been reported and a queue that is empty are opposite facts and they look
// identical if the window renders a missing field as a zero, so most of what is
// checked below is that it does not.

import { fieldText } from "../seed.mjs";

/// How long to wait for the slow poll to come round again.
///
/// `STATUS_POLL_MS` in app.js is one second, and this has to be longer than one
/// full round trip past it or the assertion races the thing it is measuring.
const AFTER_A_POLL = 1300;

/// Reads the queue cell, its dot, and the words behind the colour.
function readQueue(page) {
  return page.evaluate(() => {
    const value = document.querySelector('[data-field="backpressure"]');
    const tick = value?.closest(".tick");
    const dot = tick?.querySelector(".dot");
    return {
      text: value?.textContent?.trim() ?? null,
      spoken:
        document.querySelector('[data-field="backpressure-state"]')?.textContent?.trim() ?? null,
      dot: dot ? [...dot.classList].filter((c) => c.startsWith("is-")).join(" ") : null,
      title: tick?.getAttribute("title") ?? null,
    };
  });
}

/// Puts a value in the price field the way a person does, and reports what the
/// window sent and what it drew afterwards.
function enterPrice(page, text) {
  return page.evaluate(async (value) => {
    const test = window.__STS_TEST__;
    const before = test.invocations.filter((i) => i.command === "set_sol_price").length;
    const input = document.querySelector('[data-action="sol-price"]');

    input.value = value;
    // `change` is what the window listens for: it is the event a text field
    // fires on Enter and on blur, which are the two moments somebody means it.
    input.dispatchEvent(new Event("change", { bubbles: true }));
    await new Promise((resolve) => setTimeout(resolve, 60));

    return {
      sent: test.invocations
        .filter((i) => i.command === "set_sol_price")
        .slice(before)
        .map((i) => i.payload),
      invalid: input.getAttribute("aria-invalid"),
      state: document.querySelector('[data-field="sol-price-stat"]')?.getAttribute("data-state"),
      shown: input.value,
    };
  }, text);
}

export default {
  name: "metrics",
  async run(t, page) {
    // --- the queue, as the engine reported it -------------------------------
    const asked = await page.evaluate(() =>
      window.__STS_TEST__.invocations.filter((i) => i.command === "get_metrics").length,
    );
    t.ok("the window asks the engine for its metrics", asked >= 1, `${asked} calls`);

    const nominal = await readQueue(page);
    t.eq("the queue reads its band and its fill", nominal.text, "nominal 4%");
    t.eq("a queue that is keeping up is live green", nominal.dot, "is-live");
    t.eq(
      "and the colour is also said in words",
      nominal.spoken,
      "queue nominal, 4% of capacity",
    );
    t.ok(
      "the depth and the capacity are both on the tooltip",
      /41 of 1,024 frames queued/.test(nominal.title ?? ""),
      nominal.title,
    );

    // --- how long a tick takes ---------------------------------------------
    t.eq("the tick percentile is milliseconds, not micros", await fieldText(page, "tick-p50"), "0.54ms");
    const tail = await page.evaluate(
      () => document.querySelector('[data-field="tick-p50"]')?.closest(".tick")?.title ?? "",
    );
    t.ok(
      "and the tail is a tooltip away, because one number cannot carry both",
      /p95 1.90ms · p99 4.20ms/.test(tail),
      tail,
    );

    // --- what is on the network --------------------------------------------
    // Intents and exits are counted apart on purpose: an intent in flight is
    // money about to be at risk and an exit in flight is money that is not
    // closed yet, and one number covering both says neither.
    t.eq("intents and exits are counted apart", await fieldText(page, "in-flight"), "0/0");

    // --- the bands ----------------------------------------------------------
    for (const [state, fill, dot] of [
      ["elevated", 62, "is-warn"],
      ["saturated", 96, "is-halt"],
    ]) {
      await page.evaluate(
        (patch) => Object.assign(window.__STS_TEST__.metrics.feed, patch),
        { state, fillPercent: fill },
      );
      await new Promise((resolve) => setTimeout(resolve, AFTER_A_POLL));
      const band = await readQueue(page);
      const a = /^[aeiou]/.test(state) ? "an" : "a";
      t.eq(`${a} ${state} queue says so`, band.text, `${state} ${fill}%`);
      t.eq(`and ${a} ${state} queue is ${dot.slice(3)}`, band.dot, dot);
    }

    // --- a number the engine did not send -----------------------------------
    // The whole point of the em dash. A queue nobody has reported and a queue
    // that is empty are opposite facts, and a rendered zero makes them look the
    // same to whoever is deciding whether to trust the panes above.
    await page.evaluate(() => {
      window.__STS_TEST__.metrics.feed.state = null;
      window.__STS_TEST__.metrics.feed.fillPercent = null;
      window.__STS_TEST__.metrics.slots.processingUs.p50Us = null;
      window.__STS_TEST__.metrics.execution.inFlightIntents = null;
    });
    await new Promise((resolve) => setTimeout(resolve, AFTER_A_POLL));

    const missing = await readQueue(page);
    t.eq("a queue the engine did not report is an em dash, not a zero", missing.text, "—");
    t.eq("and it carries no band colour at all", missing.dot, "");
    t.eq("and says as much in words", missing.spoken, "queue state unknown");
    t.eq("an untimed tick is an em dash", await fieldText(page, "tick-p50"), "—");
    t.eq("and so is an unreported execution state", await fieldText(page, "in-flight"), "—");

    // --- what SOL is worth --------------------------------------------------
    // It starts unset, and unset is drawn as a warning rather than as a blank.
    // Until it is set the engine cannot compare a lamport number against a
    // dollar threshold, so every candidate reads as too small to trade — and an
    // engine refusing everything looks exactly like a quiet market from here.
    const start = await page.evaluate(() => {
      const input = document.querySelector('[data-action="sol-price"]');
      return {
        state: document.querySelector('[data-field="sol-price-stat"]')?.getAttribute("data-state"),
        placeholder: input?.getAttribute("placeholder"),
        value: input?.value,
        title: input?.getAttribute("title") ?? "",
      };
    });
    t.eq("the price starts unset", start.state, "unset");
    t.eq("and the field says so rather than showing a guess", start.placeholder, "unset");
    t.eq("and holds no number at all", start.value, "");
    t.ok(
      "and says what an unset price costs the operator",
      /too small to trade/.test(start.title),
      start.title,
    );

    // Refused here rather than sent, because "that is not a price" is this
    // window's sentence and the engine's refusal is a different one.
    for (const bad of ["abc", "0", "-5"]) {
      const result = await enterPrice(page, bad);
      t.eq(`${JSON.stringify(bad)} is not sent to the engine`, result.sent.length, 0);
      t.eq(`and ${JSON.stringify(bad)} marks the field invalid`, result.invalid, "true");
      t.eq(`and ${JSON.stringify(bad)} does not claim a price is set`, result.state, "unset");
    }

    // An engine that says no. The field must not claim a price the engine never
    // took, for the same reason the replay switch is drawn from the engine's
    // answer rather than from the click.
    await page.evaluate(() => {
      const original = window.__TAURI_INTERNALS__.invoke;
      window.__STS_TEST__.refusedPrices = 0;
      window.__STS_TEST__.restoreInvoke = () => {
        window.__TAURI_INTERNALS__.invoke = original;
      };
      window.__TAURI_INTERNALS__.invoke = (command, payload) => {
        if (command === "set_sol_price") {
          // Counted here rather than read back off `invocations`. This stands
          // in front of the fake engine, so a call it refuses never reaches the
          // recorder behind it and the log would show the send as never made.
          window.__STS_TEST__.refusedPrices += 1;
          return Promise.reject(new Error("the engine is not taking a price right now"));
        }
        return original(command, payload);
      };
    });
    const refused = await enterPrice(page, "142.50");
    const attempted = await page.evaluate(() => {
      window.__STS_TEST__.restoreInvoke();
      return window.__STS_TEST__.refusedPrices;
    });
    t.eq("a refused price still reached the engine", attempted, 1);
    t.eq("but the window does not claim it was set", refused.state, "unset");
    t.eq("and the field is marked invalid", refused.invalid, "true");

    // --- and a price it takes ----------------------------------------------
    const set = await enterPrice(page, "$142.50");
    t.eq("a price is sent in whole cents", set.sent[0]?.centsPerSol, 14_250);
    t.eq("a dollar sign is stripped rather than refused", set.state, "set");
    t.eq("and the field is redrawn from what came back", set.shown, "142.50");
    t.eq("and it is no longer marked invalid", set.invalid, "false");

    // Rounded rather than truncated: 142.999 is 143.00 to whoever typed it.
    const rounded = await enterPrice(page, "142.999");
    t.eq("a fractional cent rounds rather than truncating", rounded.sent[0]?.centsPerSol, 14_300);

    // A bad value after a good one must not roll the good one back.
    const afterBad = await enterPrice(page, "nonsense");
    t.eq("a later bad entry is not sent", afterBad.sent.length, 0);
    t.eq("and leaves the price the engine took standing", afterBad.state, "set");

    // --- a build with no metrics command ------------------------------------
    // The window asks once, is told there is no such command, and leaves the
    // cells reading unknown instead of asking again once a second forever.
    await page.goto(`${page.origin}?metrics=0`);
    await page.settle();
    await new Promise((resolve) => setTimeout(resolve, AFTER_A_POLL));

    const without = await page.evaluate(() => ({
      asked: window.__STS_TEST__.invocations.filter((i) => i.command === "get_metrics").length,
      queue: document.querySelector('[data-field="backpressure"]')?.textContent?.trim(),
      tick: document.querySelector('[data-field="tick-p50"]')?.textContent?.trim(),
    }));
    t.eq("a build without the command is asked exactly once", without.asked, 1);
    t.eq("and the queue reads unknown rather than empty", without.queue, "—");
    t.eq("and the tick percentile stays an em dash", without.tick, "—");
  },
};
