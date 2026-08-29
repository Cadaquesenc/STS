//! The journal and the alerting engine, against a real `sts.db` and a real
//! socket.
//!
//! The unit tests in `journal.rs` and `alerting.rs` cover one behaviour each.
//! These are the ones that need more than one thing to be true at once:
//!
//! - the book survives the process that wrote it, which is the only property
//!   that makes it a journal rather than a log;
//! - migration 3 lands on a file that predates it without disturbing the two
//!   ledgers already there, and is refused if it changed after shipping;
//! - the alert and the row describe the same numbers, because an alert that
//!   disagrees with the book is worse than no alert;
//! - the same run written twice produces the same bytes, which is Phase 3's
//!   acceptance criterion and the reason none of these keys are generated.
//!
//! Everything goes through the public API and then reopens the file with a
//! plain `rusqlite` connection to check what is actually on disk, rather than
//! asking the same code that wrote it — the convention `e2e_integration.rs`
//! sets and for the same reason.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sts_lib::alerting::{
    Alert, AlertDispatcher, AlertKind, AlertSeverity, AlertSink, AlertThresholds, AlertUnit,
    Observation,
};
use sts_lib::db::{
    latest_schema_version, ClusterRow, Database, ExecutionMode, Side, TickMetricRow,
};
use sts_lib::execution::TipStance;
use sts_lib::journal::{
    FillRow, JournalFilter, RouteDecision, RouteRow, SignatureKind, SignatureRow, SignatureStatus,
    TipRow, TradeRow,
};
use sts_lib::strategy::fixed::Q18;
use sts_lib::telemetry::TelemetryHub;
use sts_lib::types::{SybilClusterMetrics, Venue};

const AT_MS: i64 = 1_700_000_000_000;
const MINT: &str = "So11111111111111111111111111111111111111112";
const TIP_ACCOUNT: &str = "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5";

/// A file of its own per test, removed when the test ends.
struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "sts-journal-e2e-{name}-{}-{}.db",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let temp = TempDb(path);
        temp.remove();
        temp
    }

    fn open(&self) -> Database {
        Database::open(&self.0).expect("sts.db opens")
    }

    fn raw(&self) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(&self.0).expect("the file opens");
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .expect("the pragma applies");
        conn
    }

    fn remove(&self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        self.remove();
    }
}

