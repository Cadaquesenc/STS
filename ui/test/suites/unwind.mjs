// The emergency unwind confirmation.
//
// Every assertion here is about one question: after the operator has pressed
// the button, does the window describe what is actually on chain? The receipt
// answers that at two levels and only one of them is usable — `exitsSent`,
// `exitsAlreadyOut` and `exitsConfirmed` are counts over the whole call, and
// `stranded[].exit.onNetwork` is the fate of one position — so the cases below
// are the ones where those two disagree.
//
// The case that matters most is the second press. The first press sends an
// exit; the second finds it rather than sending it, so `exitsSent` comes back
// zero while a transaction of the operator's is still in the air. A window that
// reads the count alone tells them nothing was ever sold at the exact moment
// selling it again by hand would open a short.

const INTENT = "alpha";

/// Presses flatten, confirms, and hands back everything the window then says.
async function unwind(page) {
  return page.evaluate(async () => {
    document.querySelector('[data-action="unwind"]').click();
    await new Promise((resolve) => setTimeout(resolve, 20));
    document.querySelector('[data-action="unwind-confirm"]').click();
    await new Promise((resolve) => setTimeout(resolve, 80));

    const rows = [...document.querySelectorAll('[data-region="unwind-stranded"] .row')];
    const note = document.querySelector('[data-field="unwind-inflight"]');
    const action = document.querySelector('[data-action="unwind"]');
    return {
      summary: document.querySelector('[data-field="unwind-result-summary"]').textContent.trim(),
      noteHidden: note.hidden,
      note: note.textContent.trim(),
      error: document.querySelector('[data-field="unwind-error"]').textContent.trim(),
      rows: rows.map((row) => ({
        exit: row.dataset.exit,
        last: row.lastElementChild.textContent.trim(),
        title: row.title,
      })),
      banner: document.querySelector('[data-field="unwind-text"]').textContent.trim(),
      actionDisabled: action.disabled,
      actionTitle: action.title,
      open: document.querySelector('[data-field="open-positions"]').textContent.trim(),
    };
  });
}

/// Closes the result and puts the obligation back in play.
///
/// `resolved: false` is the engine's own way of saying an obligation it was
/// asked about is still open, and it is the path by which a second press
/// happens at all — the window will not offer one it believes it has already
/// acted on.
async function reopen(page, intentIds) {
  await page.evaluate((ids) => {
    document.querySelector('[data-action="unwind-done"]').click();
    for (const id of ids) window.__STS_TEST__.pushUnwind({ intentId: id, resolved: false });
  }, intentIds);
  await page.settle();
}

/// Replaces what the engine answers, starting from the shipped build's answer.
async function answerWith(page, shape) {
  await page.evaluate((source) => {
    const test = window.__STS_TEST__;
    const shapeIt = new Function(`return (${source})`)();
    test.unwindReceipt = (payload) => shapeIt(test.baseReceipt(payload), test);
  }, shape.toString());
}

