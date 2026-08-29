//! Walk-forward evaluation: a purge, an embargo, a group split, and the stress
//! grid measured on the far side of them.
//!
//! `backtest.rs` prices a whole corpus at once and reports what it was worth.
//! That number is in-sample by construction, and the roadmap's Phase 3 is
//! explicit that an in-sample number may not promote anything. This module is
//! the split that makes an out-of-sample number possible, and the assertions
//! that make it checkable.
//!
//! Four things are load-bearing.
//!
//! **Nothing is fitted here, and the report says so.** No parameter in this
//! engine is estimated from data yet — `strategy::entry` ships with a zero edge
//! and refuses every trade precisely because no holdout has produced a number to
//! put in that field. So the training side of every fold is reported and is not
//! learned from. That is not a weakness of the harness; it is the harness
//! arriving before the thing it exists to constrain, which is the only order in
//! which a leakage barrier is worth anything. When a fitted policy does arrive
//! the split is already here, already asserted, and already refusing runs that
//! violate it.
//!
//! **The split is two constraints, not one.**
//! `REPLAY_AND_SIMULATION_SPEC.md` §22: folds are ordered by time with a purge
//! and an embargo, *and* whole funder groups go to one side. Either alone is
//! not a split of this corpus. 45.3% of its wallets appear in more than one
//! launch and one appears in 1 829 of them, so a time cut alone leaves half the
//! wallet population on both sides; and a group cut alone leaves a model
//! trained on tomorrow.
//!
//! **What cannot be driven to zero is measured instead.** Wallet overlap
//! between folds is a report, never an assertion, for the reason §22 gives: the
//! 1 829-launch wallet is in every fold however the corpus is cut. A number
//! computed on folds with 30% overlap is not the same number as one computed on
//! folds with 5%, so the overlap sits beside every metric rather than being
//! quietly assumed away. That is property R21.
//!
//! **The stress grid is a floor under the cost, not an estimate of it.** Each
//! cell haircuts the exit leg by a gap and then by an execution drag, compounded
//! rather than added, which is how `strategy::entry::stress` composes the same
//! two buckets. What it does not do is re-quote the exit through the thinner
//! pool the gap leaves behind, and that impact is a cost this grid omits. The
//! omission points the flattering way, so `StressCell::optimistic` is on every
//! cell that carries a haircut and exists to be impossible to quote without
//! seeing.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::backtest::{
    fixture_files, floor_div_i128, isqrt, lamports_to_usd_cents, mul_div_ceil, mul_div_floor,
    read_manifest, return_moments, summarise_performance, summarise_risk, summarise_rug,
    BacktestConfig, BacktestError, ClosedTrade, Evaluator, IntegritySummary, LaunchReport,
    PerformanceSummary, RiskSummary, RugClass, RugSummary, StrandedPosition, MICROS,
};
use crate::replay::BPS_DENOMINATOR;
use crate::strategy::entry::{GAP_BUCKETS_BPS, SLIPPAGE_BUCKETS_BPS};

/// The schema string on the report this module emits.
pub const REPORT_SCHEMA: &str = "sts.walkforward.report.v1";

/// Blocks the corpus is cut into. The first is training-only, so `k` blocks
/// produce `k - 1` test folds.
pub const DEFAULT_FOLDS: usize = 5;

/// §22: "one hour is the default, and it is policy."
pub const DEFAULT_EMBARGO_MS: i64 = 3_600_000;

/// The family-wise error rate the per-test level is derived from. 5%.
pub const DEFAULT_FAMILY_ALPHA_BPS: u16 = 500;

/// The tail the conditional value at risk is taken over, in percent.
pub const DEFAULT_CVAR_PCT: u32 = 5;

/// One-sided normal quantiles, `(alpha in millionths, z in millionths)`,
/// largest alpha first.
///
/// A table rather than an inverse-error-function call, for the reason every
/// other transcendental in this crate carries its own implementation: two runs
/// of one fixture have to produce the same bytes, and the last digit of a
/// `f64::erf_inv` is a property of the host's libm.
///
/// [`one_sided_z_micros`] reads it conservatively — the level actually used is
/// never laxer than the level asked for — so a quantile between two rows is
/// answered with the stricter row rather than interpolated.
const Z_ONE_SIDED: [(u64, u64); 14] = [
    (250_000, 674_490),
    (100_000, 1_281_552),
    (50_000, 1_644_854),
    (25_000, 1_959_964),
    (10_000, 2_326_348),
    (5_000, 2_575_829),
    (2_500, 2_807_034),
    (1_000, 3_090_232),
    (500, 3_290_527),
    (250, 3_480_756),
    (100, 3_719_016),
    (50, 3_890_592),
    (10, 4_264_891),
    (1, 4_753_424),
];

/// The one-sided `z` for a tail of `alpha_micros`, and whether the table ran out.
///
/// Conservative in both directions: an alpha between two rows is answered with
/// the row whose `z` is larger, and an alpha below the smallest row is answered
/// with the largest `z` in the table and a `true` that says the number is a
/// floor rather than the quantile asked for. A lower confidence bound built on
/// a `z` that is too large is too low, which is the direction a bound is allowed
/// to be wrong in.
pub fn one_sided_z_micros(alpha_micros: u64) -> (u64, bool) {
    for &(alpha, z) in Z_ONE_SIDED.iter() {
        if alpha <= alpha_micros {
            return (z, false);
        }
    }
    let (_, z) = Z_ONE_SIDED[Z_ONE_SIDED.len() - 1];
    (z, true)
}

// ===========================================================================
// Grouping
// ===========================================================================

/// What decides which side of a fold a launch goes to, beyond its time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GroupBy {
    /// §22: the root funder, with unresolved records grouped by deployer.
    Funder,
    /// The deployer alone. Weaker, and here because a corpus whose funding never
    /// resolved has nothing else to group on.
    Deployer,
    /// Every launch is its own group. The constraint is not applied, the report
    /// says so, and R8 holds for a reason that proves nothing.
    None,
}

impl GroupBy {
    pub const fn as_str(self) -> &'static str {
        match self {
            GroupBy::Funder => "funder",
            GroupBy::Deployer => "deployer",
            GroupBy::None => "none",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "funder" => Some(GroupBy::Funder),
            "deployer" => Some(GroupBy::Deployer),
            "none" => Some(GroupBy::None),
            _ => None,
        }
    }
}

/// Which kind of key a group was named by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GroupSource {
    Funder,
    Deployer,
    /// Neither resolved, so the launch is a group of one and is reported as
    /// such. Not "independent" — unknown.
    Mint,
}

/// One launch, as the splitter sees it.
///
/// Produced by [`Evaluator::cohorts`](crate::backtest::Evaluator::cohorts)
/// rather than read off a [`LaunchReport`]: a report carries the clusters that
/// cleared the reporting floor, and a split needs every wallet and every funder
/// the recording saw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchCohort {
    pub mint: String,
    pub first_event_ms: i64,
    pub last_event_ms: i64,
    pub creator: Option<String>,
    /// Distinct funders behind this launch's buyers, sorted.
    pub funders: Vec<String>,
    /// Distinct buyer wallets, sorted.
    pub wallets: Vec<String>,
}

/// A set of launches that may not be split.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchGroup {
    /// The smallest key in the component, so the name is a function of the
    /// membership rather than of the order the launches arrived in.
    pub id: String,
    pub source: GroupSource,
    /// Sorted.
    pub mints: Vec<String>,
}

/// Groups launches so that no group can land on both sides of a fold.
///
/// Under [`GroupBy::Funder`] this is the connected components of the graph whose
/// vertices are launches and whose edges are shared keys, where a launch's keys
/// are its funders if any resolved, its deployer if not, and its own mint if
/// neither. Components rather than "the largest funder", because a launch with
/// three funders shares each of them with somebody: picking one and ignoring the
/// rest would satisfy the assertion by narrowing what it asserts.
///
/// The keys are namespaced — `funder:`, `deployer:`, `mint:` — so a deployer
/// that is also somebody's funder cannot fuse two components that share no
/// actual party. §22 puts unresolved records in a deployer group and funded
/// records in a funder group; that is two grouping rules, and two rules that
/// index the same address space are one rule with a collision in it.
///
/// A component that swallows the corpus is a real outcome and not an error:
/// §22's own example is a funder in 1 107 records. The caller sees it in
/// [`GroupingSummary::largest_group_launches`] and every fold it empties is
/// reported as emptied.
pub fn group_launches(cohorts: &[LaunchCohort], by: GroupBy) -> Vec<LaunchGroup> {
    let keys_of = |cohort: &LaunchCohort| -> Vec<String> {
        match by {
            GroupBy::None => vec![format!("mint:{}", cohort.mint)],
            GroupBy::Deployer => match &cohort.creator {
                Some(creator) => vec![format!("deployer:{creator}")],
                None => vec![format!("mint:{}", cohort.mint)],
            },
            GroupBy::Funder => {
                if !cohort.funders.is_empty() {
                    cohort
                        .funders
                        .iter()
                        .map(|funder| format!("funder:{funder}"))
                        .collect()
                } else if let Some(creator) = &cohort.creator {
                    vec![format!("deployer:{creator}")]
                } else {
                    vec![format!("mint:{}", cohort.mint)]
                }
            }
        }
    };

    // Union-find over launch indices, keyed by the shared key.
    let mut parent: Vec<usize> = (0..cohorts.len()).collect();
    fn find(parent: &mut [usize], mut index: usize) -> usize {
        while parent[index] != index {
            parent[index] = parent[parent[index]];
            index = parent[index];
        }
        index
    }

    let mut first_seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut launch_keys: Vec<Vec<String>> = Vec::with_capacity(cohorts.len());
    for (index, cohort) in cohorts.iter().enumerate() {
        let keys = keys_of(cohort);
        for key in &keys {
            match first_seen.get(key) {
                Some(&other) => {
                    let a = find(&mut parent, index);
                    let b = find(&mut parent, other);
                    if a != b {
                        // Towards the smaller index, so the representative of a
                        // component is a function of the component.
                        if a < b {
                            parent[b] = a;
                        } else {
                            parent[a] = b;
                        }
                    }
                }
                None => {
                    first_seen.insert(key.clone(), index);
                }
            }
        }
        launch_keys.push(keys);
    }

    let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for index in 0..cohorts.len() {
        let root = find(&mut parent, index);
        members.entry(root).or_default().push(index);
    }

    let mut groups: Vec<LaunchGroup> = Vec::with_capacity(members.len());
    for (_, indices) in members {
        let mut keys: BTreeSet<&str> = BTreeSet::new();
        for &index in &indices {
            for key in &launch_keys[index] {
                keys.insert(key.as_str());
            }
        }
        // The smallest key names the group. Funders sort before mints and
        // deployers under this prefixing only by accident, so the source is read
        // off the key that won rather than assumed.
        let id = keys
            .iter()
            .next()
            .map(|key| (*key).to_string())
            .unwrap_or_else(|| format!("mint:{}", cohorts[indices[0]].mint));
        let source = if id.starts_with("funder:") {
            GroupSource::Funder
        } else if id.starts_with("deployer:") {
            GroupSource::Deployer
        } else {
            GroupSource::Mint
        };
        let mut mints: Vec<String> = indices
            .iter()
            .map(|&index| cohorts[index].mint.clone())
            .collect();
        mints.sort();
        groups.push(LaunchGroup { id, source, mints });
    }
    groups.sort_by(|a, b| a.id.cmp(&b.id));
    groups
}

