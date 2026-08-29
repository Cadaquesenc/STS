// The wallet cluster pane, and whether it can be told what it is looking at.
//
// Everything here is one question asked four ways: **can an operator tell the
// difference between "clean", "unknown" and "nobody looked"?** The pane renders
// numbers that decide whether a launch is somebody's twelfth keypair, and all
// three of those states produce a screen with no red on it. A window that
// rendered them identically would be at its most convincing exactly when it
// knew least.

import { LAMPORTS, observation, push, account, fieldText } from "../seed.mjs";

/// One FundingCluster, in the camelCase `clustering.rs` serialises to.
function cluster(overrides = {}) {
  return Object.assign(
    {
      root: "operator11111111111111111111111111111111111",
      rootKind: "WALLET",
      sharedHub: false,
      wallets: ["puppet0", "puppet1", "puppet2"],
      walletCount: 3,
      buyVolumeLamports: 18 * LAMPORTS,
      sellVolumeLamports: 0,
      flowShareBps: 6_200,
      tokenBalance: 30_000_000_000_000,
      ownershipBps: 900,
      holdingHhiBps: 3_400,
      holdingEntropyMicros: 940_000,
      costumeRing: false,
      fundingRing: {
        matchedWallets: 3,
        clusterWallets: 3,
        shareBps: 10_000,
        medianFundingLamports: 3 * LAMPORTS,
        uniformityMicros: 990_000,
      },
      syncMicros: 880_000,
      fundMicros: 970_000,
      launchShareMicros: 620_000,
      temporalInfluenceMicros: 924_000,
      firstBuyMs: 1_700_000_000_000,
      firstBuySpanMs: 1_200,
      preMigrationBuyLamports: 18 * LAMPORTS,
      preMigrationWallets: 3,
      memberFundingLamports: [3 * LAMPORTS, 3 * LAMPORTS, 3 * LAMPORTS],
      truncated: false,
    },
    overrides,
  );
}

/// One ClusterGraphReport, in the same shape.
function report(overrides = {}) {
  return Object.assign(
    {
      schema: "sts.clustering.report.v1",
      mint: account(0),
      launchMs: 1_700_000_000_000,
      migrationMs: 1_700_000_030_000,
      policyVersion: 2,
      graph: {
        nodes: 26,
        edges: 31,
        inferredRouters: [],
        absorbingNodes: 1,
        selfLoopsDropped: 0,
        duplicatesDropped: 0,
      },
      participants: 15,
      buyVolumeLamports: 40 * LAMPORTS,
      attributedVolumeLamports: 34 * LAMPORTS,
      unattributedVolumeLamports: 6 * LAMPORTS,
      clusters: [cluster()],
      unclusteredWallets: 2,
      clustersBelowFloor: 1,
      launchFundMicros: 610_000,
      dev: null,
      insider: null,
      truncated: false,
      proof: null,
    },
    overrides,
  );
}

/// A LineageProof over `claimed` edges with the given verdict split.
function proof({ claimed, confirmed = 0, singleSource = 0, unverified = 0, contradicted = 0 }) {
  return {
    schema: "sts.chainproof.lineage.v1",
    policy: {
      version: 1,
      quorum: 2,
      slotTolerance: 0,
      timeToleranceMs: 1_000,
      amountTolerance: 0,
      unverifiedConfidenceBps: 5_000,
    },
    edges: [],
    claimed,
    confirmed,
    singleSource,
    unverified,
    contradicted,
    unclaimedAttestations: 0,
    complete: claimed > 0 && confirmed === claimed,
  };
}

/// One DevTrace, in the same shape.
///
/// The default is the ordinary case the pane has to render without drama: a
/// deployer two hops off a person, linked to nobody in particular.
function devTrace(overrides = {}) {
  return Object.assign(
    {
      wallet: "deployer111111111111111111111111111111111",
      trace: {
        wallet: "deployer111111111111111111111111111111111",
        truncated: false,
        parent: "operator11111111111111111111111111111111111",
        parentPosteriorMicros: 810_000,
      },
      origin: "operator11111111111111111111111111111111111",
      originKind: "WALLET",
      hops: 2,
      exitNode: null,
      siblings: [],
      siblingBuyLamports: 0,
      fundedBuyers: [],
      fundedBuyLamports: 0,
      clusterRoot: null,
      fundsCluster: false,
    },
    overrides,
  );
}