export default {
  name: "unwind",
  async run(t, page) {
    await page.evaluate(() => {
      window.__STS_TEST__.baseReceipt = window.__STS_TEST__.unwindReceipt;
    });

    // --- an execution is abandoned with money still out ---------------------
    await page.evaluate(() => window.__STS_TEST__.pushExecution({}));
    await page.settle();

    const raised = await page.evaluate(() => ({
      open: document.querySelector(".unwind").dataset.open,
      text: document.querySelector('[data-field="unwind-text"]').textContent.trim(),
      disabled: document.querySelector('[data-action="unwind"]').disabled,
      counted: document.querySelector('[data-field="open-positions"]').textContent.trim(),
    }));
    t.eq("an abandoned position raises the banner", raised.open, "true");
    t.eq("which says what is out there", raised.text, "1 position left on chain");
    t.ok("and the control is offered", !raised.disabled);
    t.eq("and an orphan counts against the position limit", raised.counted, "1 open");

    // --- the shipped build: no signer, so nothing could have been sold ------
    const nothing = await unwind(page);
    t.ok(
      "with no execution backend the modal says nothing was sold",
      nothing.summary.includes("Nothing was sold — there is no send path in this build"),
      nothing.summary,
    );
    t.ok("and there is no transaction to warn about", nothing.noteHidden);
    t.eq(
      "the row's last column is the state the money was left at risk in",
      nothing.rows[0].last,
      "confirmed",
    );
    t.eq("and the row is not marked as having an exit out", nothing.rows[0].exit, "none");
    t.eq(
      "the banner says the same thing the modal did",
      nothing.banner,
      "1 position still on chain — engine halted, nothing sold, flatten by hand",
    );
    t.ok(
      "and the control is closed off with that reason",
      nothing.actionTitle.includes("Nothing was sold"),
      nothing.actionTitle,
    );

    // --- the second press, over an exit that is still flying ----------------
    // `exitsSent` is zero because this press sent nothing; `exitsAlreadyOut` is
    // one because the previous press's transaction is still on the network.
    await reopen(page, [INTENT]);
    await answerWith(page, (receipt, test) => {
      receipt.exitsSent = 0;
      receipt.exitsAlreadyOut = 1;
      receipt.exitsInFlight = 1;
      receipt.signer = "mock-solana-signer";
      receipt.stranded = receipt.stranded.map((position) =>
        test.stranded(position.intentId, test.exitInFlight(position.intentId)),
      );
      return receipt;
    });

    const flying = await unwind(page);
    t.ok(
      "a second press over a flying exit does not claim there is no send path",
      !flying.summary.includes("no send path in this build"),
      flying.summary,
    );
    t.ok(
      "it says a transaction is on the network",
      flying.summary.includes("an exit is on the network for it"),
      flying.summary,
    );
    t.ok(
      "and that this press is not what put it there",
      flying.summary.includes("sent by an earlier press"),
      flying.summary,
    );
    t.ok(
      "and that a transaction out is not a position closed",
      flying.summary.includes("Nothing here is closed until it confirms"),
      flying.summary,
    );
    t.eq("the row reads as an exit in flight", flying.rows[0].last, "exit in flight");
    t.eq("and is marked as one", flying.rows[0].exit, "onNetwork");
    t.ok(
      "the exit's own sentence is on the row",
      flying.rows[0].title.includes("an exit is on the network and has not confirmed"),
      flying.rows[0].title,
    );
    t.ok(
      "with the exit signature somebody has to follow",
      flying.rows[0].title.includes("exit-sig-alpha"),
      flying.rows[0].title,
    );
    t.ok(
      "and the state the money was left at risk in, which the column no longer shows",
      flying.rows[0].title.includes("at risk in confirmed"),
      flying.rows[0].title,
    );
    t.eq("the note against selling it again is up", flying.noteHidden, false);
    t.ok(
      "and says what selling it again would do",
      flying.note.includes("opens a short if it lands"),
      flying.note,
    );
    t.eq(
      "the banner stops saying nothing was sold",
      flying.banner,
      "1 position — exit on the network, not confirmed",
    );
    t.ok(
      "and the control says to follow the signature instead of pressing again",
      flying.actionTitle.includes("follow the signature"),
      flying.actionTitle,
    );
    t.ok("and is closed off", flying.actionDisabled);

    // --- one flying, one with nothing out ----------------------------------
    await reopen(page, [INTENT]);
    await page.evaluate(() =>
      window.__STS_TEST__.pushExecution({ intentId: "beta", seq: 7, signature: "sig-beta" }),
    );
    await page.settle();
    await answerWith(page, (receipt, test) => {
      receipt.exitsSent = 1;
      receipt.exitsInFlight = 1;
      receipt.exitsFailed = 1;
      receipt.signer = "mock-solana-signer";
      receipt.stranded = receipt.stranded.map((position) =>
        test.stranded(
          position.intentId,
          position.intentId === "alpha"
            ? test.exitInFlight(position.intentId)
            : test.exitFailed(position.intentId),
        ),
      );
      return receipt;
    });

    const mixed = await unwind(page);
    t.ok(
      "a mixed result is described as both halves rather than as the worse one",
      mixed.summary ===
        "The engine is halted. 1 of 2 has an exit on the network and is not closed until it " +
          "confirms; the other 1 has nothing out and has to be closed by hand.",
      mixed.summary,
    );
    t.eq(
      "exactly one row is marked as flying",
      mixed.rows.filter((row) => row.exit === "onNetwork").length,
      1,
    );
    t.eq(
      "and the other carries the reason its exit did not go",
      mixed.rows.filter((row) => row.title.includes("the curve is depleted")).length,
      1,
    );
    t.eq(
      "the banner counts the two separately",
      mixed.banner,
      "2 positions still on chain — 1 with an exit out, 1 with nothing sold",
    );

    // --- a signer that was there and could not sell -------------------------
    // "Nothing was sold" is true here too, and saying it the same way would
    // file a broken signer under a note about what this build does not have.
    await page.evaluate(() => window.__STS_TEST__.pushUnwind({ intentId: "beta", resolved: true }));
    await reopen(page, [INTENT]);
    await answerWith(page, (receipt, test) => {
      receipt.exitsFailed = 1;
      receipt.signer = "mock-solana-signer";
      receipt.stranded = receipt.stranded.map((position) =>
        test.stranded(position.intentId, test.exitFailed(position.intentId)),
      );
      return receipt;
    });

    const refused = await unwind(page);
    t.ok(
      "a signer that failed is not reported as a build with no signer",
      !refused.summary.includes("no send path in this build"),
      refused.summary,
    );
    t.ok(
      "the backend that could not sell is named",
      refused.summary.includes("mock-solana-signer sold none of it"),
      refused.summary,
    );
    t.eq("there is nothing in the air to warn about", refused.noteHidden, true);
    t.eq("and the row is marked as a failed attempt", refused.rows[0].exit, "failed");

    // --- everything sold, landed and booked ---------------------------------
    await reopen(page, [INTENT]);
    await answerWith(page, (receipt) => {
      receipt.exitsSent = 1;
      receipt.exitsConfirmed = 1;
      receipt.signer = "mock-solana-signer";
      receipt.flattened = receipt.stranded.map((position) => ({
        intentId: position.intentId,
        exitIntentId: "exit-" + position.intentId,
        mint: position.mint,
        venue: "pumpFunCurve",
        signature: "exit-sig-" + position.intentId,
        tokens: 1_000_000,
        costBasisLamports: position.sizeLamports,
        outLamports: 240_000_000,
        realizedPnlLamports: -10_000_000,
        mode: "paper",
      }));
      receipt.stranded = [];
      return receipt;
    });

    const closed = await unwind(page);
    t.eq(
      "a position the engine left out of the stranded list is the engine saying it is closed",
      closed.summary,
      "The engine is halted. 1 position sold, landed and booked; nothing was left on chain.",
    );
    t.eq("there is nothing left to list", closed.rows.length, 0);
    t.eq("the banner clears", closed.banner, "no positions awaiting unwind");
    t.eq("and the position stops counting against the limit", closed.open, "0 open");
    t.eq("nothing failed on the way", closed.error, "");
  },
};