// ===========================================================================
// Configuration
// ===========================================================================

/// Everything the split and the grid are a function of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalkForwardConfig {
    /// Blocks the corpus is cut into. The first is training-only.
    pub folds: usize,
    /// Extra margin on the purge, in milliseconds. The purge itself is
    /// unconditional: a training launch whose outcome window reaches the test
    /// window's start is removed whatever this is.
    pub purge_ms: i64,
    pub embargo_ms: i64,
    pub group_by: GroupBy,
    /// The gap buckets, in basis points, applied to the exit leg.
    pub gaps_bps: Vec<u16>,
    /// The execution-drag buckets, in basis points.
    pub slippage_bps: Vec<u16>,
    /// The tail the CVaR is taken over, in percent.
    pub cvar_pct: u32,
    /// The family-wise error rate, in basis points.
    pub family_alpha_bps: u16,
    /// The pricing configuration the corpus was evaluated under.
    pub backtest: BacktestConfig,
}

impl Default for WalkForwardConfig {
    fn default() -> Self {
        WalkForwardConfig {
            folds: DEFAULT_FOLDS,
            purge_ms: 0,
            embargo_ms: DEFAULT_EMBARGO_MS,
            group_by: GroupBy::Funder,
            gaps_bps: GAP_BUCKETS_BPS.to_vec(),
            slippage_bps: SLIPPAGE_BUCKETS_BPS.to_vec(),
            cvar_pct: DEFAULT_CVAR_PCT,
            family_alpha_bps: DEFAULT_FAMILY_ALPHA_BPS,
            backtest: BacktestConfig::default(),
        }
    }
}

// ===========================================================================
// The report
// ===========================================================================

/// One fixture directory that went into the corpus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorpusPart {
    pub source: String,
    pub launches: u32,
    pub integrity: IntegritySummary,
    pub gate_ready: bool,
    /// Why this part may not back a number. Empty when it may.
    pub refusals: Vec<String>,
}

/// What was read, and whether it can back a number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorpusSummary {
    pub parts: Vec<CorpusPart>,
    pub launches: u32,
    pub buyers: u32,
    pub first_event_ms: Option<i64>,
    pub last_event_ms: Option<i64>,
    pub span_ms: i64,
    pub gate_ready: bool,
}

/// How the corpus fell into groups.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupingSummary {
    pub group_by: GroupBy,
    /// False under [`GroupBy::None`], where R8 holds vacuously.
    pub applied: bool,
    pub groups: u32,
    pub largest_group_launches: u32,
    /// The largest group over all launches, in basis points. A corpus that is
    /// one group cannot be split at all, and this is the number that says so.
    pub largest_group_share_bps: u16,
    pub grouped_by_funder: u32,
    pub grouped_by_deployer: u32,
    pub grouped_by_mint: u32,
}

/// The level every lower bound in the report is taken at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultipleTesting {
    /// Always `bonferroni` for now. Named so a later change to Benjamini–
    /// Hochberg is visible in the report rather than inferred from the numbers.
    pub method: String,
    /// Every cell in every fold, plus the pooled cells. A grid searched for the
    /// bucket that clears is a grid that will find one.
    pub tests: u32,
    pub family_alpha_bps: u16,
    pub per_test_alpha_micros: u64,
    pub z_micros: u64,
    /// The requested level was below the smallest row of the quantile table, so
    /// `z_micros` is a floor and every bound built on it is conservative by an
    /// unknown margin.
    pub beyond_table: bool,
}

/// One side of one fold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoldSide {
    pub launches: u32,
    /// Sorted, so two runs list them the same way.
    pub mints: Vec<String>,
    pub groups: u32,
    pub wallets: u32,
    pub trades: u32,
    pub first_event_ms: Option<i64>,
    pub last_event_ms: Option<i64>,
}

/// What the training side lost, and to which rule.
///
/// Classified by the first rule that removed a launch, so the counts add up to
/// the launches dropped and no launch is counted twice. With the default
/// one-hour embargo the purge removes nothing — it is subsumed — and the zero
/// there is the evidence for that sentence rather than an absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoldExclusions {
    pub candidates: u32,
    pub purged: u32,
    pub embargoed: u32,
    pub group_excluded: u32,
    pub kept: u32,
}

/// R21: how much of the test fold's wallet population the training side also saw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletOverlap {
    pub train_wallets: u32,
    pub test_wallets: u32,
    pub shared_wallets: u32,
    /// Shared over the test fold's own wallets, in basis points. `None` when the
    /// test fold recorded no buyers at all, which is UNKNOWN and not zero.
    pub share_of_test_bps: Option<u16>,
}

/// R7 and R8, as they came out on this fold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoldAssertions {
    /// `max(train.last_event_ms) + embargo_ms <= min(test.first_event_ms)`.
    pub embargo_holds: bool,
    /// How much room the embargo had, in milliseconds. Negative is a breach.
    /// `None` when the training side is empty and the assertion is vacuous.
    pub embargo_margin_ms: Option<i64>,
    /// `funder_groups(train) ∩ funder_groups(test) == ∅`.
    pub groups_disjoint: bool,
    /// The groups on both sides, if any. Sorted, and present so a breach names
    /// itself rather than being a `false`.
    pub shared_groups: Vec<String>,
}

impl FoldAssertions {
    pub fn holds(&self) -> bool {
        self.embargo_holds && self.groups_disjoint
    }
}

/// Why a launch contributed no trade.
///
/// The roadmap asks for zero-trade periods to be decomposed rather than
/// reported as a zero. The four counts partition the fold's launches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZeroTradeDecomposition {
    pub launches: u32,
    /// The recording holds no entry for this launch at all.
    pub no_entry_recorded: u32,
    /// An entry was recorded and the curve refused to price it.
    pub entry_refused_by_curve: u32,
    /// Entered, and the stream ended with the position still open.
    pub entered_and_stranded: u32,
    /// Entered and closed at least one parcel.
    pub entered_and_closed: u32,
}

/// How often there was no way out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitAvailability {
    pub positions_opened: u32,
    pub positions_stranded: u32,
    pub no_executable_exits: u32,
    /// No-executable-exits over positions opened, in basis points. `None` when
    /// nothing was opened.
    pub no_exit_rate_bps: Option<u16>,
}

/// One launch cohort's contribution, for the breakdowns doctrine asks be kept
/// apart from the headline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CohortCell {
    pub cohort: RugClass,
    pub launches: u32,
    pub entered: u32,
    pub trades: u32,
    pub realized_pnl_lamports: i64,
}

/// One `(gap, slippage)` cell of the stress grid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StressCell {
    pub gap_bps: u16,
    pub slippage_bps: u16,
    pub trades: u32,
    pub winners: u32,
    pub stressed_pnl_lamports: i64,
    pub stressed_pnl_usd_cents: i64,
    /// Stressed PnL over starting equity, in basis points.
    pub return_on_equity_bps: i32,
    /// Mean per-trade return, in millionths of a basis point.
    pub mean_return_bps_micros: i64,
    pub stddev_return_bps_micros: u64,
    /// The one-sided lower confidence bound on the mean, at the level
    /// [`MultipleTesting`] reports. `None` under two trades, where there is no
    /// dispersion to bound with.
    pub ev_lcb_bps_micros: Option<i64>,
    /// `ev_lcb_bps_micros > 0`. False when the bound is absent, because an
    /// unmeasured expectancy is not a positive one.
    pub positive_ev_lcb: bool,
    /// The mean of the worst `cvar_pct` of returns, in basis points.
    pub cvar_bps: Option<i32>,
    pub worst_return_bps: Option<i32>,
    /// Trades the haircut left with nothing coming back at all.
    pub no_proceeds_trades: u32,
    /// True on every cell that carries a haircut: the grid does not re-quote the
    /// exit through the thinner pool the gap leaves, so the cost is a floor.
    /// False on the `(0, 0)` baseline, which is the run as it was priced.
    pub optimistic: bool,
}