/// One InsiderFinding, in the same shape.
function insider(overrides = {}) {
  return Object.assign(
    {
      root: "operator11111111111111111111111111111111111",
      wallets: 3,
      scoreMicros: 842_000,
      measuredWeightBps: 10_000,
      components: {
        syncMicros: 880_000,
        launchShareMicros: 620_000,
        ownershipMicros: 90_000,
        uniformityMicros: 990_000,
      },
      reasons: ["SHARED_FUNDER", "SYNCHRONISED_OPEN"],
      preMigrationShareBps: 10_000,
      preMigrationBuyLamports: 18 * LAMPORTS,
      truncated: false,
    },
    overrides,
  );
}

/// Files a report under the first candidate's account and selects it.
async function show(page, stored) {
  await page.evaluate((value) => {
    const test = window.__STS_TEST__;
    test.clusterReports.clear();
    if (value) test.clusterReports.set(value.mint, value);
  }, stored ?? null);

  await page.evaluate(() => {
    const row = document.querySelector('[data-region="radar-rows"] .row:not([hidden])');
    row?.click();
  });
  await page.settle();
}

const badge = (page) =>
  page.evaluate(() => {
    const el = document.querySelector('[data-field="cluster-evidence"]');
    return { risk: el.dataset.risk, text: el.textContent.trim(), title: el.title };
  });

const rows = (page) =>
  page.evaluate(() =>
    [...document.querySelectorAll('[data-region="cluster-rows"] .cluster-grid')].map((row) => ({
      cells: [...row.children].map((cell) => cell.textContent.trim()),
      title: row.title,
    })),
  );

const strip = (page) =>
  page.evaluate(() =>
    Object.fromEntries(
      ["cluster-creator", "cluster-dev-origin", "cluster-insider", "cluster-dev-claim"].map(
        (name) => {
          const el = document.querySelector(`[data-field="${name}"]`);
          return [
            name,
            {
              text: el.textContent.trim(),
              title: el.title,
              faint: el.classList.contains("faint"),
            },
          ];
        },
      ),
    ),
  );

const emptyState = (page) =>
  page.evaluate(() => {
    const el = document.querySelector('[data-region="cluster-empty"]');
    return {
      hidden: el.hidden,
      title: el.querySelector(".empty-title").textContent.trim(),
      note: el.querySelector(".empty-note").textContent.trim(),
    };
  });