/// Every journal table, as text, in a fixed order. What two runs are compared
/// on.
fn dump(conn: &rusqlite::Connection) -> String {
    let mut out = String::new();
    for (table, order) in [
        ("journal_trades", "trade_id"),
        ("journal_fills", "trade_id, seq"),
        ("journal_routes", "trade_id, seq"),
        ("journal_tips", "trade_id, attempt"),
        ("journal_signatures", "signature"),
    ] {
        out.push_str(table);
        out.push('\n');
        let mut statement = conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY {order}"))
            .expect("the query prepares");
        let columns = statement.column_count();
        let rows = statement
            .query_map([], |row| {
                Ok((0..columns)
                    .map(|index| match row.get_ref(index) {
                        Ok(rusqlite::types::ValueRef::Null) => "null".to_string(),
                        Ok(rusqlite::types::ValueRef::Integer(n)) => n.to_string(),
                        Ok(rusqlite::types::ValueRef::Text(t)) => {
                            String::from_utf8_lossy(t).into_owned()
                        }
                        // A real would be a float in a schema that promises
                        // none, so it is rendered in a way a diff cannot miss.
                        Ok(rusqlite::types::ValueRef::Real(n)) => format!("FLOAT:{n}"),
                        _ => "blob".to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("|"))
            })
            .expect("the query runs");
        for row in rows {
            out.push_str("  ");
            out.push_str(&row.expect("the row reads"));
            out.push('\n');
        }
    }
    out
}

/// One bad exit: opened, routed, tipped, filled badly, sent, and dropped.
///
/// Returns the trade id. The numbers are chosen so every threshold in
/// `AlertThresholds::default` is crossed by exactly one of them, which is what
/// lets the alert assertions below name a specific figure.
fn record_a_bad_exit(db: &Database, trade_id: &str) -> (TradeRow, FillRow, TipRow, SignatureRow) {
    let mut trade = TradeRow::opened(
        trade_id,
        MINT,
        Side::Sell,
        ExecutionMode::Paper,
        500_000_000,
        AT_MS,
    );
    trade.venue = Some(Venue::PumpFunCurve);
    trade.tokens = 1_000_000_000;
    trade.tip_lamports = 2_000_000;
    trade.slippage_bps = Some(1_800);

    // 18% under the quote: past the 500 bps floor and past the 1500 bps
    // critical line.
    let fill = FillRow::settle(
        trade_id,
        0,
        1_000_000_000,
        410_000_000,
        5_000_000,
        500_000_000,
        250_000_001,
        AT_MS + 400,
    )
    .expect("a real fill");
    let tip = TipRow {
        trade_id: trade_id.to_string(),
        attempt: 4,
        account: TIP_ACCOUNT.to_string(),
        // Past its own ceiling, which `TipPolicy` should make impossible and
        // which the journal records rather than asserts about.
        lamports: 2_000_000,
        stance: TipStance::Emergency,
        ev_net_lamports: Some(30_000_000),
        ceiling_lamports: 1_000_000,
        at_ms: AT_MS + 100,
    };
    let signature =
        SignatureRow::broadcast("9".repeat(64), trade_id, SignatureKind::Exit, AT_MS + 200)
            .settled_as(SignatureStatus::Dropped, AT_MS + 40_000);

    db.record_journal_trades(&[trade.clone()])
        .expect("the trade is written");
    db.record_journal_routes(&[
        RouteRow {
            trade_id: trade_id.to_string(),
            seq: 0,
            venue: Venue::PumpFunCurve,
            decision: RouteDecision::Chosen,
            tokens: 1_000_000_000,
            quoted_out_lamports: 500_000_000,
            min_out_lamports: 485_000_000,
            max_slippage_bps: 300,
            simulated_at_ms: AT_MS - 150,
            at_ms: AT_MS + 50,
        },
        RouteRow {
            trade_id: trade_id.to_string(),
            seq: 1,
            venue: Venue::RaydiumAmmV4,
            decision: RouteDecision::Rejected {
                because: "no pool for this mint".to_string(),
            },
            tokens: 1_000_000_000,
            quoted_out_lamports: 0,
            min_out_lamports: 0,
            max_slippage_bps: 300,
            simulated_at_ms: AT_MS - 150,
            at_ms: AT_MS + 50,
        },
    ])
    .expect("the routes are written");
    db.record_journal_tips(std::slice::from_ref(&tip))
        .expect("the tip is written");
    db.record_journal_fills(std::slice::from_ref(&fill))
        .expect("the fill is written");
    db.record_journal_signatures(std::slice::from_ref(&signature))
        .expect("the signature is written");

    (trade, fill, tip, signature)
}

// ---------------------------------------------------------------------------

#[test]
fn a_trade_survives_the_process_that_recorded_it() {
    let temp = TempDb::new("restart");
    let (trade, fill, tip, signature) = {
        let db = temp.open();
        let written = record_a_bad_exit(&db, "t-1");
        let closed = written.0.clone().closed_at(410_000_000, AT_MS + 60_000);
        db.record_journal_trades(std::slice::from_ref(&closed))
            .expect("the close is written");
        db.close();
        (closed, written.1, written.2, written.3)
    };

    // A second process, opening the same file.
    let db = temp.open();
    let detail = db
        .journal_trade_detail("t-1")
        .expect("reads")
        .expect("the trade is there");
    assert_eq!(detail.trade, trade);
    assert_eq!(detail.fills, vec![fill]);
    assert_eq!(detail.tips, vec![tip]);
    assert_eq!(detail.signatures, vec![signature]);
    assert_eq!(detail.routes.len(), 2);
    assert!(detail.routes[0].decision.was_chosen());
    assert_eq!(
        detail.routes[1].decision.because(),
        Some("no pool for this mint")
    );

    // And the money adds up the same way from the totals as from the row.
    let totals = db.journal_totals(&JournalFilter::default()).expect("sums");
    assert_eq!(totals.trades, 1);
    assert_eq!(totals.closed, 1);
    assert_eq!(totals.proceeds_lamports, 410_000_000);
    // 410 out, 500 in, 2 of tip. No venue fee on the trade row, which is on the
    // fill.
    assert_eq!(totals.realized_pnl_lamports, -92_000_000);
    assert_eq!(totals.worst_slippage_bps, Some(1_800));
}

/// Puts a current file back into the shape an older build left it in: the two
/// ledgers, no journal, no forensic log, and the six `REAL` columns migration 4
/// took away.
///
/// Written as a rollback of a current file rather than as a copy of migration
/// 1's text, because the thing being tested is that *this* build comes forward
/// from *that* shape — and keeping a second copy of the old schema in the test
/// suite is how the two drift apart until the test is migrating something no
/// build ever wrote.
///
/// The `clusters`, `execution_logs` and `tick_metrics` bodies below are
/// migration 1's, `REAL` columns included. Every value is carried back through
/// the same conversion migration 4 will apply forwards, so a row seeded here
/// and read after the reopen is a round trip rather than a fresh write.
///
/// Migration 5's three tables are dropped outright rather than carried back.
/// There is nothing to carry: an older build wrote no forensic rows, and the
/// counters come back seeded at zero — which is the correct reading of a file
/// that has never recorded a verdict.
fn roll_back_to_the_shape_before_the_journal(conn: &rusqlite::Connection) {
    conn.execute_batch(
        "DROP TRIGGER journal_snapshots_are_immutable;
         DROP TRIGGER journal_state_log_is_append_only;
         DROP TRIGGER journal_revisions_are_the_three_modes;
         DROP TRIGGER journal_revisions_only_go_forward;
         DROP TABLE journal_snapshots;
         DROP TABLE journal_state_log;
         DROP TABLE journal_revisions;

         DROP TRIGGER journal_trades_identity_is_immutable;
         DROP TABLE journal_signatures;
         DROP TABLE journal_tips;
         DROP TABLE journal_routes;
         DROP TABLE journal_fills;
         DROP TABLE journal_trades;

         ALTER TABLE candidates ADD COLUMN bonding_curve_pct REAL
             GENERATED ALWAYS AS (curve_progress_bps / 100.0) VIRTUAL;

         CREATE TABLE clusters_before (
             cluster_id           TEXT NOT NULL,
             version              INTEGER NOT NULL,
             root_wallet          TEXT NOT NULL,
             wallet_count         INTEGER NOT NULL CHECK (wallet_count >= 1),
             hhi                  INTEGER NOT NULL CHECK (hhi BETWEEN 0 AND 10000),
             temporal_influence   REAL NOT NULL CHECK (temporal_influence BETWEEN 0.0 AND 1.0),
             spectral_separation  REAL NOT NULL CHECK (spectral_separation BETWEEN 0.0 AND 1.0),
             interaction_entropy  REAL NOT NULL CHECK (interaction_entropy BETWEEN 0.0 AND 1.0),
             flag_sybil           INTEGER NOT NULL CHECK (flag_sybil IN (0, 1)),
             computed_at_ms       INTEGER NOT NULL,
             PRIMARY KEY (cluster_id, version)
         );
         INSERT INTO clusters_before
             SELECT cluster_id, version, root_wallet, wallet_count, hhi,
                    temporal_influence_micros  / 1000000.0,
                    spectral_separation_micros / 1000000.0,
                    interaction_entropy_micros / 1000000.0,
                    flag_sybil, computed_at_ms
               FROM clusters;
         DROP TABLE clusters;
         ALTER TABLE clusters_before RENAME TO clusters;

         CREATE TABLE execution_logs_before (
             intent_id     TEXT NOT NULL,
             seq           INTEGER NOT NULL CHECK (seq >= 0),
             mint          TEXT NOT NULL,
             state         TEXT NOT NULL CHECK (state IN (
                             'intent_created', 'validated', 'sent',
                             'confirmed', 'completed', 'aborted')),
             prev_state    TEXT CHECK (prev_state IN (
                             'intent_created', 'validated', 'sent',
                             'confirmed', 'completed', 'aborted')),
             side          TEXT NOT NULL CHECK (side IN ('buy', 'sell')),
             size_lamports INTEGER NOT NULL CHECK (size_lamports > 0),
             price         REAL CHECK (price IS NULL OR price > 0.0),
             signature     TEXT,
             latency_ms    INTEGER CHECK (latency_ms IS NULL OR latency_ms >= 0),
             needs_unwind  INTEGER NOT NULL DEFAULT 0 CHECK (needs_unwind IN (0, 1)),
             mode          TEXT NOT NULL CHECK (mode IN ('live', 'paper', 'replay')),
             abort_reason  TEXT CHECK (abort_reason IN (
                             'risk_gate', 'circuit_breaker', 'kill_switch', 'stale',
                             'send_failed', 'not_confirmed', 'operator')),
             at_ms         INTEGER NOT NULL,
             PRIMARY KEY (intent_id, seq)
         );
         INSERT INTO execution_logs_before
             SELECT intent_id, seq, mint, state, prev_state, side, size_lamports,
                    price_q18 / 1000000000000000000.0,
                    signature, latency_ms, needs_unwind, mode, abort_reason, at_ms
               FROM execution_logs;
         DROP TABLE execution_logs;
         ALTER TABLE execution_logs_before RENAME TO execution_logs;
         CREATE UNIQUE INDEX execution_logs_signature
             ON execution_logs (signature) WHERE signature IS NOT NULL;

         CREATE TABLE tick_metrics_before (
             rpc_endpoint   TEXT NOT NULL,
             timestamp      INTEGER NOT NULL,
             latency_ms     INTEGER NOT NULL CHECK (latency_ms >= 0),
             dropped_msgs   INTEGER NOT NULL CHECK (dropped_msgs >= 0),
             parsed_per_sec REAL NOT NULL CHECK (parsed_per_sec >= 0.0),
             PRIMARY KEY (rpc_endpoint, timestamp)
         ) WITHOUT ROWID;
         INSERT INTO tick_metrics_before
             SELECT rpc_endpoint, timestamp, latency_ms, dropped_msgs,
                    parsed_per_sec_micros / 1000000.0
               FROM tick_metrics;
         DROP TABLE tick_metrics;
         ALTER TABLE tick_metrics_before RENAME TO tick_metrics;

         DELETE FROM schema_migrations WHERE version IN (3, 4, 5);",
    )
    .expect("the older shape is arranged");

    let version: i64 = conn
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .expect("asks");
    assert_eq!(
        version, 2,
        "the file is not at the version this is testing from"
    );
}

#[test]
fn the_journal_migration_lands_on_a_file_that_predates_it() {
    let temp = TempDb::new("migrate-forward");

    // A file as an older build left it: the two ledgers, no journal, and the
    // floats. Built by rolling a current one back, which reaches the same end
    // state and does not require keeping a copy of an old binary around.
    {
        let db = temp.open();
        db.close();
        let conn = temp.raw();
        conn.execute_batch(
            "INSERT INTO audit_log (event_type, payload, created_at)
                  VALUES ('older_build', '{}', 1);",
        )
        .expect("the ledger beside it has something in it");
        roll_back_to_the_shape_before_the_journal(&conn);
    }

    // The current build opens it.
    let db = temp.open();
    assert_eq!(db.schema_version(), latest_schema_version());
    db.record_journal_trades(&[TradeRow::opened(
        "t-1",
        MINT,
        Side::Buy,
        ExecutionMode::Live,
        1_000,
        AT_MS,
    )])
    .expect("the journal takes writes");
    db.close();

    let conn = temp.raw();
    // Every migration the rollback removed ran again, and each was recorded.
    // Against `latest_schema_version` rather than a literal, so adding a
    // migration does not mean editing a test about migration 3.
    let versions: Vec<i64> = conn
        .prepare("SELECT version FROM schema_migrations ORDER BY version")
        .expect("prepares")
        .query_map([], |row| row.get(0))
        .expect("runs")
        .collect::<Result<_, _>>()
        .expect("reads");
    assert_eq!(
        versions,
        (1..=latest_schema_version()).collect::<Vec<i64>>()
    );

    // And what was already in the file is untouched.
    let audit: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE event_type = 'older_build'",
            [],
            |row| row.get(0),
        )
        .expect("asks");
    assert_eq!(audit, 1, "the migration disturbed the ledger beside it");
}

#[test]
fn the_floats_in_an_older_file_come_forward_as_the_integers_they_meant() {
    // Migration 4's other half. The test above proves it runs on a file that
    // predates it; this one proves it carries what was in that file rather than
    // creating four empty tables beside the data.
    let temp = TempDb::new("migrate-floats");

    {
        let db = temp.open();
        db.record_clusters(&[ClusterRow {
            cluster_id: "cluster-a".to_string(),
            version: 1,
            root_wallet: "Root1111111111111111111111111111111111111111".to_string(),
            metrics: SybilClusterMetrics::new(9, 6_412, 910_000, 770_000, 120_000),
            flag_sybil: true,
            computed_at_ms: AT_MS,
        }])
        .expect("writes");
        db.record_tick_metrics(&[TickMetricRow {
            rpc_endpoint: "helius".to_string(),
            timestamp_ms: AT_MS,
            latency_ms: 40,
            dropped_msgs: 0,
            parsed_per_sec_micros: 812_500_000,
        }])
        .expect("writes");
        db.close();

        let conn = temp.raw();
        roll_back_to_the_shape_before_the_journal(&conn);
        // Read back through the old shape, so the seeding really did go in as
        // the floats an older build would have written.
        let temporal: f64 = conn
            .query_row("SELECT temporal_influence FROM clusters", [], |row| {
                row.get(0)
            })
            .expect("reads a float");
        assert!((temporal - 0.91).abs() < 1e-9, "{temporal}");
    }

    let db = temp.open();
    assert_eq!(db.schema_version(), latest_schema_version());
    db.close();

    let conn = temp.raw();
    let (temporal, spectral, entropy): (i64, i64, i64) = conn
        .query_row(
            "SELECT temporal_influence_micros, spectral_separation_micros, \
                    interaction_entropy_micros FROM clusters",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("reads them back as integers");
    assert_eq!((temporal, spectral, entropy), (910_000, 770_000, 120_000));

    let rate: i64 = conn
        .query_row(
            "SELECT parsed_per_sec_micros FROM tick_metrics",
            [],
            |row| row.get(0),
        )
        .expect("reads it back as an integer");
    assert_eq!(rate, 812_500_000, "812.5 messages a second, exactly");
}

#[test]
fn a_journal_migration_that_changed_after_it_shipped_is_refused() {
    let temp = TempDb::new("checksum");
    {
        let db = temp.open();
        db.close();
    }
    temp.raw()
        .execute(
            "UPDATE schema_migrations SET checksum = 'fnv1a64:0000000000000000' WHERE version = 3",
            [],
        )
        .expect("the checksum is tampered with");

    // `Database` is not `Debug`, so the error comes out of a match rather than
    // an `expect_err`.
    let message = match Database::open(&temp.0) {
        Ok(_) => panic!("the tampered file opened"),
        Err(err) => format!("{err}"),
    };
    assert!(message.contains("migration 3"), "{message}");
    assert!(
        message.contains("two different schemas"),
        "{message} does not say why it refused",
    );
}

#[test]
fn the_reopened_file_still_refuses_what_the_schema_refuses() {
    // The `CHECK`s and the trigger live in the file, not in the process that
    // created it, and this is the test that says so.
    let temp = TempDb::new("constraints-persist");
    {
        let db = temp.open();
        db.record_journal_trades(&[TradeRow::opened(
            "t-1",
            MINT,
            Side::Buy,
            ExecutionMode::Paper,
            1,
            AT_MS,
        )])
        .expect("writes");
        db.close();
    }

    let conn = temp.raw();
    assert!(
        conn.execute(
            "INSERT INTO journal_fills (trade_id, seq, tokens, lamports, fee_lamports,
                 price_q18, quoted_q18, slippage_bps, slot, at_ms)
             VALUES ('ghost', 0, 1, 1, 0, 1, 1, 0, 0, 0)",
            [],
        )
        .is_err(),
        "a fill with no trade was accepted by the reopened file",
    );
    assert!(
        conn.execute("UPDATE journal_trades SET mint = 'somewhere else'", [])
            .is_err(),
        "the identity trigger did not survive the reopen",
    );
}

// ---------------------------------------------------------------------------

/// Collects alerts, so the test can compare them with the rows.
#[derive(Default)]
struct Collector(parking_lot::Mutex<Vec<Alert>>);

impl AlertSink for Collector {
    fn deliver(&self, alert: &Alert) {
        self.0.lock().push(alert.clone());
    }

    fn name(&self) -> &str {
        "collector"
    }
}

#[test]
fn the_book_and_the_alert_feed_describe_the_same_bad_exit() {
    let temp = TempDb::new("agreement");
    let db = temp.open();
    let (_trade, fill, tip, signature) = record_a_bad_exit(&db, "t-1");

    let hub = Arc::new(TelemetryHub::start());
    let dispatcher = AlertDispatcher::new(Arc::clone(&hub));
    let collector = Arc::new(Collector::default());
    dispatcher.attach_sink(Arc::clone(&collector) as Arc<dyn AlertSink>);

    // The same three facts the journal just recorded, handed to the thresholds.
    let mut fired = Vec::new();
    fired.extend(dispatcher.observe(
        &Observation::Filled {
            trade_id: "t-1",
            mint: MINT,
            mode: ExecutionMode::Paper,
            fill: &fill,
            route_bound_bps: 300,
        },
        AT_MS + 400,
    ));
    fired.extend(dispatcher.observe(
        &Observation::Tipped {
            mint: MINT,
            mode: ExecutionMode::Paper,
            tip: &tip,
        },
        AT_MS + 100,
    ));
    fired.extend(dispatcher.observe(
        &Observation::Settled {
            trade_id: "t-1",
            mint: MINT,
            mode: ExecutionMode::Paper,
            status: signature.status,
            elapsed_ms: 40_000,
            rebroadcasts: 0,
        },
        AT_MS + 40_000,
    ));

    let kinds: Vec<AlertKind> = fired.iter().map(|a| a.kind).collect();
    assert_eq!(
        kinds,
        vec![
            AlertKind::SlippageSpike,
            AlertKind::TipOverrun,
            AlertKind::ExitFailed
        ],
        "a dropped exit that slipped and overtipped is three separate things to be told",
    );
    assert!(fired.iter().all(|a| a.severity == AlertSeverity::Critical));

    // Every alert names the number the row holds, in the row's own unit.
    let detail = db
        .journal_trade_detail("t-1")
        .expect("reads")
        .expect("is there");
    let slippage = &fired[0];
    assert_eq!(slippage.observed, u64::from(detail.fills[0].slippage_bps));
    assert_eq!(slippage.unit, AlertUnit::BasisPoints);
    let overrun = &fired[1];
    assert_eq!(overrun.observed, detail.tips[0].lamports);
    assert_eq!(overrun.threshold, detail.tips[0].ceiling_lamports);
    assert_eq!(overrun.unit, AlertUnit::Lamports);
    assert!(detail.tips[0].is_over_ceiling());

    // And what the sink received is what fired, not a copy that drifted.
    assert_eq!(collector.0.lock().clone(), fired);

    // The overruns query finds the same tip without a table scan.
    let overruns = db.journal_tip_overruns(10).expect("reads");
    assert_eq!(overruns, vec![detail.tips[0].clone()]);

    dispatcher.shutdown();
    hub.shutdown();
}

#[test]
fn a_quiet_run_raises_nothing_at_all() {
    // The other half of the test above, and the more common one: a trade that
    // went the way it was supposed to must produce an empty feed. An alerting
    // engine that cries on a good fill is one an operator learns to ignore.
    let temp = TempDb::new("quiet");
    let db = temp.open();
    let hub = Arc::new(TelemetryHub::start());
    let dispatcher = AlertDispatcher::new(Arc::clone(&hub));

    let trade = TradeRow::opened(
        "t-1",
        MINT,
        Side::Sell,
        ExecutionMode::Live,
        500_000_000,
        AT_MS,
    );
    db.record_journal_trades(&[trade]).expect("writes");
    // 1% under a quote on a route that accepted 3%.
    let fill = FillRow::settle(
        "t-1",
        0,
        1_000_000_000,
        495_000_000,
        5_000_000,
        500_000_000,
        1,
        AT_MS,
    )
    .expect("a fill");
    db.record_journal_fills(std::slice::from_ref(&fill))
        .expect("writes");

    assert!(dispatcher
        .observe(
            &Observation::Filled {
                trade_id: "t-1",
                mint: MINT,
                mode: ExecutionMode::Live,
                fill: &fill,
                route_bound_bps: 300,
            },
            AT_MS,
        )
        .is_empty());
    assert!(dispatcher
        .observe(
            &Observation::Settled {
                trade_id: "t-1",
                mint: MINT,
                mode: ExecutionMode::Live,
                status: SignatureStatus::Confirmed,
                elapsed_ms: 900,
                rebroadcasts: 0,
            },
            AT_MS,
        )
        .is_empty());

    let snapshot = dispatcher.snapshot();
    assert_eq!(snapshot.raised, 0);
    assert_eq!(snapshot.suppressed, 0);
    hub.shutdown();
}

#[test]
fn a_bad_minute_is_one_alert_and_a_count_of_the_rest() {
    // Forty fills on the same trade, all past the bound. The book keeps forty
    // rows and the operator is told once — which is the whole argument for the
    // cooldown, tested against the two of them together because it is only
    // wrong when they disagree.
    let temp = TempDb::new("storm");
    let db = temp.open();
    let hub = Arc::new(TelemetryHub::start());
    let dispatcher = AlertDispatcher::new(Arc::clone(&hub));
    dispatcher
        .set_thresholds(AlertThresholds {
            cooldown_ms: 60_000,
            ..AlertThresholds::default()
        })
        .expect("valid");

    db.record_journal_trades(&[TradeRow::opened(
        "t-1",
        MINT,
        Side::Sell,
        ExecutionMode::Paper,
        1_000_000,
        AT_MS,
    )])
    .expect("writes");

    let mut raised = 0usize;
    for seq in 0..40u32 {
        let fill = FillRow::settle(
            "t-1",
            seq,
            1_000_000,
            800_000,
            0,
            1_000_000,
            u64::from(seq),
            AT_MS + i64::from(seq) * 100,
        )
        .expect("a fill");
        db.record_journal_fills(std::slice::from_ref(&fill))
            .expect("writes");
        raised += dispatcher
            .observe(
                &Observation::Filled {
                    trade_id: "t-1",
                    mint: MINT,
                    mode: ExecutionMode::Paper,
                    fill: &fill,
                    route_bound_bps: 300,
                },
                AT_MS + i64::from(seq) * 100,
            )
            .len();
    }

    let detail = db
        .journal_trade_detail("t-1")
        .expect("reads")
        .expect("is there");
    assert_eq!(
        detail.fills.len(),
        40,
        "the book lost a fill to the cooldown"
    );
    assert_eq!(raised, 1, "the operator was told forty times");
    assert_eq!(dispatcher.snapshot().suppressed, 39);
    hub.shutdown();
}

// ---------------------------------------------------------------------------

#[test]
fn the_same_run_written_twice_is_the_same_bytes() {
    // Phase 3's acceptance criterion, against the journal. Two files, the same
    // sequence, no shared state — and because every key is the caller's rather
    // than an autoincrement, the two dumps have to match exactly.
    let first = TempDb::new("determinism-a");
    let second = TempDb::new("determinism-b");

    for temp in [&first, &second] {
        let db = temp.open();
        for index in 0..12u32 {
            let id = format!("t-{index}");
            record_a_bad_exit(&db, &id);
        }
        db.close();
    }

    let a = dump(&first.raw());
    let b = dump(&second.raw());
    assert_eq!(a, b, "two runs of the same sequence wrote different books");
    assert!(!a.contains("FLOAT:"), "a float reached the file:\n{a}");
    assert!(a.contains("t-11"), "the dump is empty");
}

#[test]
fn a_price_reaches_the_file_as_the_integer_it_was_computed_as() {
    let temp = TempDb::new("raw-price");
    let db = temp.open();
    db.record_journal_trades(&[TradeRow::opened(
        "t-1",
        MINT,
        Side::Buy,
        ExecutionMode::Paper,
        1,
        AT_MS,
    )])
    .expect("writes");

    // A ratio that does not terminate: 1/3 of a lamport per base unit, which is
    // the case a decimal column or a float would round and this must not.
    let fill = FillRow::settle("t-1", 0, 3, 1, 0, 1, 0, AT_MS).expect("a fill");
    assert_eq!(fill.price, Q18::ratio_floor(1, 3).expect("a ratio"));
    db.record_journal_fills(std::slice::from_ref(&fill))
        .expect("writes");
    db.close();

    // Read as an integer by something that has never heard of `Q18`.
    let conn = temp.raw();
    let (raw, kind): (i64, String) = conn
        .query_row(
            "SELECT price_q18, typeof(price_q18) FROM journal_fills WHERE trade_id = 't-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("asks");
    assert_eq!(kind, "integer", "the price is stored as a {kind}");
    assert_eq!(raw, 333_333_333_333_333_333);
    assert_eq!(Q18::from_i64_raw(raw), Some(fill.price));
}

#[test]
fn heavy_ingestion_loses_neither_a_row_nor_an_alert() {
    // Six threads, each recording its own trade's fills and holding every one
    // against the thresholds, through one `Database` and one dispatcher. The
    // point is that the two shared things — SQLite's single writer and the
    // cooldown map — are both correct under contention, not just fast.
    const THREADS: u32 = 6;
    const PER_THREAD: u32 = 60;

    let temp = TempDb::new("load");
    let db = Arc::new(temp.open());
    let hub = Arc::new(TelemetryHub::start());
    let dispatcher = Arc::new(AlertDispatcher::new(Arc::clone(&hub)));
    // No cooldown, so every fill past the bound is expected to be delivered and
    // the arithmetic below is exact rather than approximate.
    dispatcher
        .set_thresholds(AlertThresholds {
            cooldown_ms: 0,
            ..AlertThresholds::default()
        })
        .expect("valid");
    let collector = Arc::new(Collector::default());
    dispatcher.attach_sink(Arc::clone(&collector) as Arc<dyn AlertSink>);

    let trades: Vec<TradeRow> = (0..THREADS)
        .map(|t| {
            TradeRow::opened(
                format!("t-{t}"),
                MINT,
                Side::Sell,
                ExecutionMode::Paper,
                1,
                AT_MS,
            )
        })
        .collect();
    db.record_journal_trades(&trades).expect("writes");

    let mut handles = Vec::new();
    for thread in 0..THREADS {
        let db = Arc::clone(&db);
        let dispatcher = Arc::clone(&dispatcher);
        handles.push(std::thread::spawn(move || {
            let id = format!("t-{thread}");
            let mut fired = 0usize;
            for seq in 0..PER_THREAD {
                // Every third fill is a bad one, so the expected alert count is
                // a number this test can state rather than observe.
                let bad = seq % 3 == 0;
                let filled = if bad { 800_000 } else { 995_000 };
                let at = AT_MS + i64::from(seq);
                let fill = FillRow::settle(
                    &id,
                    seq,
                    1_000_000,
                    filled,
                    0,
                    1_000_000,
                    u64::from(seq),
                    at,
                )
                .expect("a fill");
                db.record_journal_fills(std::slice::from_ref(&fill))
                    .expect("writes");
                fired += dispatcher
                    .observe(
                        &Observation::Filled {
                            trade_id: &id,
                            mint: MINT,
                            mode: ExecutionMode::Paper,
                            fill: &fill,
                            route_bound_bps: 300,
                        },
                        at,
                    )
                    .len();
            }
            fired
        }));
    }

    let fired: usize = handles
        .into_iter()
        .map(|h| h.join().expect("no panic"))
        .sum();
    let expected = (0..PER_THREAD).filter(|seq| seq % 3 == 0).count() * THREADS as usize;
    assert_eq!(fired, expected, "an alert was lost or invented under load");
    assert_eq!(
        collector.0.lock().len(),
        expected,
        "the sink and the caller disagree"
    );
    assert_eq!(dispatcher.snapshot().raised as usize, expected);

    let conn = temp.raw();
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM journal_fills", [], |row| row.get(0))
        .expect("counts");
    assert_eq!(
        rows,
        i64::from(THREADS * PER_THREAD),
        "a fill was lost under contention"
    );

    // Every sequence number the dispatcher handed out is distinct, which is
    // what makes a gap in them mean a dropped delivery rather than a race.
    let seqs: BTreeMap<u64, ()> = collector
        .0
        .lock()
        .iter()
        .map(|alert| (alert.seq, ()))
        .collect();
    assert_eq!(seqs.len(), expected, "two alerts share a sequence number");

    dispatcher.shutdown();
    hub.shutdown();
}

#[test]
fn a_webhook_carries_the_alert_off_the_engine_thread() {
    // The property that matters is not that the POST arrives — `alerting.rs`
    // covers that — but that the engine's thread does not wait for it. The
    // endpoint here accepts and then sits on the connection for longer than the
    // observation is allowed to take.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
    let address = listener.local_addr().expect("has an address");
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accepts");
        std::thread::sleep(Duration::from_millis(750));
        drop(stream);
    });

    let hub = Arc::new(TelemetryHub::start());
    let dispatcher = AlertDispatcher::new(Arc::clone(&hub));
    dispatcher
        .attach_webhook(&sts_lib::alerting::WebhookConfig {
            url: format!("http://{address}/hook"),
            timeout_ms: 2_000,
            queue_depth: 8,
            ..sts_lib::alerting::WebhookConfig::default()
        })
        .expect("starts");

    let started = std::time::Instant::now();
    let fired = dispatcher.observe(
        &Observation::Settled {
            trade_id: "t-1",
            mint: MINT,
            mode: ExecutionMode::Live,
            status: SignatureStatus::Failed,
            elapsed_ms: 1,
            rebroadcasts: 0,
        },
        AT_MS,
    );
    let elapsed = started.elapsed();
    assert_eq!(fired.len(), 1);
    assert!(
        elapsed < Duration::from_millis(500),
        "observing took {elapsed:?}, so the engine waited for the webhook",
    );

    dispatcher.shutdown();
    hub.shutdown();
    server.join().expect("the server did not panic");
}