/// Everything measured on one side of one fold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SideMetrics {
    pub performance: PerformanceSummary,
    pub risk: RiskSummary,
    pub rug: RugSummary,
    pub zero_trade: ZeroTradeDecomposition,
    pub exits: ExitAvailability,
    pub cohorts: Vec<CohortCell>,
    pub stress: Vec<StressCell>,
}

/// One walk-forward fold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fold {
    /// One-based, and it counts test folds rather than blocks: fold 1 tests the
    /// second block. The first block is training-only and is never a test set,
    /// because a walk-forward's first block has no past to be trained on.
    pub index: usize,
    pub train: FoldSide,
    pub test: FoldSide,
    pub exclusions: FoldExclusions,
    pub wallet_overlap: WalletOverlap,
    pub assertions: FoldAssertions,
    /// Reported so a train/test divergence is visible. Nothing is fitted on it.
    pub train_metrics: SideMetrics,
    pub test_metrics: SideMetrics,
}

/// Every test fold's launches, taken together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PooledOutOfSample {
    pub folds: u32,
    pub launches: u32,
    pub trades: u32,
    pub metrics: SideMetrics,
    /// Test folds whose training side ended up empty.
    ///
    /// Not a refusal, because nothing in this engine is fitted yet and an empty
    /// training side costs a measurement that is not being made. It is here so
    /// that it is impossible to read the pooled numbers as evidence a split was
    /// exercised when it was not — and so that the day a policy is fitted, this
    /// being non-zero is the first thing that has to become a refusal.
    pub folds_without_training: u32,
    /// The worst cell of the pooled grid, by lower bound. `None` when no cell
    /// produced one.
    pub worst_cell: Option<StressCell>,
    /// Every cell of the pooled grid cleared zero on its lower bound. The
    /// roadmap's Gate 6A condition, and false whenever any cell is unmeasured.
    pub stressed_ev_lcb_positive: bool,
}

/// The whole thing.
///
/// No timestamp, no host, no elapsed time, and every collection in it sorted:
/// property R1 applies to this report exactly as it applies to
/// `backtest::ForensicReport`, and for the same reason — a report that cannot be
/// diffed cannot be the evidence a gate turns on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalkForwardReport {
    pub schema: String,
    pub source: String,
    pub config: WalkForwardConfig,
    pub corpus: CorpusSummary,
    pub grouping: GroupingSummary,
    pub multiple_testing: MultipleTesting,
    pub folds: Vec<Fold>,
    pub pooled: PooledOutOfSample,
    /// Why this report may not back a gate dossier. Empty when it may.
    pub refusals: Vec<String>,
    /// Whether the *evidence* is admissible: the corpus verified, every fold's
    /// split assertions held, and no fold was empty. Deliberately not a verdict
    /// on the economics — a corpus can be perfectly admissible and say the
    /// strategy loses money, and conflating the two is how a gate starts
    /// answering the wrong question.
    pub gate_ready: bool,
}

impl WalkForwardReport {
    /// The report as indented JSON, ending in a newline.
    pub fn to_json(&self) -> String {
        let mut text = serde_json::to_string_pretty(self)
            .unwrap_or_else(|err| format!("{{\"error\":\"{err}\"}}"));
        text.push('\n');
        text
    }
}

// ===========================================================================
// Reading a corpus
// ===========================================================================

/// A corpus, and the launches and cohorts it produced.
#[derive(Debug, Clone)]
pub struct Corpus {
    pub source: String,
    pub parts: Vec<CorpusPart>,
    pub launches: Vec<LaunchReport>,
    pub cohorts: Vec<LaunchCohort>,
    pub refusals: Vec<String>,
}

/// Reads one fixture directory: its manifest, its streams, its launches and its
/// cohorts, from one pass of one evaluator.
///
/// `backtest::evaluate_directory` does the same walk and returns only the
/// report. The cohorts have to come off the same evaluator as the report or the
/// two would describe different runs, and `finish` consumes it — hence the
/// duplication of the directory walk rather than a second pass over the files.
pub fn read_fixture_dir(dir: &Path, config: BacktestConfig) -> Result<Corpus, BacktestError> {
    let manifest = read_manifest(dir)?;
    let files = fixture_files(dir)?;
    let mut evaluator = Evaluator::new(config);
    if let Some(manifest) = manifest.clone() {
        evaluator = evaluator.with_manifest(manifest);
    }
    for path in files {
        let text = std::fs::read_to_string(&path).map_err(|err| BacktestError::Io {
            path: path.display().to_string(),
            detail: err.to_string(),
        })?;
        let stream_id = match &manifest {
            Some(manifest) => manifest.stream_id.clone(),
            None => path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("stream")
                .to_string(),
        };
        let file = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("stream.jsonl")
            .to_string();
        evaluator.ingest(&stream_id, &file, &text);
    }

    let cohorts = evaluator.cohorts();
    let source = dir.display().to_string();
    let report = evaluator.finish(&source);
    Ok(Corpus {
        parts: vec![CorpusPart {
            source: source.clone(),
            launches: report.launches.len() as u32,
            integrity: report.integrity.clone(),
            gate_ready: report.gate_ready,
            refusals: report.refusals.clone(),
        }],
        source,
        launches: report.launches,
        cohorts,
        refusals: Vec::new(),
    })
}

/// Reads a corpus: a directory of streams, or a directory of such directories.
///
/// The generator writes one case per directory and one launch per case, so a
/// corpus large enough to have folds in it is a directory of directories. Both
/// shapes are read here rather than making the caller know which it has: if the
/// directory holds `.jsonl` files it is one fixture directory, and if it holds
/// only subdirectories each of them is read as a fixture directory of its own
/// and the launches are unioned.
///
/// Each subdirectory keeps its own manifest and its own chain. That is the point
/// of reading them separately — a corpus is several recordings, and pretending
/// several chains are one is how a rotation boundary and a forgery start looking
/// the same.
pub fn read_corpus(dir: &Path, config: BacktestConfig) -> Result<Corpus, BacktestError> {
    match fixture_files(dir) {
        Ok(_) => return read_fixture_dir(dir, config),
        Err(BacktestError::NoFixtures { .. }) => {}
        Err(err) => return Err(err),
    }

    let entries = std::fs::read_dir(dir).map_err(|err| BacktestError::Io {
        path: dir.display().to_string(),
        detail: err.to_string(),
    })?;
    let mut children: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| BacktestError::Io {
            path: dir.display().to_string(),
            detail: err.to_string(),
        })?;
        let path = entry.path();
        if path.is_dir() {
            children.push(path);
        }
    }
    // Sorted, because `read_dir` hands them back in whatever order the
    // filesystem felt like and a corpus whose part order is the filesystem's is
    // a corpus that is not reproducible on another machine.
    children.sort();
    if children.is_empty() {
        return Err(BacktestError::NoFixtures {
            path: dir.display().to_string(),
        });
    }

    let mut corpus = Corpus {
        source: dir.display().to_string(),
        parts: Vec::new(),
        launches: Vec::new(),
        cohorts: Vec::new(),
        refusals: Vec::new(),
    };
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for child in children {
        let part = match read_corpus(&child, config) {
            Ok(part) => part,
            // A subdirectory with no streams under it is not a fixture
            // directory. Skipped rather than fatal: a corpus directory is
            // allowed to hold a `reports/` beside its cases.
            Err(BacktestError::NoFixtures { .. }) => continue,
            Err(err) => return Err(err),
        };
        // A mint read twice is a launch weighted twice. Refused *and* dropped:
        // the refusal is what stops the report being quoted, and the drop is
        // what stops the arithmetic underneath it being nonsense in the
        // meantime. Keeping the first occurrence rather than the last, so the
        // choice is the directory order the parts were read in and not the
        // filesystem's opinion.
        let mut duplicates: BTreeSet<String> = BTreeSet::new();
        for launch in &part.launches {
            if let Some(first) = seen.get(&launch.mint) {
                corpus.refusals.push(format!(
                    "{}: the mint {} was already read from {first} and this copy is dropped; a \
                     launch counted twice is a launch weighted twice",
                    part.source, launch.mint
                ));
                duplicates.insert(launch.mint.clone());
            } else {
                seen.insert(launch.mint.clone(), part.source.clone());
            }
        }
        corpus.parts.extend(part.parts);
        corpus.launches.extend(
            part.launches
                .into_iter()
                .filter(|launch| !duplicates.contains(&launch.mint)),
        );
        corpus.cohorts.extend(
            part.cohorts
                .into_iter()
                .filter(|cohort| !duplicates.contains(&cohort.mint)),
        );
        corpus.refusals.extend(part.refusals);
    }
    if corpus.parts.is_empty() {
        return Err(BacktestError::NoFixtures {
            path: dir.display().to_string(),
        });
    }
    Ok(corpus)
}

// ===========================================================================
// The harness
// ===========================================================================