export default {
  name: "cluster",
  async run(t, page) {
    // --- nothing selected ---------------------------------------------------
    const blank = await page.evaluate(() =>
      ["cluster-hhi", "cluster-temporal", "cluster-entropy", "cluster-separation"].map((name) =>
        document.querySelector(`[data-field="${name}"]`).textContent.trim(),
      ),
    );
    t.every(
      "every cluster score is an em dash before anything is selected",
      blank,
      (text) => text === "—",
      (text) => JSON.stringify(text),
    );
    t.eq("and the evidence badge does not know either", (await badge(page)).risk, "unknown");

    await push(page, [observation({ index: 0, slot: 312_905_100, realSol: 40 * LAMPORTS, mcap: 70 * LAMPORTS })]);

    // The layout-shift counter starts here, the way `layout.mjs` starts its
    // own. Boot moves the window once — the first bundle-telemetry poll lands
    // around 55ms and widens the deck cells under the top bar — and a CLS
    // number that included it would be a claim about startup wearing the label
    // of a claim about this pane. It also made the assertion intermittent,
    // firing on about one run in six depending on whether the poll beat the
    // first push, which is worse than either answer.
    await page.evaluate(() => window.__STS_TEST__.reset());

    // --- selected, and nobody has looked ------------------------------------
    await show(page, null);
    const unanalysed = await emptyState(page);
    t.eq("a selected subject with no report says it was not analysed", unanalysed.title, "not analysed");
    t.ok(
      "and says so in the words that distinguish it from a clean result",
      unanalysed.note.includes("no forensic report has been recorded"),
      unanalysed.note,
    );
    t.eq("the rows region is empty", (await rows(page)).length, 0);
    t.every(
      "and every score is still an em dash rather than a zero",
      await page.evaluate(() =>
        ["cluster-hhi", "cluster-temporal", "cluster-entropy", "cluster-separation"].map((name) =>
          document.querySelector(`[data-field="${name}"]`).textContent.trim(),
        ),
      ),
      (text) => text === "—",
      (text) => JSON.stringify(text),
    );

    // --- a report, with nothing verified under it ---------------------------
    await show(page, report());
    t.eq("the loudest cluster's holding concentration is rendered", await fieldText(page, "cluster-hhi"), "34");
    t.eq("its sybil reading is rendered as a percentage", await fieldText(page, "cluster-temporal"), "92.4");
    t.eq(
      "the funding-side entropy is the uniformity, not the holdings entropy",
      await fieldText(page, "cluster-entropy"),
      "99.0",
    );
    t.eq(
      "and separation stays UNKNOWN, because nothing in this build computes it",
      await fieldText(page, "cluster-separation"),
      "—",
    );

    const listed = await rows(page);
    t.eq("one row per hand", listed.length, 1);
    t.eq("the row leads with the origin that paid for the group", listed[0].cells[0], "oper…1111");
    t.eq("and its share of the launch's buying", listed[0].cells[1], "62");
    t.ok("the members are on the tooltip", listed[0].title.includes("3 wallets"), listed[0].title);

    const unverified = await badge(page);
    t.eq("with no witness the badge reads unverified", unverified.text, "unverified");
    t.eq("and carries no clean band", unverified.risk, "unknown");
    t.ok(
      "the tooltip refuses to let that read as a pass",
      unverified.title.includes("not a pass"),
      unverified.title,
    );

    // --- one provider is not a quorum ---------------------------------------
    await show(page, report({ proof: proof({ claimed: 31, singleSource: 31 }) }));
    const partial = await badge(page);
    t.eq("31 edges nobody corroborated read as unconfirmed", partial.text, "31 unconfirmed");
    t.eq("and the band is neither clean nor alarming", partial.risk, "unconfirmed");
    t.ok(
      "the tooltip states the asymmetry rather than assuming it is known",
      partial.title.includes("may never clear one"),
      partial.title,
    );

    // --- the chain contradicts the request ----------------------------------
    await show(page, report({ proof: proof({ claimed: 31, confirmed: 29, contradicted: 2 }) }));
    const contradicted = await badge(page);
    t.eq("a contradicted edge is what the badge leads with", contradicted.text, "2 contradicted");
    t.eq("and it is the loud band", contradicted.risk, "contradicted");
    t.ok(
      "the tooltip says the numbers above are over what survived",
      contradicted.title.includes("dropped before the graph was built"),
      contradicted.title,
    );

    // --- every edge confirmed ------------------------------------------------
    await show(page, report({ proof: proof({ claimed: 31, confirmed: 31 }) }));
    const verified = await badge(page);
    t.eq("a complete proof is the only thing that reads as verified", verified.text, "chain-verified");
    t.eq("and the only one that gets the clean band", verified.risk, "verified");
    t.ok(
      "the tooltip names what it licenses, which is narrow",
      verified.title.includes("may clear a launch"),
      verified.title,
    );

    // --- a launch that was traced and produced no cluster --------------------
    await show(page, report({ clusters: [], unclusteredWallets: 15, proof: proof({ claimed: 31, confirmed: 31 }) }));
    const nothing = await emptyState(page);
    t.eq("a traced launch with no cluster says that, not 'not analysed'", nothing.title, "no cluster resolved");
    t.ok(
      "and counts the wallets it could not place rather than dropping them",
      nothing.note.includes("15 are UNKNOWN"),
      nothing.note,
    );
    t.eq(
      "the scores go back to em dashes when there is no loudest cluster",
      await fieldText(page, "cluster-hhi"),
      "—",
    );

    // --- UNKNOWN components -------------------------------------------------
    await show(
      page,
      report({
        clusters: [cluster({ temporalInfluenceMicros: null, holdingHhiBps: null, fundingRing: null })],
      }),
    );
    t.eq("an unmeasurable sybil reading is an em dash", await fieldText(page, "cluster-temporal"), "—");
    t.eq("so is an unmeasurable concentration", await fieldText(page, "cluster-hhi"), "—");
    const unknownRow = (await rows(page))[0];
    t.eq("and the row's own cell says so too", unknownRow.cells[3], "—");
    t.ok(
      "with the reason on the tooltip, in the words the engine uses",
      unknownRow.title.includes("would read as 'these wallets are unrelated'"),
      unknownRow.title,
    );

    // --- a cluster behind an exchange ---------------------------------------
    await show(page, report({ clusters: [cluster({ sharedHub: true, rootKind: "EXCHANGE" })] }));
    t.ok(
      "a cluster rooted at an exit node says why it is not a finding",
      (await rows(page))[0].title.includes("popular exit node"),
      (await rows(page))[0].title,
    );

    // --- the creator strip ---------------------------------------------------
    //
    // The pane's empty state has promised the creator since it was written, and
    // until now the window never rendered one: `clustering.rs` computed the
    // whole `DevTrace` and it stopped at the bridge. Everything below is the
    // same question the rest of this suite asks, pointed at the deployer —
    // **can an operator tell "the creator paid for this book" from "the creator
    // came out of Coinbase" from "nobody looked"?**

    await show(page, null);
    let dev = await strip(page);
    t.eq("with no report the creator is an em dash", dev["cluster-creator"].text, "\u2014");
    t.eq(
      "and the sentence says nobody looked rather than leaving the line blank",
      dev["cluster-dev-claim"].text,
      "No analysis has been recorded for this subject.",
    );

    await show(page, report());
    dev = await strip(page);
    t.eq(
      "a report that names no creator says that, which is a different silence",
      dev["cluster-dev-claim"].text,
      "This report names no creator.",
    );
    t.ok(
      "and calls it UNKNOWN rather than clean",
      dev["cluster-dev-claim"].title.includes("not a clean result"),
      dev["cluster-dev-claim"].title,
    );
    t.eq("with no finding the insider score is an em dash", dev["cluster-insider"].text, "\u2014");
    t.ok(
      "and its tooltip refuses to let an absent finding read as a clean one",
      dev["cluster-insider"].title.includes("not a clean reading"),
      dev["cluster-insider"].title,
    );

    // --- an ordinary creator -------------------------------------------------
    await show(page, report({ dev: devTrace(), insider: insider() }));
    dev = await strip(page);
    t.eq("the creator is rendered, shortened the way every key here is", dev["cluster-creator"].text, "depl\u20261111");
    t.eq("with whoever paid it in the next cell", dev["cluster-dev-origin"].text, "oper\u20261111");
    t.eq("and the insider score as a percentage", dev["cluster-insider"].text, "84.2");
    t.ok(
      "the reasons behind that score are on its tooltip, in words rather than in enum spelling",
      dev["cluster-insider"].title.includes("one origin paid for a majority of this launch's buying"),
      dev["cluster-insider"].title,
    );
    t.eq(
      "a funder that paid no other buyer is stated as the narrow finding it is",
      dev["cluster-dev-claim"].text,
      "The creator's funder paid no other opening buyer.",
    );
    t.ok(
      "and is not allowed to read as a clearance",
      dev["cluster-dev-claim"].title.includes("does not say the launch is clean"),
      dev["cluster-dev-claim"].title,
    );
    t.eq("so it is drawn as background rather than as a finding", dev["cluster-dev-claim"].faint, true);

    // --- buyers who share the creator's funder --------------------------------
    await show(
      page,
      report({
        dev: devTrace({ siblings: ["puppet0", "puppet1"], siblingBuyLamports: 12 * LAMPORTS }),
        insider: insider(),
      }),
    );
    dev = await strip(page);
    t.eq(
      "buyers paid by whoever paid the creator are counted in the sentence",
      dev["cluster-dev-claim"].text,
      "2 opening buyers were paid by whoever paid the creator.",
    );
    t.eq("and that is drawn as a finding", dev["cluster-dev-claim"].faint, false);
    t.ok(
      "with the wallets and what they bought on the tooltip",
      dev["cluster-dev-claim"].title.includes("12.000 SOL"),
      dev["cluster-dev-claim"].title,
    );

    // --- the creator paid for the book itself ---------------------------------
    await show(
      page,
      report({
        dev: devTrace({
          fundedBuyers: ["puppet0", "puppet1", "puppet2"],
          fundedBuyLamports: 9 * LAMPORTS,
          fundsCluster: true,
          siblings: ["puppet0"],
          siblingBuyLamports: 4 * LAMPORTS,
        }),
        insider: insider({ reasons: ["DEV_FUNDED_CLUSTER", "SHARED_FUNDER"] }),
      }),
    );
    dev = await strip(page);
    t.eq(
      "a creator that funded the buyers outranks every softer reading of the same trace",
      dev["cluster-dev-claim"].text,
      "The creator paid for 3 of the opening buyers itself.",
    );
    t.ok(
      "and the tooltip keeps that apart from merely sharing an origin",
      dev["cluster-dev-claim"].title.includes("share an origin too"),
      dev["cluster-dev-claim"].title,
    );
    t.ok(
      "the strongest reason is spelled out on the score as well",
      dev["cluster-insider"].title.includes("it paid for this book"),
      dev["cluster-insider"].title,
    );

    // --- a creator out of an exchange -----------------------------------------
    //
    // The one that decides whether this strip is worth having. `DevTrace.siblings`
    // is every opening buyer whose parent is the dev's origin, and when that
    // origin is an exchange the co-customers of that exchange are exactly what
    // lands in the list — `build_dev_trace` applies no absorbing test to it, and
    // `clustering.rs` builds hub-rooted clusters too and flags them rather than
    // dropping them. So the fixture below is a shape the engine really produces,
    // and with the exit-node reading ordered after the sibling reading the strip
    // would print "3 opening buyers were paid by whoever paid the creator" about
    // a CEX hot wallet. That is the blob the whole module is built to refuse,
    // arriving through the one surface that had not been taught to refuse it.
    await show(
      page,
      report({
        dev: devTrace({
          origin: "cex11111111111111111111111111111111111111",
          originKind: "EXCHANGE",
          exitNode: "cex11111111111111111111111111111111111111",
          hops: 1,
          siblings: ["puppet0", "puppet1", "puppet2"],
          siblingBuyLamports: 18 * LAMPORTS,
          clusterRoot: "cex11111111111111111111111111111111111111",
        }),
      }),
    );
    dev = await strip(page);
    t.eq(
      "a creator out of an exchange is an exit node and never a funder",
      dev["cluster-dev-claim"].text,
      "The creator came out of an exchange.",
    );
    t.ok(
      "and the sentence says in as many words that it links the creator to nobody",
      dev["cluster-dev-claim"].title.includes("links the creator to nobody"),
      dev["cluster-dev-claim"].title,
    );
    t.eq(
      "so the origin cell does not get the styling a person's address gets",
      dev["cluster-dev-origin"].faint,
      true,
    );
    t.ok(
      "though the venue is still named, because which exchange it was is worth knowing",
      dev["cluster-dev-origin"].title.includes("An exit node"),
      dev["cluster-dev-origin"].title,
    );

    // --- an inferred router is not called an exchange -------------------------
    await show(page, report({ dev: devTrace({ originKind: "ROUTER" }) }));
    dev = await strip(page);
    t.eq(
      "a fan-out the graph inferred is described as one, not promoted to a venue",
      dev["cluster-dev-claim"].text,
      "The creator came out of an address the graph inferred was a router.",
    );

    // --- sharing an origin with the cluster -----------------------------------
    await show(
      page,
      report({
        dev: devTrace({ clusterRoot: "operator11111111111111111111111111111111111" }),
        insider: insider({ reasons: ["DEV_SHARES_ORIGIN"] }),
      }),
    );
    dev = await strip(page);
    t.eq(
      "a creator behind the cluster's own origin is named as that",
      dev["cluster-dev-claim"].text,
      "The creator shares an origin with the loudest cluster.",
    );
    t.ok(
      "and the tooltip states that sharing an origin is the weaker of the two claims",
      dev["cluster-dev-claim"].title.includes("weaker than being one"),
      dev["cluster-dev-claim"].title,
    );

    // --- a creator nobody funded ----------------------------------------------
    await show(page, report({ dev: devTrace({ origin: null, originKind: null, hops: 0 }) }));
    dev = await strip(page);
    t.eq("an origin nothing reached is an em dash", dev["cluster-dev-origin"].text, "\u2014");
    t.eq(
      "and the sentence says the traversal found nothing rather than that there was nothing",
      dev["cluster-dev-claim"].text,
      "Nobody funded the creator inside the lookback.",
    );
    t.ok(
      "in the words tracer.rs uses for it",
      dev["cluster-dev-claim"].title.includes("neither 'self-funded' nor 'clean'"),
      dev["cluster-dev-claim"].title,
    );

    // --- a score resting on half the evidence ---------------------------------
    await show(
      page,
      report({
        dev: devTrace(),
        insider: insider({
          measuredWeightBps: 6_500,
          components: {
            syncMicros: 880_000,
            launchShareMicros: 620_000,
            ownershipMicros: null,
            uniformityMicros: null,
          },
        }),
      }),
    );
    dev = await strip(page);
    t.ok(
      "a score measured over part of the evidence says how much of it",
      dev["cluster-insider"].title.includes("65% of the evidence"),
      dev["cluster-insider"].title,
    );
    t.ok(
      "and refuses the component that was left out a passing grade",
      dev["cluster-insider"].title.includes("A missing test is not a passed test"),
      dev["cluster-insider"].title,
    );

    // --- a budget-bound trail -------------------------------------------------
    await show(
      page,
      report({
        dev: devTrace({
          siblings: ["puppet0"],
          siblingBuyLamports: 4 * LAMPORTS,
          trace: { wallet: "deployer111111111111111111111111111111111", truncated: true },
        }),
        insider: insider({ truncated: true }),
      }),
    );
    dev = await strip(page);
    t.ok(
      "a trail a budget bound says its numbers are lower bounds",
      dev["cluster-dev-claim"].title.includes("lower bound"),
      dev["cluster-dev-claim"].title,
    );
    t.ok(
      "and the score says which direction that asymmetry runs in",
      dev["cluster-insider"].title.includes("may never clear one"),
      dev["cluster-insider"].title,
    );

    // --- a reason this window has not been taught -----------------------------
    //
    // The engine is ahead of the window more often than the other way round. A
    // map lookup that dropped what it did not recognise would under-report at
    // exactly the moment `clustering.rs` learned to detect something new.
    await show(page, report({ dev: devTrace(), insider: insider({ reasons: ["SOME_FUTURE_SHAPE"] }) }));
    dev = await strip(page);
    t.ok(
      "an unrecognised reason is shown as the engine spelled it rather than dropped",
      dev["cluster-insider"].title.includes("SOME_FUTURE_SHAPE"),
      dev["cluster-insider"].title,
    );

    // --- the reserve under the sentence, measured rather than eyeballed -------
    //
    // Sized by arithmetic and then checked, because the last reserve in this
    // window that was sized by looking at it came out three pixels short of the
    // longest string the code could produce. Every sentence `devClaim` can
    // return is rendered and measured against the box it was given.
    const measured = [];
    for (const stored of [
      null,
      report(),
      report({ dev: devTrace() }),
      report({ dev: devTrace({ origin: null, originKind: null, hops: 0 }) }),
      report({ dev: devTrace({ originKind: "ROUTER" }) }),
      report({ dev: devTrace({ originKind: "EXCHANGE" }) }),
      report({ dev: devTrace({ clusterRoot: "operator11111111111111111111111111111111111" }) }),
      report({ dev: devTrace({ siblings: ["puppet0", "puppet1"], siblingBuyLamports: 12 * LAMPORTS }) }),
      report({ dev: devTrace({ siblings: ["puppet0"], siblingBuyLamports: 4 * LAMPORTS }) }),
      report({
        dev: devTrace({ fundedBuyers: ["puppet0", "puppet1", "puppet2"], fundedBuyLamports: 9 * LAMPORTS }),
      }),
      report({ dev: devTrace({ fundedBuyers: ["puppet0"], fundedBuyLamports: 3 * LAMPORTS }) }),
    ]) {
      await show(page, stored);
      measured.push(
        await page.evaluate(() => {
          const el = document.querySelector('[data-field="cluster-dev-claim"]');
          return { text: el.textContent.trim(), natural: el.scrollHeight, box: el.clientHeight };
        }),
      );
    }
    t.every(
      "no sentence the strip can print is clipped by the reserve under it",
      measured,
      (m) => m.natural <= m.box,
      (m) => `${m.natural}px of text in a ${m.box}px box: ${JSON.stringify(m.text)}`,
    );

    // --- the bands are real -------------------------------------------------
    //
    // A `data-risk` value with no rule behind it falls back to the base badge
    // and renders in the inherited colour, which looks deliberate and says
    // nothing. Four states have to be four colours, or the badge is decoration.
    const bands = {};
    for (const [state, stored] of [
      ["unverified", report()],
      ["unconfirmed", report({ proof: proof({ claimed: 4, singleSource: 4 }) })],
      ["contradicted", report({ proof: proof({ claimed: 4, confirmed: 3, contradicted: 1 }) })],
      ["verified", report({ proof: proof({ claimed: 4, confirmed: 4 }) })],
    ]) {
      await show(page, stored);
      bands[state] = await page.evaluate(() => {
        const el = document.querySelector('[data-field="cluster-evidence"]');
        const style = getComputedStyle(el);
        return `${style.color} ${style.borderStyle}`;
      });
    }
    t.eq(
      "each evidence state is drawn differently from every other",
      new Set(Object.values(bands)).size,
      4,
      JSON.stringify(bands),
    );

    // --- the pane does not move, and what that does and does not prove -------
    //
    // Verified by falsification rather than by passing: with `.section-viewport`
    // changed from `height` to `min-height` this goes red at 0.0061, and names
    // the stream viewport, both tool strips and both section heads below it
    // moving down as the list grows. That is the failure it exists for.
    //
    // What it cannot see is a **cold-start** shift. `goto` is a warm reload
    // against a primed cache, so a cell that changes width the first time it
    // fills — an em dash becoming a value, a placeholder word being replaced by
    // a shorter one — races differently here than on a real first paint, and
    // may never race at all. The two `min-width` reserves in `styles.css` are
    // there because that class of shift was found by measurement rather than by
    // this observer, and `layout.mjs` asserts the property those reserves
    // install instead. Do not read a green here as "the pane never moves".
    const layout = await page.evaluate(() => ({
      cls: window.__STS_TEST__.cls,
      shifts: window.__STS_TEST__.shifts,
    }));
    t.eq(
      "nothing in this pane shifted the layout while all of that was rendered",
      layout.cls,
      0,
      // Named, not just counted. A CLS failure that says only "0.0008" is one
      // somebody has to reproduce by hand, and the harness already has the
      // element and the two boxes on the entry.
      JSON.stringify(layout.shifts),
    );
  },
};
