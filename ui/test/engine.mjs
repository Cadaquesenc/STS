// The engine the window is tested against.
//
// It is injected before `app.js` evaluates and it is the only thing in the page
// that is not the shipped window: everything under test — the curve module, the
// tick stream, the replay bar — reads its numbers through this and has no idea
// it is not talking to Rust.
//
// Two rules it keeps deliberately:
//
//   1. It answers in the exact serialisation the Rust side produces. Every
//      struct here is `rename_all = "camelCase"` in `src-tauri`, so every key
//      below is the camelCase of a real field. A fake that answered in a shape
//      the backend never sends would test the harness rather than the window.
//   2. It refuses commands that are not registered, the same way Tauri does,
//      because "this build has no such command" is a state the window is
//      supposed to render honestly and one of the assertions is that it does.

export const FAKE_ENGINE = String.raw`
(() => {
  // One HistogramSnapshot, in the shape src-tauri/src/metrics.rs serialises.
  // The quantiles are spread around the mean the way a real latency
  // distribution is — a long right tail — rather than all sitting on it, so a
  // window that showed p99 where it meant p50 would be visibly wrong.
  const histogram = (count, meanUs) => ({
    count,
    sumUs: count * meanUs,
    minUs: Math.round(meanUs / 3),
    maxUs: meanUs * 6,
    meanUs,
    p50Us: Math.round(meanUs * 0.9),
    p95Us: Math.round(meanUs * 2.1),
    p99Us: Math.round(meanUs * 3.4),
    p999Us: Math.round(meanUs * 5.2),
    buckets: [],
  });

  const test = {
    // --- layout ---------------------------------------------------------
    cls: 0,
    shifts: [],

    // --- the telemetry channel -------------------------------------------
    callbacks: new Map(),
    nextCallbackId: 1,
    sink: null,
    // One index per channel, not one for the process. A tauri::ipc::Channel
    // numbers its own frames from zero and the receiving end buffers anything
    // that arrives out of order until the gap is filled — so a fake that shared
    // one counter between the telemetry channel and the alert channel would
    // hand each of them a sequence full of holes, and everything after the
    // first frame would sit in the reorder buffer forever. Two counters,
    // because there are two channels.
    channelIndex: 0,
    alertChannelIndex: 0,
    seq: 0,

    // --- what the engine answers -----------------------------------------
    bridgeLive: true,
    // Commands the build does not have. Calling one rejects the way Tauri does.
    //
    // The replay pair is here because a build without a replay control is a
    // real state the window renders, and one of the assertions is about it;
    // the ?replay=1 build below is the one that has them. Everything else
    // lib.rs registers is registered here.
    unregistered: new Set([
      "get_replay_status",
      "set_replay_playback",
      "set_replay_speed",
      "set_replay_transport",
    ]),
    invocations: [],
    replay: null,
    /// Whether this build's ReplayStatus carries a revision. True for the
    /// shipped one; the ?revision=0 build below is the older shape.
    numbersEditions: true,
    /// Whether a feed endpoint is connected, which is what
    /// refuse_over_a_live_feed refuses over. False here because the suite's
    /// ordinary state is a window in replay, and a fake that refused every
    /// transport press by default would test the refusal and nothing else.
    feedIsLive: false,
    ingestionTick: 0,
    // ClusterGraphReport by mint, in the camelCase src-tauri/src/clustering.rs
    // serialises to. Empty by default: the ordinary state of this build is that
    // nobody has run an analysis, and a fake that pre-filled one would hide the
    // window's answer for the state it is actually in most of the time.
    clusterReports: new Map(),
    // What get_bundle_telemetry answers. Every key is the camelCase of a real
    // field on BundleTelemetry in src-tauri/src/bundle.rs, per the second rule
    // above, and the numbers are consistent with each other rather than picked
    // independently: 47 of 62 resolved is the 758_064 millionths below, and the
    // three latency stages add up.
    bundles: {
      atSlot: 312_905_150,
      floor: {
        lamports: 148_500,
        observedLamports: 90_000,
        multiplierMicros: 1_650_000,
        saturationMicros: 820_000,
        // Deliberately a number rather than null here. The null case is what
        // the unfitted-schedule assertion overrides it to, and having both
        // shapes exercised is the point of stating this one as a number.
        proximityMicros: 320_000,
        landRateMicros: 610_000,
        slotsObserved: 32,
        headSlot: 312_905_150,
        clamp: "unclamped",
      },
      counts: {
        opened: 62,
        submitted: 62,
        retried: 19,
        landed: 47,
        evictedRetention: 9,
        evictedLeaderBoundary: 4,
        rejected: 2,
        live: 3,
        inFlight: 2,
      },
      land: {
        overallMicros: 758_064,
        firstAttemptMicros: 564_516,
        windowMicros: 610_000,
      },
      latency: {
        priceToSubmit: histogram(47, 8_000),
        submitToLand: histogram(47, 244_000),
        priceToLand: histogram(47, 252_000),
      },
      tip: {
        pricings: 81,
        committedLamports: 9_720_000,
        paidLamports: 5_640_000,
        forfeitedLamports: 1_800_000,
        minLamports: 10_000,
        maxLamports: 225_000,
        meanLamports: 120_000,
      },
      live: [
        {
          id: "bundle-0a1f",
          state: "inFlight",
          openedSlot: 312_905_148,
          openedRotation: 78_226_287,
          attemptSlot: 312_905_150,
          attempt: 2,
          tipLamports: 148_500,
          pricedAtMs: 1_700_000_000_000,
          submittedAtMs: 1_700_000_000_008,
          settledAtMs: null,
          eviction: null,
        },
      ],
    },

    // What get_metrics answers. Every key is the camelCase of a real field on
    // MetricsSnapshot in src-tauri/src/metrics.rs, per the second rule above.
    metrics: {
      atMs: 1_700_000_000_000,
      startedAtMs: 1_699_999_875_000,
      uptimeMs: 125_000,
      source: "engine",
      aggregation: "totals since start",
      resets: "never",
      slots: {
        ticks: 4_812,
        newestSlot: 312_905_150,
        regressions: 2,
        missed: 7,
        sinceLastTickMs: 180,
        processingUs: {
          count: 4_812,
          minUs: 40,
          maxUs: 9_100,
          meanUs: 610,
          p50Us: 540,
          p95Us: 1_900,
          p99Us: 4_200,
          p999Us: 8_800,
          buckets: [],
        },
        gapUs: { count: 4_811, buckets: [] },
        jitterUs: { count: 4_811, buckets: [] },
      },
      feed: {
        ingested: 1_204_336,
        dropped: 412,
        drops: [],
        lossBps: 3,
        overrun: 0,
        overrunBps: 0,
        state: "nominal",
        transitions: 5,
        bands: [],
        depth: 41,
        capacity: 1_024,
        deepest: 612,
        fillPercent: 4,
        observations: 4_812,
      },
      execution: {
        inFlightIntents: 0,
        inFlightExits: 0,
        intents: [],
        signer: [],
        unobserved: 0,
      },
    },
    // Micro-dollars per SOL. Zero is SolPrice::UNKNOWN and is the state the
    // engine starts in, which is the whole reason the control exists.
    solPrice: 0,

    // --- the trade journal ------------------------------------------------
    // Rows in the shape TradeRow serialises to in src-tauri/src/journal.rs.
    // Deliberately a mixture: two closed at a profit, one closed at a loss, one
    // still open with no realised number at all, and one in a different mode —
    // which is what the filters are for and what a fake of five identical rows
    // could not exercise.
    //
    // The arithmetic is consistent rather than decorative. Every closed row's
    // realizedPnlLamports is exactly proceeds - costBasis - fee - tip, so a
    // window that summed the columns itself instead of reading the engine's
    // number would still agree here — and journalTotals below is the sum of
    // these rows, so a window that showed the page's sum where the filter's
    // total belongs would not.
    journal: [
      {
        tradeId: "trade-0004",
        mint: "So11111111111111111111111111111111111111112",
        side: "buy",
        mode: "paper",
        venue: "pumpFunCurve",
        notionalLamports: 250_000_000,
        tokens: 9_100_000_000,
        costBasisLamports: 250_000_000,
        proceedsLamports: null,
        realizedPnlLamports: null,
        feeLamports: 2_500_000,
        tipLamports: 148_500,
        slippageBps: 41,
        openedAtMs: 1_700_000_004_000,
        closedAtMs: null,
      },
      {
        tradeId: "trade-0003",
        mint: "4hRtHkPq11111111111111111111111111111111111",
        side: "buy",
        mode: "paper",
        venue: "pumpFunCurve",
        notionalLamports: 500_000_000,
        tokens: 18_000_000_000,
        costBasisLamports: 500_000_000,
        proceedsLamports: 421_000_000,
        realizedPnlLamports: -84_148_500,
        feeLamports: 5_000_000,
        tipLamports: 148_500,
        slippageBps: 1_620,
        openedAtMs: 1_700_000_003_000,
        closedAtMs: 1_700_000_003_900,
      },
      {
        tradeId: "trade-0002",
        mint: "9WzDXwBb11111111111111111111111111111111111",
        side: "buy",
        mode: "live",
        venue: "raydiumAmmV4",
        notionalLamports: 120_000_000,
        tokens: 4_400_000_000,
        costBasisLamports: 120_000_000,
        proceedsLamports: 139_000_000,
        realizedPnlLamports: 17_651_500,
        feeLamports: 1_200_000,
        tipLamports: 148_500,
        slippageBps: 88,
        openedAtMs: 1_700_000_002_000,
        closedAtMs: 1_700_000_002_800,
      },
      {
        tradeId: "trade-0001",
        mint: "7xKXtg2C11111111111111111111111111111111111",
        side: "buy",
        mode: "paper",
        venue: "pumpFunCurve",
        notionalLamports: 80_000_000,
        tokens: 3_000_000_000,
        costBasisLamports: 80_000_000,
        proceedsLamports: 96_000_000,
        realizedPnlLamports: 15_051_500,
        feeLamports: 800_000,
        tipLamports: 148_500,
        slippageBps: 12,
        openedAtMs: 1_700_000_001_000,
        closedAtMs: 1_700_000_001_700,
      },
    ],

    // What get_alert_status answers, in the shape AlertSnapshot serialises to
    // in src-tauri/src/alerting.rs. The thresholds are that module's own
    // defaults, so an alert pushed below one of them is below the number the
    // shipped engine would have used.
    alertStatus: {
      thresholds: {
        slippageBps: 500,
        criticalSlippageBps: 1_500,
        tipGraceLamports: 0,
        confirmMs: 30_000,
        criticalConfirmMs: 90_000,
        rebroadcasts: 3,
        clusterShareBps: 4_000,
        clusterSize: 3,
        clusterEntropyMicros: 500_000,
        cooldownMs: 60_000,
      },
      raised: 0,
      suppressed: 2,
      byKind: [],
      subscribers: 0,
      webhooks: [],
    },
    // The alert channel, which is a second channel beside the telemetry one.
    alertSink: null,
    alertSeq: 1,

    // What get_geyser_telemetry answers, in the shape GeyserSnapshot
    // serialises to in src-tauri/src/geyser.rs — every field an integer, and
    // the ring in the shape RingMetrics serialises to in subslot.rs.
    //
    // The three heads are ordered the way a real cluster orders them:
    // finalized behind confirmed behind the chain head. A fake that put them
    // level would never exercise the drift the view is named for.
    geyser: {
      updates: 1_204_336,
      accounts: 918_224,
      slots: 4_812,
      transactions: 281_300,
      pings: 40,
      startupSkipped: 12_004,
      decodeFailures: 3,
      events: 902_118,
      connects: 2,
      connectFailures: 1,
      disconnects: 1,
      reconnectWaitMs: 1_800,
      staleWrites: 47,
      headSlot: 312_905_150,
      confirmedHead: 312_905_118,
      finalizedHead: 312_905_087,
      reorgs: 2,
      ring: {
        buffered: 61,
        released: 902_118,
        late: 18,
        shed: 4,
        forcedReleases: 91,
        rolledBack: 26,
        unrecoverableReorgs: 1,
        outOfOrderArrivals: 3_140,
      },
    },

    reset() {
      this.cls = 0;
      this.shifts = [];
      this.invocations = [];
    },
  };

  // --- layout shift --------------------------------------------------------
  // Shifts within 500ms of a real interaction carry hadRecentInput and are
  // excluded by the metric's own definition: a panel that opens because
  // somebody clicked is a layout change they asked for. Everything else is a
  // number moving under the eye of whoever was reading it.
  try {
    new PerformanceObserver((list) => {
      for (const entry of list.getEntries()) {
        if (entry.hadRecentInput) continue;
        test.cls += entry.value;
        test.shifts.push({
          value: entry.value,
          at: Math.round(entry.startTime),
          // Named *and* measured. A failure that says only "five DIV.tick
          // moved" is a failure somebody has to go and reproduce by hand; the
          // interesting part is always which cell and by how far, and both are
          // on the entry already.
          sources: (entry.sources || []).map((source) => {
            const node = source.node;
            if (!node) return "unknown";
            const classes = typeof node.className === "string" ? node.className : "";
            const name =
              (node.nodeName || "node") + (classes ? "." + classes.split(/\s+/).join(".") : "");
            const field =
              node.dataset && node.dataset.field
                ? node.dataset.field
                : node.querySelector && node.querySelector("[data-field]")
                  ? node.querySelector("[data-field]").dataset.field
                  : null;
            const box = (rect) =>
              rect ? Math.round(rect.x) + "+" + Math.round(rect.width) : "?";
            return (
              name +
              (field ? "[" + field + "]" : "") +
              " " +
              box(source.previousRect) +
              "\u2192" +
              box(source.currentRect)
            );
          }),
        });
      }
    }).observe({ type: "layout-shift", buffered: true });
  } catch (err) {
    test.observerError = String(err);
  }

  // --- the ingestion snapshot ----------------------------------------------
  // Deliberately not constant. Every counter here changes on every poll, which
  // is the condition the fixed-width numeric columns exist for: if a widening
  // number can move the layout, ten polls a second is where it shows.
  test.ingestionSnapshot = () => {
    const n = test.ingestionTick++;
    const jitter = (base, spread) => base + ((n * 37) % spread);
    return {
      framesPerSec: jitter(900, 900) + 0.5,
      candidatesPerSec: jitter(1, 40) / 7,
      droppedFastPath: 0,
      droppedStandard: 0,
      droppedWal: 0,
      dispatchMeanUs: jitter(200, 9000),
      overBudget: 0,
      healthyEndpoints: 3,
      endpoints: ["helius", "quickNode", "triton"].map((provider, index) => ({
        provider,
        url: "wss://" + provider + ".example/ws",
        transport: "websocket",
        connected: true,
        health: "healthy",
        latencyP50Ms: jitter(10 + index * 7, 90),
        latencyP95Ms: jitter(40 + index * 9, 400),
        frames: jitter(1000, 100000),
        connects: 1,
        consecutiveFailures: 0,
        backoffRemainingMs: 0,
      })),
    };
  };

  /// active, derived rather than stored.
  ///
  /// PlaybackState::is_active: true for everything except stopped, and that
  /// includes ended. A fixture that ran out of records did not put the feeds
  /// back; it left the clock on the recording's timeline with nothing arriving,
  /// and reporting that as live is the mistake the bar exists to prevent.
  const derive = (status) =>
    Object.assign({}, status, { active: status.state !== "stopped" });

  /// ReplaySession::start — open, rewind, play.
  test.rewound = (status) =>
    derive(
      Object.assign({}, status, {
        state: "playing",
        recordsPlayed: 0,
        slot: status.firstSlot,
        clamped: 0,
        slotRegressions: 0,
      }),
    );

  /// ReplaySession::stop — the playhead stays where it stopped and the fixture
  /// stays open, so the bar can still say which recording it was.
  test.stopped = (status) => derive(Object.assign({}, status, { state: "stopped" }));

  /// What a step and a fast-forward do: play that many records, whatever the
  /// clock says, then hold — or end, if the cursor ran out.
  ///
  /// Played rather than skipped, which is why the slot walks with the record
  /// count instead of being set from it.
  test.played = (status, records) => {
    const wanted = Math.max(0, Math.min(records, status.recordCount - status.recordsPlayed));
    const played = status.recordsPlayed + wanted;
    return derive(
      Object.assign({}, status, {
        recordsPlayed: played,
        slot: status.slot + wanted,
        state: played >= status.recordCount ? "ended" : "paused",
      }),
    );
  };

  /// Stamps a status with the next edition number, the way ReplaySession bumps
  /// its revision under the lock on the way out of a change.
  ///
  /// Every mutating replay command here goes through this, because the window
  /// uses the number to throw away answers that arrive out of order — and a
  /// fake that handed out the same edition for two different states would let a
  /// window pass that discarded the newer one.
  ///
  /// Deliberately unconditional, unlike the Rust: the real session only counts
  /// changes, and a fake that guessed at which presses were no-ops would be
  /// asserting its own opinion of the engine rather than the window's handling
  /// of what it is sent. Numbering every answer is the stricter fake — the
  /// window has to cope with editions it has already seen either way.
  const edition = (status) => {
    if (!test.numbersEditions) return status;
    status.revision = (test.replay?.revision ?? 0) + 1;
    return status;
  };

  const answer = (command, payload) => {
    test.invocations.push({ command, payload });

    if (!test.bridgeLive) {
      return Promise.reject(new Error("the engine is not answering"));
    }
    if (test.unregistered.has(command)) {
      return Promise.reject(
        new Error("Command " + command + " not found"),
      );
    }

    switch (command) {
      case "get_ingestion_metrics":
        return Promise.resolve(test.ingestionSnapshot());

      case "get_engine_status":
        return Promise.resolve({
          state: "running",
          uptimeMs: 125_000,
          killSwitchArmed: false,
          killSwitchAtMs: null,
        });

      case "stream_telemetry": {
        const channel = payload && payload.onEvent;
        const id = channel && channel.id;
        test.sink = test.callbacks.get(id) || null;
        return Promise.resolve({ subscriberId: 1, fromSeq: 0 });
      }

      case "trigger_kill_switch":
        return Promise.resolve({ alreadyArmed: false, atMs: 1_700_000_000_000 });

      case "trigger_emergency_unwind":
        return Promise.resolve(test.unwindReceipt(payload));

      case "get_metrics":
        return Promise.resolve(test.metrics);

      case "get_bundle_telemetry":
        return Promise.resolve(test.bundles);

      case "set_sol_price": {
        const cents = payload && payload.centsPerSol;
        // The same refusal set_sol_price makes in lib.rs, in the same
        // direction: a price of zero would make every candidate look too small
        // to trade, and the window has to render that as a refusal rather than
        // as a price it accepted.
        if (!Number.isFinite(cents) || cents <= 0) {
          return Promise.reject(
            new Error(
              "a SOL price of zero would make every candidate look too small to trade",
            ),
          );
        }
        test.solPrice = cents * 10_000;
        return Promise.resolve({ microUsdPerSol: test.solPrice });
      }

      case "get_replay_status":
        return Promise.resolve(test.replay);

      case "set_replay_playback": {
        let next = Object.assign({}, test.replay || {});
        if (payload && "speed" in payload) next.speed = payload.speed;
        if (payload && "active" in payload) {
          // ReplaySession::start opens the fixture, rewinds it and plays;
          // ::stop leaves the playhead exactly where it stopped and keeps the
          // fixture open. Both are modelled, because the difference between
          // them is what makes stop-then-play rewind and resume not.
          next = payload.active === true ? test.rewound(next) : test.stopped(next);
        }
        test.replay = edition(next);
        return Promise.resolve(test.replay);
      }

      case "set_replay_transport": {
        // ReplaySession::control, in the same order and with the same
        // refusals. Nothing here writes "active": it is derived from "state",
        // exactly as PlaybackState::is_active derives it in Rust, and the
        // command has no way to reach it either way. That property is what half
        // the assertions about this control are checking.
        let next = Object.assign({}, test.replay || {});
        if (payload && "speed" in payload && payload.speed !== undefined) {
          // Applied first and independently, the way the command's own doc
          // comment says: a window may send a chip and a button in one message.
          next.speed = payload.speed;
        }

        const control = payload && payload.control;
        const records = payload && Number.isFinite(payload.records) ? payload.records : null;

        // refuse_over_a_live_feed. Pause and stop take a fixture off the clock
        // or leave it where it is, so neither is gated; everything else is.
        // The window has to render this refusal as a refusal rather than as a
        // press that worked, which is the whole reason it is modelled.
        if (test.feedIsLive && control !== "pause" && control !== "stop") {
          return Promise.reject(
            new Error(
              "helius is still connected. This build replays a fixture into the clock and " +
                "the cockpit, not into ingestion, so live candidates would keep filling the " +
                "panes under a bar saying they were recorded. Stop the feeds first.",
            ),
          );
        }

        switch (control) {
          case "play":
            next = test.rewound(next);
            break;
          case "stop":
            next = test.stopped(next);
            break;
          case "pause":
            // A no-op from anywhere but playing. Pausing a stopped session
            // would raise the bar over a window nobody put in replay, and
            // pausing an ended one offers a resume with nothing to resume into.
            if (next.state === "playing") next.state = "paused";
            break;
          case "resume":
            // A no-op from ended: there is no record left to carry on to, and
            // restarting would make the button mean two things.
            if (next.state !== "ended") next.state = "playing";
            break;
          case "step":
            next = test.played(next, records === null ? 1 : records);
            break;
          case "fastForward":
            // No count is every record that is left, which is what makes this
            // the backtest runner as well as a transport control.
            next = test.played(next, records === null ? next.recordCount - next.recordsPlayed : records);
            break;
          default:
            // What serde does with a seventh control: refuses to deserialise.
            // Deliberately not the "no such command" error, which would make
            // the window disable a control that is there.
            return Promise.reject(
              new Error(
                "invalid args for command set_replay_transport: unknown control",
              ),
            );
        }
        test.replay = edition(next);
        return Promise.resolve(test.replay);
      }

      case "set_replay_speed": {
        // Deliberately reads nothing but speed. The command's whole reason to
        // exist is that a caller holding it cannot start or stop a fixture, and
        // a fake that honoured active here would let a broken window pass.
        const next = Object.assign({}, test.replay || {});
        if (payload && "speed" in payload) next.speed = payload.speed;
        test.replay = edition(next);
        return Promise.resolve(test.replay);
      }

      // --- the trade journal --------------------------------------------
      //
      // The filter is applied here rather than ignored, because half the
      // assertions about this pane are that the window sends the filter it
      // drew: a fake that answered the same four rows whatever it was handed
      // would pass a window whose chips were wired to nothing.
      case "query_journal": {
        const filter = (payload && payload.filter) || {};
        return Promise.resolve(test.filteredJournal(filter));
      }

      case "journal_totals": {
        const filter = (payload && payload.filter) || {};
        return Promise.resolve(test.journalTotals(test.filteredJournal(filter)));
      }

      case "journal_trade_detail": {
        const id = payload && payload.tradeId;
        const trade = test.journal.find((row) => row.tradeId === id) || null;
        // None for a trade nothing recorded is a real answer and not an
        // error, which is what the command's own doc comment says.
        return Promise.resolve(
          trade ? { trade, fills: [], routes: [], tips: [], signatures: [] } : null,
        );
      }

      // --- alerting -------------------------------------------------------
      case "get_alert_status":
        return Promise.resolve(test.alertStatus);

      case "set_alert_thresholds": {
        const next = (payload && payload.thresholds) || {};
        // The same refusal AlertThresholds::validate makes, in the same
        // direction: a critical line under the warning one would mean every
        // critical alert fires as a warning first and never as a critical.
        if (
          Number.isFinite(next.criticalSlippageBps) &&
          Number.isFinite(next.slippageBps) &&
          next.criticalSlippageBps < next.slippageBps
        ) {
          return Promise.reject(
            new Error("the critical threshold is below the warning one"),
          );
        }
        test.alertStatus = Object.assign({}, test.alertStatus, {
          thresholds: Object.assign({}, test.alertStatus.thresholds, next),
        });
        return Promise.resolve(test.alertStatus);
      }

      case "stream_alerts": {
        const channel = payload && payload.onAlert;
        const id = channel && channel.id;
        test.alertSink = test.callbacks.get(id) || null;
        return Promise.resolve({ subscriberId: 2, fromSeq: test.alertSeq });
      }

      // --- the geyser feed ------------------------------------------------
      case "get_geyser_telemetry":
        return Promise.resolve(test.geyser);

      // --- the forensic report --------------------------------------------
      //
      // Keyed the way ClusterRegistry is keyed, and answering null for a key
      // nobody analysed. The null is not an error and the window has to render
      // it as "nobody looked" rather than as "nothing found", which is one of
      // the things the cluster suite asserts.
      case "get_cluster_report": {
        const mint = payload && payload.mint;
        return Promise.resolve(test.clusterReports.get(mint) || null);
      }

      default:
        return Promise.reject(new Error("Command " + command + " not found"));
    }
  };

  window.__TAURI_INTERNALS__ = {
    invoke: (command, payload) => answer(command, payload),
    transformCallback(fn) {
      const id = test.nextCallbackId++;
      test.callbacks.set(id, fn);
      return id;
    },
    unregisterCallback(id) {
      test.callbacks.delete(id);
    },
  };

  // --- pushing events ------------------------------------------------------

  test.push = (event) => {
    if (!test.sink) return false;
    const full = Object.assign(
      {
        seq: test.seq++,
        atMs: 1_700_000_000_000 + test.seq * 400,
        level: "info",
        source: "lifecycle",
        message: "",
        data: {},
      },
      event,
    );
    test.sink({ index: test.channelIndex++, message: full });
    return true;
  };

  /// One candidate observation, in the shape CandidateEvent serialises to.
  test.pushCandidate = (view, extra) => {
    const full = Object.assign(
      {
        provider: "helius",
        program: "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",
        creator: "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM",
        curveComplete: false,
        slotsSinceLaunch: 40,
        virtualTokenReserves: 347_400_000_000_000,
      },
      view,
    );
    return test.push(
      Object.assign(
        {
          source: "ingestion",
          message: "candidate",
          data: {
            view: full,
            route: "fastPath",
            marketCapUsdCents: 4_000_431,
            receivedAtMs: 1_700_000_000_000,
            dispatchLatencyUs: 412,
          },
        },
        extra || {},
      ),
    );
  };

  test.pushReplay = (status) => test.push({ source: "replay", message: "replay", data: status });

  /// One row of execution_logs, in the shape SCHEMA.md gives its columns.
  test.pushExecution = (row) =>
    test.push({
      source: "execution",
      message: "execution",
      data: Object.assign(
        {
          intentId: "alpha",
          seq: 4,
          mint: "So11111111111111111111111111111111111111112",
          state: "aborted",
          prevState: "confirmed",
          side: "buy",
          sizeLamports: 250_000_000,
          signature: "sig-alpha",
          needsUnwind: true,
          abortReason: "operator",
          mode: "paper",
        },
        row,
      ),
    });

  /// The engine's word that one obligation is closed, or that it is not.
  test.pushUnwind = (data) => test.push({ source: "unwind", message: "unwind", data: data });

  /// One StrandedPosition, as engine.rs serialises it.
  ///
  /// The exit is null by default because that is what a build with no execution
  /// backend produces: nothing was attempted, so there is nothing to report
  /// about an attempt. A test that wants a transaction in the air says so.
  test.stranded = (intentId, exit) => ({
    intentId: intentId,
    mint: "So11111111111111111111111111111111111111112",
    side: "buy",
    sizeLamports: 250_000_000,
    signature: "sig-" + intentId,
    atRiskIn: "confirmed",
    mode: "paper",
    conditional: false,
    exit: exit || null,
  });

  /// One StrandedExit with a transaction on the network, in the words
  /// stranded_exit uses for the InFlight outcome.
  test.exitInFlight = (intentId) => ({
    exitIntentId: "exit-" + intentId,
    signature: "exit-sig-" + intentId,
    state: "exitBroadcast",
    failure: null,
    detail:
      "an exit is on the network and has not confirmed; it closes nothing until it does",
    onNetwork: true,
  });

  /// One StrandedExit that was attempted and never reached the network.
  test.exitFailed = (intentId) => ({
    exitIntentId: "exit-" + intentId,
    signature: null,
    state: "exitFailed",
    failure: "noRoute",
    detail: "the curve is depleted; there is no route out at this size",
    onNetwork: false,
  });

  /// What trigger_emergency_unwind answers.
  ///
  /// The default is the shipped application's own answer: halted, no signer
  /// installed, nothing sold, and everything asked about still on chain. A
  /// suite asking a different question of it replaces this.
  test.unwindReceipt = (payload) => {
    const intentIds = (payload && payload.intentIds) || [];
    return {
      atMs: 1_700_000_000_000,
      reason: (payload && payload.reason) || "flattened from the UI",
      actor: "ui",
      killSwitch: {
        armed: true,
        alreadyArmed: false,
        atMs: 1_700_000_000_000,
        reason: "flattened from the UI",
        auditId: 1,
      },
      aborted: intentIds.length,
      exitsSent: 0,
      exitsConfirmed: 0,
      exitsInFlight: 0,
      exitsFailed: 0,
      exitsAlreadyOut: 0,
      signer: null,
      signerLive: false,
      flattened: [],
      realizedPnlLamports: 0,
      resolved: [],
      stranded: intentIds.map((intentId) => test.stranded(intentId)),
      strandedKnown: true,
      auditId: 2,
      problems: [],
    };
  };

  // --- the journal ---------------------------------------------------------

  /// JournalFilter, applied the way query_journal applies it.
  ///
  /// Newest first, because that is the order the SQL is written in. Only the
  /// fields the window actually sends are honoured; a fake that quietly
  /// honoured a field nothing sends would hide a window that never sent it.
  test.filteredJournal = (filter) => {
    let rows = test.journal.slice();
    if (filter.mode) rows = rows.filter((row) => row.mode === filter.mode);
    if (filter.onlyClosed) rows = rows.filter((row) => row.closedAtMs !== null);
    if (Number.isFinite(filter.maxRealizedPnlLamports)) {
      rows = rows.filter(
        (row) =>
          Number.isFinite(row.realizedPnlLamports) &&
          row.realizedPnlLamports <= filter.maxRealizedPnlLamports,
      );
    }
    if (Number.isFinite(filter.minRealizedPnlLamports)) {
      rows = rows.filter(
        (row) =>
          Number.isFinite(row.realizedPnlLamports) &&
          row.realizedPnlLamports >= filter.minRealizedPnlLamports,
      );
    }
    rows.sort((a, b) => b.openedAtMs - a.openedAtMs);
    const offset = Number.isFinite(filter.offset) ? filter.offset : 0;
    const limit = Number.isFinite(filter.limit) && filter.limit > 0 ? filter.limit : 50;
    return rows.slice(offset, offset + limit);
  };

  /// JournalTotals over whatever the filter matched.
  ///
  /// Summed from the matched rows and not from the page, which is the whole
  /// distinction the two commands exist to draw. With a limit smaller than the
  /// match these two answers differ, and one of the assertions is that the
  /// window shows the second where the second belongs.
  test.journalTotals = (rows) => {
    const sum = (pick) => rows.reduce((total, row) => total + (pick(row) || 0), 0);
    const slippages = rows
      .map((row) => row.slippageBps)
      .filter((value) => Number.isFinite(value));
    return {
      trades: rows.length,
      closed: rows.filter((row) => row.closedAtMs !== null).length,
      notionalLamports: sum((row) => row.notionalLamports),
      costBasisLamports: sum((row) => row.costBasisLamports),
      proceedsLamports: sum((row) => row.proceedsLamports),
      realizedPnlLamports: sum((row) => row.realizedPnlLamports),
      feeLamports: sum((row) => row.feeLamports),
      tipLamports: sum((row) => row.tipLamports),
      worstSlippageBps: slippages.length ? Math.max(...slippages) : null,
    };
  };

  // --- alerts --------------------------------------------------------------

  /// One alert down the dedicated channel, in the shape Alert serialises to.
  ///
  /// The sequence is assigned here and increments once per alert, exactly as
  /// the dispatcher's does, because the window gates the feed on it and a fake
  /// that reused a sequence would be testing the gate rather than the pane.
  test.pushAlert = (alert) => {
    const full = Object.assign(
      {
        seq: test.alertSeq++,
        atMs: 1_700_000_000_000 + test.alertSeq * 400,
        kind: "slippageSpike",
        severity: "warn",
        mode: "paper",
        subject: "trade-0003",
        mint: "4hRtHkPq11111111111111111111111111111111111",
        message: "a fill came in further under its quote than the route allowed",
        observed: 1_620,
        threshold: 500,
        unit: "basisPoints",
      },
      alert,
    );
    test.alertStatus = Object.assign({}, test.alertStatus, {
      raised: test.alertStatus.raised + 1,
    });
    // Kept so a suite can re-send exactly what was sent, which is what a
    // reconnected channel does.
    test.lastAlert = full;
    if (!test.alertSink) return false;
    test.alertSink({ index: test.alertChannelIndex++, message: full });
    return true;
  };

  /// The same alert, re-sent down the channel without a new sequence.
  ///
  /// A real reconnect replays what the subscriber missed and overlaps what it
  /// did not, so this is a state the window is in on any restart. The
  /// assertion is that the second copy changes nothing.
  test.replayAlert = (alert) => {
    if (!test.alertSink) return false;
    test.alertSink({ index: test.alertChannelIndex++, message: alert });
    return true;
  };

  /// The same alert as a telemetry line, which is the other path every alert
  /// takes. A window listening to both must apply it once.
  test.pushAlertOnHub = (alert) =>
    test.push({ source: "alert", message: alert.message, data: { alert } });

  // --- the geyser feed -----------------------------------------------------

  /// One released tick, carrying the TickKey from src-tauri/src/subslot.rs.
  ///
  /// micros is the offset inside the slot, which is what makes the jitter a
  /// sub-slot measurement rather than a slot one.
  test.pushGeyserTick = (slot, micros, extra) =>
    test.push({
      source: "geyser",
      message: "tick",
      data: Object.assign(
        { key: { slot, micros, writeVersion: 1, seq: test.seq } },
        extra || {},
      ),
    });

  /// A run of arrivals at a fixed cadence, with an optional wobble.
  ///
  /// stepUs is the gap between arrivals and wobbleUs is added to every
  /// other one, so the jitter this produces is wobbleUs and not something
  /// that has to be derived — an expected value computed by the same
  /// arithmetic as the actual one would not be an assertion.
  test.pushGeyserRun = (count, { slot = 312_905_150, startUs = 0, stepUs = 40_000, wobbleUs = 0 } = {}) => {
    let at = startUs;
    let currentSlot = slot;
    for (let index = 0; index < count; index += 1) {
      test.pushGeyserTick(currentSlot, at);
      at += stepUs + (index % 2 === 0 ? wobbleUs : 0);
      // A slot is 400ms. Past it the arrival belongs to the next one, which is
      // what makes the ring's addresses walk forward the way a real feed does.
      while (at >= 400_000) {
        at -= 400_000;
        currentSlot += 1;
      }
    }
    return count;
  };

  // Whether this build has a replay control is decided before the window loads,
  // because the window latches the answer the first time it asks — it stops
  // polling a command the engine says it does not have, which is the behaviour
  // one of the assertions is about. Flipping it afterwards would be testing a
  // state the real thing can never be in.
  if (new URLSearchParams(location.search).get("replay") === "1") {
    test.unregistered.delete("get_replay_status");
    test.unregistered.delete("set_replay_playback");
    test.unregistered.delete("set_replay_speed");
    // A build with a replay control and no transport is its own state — the
    // command was added after the bar was — and the window has a branch for it.
    if (new URLSearchParams(location.search).get("transport") !== "0") {
      test.unregistered.delete("set_replay_transport");
    }
    // ReplayStatus, in the shape src-tauri/src/replay.rs serialises it: the
    // four-state PlaybackState rather than a boolean, both virtualised clocks,
    // and the ledger the paper runner fills in.
    test.replay = {
      active: false,
      state: "stopped",
      streamId: "phase3-2026-08-14",
      chainHead: "9f2c1ab74e0d5c8831bb0e6f4a27d9c05e1f3a7b6c8d90e1f2a3b4c5d6e7f809",
      chainVerified: true,
      fixtureComplete: true,
      speed: "1",
      slot: 312905118,
      firstSlot: 312900000,
      lastSlot: 313000000,
      recordsPlayed: 4812,
      recordCount: 91244,
      clamped: 0,
      slotRegressions: 0,
      atMs: 1_700_000_000_000,
      ledger: {
        eventsApplied: 4_812,
        eventsUndecodable: 0,
        eventsFiltered: 1_204,
      },
      revision: 1,
    };

    // A build whose ReplayStatus has no revision on it. Every build before the
    // field was added is in this state, and the window has a second way to
    // order statuses for exactly that case — a digest of everything the bar
    // draws — which is a path nothing would otherwise exercise.
    if (new URLSearchParams(location.search).get("revision") === "0") {
      test.numbersEditions = false;
      delete test.replay.revision;
    }
  }

  // A build with no get_metrics. Not a state the shipped engine is in, but the
  // window has a branch for it and a branch nothing exercises is a branch that
  // stops working quietly: the assertion is that it asks once and then leaves
  // the queue reading unknown rather than asking again every second forever.
  if (new URLSearchParams(location.search).get("metrics") === "0") {
    test.unregistered.add("get_metrics");
  }

  // A build with no trade journal and no alerting engine. This is the state
  // every build before feat/trade-journal-sqlite-alerting was in, and the
  // window has to render it as "this build has no journal" rather than as a
  // journal with nothing in it — the second reads as an engine that has never
  // traded, which is a different and much more comfortable claim.
  if (new URLSearchParams(location.search).get("journal") === "0") {
    test.unregistered.add("query_journal");
    test.unregistered.add("journal_totals");
    test.unregistered.add("journal_trade_detail");
    test.unregistered.add("get_alert_status");
    test.unregistered.add("set_alert_thresholds");
    test.unregistered.add("stream_alerts");
  }

  // A build with the Geyser pipeline compiled in and nothing dialling it, which
  // is the shipped state until something puts a GeyserSource behind it. The
  // 0x100 view still opens; it says there is no feed rather than drawing a
  // grid of zeroes, because a grid of zeroes is a claim that the feed is
  // perfectly steady.
  if (new URLSearchParams(location.search).get("geyser") === "0") {
    test.unregistered.add("get_geyser_telemetry");
  }

  window.__STS_TEST__ = test;
})();
`;