/// Runs a walk-forward over a corpus already in memory.
///
/// Separated from the reading so the whole harness is testable from a list of
/// launches and cohorts, with no filesystem and no fixture format in the way.
pub fn evaluate(corpus: &Corpus, config: &WalkForwardConfig) -> WalkForwardReport {
    let by_mint: BTreeMap<&str, &LaunchReport> = corpus
        .launches
        .iter()
        .map(|launch| (launch.mint.as_str(), launch))
        .collect();
    let cohort_of: BTreeMap<&str, &LaunchCohort> = corpus
        .cohorts
        .iter()
        .map(|cohort| (cohort.mint.as_str(), cohort))
        .collect();

    let mut refusals = corpus.refusals.clone();
    let corpus_summary = summarise_corpus(corpus);
    if !corpus_summary.gate_ready {
        refusals.push("the corpus did not fully verify".to_string());
    }

    let groups = group_launches(&corpus.cohorts, config.group_by);
    let mut group_of: BTreeMap<&str, &str> = BTreeMap::new();
    for group in &groups {
        for mint in &group.mints {
            group_of.insert(mint.as_str(), group.id.as_str());
        }
    }
    let grouping = summarise_grouping(&groups, corpus.cohorts.len(), config.group_by);

    // Time order, ties broken by mint so the cut is a total order.
    let mut ordered: Vec<&LaunchCohort> = corpus.cohorts.iter().collect();
    ordered.sort_by(|a, b| {
        a.first_event_ms
            .cmp(&b.first_event_ms)
            .then_with(|| a.mint.cmp(&b.mint))
    });

    let blocks = cut_blocks(ordered.len(), config.folds);
    if config.folds < 2 {
        refusals.push(format!(
            "{} fold(s) leaves nothing to test: the first block is training-only, so a \
             walk-forward needs at least two",
            config.folds
        ));
    }
    if ordered.len() < config.folds {
        refusals.push(format!(
            "the corpus holds {} launch(es) and {} fold(s) were asked for; a block cannot hold \
             less than one launch",
            ordered.len(),
            config.folds
        ));
    }

    let cells = grid(config);
    let test_folds = blocks.len().saturating_sub(1);
    // Every cell in every fold, and the pooled grid on top. A grid searched for
    // the bucket that clears is a grid that will find one, so the level is
    // divided by how many were looked at.
    let tests = ((test_folds as u64 + 1) * cells.len() as u64).max(1);
    let per_test_alpha_micros =
        u64::from(config.family_alpha_bps) * (MICROS / u64::from(BPS_DENOMINATOR)) / tests;
    let (z_micros, beyond_table) = one_sided_z_micros(per_test_alpha_micros);
    let multiple_testing = MultipleTesting {
        method: "bonferroni".to_string(),
        tests: tests.min(u64::from(u32::MAX)) as u32,
        family_alpha_bps: config.family_alpha_bps,
        per_test_alpha_micros,
        z_micros,
        beyond_table,
    };

    let mut folds: Vec<Fold> = Vec::with_capacity(test_folds);
    let mut pooled_mints: Vec<String> = Vec::new();
    for index in 1..blocks.len() {
        let test: Vec<&LaunchCohort> = blocks[index].iter().map(|&i| ordered[i]).collect();
        let candidates: Vec<&LaunchCohort> = blocks[..index]
            .iter()
            .flat_map(|block| block.iter().map(|&i| ordered[i]))
            .collect();

        let test_start = test.iter().map(|c| c.first_event_ms).min();
        let test_groups: BTreeSet<&str> = test
            .iter()
            .filter_map(|c| group_of.get(c.mint.as_str()).copied())
            .collect();

        let mut train: Vec<&LaunchCohort> = Vec::new();
        let mut exclusions = FoldExclusions {
            candidates: candidates.len() as u32,
            purged: 0,
            embargoed: 0,
            group_excluded: 0,
            kept: 0,
        };
        for cohort in candidates {
            // Classified by the first rule that removes it, so the counts add to
            // the launches dropped.
            if let Some(start) = test_start {
                if cohort.last_event_ms.saturating_add(config.purge_ms) >= start {
                    exclusions.purged += 1;
                    continue;
                }
                if cohort.last_event_ms.saturating_add(config.embargo_ms) > start {
                    exclusions.embargoed += 1;
                    continue;
                }
            }
            let group = group_of.get(cohort.mint.as_str()).copied();
            if config.group_by != GroupBy::None {
                if let Some(group) = group {
                    if test_groups.contains(group) {
                        exclusions.group_excluded += 1;
                        continue;
                    }
                }
            }
            train.push(cohort);
        }
        exclusions.kept = train.len() as u32;

        let assertions = assert_split(&train, &test, &group_of, config.embargo_ms);
        if !assertions.embargo_holds {
            refusals.push(format!(
                "fold {index}: R7 breached — the training side ends inside the embargo before the \
                 test window"
            ));
        }
        if !assertions.groups_disjoint {
            refusals.push(format!(
                "fold {index}: R8 breached — {} group(s) are on both sides of the split",
                assertions.shared_groups.len()
            ));
        }
        if test.is_empty() {
            refusals.push(format!("fold {index}: the test side is empty"));
        }

        let overlap = wallet_overlap(&train, &test);
        let train_reports = reports_for(&train, &by_mint);
        let test_reports = reports_for(&test, &by_mint);

        for cohort in &test {
            pooled_mints.push(cohort.mint.clone());
        }

        folds.push(Fold {
            index,
            train: side(&train, &train_reports, &group_of),
            test: side(&test, &test_reports, &group_of),
            exclusions,
            wallet_overlap: overlap,
            assertions,
            train_metrics: measure(&train_reports, config, &cells, z_micros),
            test_metrics: measure(&test_reports, config, &cells, z_micros),
        });
    }

    let pooled_cohorts: Vec<&LaunchCohort> = pooled_mints
        .iter()
        .filter_map(|mint| cohort_of.get(mint.as_str()).copied())
        .collect();
    let pooled_reports = reports_for(&pooled_cohorts, &by_mint);
    let pooled_metrics = measure(&pooled_reports, config, &cells, z_micros);
    let worst_cell = pooled_metrics
        .stress
        .iter()
        .filter(|cell| cell.ev_lcb_bps_micros.is_some())
        .min_by_key(|cell| cell.ev_lcb_bps_micros.unwrap_or(i64::MAX))
        .cloned();
    let stressed_ev_lcb_positive = !pooled_metrics.stress.is_empty()
        && pooled_metrics
            .stress
            .iter()
            .all(|cell| cell.positive_ev_lcb);
    let pooled = PooledOutOfSample {
        folds: folds.len() as u32,
        folds_without_training: folds.iter().filter(|fold| fold.train.launches == 0).count() as u32,
        launches: pooled_reports.len() as u32,
        trades: pooled_metrics.performance.trades,
        metrics: pooled_metrics,
        worst_cell,
        stressed_ev_lcb_positive,
    };

    if folds.is_empty() {
        refusals.push("no test fold was produced".to_string());
    }

    refusals.sort();
    refusals.dedup();
    WalkForwardReport {
        schema: REPORT_SCHEMA.to_string(),
        source: corpus.source.clone(),
        config: config.clone(),
        corpus: corpus_summary,
        grouping,
        multiple_testing,
        folds,
        pooled,
        gate_ready: refusals.is_empty(),
        refusals,
    }
}

/// Cuts `n` items into `folds` contiguous blocks of near-equal count.
///
/// `i * n / folds` rather than a fixed block size, so the remainder is spread
/// across the early blocks instead of landing entirely in the last one. An empty
/// block is possible only when `folds > n`, which the caller has already refused.
fn cut_blocks(n: usize, folds: usize) -> Vec<Vec<usize>> {
    let folds = folds.max(1);
    let mut blocks = Vec::with_capacity(folds);
    for index in 0..folds {
        let start = index * n / folds;
        let end = (index + 1) * n / folds;
        blocks.push((start..end).collect());
    }
    blocks
}

fn assert_split(
    train: &[&LaunchCohort],
    test: &[&LaunchCohort],
    group_of: &BTreeMap<&str, &str>,
    embargo_ms: i64,
) -> FoldAssertions {
    let train_end = train.iter().map(|c| c.last_event_ms).max();
    let test_start = test.iter().map(|c| c.first_event_ms).min();
    let (embargo_holds, margin) = match (train_end, test_start) {
        (Some(end), Some(start)) => {
            let margin = start.saturating_sub(end.saturating_add(embargo_ms));
            (margin >= 0, Some(margin))
        }
        // An empty training side, or an empty test side, satisfies the
        // assertion by having nothing to violate it. Reported as vacuous rather
        // than as a pass, which is what the `None` margin is for.
        _ => (true, None),
    };

    let train_groups: BTreeSet<&str> = train
        .iter()
        .filter_map(|c| group_of.get(c.mint.as_str()).copied())
        .collect();
    let test_groups: BTreeSet<&str> = test
        .iter()
        .filter_map(|c| group_of.get(c.mint.as_str()).copied())
        .collect();
    let shared: Vec<String> = train_groups
        .intersection(&test_groups)
        .map(|group| (*group).to_string())
        .collect();

    FoldAssertions {
        embargo_holds,
        embargo_margin_ms: margin,
        groups_disjoint: shared.is_empty(),
        shared_groups: shared,
    }
}

fn wallet_overlap(train: &[&LaunchCohort], test: &[&LaunchCohort]) -> WalletOverlap {
    let train_wallets: BTreeSet<&str> = train
        .iter()
        .flat_map(|c| c.wallets.iter().map(|w| w.as_str()))
        .collect();
    let test_wallets: BTreeSet<&str> = test
        .iter()
        .flat_map(|c| c.wallets.iter().map(|w| w.as_str()))
        .collect();
    let shared = train_wallets.intersection(&test_wallets).count();
    WalletOverlap {
        train_wallets: train_wallets.len() as u32,
        test_wallets: test_wallets.len() as u32,
        shared_wallets: shared as u32,
        share_of_test_bps: if test_wallets.is_empty() {
            None
        } else {
            Some(mul_div_floor(
                shared as u128,
                u128::from(BPS_DENOMINATOR),
                test_wallets.len() as u128,
            ) as u16)
        },
    }
}

fn reports_for(
    cohorts: &[&LaunchCohort],
    by_mint: &BTreeMap<&str, &LaunchReport>,
) -> Vec<LaunchReport> {
    let mut reports: Vec<LaunchReport> = cohorts
        .iter()
        .filter_map(|cohort| by_mint.get(cohort.mint.as_str()).map(|r| (*r).clone()))
        .collect();
    reports.sort_by(|a, b| {
        a.first_event_ms
            .cmp(&b.first_event_ms)
            .then_with(|| a.mint.cmp(&b.mint))
    });
    reports
}

fn side(
    cohorts: &[&LaunchCohort],
    reports: &[LaunchReport],
    group_of: &BTreeMap<&str, &str>,
) -> FoldSide {
    let mut mints: Vec<String> = cohorts.iter().map(|c| c.mint.clone()).collect();
    mints.sort();
    let groups: BTreeSet<&str> = cohorts
        .iter()
        .filter_map(|c| group_of.get(c.mint.as_str()).copied())
        .collect();
    let wallets: BTreeSet<&str> = cohorts
        .iter()
        .flat_map(|c| c.wallets.iter().map(|w| w.as_str()))
        .collect();
    FoldSide {
        launches: cohorts.len() as u32,
        mints,
        groups: groups.len() as u32,
        wallets: wallets.len() as u32,
        trades: reports.iter().map(|r| r.trades.len() as u32).sum(),
        first_event_ms: cohorts.iter().map(|c| c.first_event_ms).min(),
        last_event_ms: cohorts.iter().map(|c| c.last_event_ms).max(),
    }
}

/// The `(gap, slippage)` pairs the grid is measured over, baseline first.
fn grid(config: &WalkForwardConfig) -> Vec<(u16, u16)> {
    let mut cells = vec![(0u16, 0u16)];
    let mut gaps: Vec<u16> = config.gaps_bps.clone();
    gaps.sort_unstable();
    gaps.dedup();
    let mut slips: Vec<u16> = config.slippage_bps.clone();
    slips.sort_unstable();
    slips.dedup();
    for gap in &gaps {
        for slip in &slips {
            if (*gap, *slip) != (0, 0) {
                cells.push((*gap, *slip));
            }
        }
    }
    cells
}

fn measure(
    reports: &[LaunchReport],
    config: &WalkForwardConfig,
    cells: &[(u16, u16)],
    z_micros: u64,
) -> SideMetrics {
    let mut trades: Vec<ClosedTrade> = reports
        .iter()
        .flat_map(|launch| launch.trades.iter().cloned())
        .collect();
    // The same total order the evaluator's own book is walked in, so a fold's
    // drawdown is the corpus's drawdown restricted to the fold rather than a
    // different arithmetic.
    trades.sort_by(|a, b| {
        a.closed_at_ms
            .cmp(&b.closed_at_ms)
            .then_with(|| a.mint.cmp(&b.mint))
            .then_with(|| a.opened_at_ms.cmp(&b.opened_at_ms))
            .then_with(|| a.tokens.cmp(&b.tokens))
    });
    let stranded: Vec<StrandedPosition> = reports
        .iter()
        .filter_map(|launch| launch.stranded.clone())
        .collect();

    SideMetrics {
        performance: summarise_performance(&trades, &stranded, reports, &config.backtest),
        risk: summarise_risk(&trades, &stranded, &config.backtest),
        rug: summarise_rug(reports),
        zero_trade: decompose_zero_trades(reports),
        exits: exit_availability(reports, &stranded),
        cohorts: cohort_cells(reports),
        stress: cells
            .iter()
            .map(|&(gap, slip)| stress_cell(&trades, gap, slip, config, z_micros))
            .collect(),
    }
}

fn decompose_zero_trades(reports: &[LaunchReport]) -> ZeroTradeDecomposition {
    let mut out = ZeroTradeDecomposition {
        launches: reports.len() as u32,
        no_entry_recorded: 0,
        entry_refused_by_curve: 0,
        entered_and_stranded: 0,
        entered_and_closed: 0,
    };
    for launch in reports {
        if !launch.trades.is_empty() {
            out.entered_and_closed += 1;
        } else if launch.entries > 0 {
            out.entered_and_stranded += 1;
        } else if launch
            .quote_failures
            .iter()
            .any(|failure| failure.context == "entry")
        {
            out.entry_refused_by_curve += 1;
        } else {
            out.no_entry_recorded += 1;
        }
    }
    out
}

fn exit_availability(reports: &[LaunchReport], stranded: &[StrandedPosition]) -> ExitAvailability {
    let opened = reports.iter().filter(|launch| launch.entries > 0).count() as u32;
    let no_exit = stranded
        .iter()
        .filter(|position| position.no_executable_exit)
        .count() as u32;
    ExitAvailability {
        positions_opened: opened,
        positions_stranded: stranded.len() as u32,
        no_executable_exits: no_exit,
        no_exit_rate_bps: if opened == 0 {
            None
        } else {
            Some(mul_div_floor(
                u128::from(no_exit),
                u128::from(BPS_DENOMINATOR),
                u128::from(opened),
            ) as u16)
        },
    }
}

fn cohort_cells(reports: &[LaunchReport]) -> Vec<CohortCell> {
    let mut cells: BTreeMap<RugClass, CohortCell> = BTreeMap::new();
    for launch in reports {
        let cell = cells.entry(launch.classified).or_insert(CohortCell {
            cohort: launch.classified,
            launches: 0,
            entered: 0,
            trades: 0,
            realized_pnl_lamports: 0,
        });
        cell.launches += 1;
        if launch.entries > 0 {
            cell.entered += 1;
        }
        cell.trades += launch.trades.len() as u32;
        cell.realized_pnl_lamports = cell
            .realized_pnl_lamports
            .saturating_add(launch.realized_pnl_lamports);
    }
    cells.into_values().collect()
}

/// What is left of `lamports` after a fall of `bps`. Floors, so the residual is
/// a cost rather than a windfall.
fn haircut(lamports: u64, bps: u16) -> u64 {
    let remaining = u128::from(BPS_DENOMINATOR).saturating_sub(u128::from(bps));
    mul_div_floor(u128::from(lamports), remaining, u128::from(BPS_DENOMINATOR)) as u64
}

fn stress_cell(
    trades: &[ClosedTrade],
    gap_bps: u16,
    slippage_bps: u16,
    config: &WalkForwardConfig,
    z_micros: u64,
) -> StressCell {
    let mut pnl: i128 = 0;
    let mut winners = 0u32;
    let mut no_proceeds = 0u32;
    let mut returns: Vec<i32> = Vec::with_capacity(trades.len());

    for trade in trades {
        // The gap first, then the drag, compounded rather than added: a 50%
        // fall and a 25% bad fill leave 0.5 x 0.75 of the proceeds, not 0.25 of
        // them. Same composition as `strategy::entry::stress`.
        let proceeds = haircut(haircut(trade.proceeds_lamports, gap_bps), slippage_bps);
        if proceeds == 0 && trade.cost_lamports > 0 {
            no_proceeds += 1;
        }
        let delta = i128::from(proceeds) - i128::from(trade.cost_lamports);
        pnl += delta;
        if delta > 0 {
            winners += 1;
        }
        let return_bps = if trade.cost_lamports == 0 {
            0
        } else {
            floor_div_i128(
                delta.saturating_mul(i128::from(BPS_DENOMINATOR)),
                i128::from(trade.cost_lamports),
            )
            .clamp(i128::from(i32::MIN), i128::from(i32::MAX)) as i32
        };
        returns.push(return_bps);
    }

    let (mean, stddev) = return_moments(&returns);
    let lcb = lower_confidence_bound(mean, stddev, returns.len(), z_micros);
    let starting = config.backtest.starting_equity_lamports;

    StressCell {
        gap_bps,
        slippage_bps,
        trades: trades.len() as u32,
        winners,
        stressed_pnl_lamports: pnl.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
        stressed_pnl_usd_cents: lamports_to_usd_cents(pnl, config.backtest.cents_per_sol),
        return_on_equity_bps: if starting == 0 {
            0
        } else {
            floor_div_i128(
                pnl.saturating_mul(i128::from(BPS_DENOMINATOR)),
                i128::from(starting),
            )
            .clamp(i128::from(i32::MIN), i128::from(i32::MAX)) as i32
        },
        mean_return_bps_micros: mean,
        stddev_return_bps_micros: stddev,
        ev_lcb_bps_micros: lcb,
        // An unmeasured expectancy is not a positive one.
        positive_ev_lcb: lcb.is_some_and(|bound| bound > 0),
        cvar_bps: cvar_bps(&returns, config.cvar_pct),
        worst_return_bps: returns.iter().copied().min(),
        no_proceeds_trades: no_proceeds,
        optimistic: gap_bps > 0 || slippage_bps > 0,
    }
}

/// `mean - z x stddev / sqrt(n)`, in millionths of a basis point.
///
/// `None` under two trades: the standard deviation is a sample one, so a single
/// observation has no dispersion to bound with and a bound invented for it would
/// be the mean wearing a confidence interval.
///
/// Both divisions round the bound **down**: the square root is taken to three
/// decimal places by scaling `n` by a million before flooring, and the margin
/// subtracted is rounded up. A lower bound that is a shade too low is a bound;
/// one that is a shade too high is a claim.
pub fn lower_confidence_bound(
    mean_bps_micros: i64,
    stddev_bps_micros: u64,
    n: usize,
    z_micros: u64,
) -> Option<i64> {
    if n < 2 {
        return None;
    }
    // isqrt(n x 10^6) is sqrt(n) x 1000, floored — a smaller denominator, so a
    // larger standard error, so a lower bound.
    let root_scaled = isqrt(n as u128 * 1_000_000).max(1);
    let standard_error = mul_div_ceil(u128::from(stddev_bps_micros), 1_000, root_scaled);
    let margin = mul_div_ceil(u128::from(z_micros), standard_error, u128::from(MICROS));
    let bound = i128::from(mean_bps_micros) - margin.min(u128::from(i64::MAX as u64)) as i128;
    Some(bound.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64)
}

/// The mean of the worst `pct` percent of returns, in basis points.
///
/// Nearest rank, floored towards negative infinity, so the number is an average
/// of returns that actually happened and never rounds towards comfort. At least
/// one trade is always in the tail — a five-percent tail of nine trades is the
/// worst one, not none of them.
pub fn cvar_bps(returns_bps: &[i32], pct: u32) -> Option<i32> {
    if returns_bps.is_empty() {
        return None;
    }
    let mut sorted = returns_bps.to_vec();
    sorted.sort_unstable();
    let count = (u128::from(returns_bps.len() as u64) * u128::from(pct)).div_ceil(100);
    let count = (count.max(1) as usize).min(sorted.len());
    let sum: i128 = sorted[..count].iter().map(|&r| i128::from(r)).sum();
    Some(
        floor_div_i128(sum, count as i128).clamp(i128::from(i32::MIN), i128::from(i32::MAX)) as i32,
    )
}

fn summarise_corpus(corpus: &Corpus) -> CorpusSummary {
    let first = corpus.cohorts.iter().map(|c| c.first_event_ms).min();
    let last = corpus.cohorts.iter().map(|c| c.last_event_ms).max();
    let wallets: BTreeSet<&str> = corpus
        .cohorts
        .iter()
        .flat_map(|c| c.wallets.iter().map(|w| w.as_str()))
        .collect();
    CorpusSummary {
        launches: corpus.launches.len() as u32,
        buyers: wallets.len() as u32,
        first_event_ms: first,
        last_event_ms: last,
        span_ms: match (first, last) {
            (Some(first), Some(last)) => last.saturating_sub(first),
            _ => 0,
        },
        gate_ready: !corpus.parts.is_empty()
            && corpus.parts.iter().all(|part| part.gate_ready)
            && corpus.refusals.is_empty(),
        parts: corpus.parts.clone(),
    }
}

fn summarise_grouping(groups: &[LaunchGroup], launches: usize, by: GroupBy) -> GroupingSummary {
    let largest = groups
        .iter()
        .map(|group| group.mints.len())
        .max()
        .unwrap_or(0);
    GroupingSummary {
        group_by: by,
        applied: by != GroupBy::None,
        groups: groups.len() as u32,
        largest_group_launches: largest as u32,
        largest_group_share_bps: if launches == 0 {
            0
        } else {
            mul_div_floor(
                largest as u128,
                u128::from(BPS_DENOMINATOR),
                launches as u128,
            ) as u16
        },
        grouped_by_funder: groups
            .iter()
            .filter(|g| g.source == GroupSource::Funder)
            .map(|g| g.mints.len() as u32)
            .sum(),
        grouped_by_deployer: groups
            .iter()
            .filter(|g| g.source == GroupSource::Deployer)
            .map(|g| g.mints.len() as u32)
            .sum(),
        grouped_by_mint: groups
            .iter()
            .filter(|g| g.source == GroupSource::Mint)
            .map(|g| g.mints.len() as u32)
            .sum(),
    }
}

/// Runs the whole thing over a corpus directory.
pub fn run(dir: &Path, config: &WalkForwardConfig) -> Result<WalkForwardReport, BacktestError> {
    let corpus = read_corpus(dir, config.backtest)?;
    let report = evaluate(&corpus, config);
    if config.backtest.gate && !report.gate_ready {
        return Err(BacktestError::Refused(report.refusals.clone()));
    }
    Ok(report)
}

// ===========================================================================
// The command line
// ===========================================================================

/// A duration written the way the milestone command writes it: `1h`, `30m`,
/// `90s`, `1500ms`, or a bare number of milliseconds.
pub fn parse_duration_ms(text: &str) -> Result<i64, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("an empty duration".to_string());
    }
    let (digits, multiplier) = if let Some(rest) = text.strip_suffix("ms") {
        (rest, 1i64)
    } else if let Some(rest) = text.strip_suffix('s') {
        (rest, 1_000)
    } else if let Some(rest) = text.strip_suffix('m') {
        (rest, 60_000)
    } else if let Some(rest) = text.strip_suffix('h') {
        (rest, 3_600_000)
    } else if let Some(rest) = text.strip_suffix('d') {
        (rest, 86_400_000)
    } else {
        (text, 1)
    };
    let value: i64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("{text} is not a duration"))?;
    if value < 0 {
        return Err(format!("{text} is negative"));
    }
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("{text} does not fit"))
}

/// A comma-separated list of whole percentages, as basis points.
///
/// Percent because that is the unit the roadmap's own command is written in —
/// `--gaps 30,50 --slippage 10,15,20,25` — and a flag that silently meant basis
/// points would read as a 0.3% gap.
pub fn parse_percent_list(text: &str) -> Result<Vec<u16>, String> {
    let mut out = Vec::new();
    for part in text.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let percent: u32 = part
            .parse()
            .map_err(|_| format!("{part} is not a whole percentage"))?;
        if percent > 100 {
            return Err(format!("{part}% is more than everything"));
        }
        out.push((percent * 100) as u16);
    }
    if out.is_empty() {
        return Err("an empty list".to_string());
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

impl fmt::Display for GroupBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::{LaunchSybil, QuoteFailure};

    fn cohort(
        mint: &str,
        first: i64,
        last: i64,
        funders: &[&str],
        wallets: &[&str],
    ) -> LaunchCohort {
        let mut funders: Vec<String> = funders.iter().map(|f| (*f).to_string()).collect();
        funders.sort();
        let mut wallets: Vec<String> = wallets.iter().map(|w| (*w).to_string()).collect();
        wallets.sort();
        LaunchCohort {
            mint: mint.to_string(),
            first_event_ms: first,
            last_event_ms: last,
            creator: Some(format!("creator-{mint}")),
            funders,
            wallets,
        }
    }

    fn empty_sybil() -> LaunchSybil {
        LaunchSybil {
            buyer_count: 0,
            buy_volume_lamports: 0,
            attributed_volume_lamports: 0,
            unattributed_volume_lamports: 0,
            fund_bps: 0,
            sync_micros: None,
            temporal_influence_micros: None,
            buyer_diversity_bps: None,
            holder_hhi_bps: None,
            holder_top1_bps: 0,
            holder_top5_bps: 0,
            holder_top10_bps: 0,
            effective_holders_micros: 0,
            clusters: Vec::new(),
            sync_truncated: false,
        }
    }

    /// A launch report carrying `trades`, each bought for `cost` and sold for
    /// `proceeds`.
    fn report(mint: &str, first: i64, last: i64, fills: &[(u64, u64)]) -> LaunchReport {
        let trades: Vec<ClosedTrade> = fills
            .iter()
            .enumerate()
            .map(|(index, &(cost, proceeds))| {
                let pnl = proceeds as i64 - cost as i64;
                ClosedTrade {
                    mint: mint.to_string(),
                    opened_at_ms: first + index as i64,
                    closed_at_ms: first + index as i64 + 1,
                    hold_ms: 1,
                    tokens: 1_000,
                    cost_lamports: cost,
                    proceeds_lamports: proceeds,
                    pnl_lamports: pnl,
                    pnl_usd_cents: 0,
                    return_bps: if cost == 0 {
                        0
                    } else {
                        floor_div_i128(
                            i128::from(pnl) * i128::from(BPS_DENOMINATOR),
                            i128::from(cost),
                        ) as i32
                    },
                }
            })
            .collect();
        LaunchReport {
            mint: mint.to_string(),
            creator: Some(format!("creator-{mint}")),
            first_event_ms: first,
            last_event_ms: last,
            events: 4,
            flow_events: 1,
            entries: fills.len() as u32,
            exits: fills.len() as u32,
            entry_gross_lamports: fills.iter().map(|f| f.0).sum(),
            exit_net_lamports: fills.iter().map(|f| f.1).sum(),
            fees_paid_lamports: 0,
            realized_pnl_lamports: trades.iter().map(|t| t.pnl_lamports).sum(),
            realized_pnl_usd_cents: 0,
            peak_real_sol_lamports: 0,
            final_real_sol_lamports: 0,
            max_drop_bps: 0,
            fastest_drop_bps: 0,
            graduated: false,
            pulls: 0,
            pulled_lamports: 0,
            classified: RugClass::Faded,
            labelled: None,
            sybil: empty_sybil(),
            adverse_selection: Vec::new(),
            trades,
            stranded: None,
            quote_failures: Vec::new(),
        }
    }

    /// Six launches, one an hour apart, each with its own funder and its own
    /// buyer, each trading once at a small loss.
    fn corpus() -> Corpus {
        let hour = 3_600_000i64;
        let mut cohorts = Vec::new();
        let mut launches = Vec::new();
        for index in 0..6i64 {
            let mint = format!("mint-{index}");
            let first = 1_700_000_000_000 + index * hour * 2;
            cohorts.push(cohort(
                &mint,
                first,
                first + 60_000,
                &[&format!("funder-{index}")],
                &[&format!("wallet-{index}")],
            ));
            launches.push(report(
                &mint,
                first,
                first + 60_000,
                &[(1_000_000, 950_000)],
            ));
        }
        Corpus {
            source: "test".to_string(),
            parts: vec![CorpusPart {
                source: "test".to_string(),
                launches: 6,
                integrity: IntegritySummary {
                    streams: 1,
                    lines: 1,
                    records: 1,
                    verified: 1,
                    unverifiable: 0,
                    rejected: 0,
                    streams_with_breaks: 0,
                    event_errors: 0,
                    gate_ready: true,
                },
                gate_ready: true,
                refusals: Vec::new(),
            }],
            launches,
            cohorts,
            refusals: Vec::new(),
        }
    }

    fn config() -> WalkForwardConfig {
        WalkForwardConfig {
            folds: 3,
            ..WalkForwardConfig::default()
        }
    }

    // -----------------------------------------------------------------------
    // the split: R7, R8, R21
    // -----------------------------------------------------------------------

    #[test]
    fn every_fold_satisfies_the_purge_and_the_embargo() {
        // R7.
        let report = evaluate(&corpus(), &config());
        assert!(!report.folds.is_empty());
        for fold in &report.folds {
            assert!(
                fold.assertions.embargo_holds,
                "fold {} breached the embargo",
                fold.index
            );
            // And the margin is a real number rather than the vacuous case: the
            // launches are two hours apart and the embargo is one.
            let margin = fold
                .assertions
                .embargo_margin_ms
                .expect("a non-empty training side");
            assert!(margin >= 0, "fold {} margin {margin}", fold.index);
        }
        assert!(report.gate_ready, "{:?}", report.refusals);
    }

    #[test]
    fn an_embargo_wider_than_the_gap_between_launches_empties_the_training_side() {
        // The embargo is doing work rather than being carried: widened past the
        // spacing, every training launch is removed and the assertion holds
        // vacuously rather than by luck.
        let config = WalkForwardConfig {
            embargo_ms: 86_400_000,
            ..config()
        };
        let report = evaluate(&corpus(), &config);
        for fold in &report.folds {
            assert_eq!(fold.train.launches, 0, "fold {}", fold.index);
            assert!(fold.exclusions.embargoed > 0 || fold.exclusions.purged > 0);
            assert_eq!(fold.assertions.embargo_margin_ms, None);
            assert!(fold.assertions.embargo_holds);
        }
    }

    #[test]
    fn the_purge_and_the_embargo_are_counted_apart() {
        // With no embargo the purge is the only rule that can remove anything on
        // time, and it removes nothing here because the launches do not overlap.
        let config = WalkForwardConfig {
            embargo_ms: 0,
            ..config()
        };
        let report = evaluate(&corpus(), &config);
        for fold in &report.folds {
            assert_eq!(fold.exclusions.purged, 0);
            assert_eq!(fold.exclusions.embargoed, 0);
        }

        // Now make the outcome windows reach into the next launch. Every
        // training record overlaps the test window's start and the purge, not
        // the embargo, is what removes them.
        let mut overlapping = corpus();
        for cohort in &mut overlapping.cohorts {
            cohort.last_event_ms = cohort.first_event_ms + 86_400_000;
        }
        let report = evaluate(&overlapping, &config);
        for fold in &report.folds {
            assert!(fold.exclusions.purged > 0, "fold {}", fold.index);
            assert_eq!(fold.exclusions.embargoed, 0);
        }
    }

    #[test]
    fn no_group_appears_on_both_sides_of_a_fold() {
        // R8. Every launch shares one funder, so the whole corpus is one group
        // and every training side is emptied by the group rule rather than by
        // the clock.
        let mut shared = corpus();
        for cohort in &mut shared.cohorts {
            cohort.funders = vec!["funder-shared".to_string()];
        }
        let config = WalkForwardConfig {
            embargo_ms: 0,
            ..config()
        };
        let report = evaluate(&shared, &config);
        assert_eq!(report.grouping.groups, 1);
        assert_eq!(report.grouping.largest_group_share_bps, 10_000);
        for fold in &report.folds {
            assert!(fold.assertions.groups_disjoint);
            assert!(fold.assertions.shared_groups.is_empty());
            assert_eq!(fold.train.launches, 0);
            assert!(fold.exclusions.group_excluded > 0);
        }
    }

    #[test]
    fn a_group_is_the_component_and_not_the_loudest_funder() {
        // A shares f1, B shares f1 and f2, C shares f2. Picking one funder per
        // launch would split A from C; the component does not.
        let cohorts = vec![
            cohort("a", 0, 1, &["f1"], &["w1"]),
            cohort("b", 2, 3, &["f1", "f2"], &["w2"]),
            cohort("c", 4, 5, &["f2"], &["w3"]),
        ];
        let groups = group_launches(&cohorts, GroupBy::Funder);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].mints, vec!["a", "b", "c"]);
        assert_eq!(groups[0].source, GroupSource::Funder);
    }

    #[test]
    fn a_deployer_that_shares_a_name_with_a_funder_does_not_fuse_two_groups() {
        // The keys are namespaced for exactly this: `x` funding one launch and
        // `x` deploying another are not evidence of one party.
        let cohorts = vec![
            cohort("a", 0, 1, &["x"], &["w1"]),
            LaunchCohort {
                creator: Some("x".to_string()),
                ..cohort("b", 2, 3, &[], &["w2"])
            },
        ];
        let groups = group_launches(&cohorts, GroupBy::Funder);
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn an_unresolved_launch_is_grouped_by_its_deployer() {
        // §22: "with unresolved records grouped by deployer".
        let cohorts = vec![
            LaunchCohort {
                creator: Some("dev".to_string()),
                ..cohort("a", 0, 1, &[], &["w1"])
            },
            LaunchCohort {
                creator: Some("dev".to_string()),
                ..cohort("b", 2, 3, &[], &["w2"])
            },
        ];
        let groups = group_launches(&cohorts, GroupBy::Funder);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].source, GroupSource::Deployer);
        assert_eq!(groups[0].mints, vec!["a", "b"]);
    }

    #[test]
    fn the_wallet_overlap_is_computed_and_present_on_every_fold() {
        // R21. It is a report and never an assertion, so a corpus where every
        // wallet is in every launch still produces folds — with the overlap at
        // 100% where anybody quoting the fold can see it.
        let mut sticky = corpus();
        for cohort in &mut sticky.cohorts {
            cohort.wallets = vec!["everywhere".to_string()];
        }
        let config = WalkForwardConfig {
            embargo_ms: 0,
            ..config()
        };
        let report = evaluate(&sticky, &config);
        assert!(report.gate_ready, "{:?}", report.refusals);
        for fold in &report.folds {
            assert_eq!(fold.wallet_overlap.test_wallets, 1);
            assert_eq!(fold.wallet_overlap.shared_wallets, 1);
            assert_eq!(fold.wallet_overlap.share_of_test_bps, Some(10_000));
        }
    }

    #[test]
    fn a_test_fold_with_no_buyers_reports_an_unknown_overlap_rather_than_zero() {
        let mut blind = corpus();
        for cohort in &mut blind.cohorts {
            cohort.wallets.clear();
        }
        let report = evaluate(&blind, &config());
        for fold in &report.folds {
            assert_eq!(fold.wallet_overlap.share_of_test_bps, None);
        }
    }

    #[test]
    fn the_first_block_is_training_only() {
        let report = evaluate(&corpus(), &config());
        // Three blocks over six launches: two test folds of two launches each.
        assert_eq!(report.folds.len(), 2);
        assert_eq!(report.folds[0].index, 1);
        assert_eq!(report.pooled.launches, 4);
        // And the first block's launches are never in a test set.
        let tested: Vec<&String> = report
            .folds
            .iter()
            .flat_map(|fold| fold.test.mints.iter())
            .collect();
        assert!(!tested.iter().any(|mint| *mint == "mint-0"));
    }

    #[test]
    fn a_corpus_with_fewer_launches_than_folds_is_refused() {
        let mut small = corpus();
        small.cohorts.truncate(2);
        small.launches.truncate(2);
        let config = WalkForwardConfig {
            folds: 5,
            ..config()
        };
        let report = evaluate(&small, &config);
        assert!(!report.gate_ready);
        assert!(report
            .refusals
            .iter()
            .any(|reason| reason.contains("launch(es)")));
    }

    #[test]
    fn one_fold_leaves_nothing_to_test() {
        let config = WalkForwardConfig {
            folds: 1,
            ..config()
        };
        let report = evaluate(&corpus(), &config);
        assert!(report.folds.is_empty());
        assert!(!report.gate_ready);
        assert!(report
            .refusals
            .iter()
            .any(|reason| reason.contains("nothing to test")));
    }

    // -----------------------------------------------------------------------
    // the grid
    // -----------------------------------------------------------------------

    #[test]
    fn the_baseline_cell_is_the_run_as_it_was_priced() {
        let report = evaluate(&corpus(), &config());
        let baseline = report
            .pooled
            .metrics
            .stress
            .iter()
            .find(|cell| cell.gap_bps == 0 && cell.slippage_bps == 0)
            .expect("a baseline cell");
        assert!(!baseline.optimistic);
        assert_eq!(
            baseline.stressed_pnl_lamports,
            report.pooled.metrics.performance.realized_pnl_lamports
        );
    }

    #[test]
    fn a_gap_and_a_drag_compound_rather_than_add() {
        // 1 000 000 in, 1 000 000 back. A 50% gap and a 20% drag leave
        // 0.5 x 0.8 = 0.4, which is 400 000 back and 600 000 lost. Adding them
        // would leave 300 000.
        let trades = vec![ClosedTrade {
            mint: "m".to_string(),
            opened_at_ms: 0,
            closed_at_ms: 1,
            hold_ms: 1,
            tokens: 1,
            cost_lamports: 1_000_000,
            proceeds_lamports: 1_000_000,
            pnl_lamports: 0,
            pnl_usd_cents: 0,
            return_bps: 0,
        }];
        let cell = stress_cell(&trades, 5_000, 2_000, &config(), 1_644_854);
        assert_eq!(cell.stressed_pnl_lamports, -600_000);
        assert!(cell.optimistic);
    }

    #[test]
    fn a_haircut_that_leaves_nothing_is_counted_as_a_trade_with_no_proceeds() {
        let trades = vec![ClosedTrade {
            mint: "m".to_string(),
            opened_at_ms: 0,
            closed_at_ms: 1,
            hold_ms: 1,
            tokens: 1,
            cost_lamports: 1_000,
            proceeds_lamports: 1,
            pnl_lamports: -999,
            pnl_usd_cents: 0,
            return_bps: -9_990,
        }];
        let cell = stress_cell(&trades, 5_000, 2_000, &config(), 1_644_854);
        assert_eq!(cell.no_proceeds_trades, 1);
        assert_eq!(cell.stressed_pnl_lamports, -1_000);
    }

    #[test]
    fn an_unmeasured_expectancy_is_not_a_positive_one() {
        // One trade has no sample dispersion, so there is no bound and the cell
        // does not read as clearing zero.
        let trades = vec![ClosedTrade {
            mint: "m".to_string(),
            opened_at_ms: 0,
            closed_at_ms: 1,
            hold_ms: 1,
            tokens: 1,
            cost_lamports: 1_000,
            proceeds_lamports: 10_000,
            pnl_lamports: 9_000,
            pnl_usd_cents: 0,
            return_bps: 90_000,
        }];
        let cell = stress_cell(&trades, 0, 0, &config(), 1_644_854);
        assert_eq!(cell.ev_lcb_bps_micros, None);
        assert!(!cell.positive_ev_lcb);
    }

    #[test]
    fn the_lower_bound_is_below_the_mean_and_falls_as_the_level_tightens() {
        let mean = 1_000 * MICROS as i64;
        let stddev = 500 * MICROS;
        let loose = lower_confidence_bound(mean, stddev, 100, 1_644_854).expect("a bound");
        let tight = lower_confidence_bound(mean, stddev, 100, 3_090_232).expect("a bound");
        assert!(loose < mean);
        assert!(tight < loose);
        // 1.644854 x 500 / 10 = 82.24 basis points off a mean of 1 000.
        assert_eq!(loose, 917_757_300);
    }

    #[test]
    fn the_quantile_table_is_read_conservatively() {
        // A level between two rows takes the stricter row.
        let (z, beyond) = one_sided_z_micros(30_000);
        assert_eq!(z, 1_959_964);
        assert!(!beyond);
        // A level past the end of the table says so rather than pretending.
        let (z, beyond) = one_sided_z_micros(0);
        assert_eq!(z, 4_753_424);
        assert!(beyond);
        // And a level that lands exactly on a row takes that row.
        let (z, beyond) = one_sided_z_micros(50_000);
        assert_eq!(z, 1_644_854);
        assert!(!beyond);
    }

    #[test]
    fn the_level_is_divided_by_how_many_cells_were_looked_at() {
        let report = evaluate(&corpus(), &config());
        let cells = grid(&config()).len() as u64;
        // Two test folds and the pooled grid.
        assert_eq!(report.multiple_testing.tests as u64, 3 * cells);
        assert!(report.multiple_testing.per_test_alpha_micros < 50_000);
        assert_eq!(report.multiple_testing.method, "bonferroni");
    }

    #[test]
    fn the_worst_tail_is_an_average_of_returns_that_happened() {
        let returns = vec![-500, -400, -300, -200, -100, 0, 100, 200, 300, 400];
        // A 20% tail of ten is the worst two: -500 and -400.
        assert_eq!(cvar_bps(&returns, 20), Some(-450));
        // A 5% tail of ten rounds up to one trade, not to none.
        assert_eq!(cvar_bps(&returns, 5), Some(-500));
        assert_eq!(cvar_bps(&[], 5), None);
    }

    // -----------------------------------------------------------------------
    // the decompositions
    // -----------------------------------------------------------------------

    #[test]
    fn a_fold_that_traded_nothing_says_why() {
        let mut reports = vec![
            report("traded", 0, 10, &[(1_000, 900)]),
            report("nothing", 0, 10, &[]),
            report("refused", 0, 10, &[]),
        ];
        reports[2].quote_failures.push(QuoteFailure {
            mint: "refused".to_string(),
            at_ms: 5,
            context: "entry".to_string(),
            reason: "the curve is complete".to_string(),
        });
        let decomposition = decompose_zero_trades(&reports);
        assert_eq!(decomposition.launches, 3);
        assert_eq!(decomposition.entered_and_closed, 1);
        assert_eq!(decomposition.no_entry_recorded, 1);
        assert_eq!(decomposition.entry_refused_by_curve, 1);
        assert_eq!(decomposition.entered_and_stranded, 0);
        assert_eq!(
            decomposition.no_entry_recorded
                + decomposition.entry_refused_by_curve
                + decomposition.entered_and_stranded
                + decomposition.entered_and_closed,
            decomposition.launches,
            "the four counts have to partition the launches"
        );
    }

    #[test]
    fn a_stranded_position_with_no_exit_is_a_rate_rather_than_a_count_alone() {
        let mut launch = report("stuck", 0, 10, &[]);
        launch.entries = 1;
        launch.stranded = Some(StrandedPosition {
            mint: "stuck".to_string(),
            opened_at_ms: 1,
            tokens: 10,
            cost_lamports: 1_000,
            marked_lamports: 0,
            marked_pnl_lamports: -1_000,
            no_executable_exit: true,
            reason: "the pool cannot pay for it".to_string(),
        });
        let stranded = vec![launch.stranded.clone().expect("stranded")];
        let availability = exit_availability(std::slice::from_ref(&launch), &stranded);
        assert_eq!(availability.positions_opened, 1);
        assert_eq!(availability.no_executable_exits, 1);
        assert_eq!(availability.no_exit_rate_bps, Some(10_000));
    }

    // -----------------------------------------------------------------------
    // determinism: R1
    // -----------------------------------------------------------------------

    #[test]
    fn two_runs_over_one_corpus_produce_the_same_bytes() {
        let corpus = corpus();
        let first = evaluate(&corpus, &config()).to_json();
        let second = evaluate(&corpus, &config()).to_json();
        assert_eq!(first, second);
        assert!(!first.contains("generated_at"));
        assert!(!first.contains("elapsed"));
    }

    #[test]
    fn the_report_has_no_floating_point_numbers_in_it() {
        let json = evaluate(&corpus(), &config()).to_json();
        for (index, line) in json.lines().enumerate() {
            let Some((_, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim().trim_end_matches(',');
            if value.starts_with('"')
                || value.is_empty()
                || matches!(value, "true" | "false" | "null" | "[" | "{")
            {
                continue;
            }
            assert!(
                !value.contains('.') && !value.contains('e') && !value.contains('E'),
                "line {} carries a float: {line}",
                index + 1
            );
        }
    }

    #[test]
    fn the_order_the_launches_arrived_in_does_not_move_the_report() {
        let corpus = corpus();
        let mut shuffled = corpus.clone();
        shuffled.cohorts.reverse();
        shuffled.launches.reverse();
        assert_eq!(
            evaluate(&corpus, &config()).to_json(),
            evaluate(&shuffled, &config()).to_json()
        );
    }

    // -----------------------------------------------------------------------
    // flags
    // -----------------------------------------------------------------------

    #[test]
    fn a_duration_is_read_in_the_unit_it_is_written_in() {
        assert_eq!(parse_duration_ms("1h"), Ok(3_600_000));
        assert_eq!(parse_duration_ms("30m"), Ok(1_800_000));
        assert_eq!(parse_duration_ms("90s"), Ok(90_000));
        assert_eq!(parse_duration_ms("1500ms"), Ok(1_500));
        assert_eq!(parse_duration_ms("250"), Ok(250));
        assert!(parse_duration_ms("-1h").is_err());
        assert!(parse_duration_ms("soon").is_err());
    }

    #[test]
    fn the_bucket_lists_are_percentages() {
        assert_eq!(parse_percent_list("30,50"), Ok(vec![3_000, 5_000]));
        assert_eq!(
            parse_percent_list("10,15,20,25"),
            Ok(vec![1_000, 1_500, 2_000, 2_500])
        );
        // Sorted and deduplicated, so two spellings of one grid are one grid.
        assert_eq!(parse_percent_list("50,30,50"), Ok(vec![3_000, 5_000]));
        assert!(parse_percent_list("101").is_err());
        assert!(parse_percent_list("").is_err());
    }

    #[test]
    fn the_grid_carries_the_baseline_and_every_pair() {
        let config = WalkForwardConfig {
            gaps_bps: vec![3_000, 5_000],
            slippage_bps: vec![1_000, 2_500],
            ..config()
        };
        let cells = grid(&config);
        assert_eq!(cells.len(), 1 + 4);
        assert_eq!(cells[0], (0, 0));
    }
}
