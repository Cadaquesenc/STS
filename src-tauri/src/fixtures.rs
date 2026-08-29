//! Synthetic fixtures: launches that never happened, recorded the way a real
//! one would have been.
//!
//! `replay.rs` says what a fixture is and `backtest.rs` says what reading one
//! means. Neither can make one. Every fixture the harness has been tested
//! against so far was hand-written inside a test function, which is fine for a
//! unit test and useless for the thing the roadmap actually asks for: a corpus
//! of *stress* cases, each built to sit on a boundary the engine is supposed to
//! get right, that a person can regenerate, inspect, and hand to
//! `sts backtest run`.
//!
//! Five claims are load-bearing here.
//!
//! **A generated fixture is a function of the scenario, the knobs and the seed,
//! and of nothing else.** No wall clock, no process id, no filesystem order, no
//! iteration over a hash map. Regenerating a case on another machine produces
//! the same bytes, so a case can be cited by name and seed in a gate record
//! rather than shipped as an opaque blob. `generation_is_a_function_of_its_seed`
//! is that property as a test.
//!
//! **Every number on the curve is an integer.** Sizes are lamports, balances
//! are token base units, shares are basis points, and every quote goes through
//! `CurveState`, which is the same integer model the evaluator will price the
//! fixture with. Nothing in this module is allowed to reach for a
//! floating-point type, and `this_module_holds_no_floating_point` enforces that
//! by reading the source rather than by asking nicely — a generator that
//! rounded differently from the evaluator would produce fixtures whose expected
//! answers are wrong in a way that looks like an engine bug.
//!
//! **The generator carries a mirror of the curve and moves it exactly the way
//! the evaluator will.** That is what makes a boundary case a boundary case:
//! `sandwich-boundary` sizes each of our entries against the virtual reserve as
//! it stands at the moment that entry is emitted, so an entry declared one
//! lamport under `b*` really is one lamport under `b*` when the evaluator
//! prices it, rather than one lamport under a threshold that had moved by then.
//!
//! **A case that is built to fail says so, in writing, next to itself.** Each
//! case directory carries an `expected.json` naming what the harness should
//! conclude — whether it verifies, which line breaks and how, what each launch
//! is labelled, which entries clear the extraction threshold. The tests assert
//! the evaluator agrees with that file. A corrupted fixture whose expected
//! answer is "whatever it did last time" is a regression suite that ratifies
//! its own bugs.
//!
//! **A tampered fixture is tampered with after it is sealed, never while.** The
//! corruption cases build a clean chain first, seal it, write the manifest from
//! *that*, and only then edit the bytes. That is the shape real evidence
//! tampering has, and it is the only shape that exercises the difference
//! between the three things `audit_stream` tells apart: a field edited in place,
//! a record spliced out, and a chain resealed from the splice onwards.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::backtest::{sandwich_viable, LineStatus, RugClass, EVENT_SCHEMA};
use crate::replay::{
    sandwich_breakeven_victim_lamports, sha256_hex, write_stream, ChainWriter, CurveState,
    DrawSource, DropClass, Manifest, Queue, RecordDraft, RecordKind, RecordOutcome, ReplayRecord,
    Segment, DEFAULT_FEE_BPS, DEFAULT_MAX_POOL_SHARE_BPS, LAMPORTS_PER_SOL,
    PUMP_GRADUATION_LAMPORTS,
};

/// The schema string on the `expected.json` beside each generated case.
pub const EXPECTED_SCHEMA: &str = "sts.fixtures.expected.v1";

/// A Solana slot, in milliseconds. The cadence the synthetic clock advances at.
const SLOT_MS: i64 = 400;

/// The smallest bundle this module will call coordinated.
///
/// Below five wallets a shared funder buying together is a person with a few
/// wallets, and a fixture that called it a Sybil bundle would be teaching the
/// detector to fire on one. The command line refuses a smaller number rather
/// than quietly raising it: a corpus generated with `--sybil-wallets 3` that
/// silently contained six is a corpus nobody can reason about.
pub const MIN_SYBIL_WALLETS: u32 = 5;

/// The smallest step the graduation ramp will take, in lamports.
///
/// The ramp closes on graduation geometrically, so without a floor it would
/// take ever smaller steps and never arrive. A thousandth of a SOL is small
/// enough that the last step does not overshoot into a curve that has run out
/// of real tokens, and large enough that the ramp terminates in a few dozen
/// buys.
const MIN_RAMP_LAMPORTS: u64 = 1_000_000;

/// The largest step the graduation ramp will take, in lamports.
///
/// Nothing forces this — one 28 SOL buy would reach the same place in one line.
/// It exists because a fixture is also read by people, and a curve walked to
/// graduation in three enormous buys does not look like anything that happens.
const MAX_RAMP_LAMPORTS: u64 = 3 * LAMPORTS_PER_SOL;

// ===========================================================================
// The scenario vocabulary
// ===========================================================================

/// What a generated case is built to stress.
///
/// A closed vocabulary rather than a free-form name, so `--scenario` can be
/// checked at the command line instead of failing halfway through a write, and
/// so the list of what the corpus covers is a thing in the source rather than a
/// thing in somebody's shell history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scenario {
    /// The control: a curve walked to graduation on ordinary flow, with one
    /// round trip of ours inside it. Nothing here is supposed to be alarming,
    /// which is the point — a corpus of nothing but disasters cannot show a
    /// false positive.
    Graduation,
    /// One funder, several wallets, one slot, then the same wallets back out
    /// through the door together and the floor goes with them.
    SybilRug,
    /// Our entries laddered across `β = φ / (1 - φ)`, plus a literal three-swap
    /// sandwich around one of them.
    SandwichBoundary,
    /// The same recording seen through a saturated engine: frames the queues
    /// could not take, frames the filters threw away, and a reconnect in the
    /// middle.
    Backpressure,
    /// Clean chains, edited afterwards, one way each.
    ChainCorruption,
}

impl Scenario {
    pub const ALL: [Scenario; 5] = [
        Scenario::Graduation,
        Scenario::SybilRug,
        Scenario::SandwichBoundary,
        Scenario::Backpressure,
        Scenario::ChainCorruption,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Scenario::Graduation => "graduation",
            Scenario::SybilRug => "sybil-rug",
            Scenario::SandwichBoundary => "sandwich-boundary",
            Scenario::Backpressure => "backpressure",
            Scenario::ChainCorruption => "chain-corruption",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Scenario::ALL.into_iter().find(|s| s.as_str() == text)
    }

    /// One line for `--help`.
    pub const fn summary(self) -> &'static str {
        match self {
            Scenario::Graduation => "a clean curve walked to graduation, with one round trip",
            Scenario::SybilRug => "one funder, a bundled buy and dump, then a pull",
            Scenario::SandwichBoundary => "entries laddered across the extraction threshold",
            Scenario::Backpressure => "saturated queues, filtered frames, and a reconnect",
            Scenario::ChainCorruption => "six chains, each broken a different way",
        }
    }
}

impl fmt::Display for Scenario {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ===========================================================================
// The knobs
// ===========================================================================

/// Everything a generated corpus depends on besides the scenario.
///
/// Copied into every `expected.json`, because a fixture without the settings it
/// was built under is a fixture nobody can regenerate, and a case that cannot
/// be regenerated is a blob rather than evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratorConfig {
    /// Everything random in here is addressed by `(seed, mint, label, index)`
    /// and drawn from `DrawSource`, never from a sequential generator. Adding
    /// one event to a scenario therefore moves that event's own draws and
    /// nothing else's.
    pub seed: String,
    /// The fee the generator prices its own sizing against. It has to match the
    /// `--fee-bps` the corpus is later evaluated at, or a boundary case is a
    /// boundary for a curve nobody ran.
    pub fee_bps: u16,
    /// Wallets in the coordinated bundle. At least `MIN_SYBIL_WALLETS`.
    pub sybil_wallets: u32,
    /// Wallets buying on their own account, for the bundle to hide among.
    pub organic_wallets: u32,
    /// How many files each stream is rotated into. The chain runs across the
    /// roll, per the replay specification's §3.3, so this changes the file
    /// layout and nothing the evaluator concludes.
    pub segments: usize,
    pub first_slot: u64,
    pub first_at_ms: i64,
    /// Which feed the recording claims to have come from. Ranked in the §6
    /// order key, so it is part of what makes a stream well-ordered.
    pub provider: String,
}

impl Default for GeneratorConfig {
    fn default() -> Self {
        GeneratorConfig {
            // The seed the roadmap's chaos and replay runs are written at.
            seed: "0x100x".to_string(),
            fee_bps: DEFAULT_FEE_BPS,
            sybil_wallets: 6,
            organic_wallets: 3,
            segments: 1,
            // A slot and a wall time far enough into plausible territory that
            // nobody mistakes a fixture for a recording, and fixed so that two
            // runs agree.
            first_slot: 300_000_000,
            first_at_ms: 1_700_000_000_000,
            provider: "helius".to_string(),
        }
    }
}

impl GeneratorConfig {
    /// Whether these knobs describe something this module can honestly build.
    pub fn check(&self) -> Result<(), FixtureError> {
        if self.sybil_wallets < MIN_SYBIL_WALLETS {
            return Err(FixtureError::TooFewWallets {
                asked: self.sybil_wallets,
                needed: MIN_SYBIL_WALLETS,
            });
        }
        if self.segments == 0 {
            return Err(FixtureError::NoSegments);
        }
        if self.fee_bps >= 10_000 {
            return Err(FixtureError::FeeTooLarge {
                fee_bps: self.fee_bps,
            });
        }
        Ok(())
    }
}

// ===========================================================================
// What can go wrong
// ===========================================================================

/// Anything that stops a corpus being generated or written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixtureError {
    Io {
        path: String,
        detail: String,
    },
    /// A file is already there and `--force` was not given. Refusing rather
    /// than overwriting: a fixture directory is somebody's evidence until they
    /// say otherwise.
    Exists {
        path: String,
    },
    UnknownScenario {
        name: String,
    },
    TooFewWallets {
        asked: u32,
        needed: u32,
    },
    NoSegments,
    FeeTooLarge {
        fee_bps: u16,
    },
}

impl fmt::Display for FixtureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FixtureError::Io { path, detail } => write!(f, "{path}: {detail}"),
            FixtureError::Exists { path } => {
                write!(f, "{path} already exists; pass --force to replace it")
            }
            FixtureError::UnknownScenario { name } => {
                write!(
                    f,
                    "unknown scenario {name:?}; known: all, {}",
                    Scenario::ALL
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            FixtureError::TooFewWallets { asked, needed } => write!(
                f,
                "a bundle of {asked} wallet(s) is not coordination; {needed} is the floor"
            ),
            FixtureError::NoSegments => {
                write!(f, "a stream has to be written to at least one file")
            }
            FixtureError::FeeTooLarge { fee_bps } => {
                write!(f, "a fee of {fee_bps} basis points leaves nothing to trade")
            }
        }
    }
}

impl std::error::Error for FixtureError {}

// ===========================================================================
// What a generated case is
// ===========================================================================

/// One file of a generated case, in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedFile {
    pub name: String,
    pub text: String,
}

/// What the generator says the harness should conclude about one launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedLaunch {
    pub mint: String,
    /// The `label` event written into the stream, which is the ground truth the
    /// classifier is graded against.
    pub labelled: RugClass,
    /// What the classifier should make of it. Equal to `labelled` in every case
    /// here, because a synthetic launch whose own generator cannot say what
    /// happened to it is not a test of anything.
    pub classified: RugClass,
    /// Per entry, in order: whether `β > φ / (1 - φ)` at the curve that entry
    /// was priced against. Computed here by the same integer comparison the
    /// evaluator uses, against the same reserves.
    pub entries_above_threshold: Vec<bool>,
    /// Wallets the generator put behind one funder in one slot, or zero when
    /// the case has no bundle.
    pub bundled_wallets: u32,
    /// Where the generator's own mirror of the curve ended up.
    ///
    /// This is the field that keeps the generator honest. Every size in a case
    /// is computed against the mirror, so if the mirror and the evaluator ever
    /// disagree about what an event does — a frame the filters rejected being
    /// replayed anyway, a rounding rule copied wrongly — every boundary the
    /// case claims to sit on is a boundary somewhere else. Comparing the two
    /// end states catches that in one number.
    pub final_real_sol_lamports: u64,
}

/// What the generator says the harness should conclude about the whole case.
///
/// Written beside the streams as `expected.json`, which the evaluator ignores —
/// it reads `.jsonl` and `manifest.json` and nothing else — so this is
/// documentation that happens to be machine-readable rather than an input that
/// could steer the result it claims to predict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expected {
    pub schema: String,
    pub scenario: Scenario,
    pub case: String,
    /// What this case is for, in a sentence.
    pub note: String,
    pub config: GeneratorConfig,
    /// Whether `sts backtest run --gate` should accept this corpus.
    pub gate_ready: bool,
    /// Records the harness should be able to parse out of the files. Not the
    /// manifest's count when the recording was cut short, and one less than the
    /// line count when the last line is a fragment rather than a record.
    pub records: u64,
    /// Frames the recording says a bounded channel could not take. They are
    /// replayed anyway — that is what recovery means here — so they move the
    /// curve and they are counted separately.
    pub frames_backpressure: u64,
    /// Frames the live filters rejected. They are *not* replayed, so the curve
    /// they would have moved stays where it was.
    pub frames_dropped_live: u64,
    /// Which file the first bad line is in, and where.
    pub break_file: Option<String>,
    pub break_line: Option<usize>,
    pub break_status: Option<LineStatus>,
    /// Why the run should be refused, in the generator's words. Not compared
    /// against the evaluator's wording — the evaluator's refusals are prose
    /// aimed at a person, and pinning that text here would make every reworded
    /// message a failing test.
    pub refusal_reasons: Vec<String>,
    pub launches: Vec<ExpectedLaunch>,
}

/// One generated case: the streams, the manifest that describes them, and what
/// the harness is supposed to say about the pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureCase {
    /// The directory name. Unique across a corpus, and safe on a filesystem.
    pub name: String,
    pub scenario: Scenario,
    pub stream_id: String,
    pub files: Vec<GeneratedFile>,
    pub manifest: Manifest,
    pub expected: Expected,
}

impl FixtureCase {
    /// Every line of every segment, in order. What a single-file reader of this
    /// case would see.
    pub fn text(&self) -> String {
        self.files.iter().map(|f| f.text.as_str()).collect()
    }
}

// ===========================================================================
// The event writers
// ===========================================================================

/// Compact JSON for one event.
///
/// Built through `serde_json::Value`, whose object is a `BTreeMap` in this
/// build, so the key order is the key order and not an insertion order that
/// changes when a field is added. These bytes are hashed into the chain, so a
/// serialiser that reordered keys between runs would produce a fixture that
/// fails its own integrity check.
fn frame_bytes(value: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&value).expect("a serde_json::Value serialises")
}

fn launch_event(mint: &str, at_ms: i64, creator: &str, real_sol_lamports: u64) -> Vec<u8> {
    frame_bytes(serde_json::json!({
        "schema": EVENT_SCHEMA,
        "kind": "launch",
        "mint": mint,
        "at_ms": at_ms,
        "creator": creator,
        "real_sol_lamports": real_sol_lamports,
    }))
}

fn buy_event(
    mint: &str,
    at_ms: i64,
    wallet: &str,
    funder: Option<&str>,
    gross_lamports: u64,
) -> Vec<u8> {
    frame_bytes(serde_json::json!({
        "schema": EVENT_SCHEMA,
        "kind": "flow",
        "mint": mint,
        "at_ms": at_ms,
        "wallet": wallet,
        "funder": funder,
        "side": "buy",
        "gross_lamports": gross_lamports,
    }))
}

fn sell_event(mint: &str, at_ms: i64, wallet: &str, tokens: u64) -> Vec<u8> {
    frame_bytes(serde_json::json!({
        "schema": EVENT_SCHEMA,
        "kind": "flow",
        "mint": mint,
        "at_ms": at_ms,
        "wallet": wallet,
        "side": "sell",
        "tokens": tokens,
    }))
}

fn entry_event(mint: &str, at_ms: i64, gross_lamports: u64, tag: &str) -> Vec<u8> {
    frame_bytes(serde_json::json!({
        "schema": EVENT_SCHEMA,
        "kind": "entry",
        "mint": mint,
        "at_ms": at_ms,
        "gross_lamports": gross_lamports,
        "tag": tag,
    }))
}

fn exit_event(mint: &str, at_ms: i64, tokens: Option<u64>, tag: &str) -> Vec<u8> {
    frame_bytes(serde_json::json!({
        "schema": EVENT_SCHEMA,
        "kind": "exit",
        "mint": mint,
        "at_ms": at_ms,
        "tokens": tokens,
        "tag": tag,
    }))
}

fn holders_event(mint: &str, at_ms: i64, holders: &[(String, u64)]) -> Vec<u8> {
    let list: Vec<serde_json::Value> = holders
        .iter()
        .map(|(wallet, balance)| serde_json::json!({ "wallet": wallet, "balance": balance }))
        .collect();
    frame_bytes(serde_json::json!({
        "schema": EVENT_SCHEMA,
        "kind": "holders",
        "mint": mint,
        "at_ms": at_ms,
        "holders": list,
    }))
}

fn pull_event(mint: &str, at_ms: i64, wallet: &str, lamports: u64) -> Vec<u8> {
    frame_bytes(serde_json::json!({
        "schema": EVENT_SCHEMA,
        "kind": "pull",
        "mint": mint,
        "at_ms": at_ms,
        "wallet": wallet,
        "lamports": lamports,
    }))
}

fn label_event(mint: &str, outcome: RugClass) -> Vec<u8> {
    frame_bytes(serde_json::json!({
        "schema": EVENT_SCHEMA,
        "kind": "label",
        "mint": mint,
        "outcome": outcome.as_str(),
    }))
}

// ===========================================================================
// Composing a stream
// ===========================================================================

/// One thing the recorder saw, before it was given a place in the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScriptedFrame {
    slot: u64,
    at_ms: i64,
    connection: u32,
    kind: RecordKind,
    frame: Option<Vec<u8>>,
    outcome: RecordOutcome,
}

/// Whether the evaluator will act on a frame with this outcome.
///
/// The rule lives in one place because the generator has to obey it exactly:
/// a frame the live engine filtered is not replayed, so the generator's mirror
/// must not move for it either, or every number the case predicts is computed
/// against a curve the evaluator never sees. Backpressure is the opposite — the
/// frame was recorded and *is* replayed, which is the whole of what recovery
/// means here.
const fn is_applied(outcome: RecordOutcome) -> bool {
    !matches!(outcome, RecordOutcome::Dropped(_))
}

/// What the evaluator will believe about one launch, kept in step frame by
/// frame.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mirror {
    curve: CurveState,
    position_tokens: u64,
    /// Tokens per wallet, from the fills the curve actually gave them. A
    /// holder snapshot built from these is a snapshot of what the curve did
    /// rather than a number somebody typed.
    balances: BTreeMap<String, u64>,
    entries_above_threshold: Vec<bool>,
}

impl Mirror {
    fn new(curve: CurveState) -> Self {
        Mirror {
            curve,
            position_tokens: 0,
            balances: BTreeMap::new(),
            entries_above_threshold: Vec::new(),
        }
    }
}

/// Builds one stream's frames while keeping every curve in it in step.
///
/// The mirror is the reason this is a struct rather than a list of string
/// literals. Sizing an entry one lamport below the extraction threshold needs
/// the virtual SOL reserve *at that instant*, and that reserve is a function of
/// every frame emitted before it. The alternative — writing the sizes out by
/// hand — produces a fixture whose boundary cases sit wherever the arithmetic
/// happened to land.
struct Composer {
    fee_bps: u16,
    draws: DrawSource,
    frames: Vec<ScriptedFrame>,
    slot: u64,
    at_ms: i64,
    connection: u32,
    /// While set, every frame lands in the slot the bundle opened in.
    bundled: bool,
    /// False until the first frame, so the first one lands on `first_slot`
    /// rather than one past it.
    advanced: bool,
    curves: BTreeMap<String, Mirror>,
}

impl Composer {
    fn new(config: &GeneratorConfig, stream_id: &str) -> Self {
        Composer {
            fee_bps: config.fee_bps,
            // The stream id is folded into the seed, so two scenarios generated
            // from one seed do not draw the same numbers in the same order.
            draws: DrawSource::new(&format!("{}::{stream_id}", config.seed)),
            frames: Vec::new(),
            slot: config.first_slot,
            at_ms: config.first_at_ms,
            connection: 1,
            bundled: false,
            advanced: false,
            curves: BTreeMap::new(),
        }
    }

    /// A draw in `[0, span)`, addressed by name and index rather than taken
    /// from a sequence.
    fn draw(&self, mint: &str, label: &str, index: u64, span: u64) -> u64 {
        self.draws.below(mint, label, index, span)
    }

    /// Moves to the next slot, unless a bundle is open.
    fn tick(&mut self) {
        if self.bundled {
            return;
        }
        if !self.advanced {
            self.advanced = true;
            return;
        }
        self.slot += 1;
        self.at_ms += SLOT_MS;
    }

    /// Lets time pass without recording anything, so a fall can be placed
    /// inside or outside the classifier's window on purpose.
    fn wait_slots(&mut self, slots: u64) {
        self.slot += slots;
        self.at_ms += SLOT_MS * slots as i64;
    }

    /// Puts everything until `end_bundle` in one slot at one instant.
    ///
    /// This is what a bundle is: several transactions a block builder placed
    /// together, indistinguishable in time to anything reading the chain
    /// afterwards. It is also what makes the synchrony kernel read exactly one:
    /// every gap in it is zero.
    fn begin_bundle(&mut self) {
        self.tick();
        self.bundled = true;
    }

    fn end_bundle(&mut self) {
        self.bundled = false;
    }

    fn push(&mut self, kind: RecordKind, frame: Option<Vec<u8>>, outcome: RecordOutcome) {
        self.frames.push(ScriptedFrame {
            slot: self.slot,
            at_ms: self.at_ms,
            connection: self.connection,
            kind,
            frame,
            outcome,
        });
    }

    /// A connection-lifecycle record. Carries no frame, so the evaluator skips
    /// it — which is exactly why a fixture should contain some.
    fn lifecycle(&mut self, kind: RecordKind) {
        self.tick();
        self.push(kind, None, RecordOutcome::Accepted);
    }

    /// The socket dropped and came back. The connection number goes up, which
    /// keeps the §6 order key increasing across the gap.
    fn reconnect(&mut self) {
        self.lifecycle(RecordKind::Closed);
        self.connection += 1;
        self.lifecycle(RecordKind::Connected);
        self.lifecycle(RecordKind::Ack);
    }

    fn mirror(&mut self, mint: &str) -> &mut Mirror {
        self.curves
            .entry(mint.to_string())
            .or_insert_with(|| Mirror::new(CurveState::LAUNCH))
    }

    fn curve(&self, mint: &str) -> CurveState {
        self.curves
            .get(mint)
            .map(|m| m.curve)
            .unwrap_or(CurveState::LAUNCH)
    }

    fn launch(&mut self, mint: &str, creator: &str, real_sol_lamports: u64) {
        self.tick();
        let frame = launch_event(mint, self.at_ms, creator, real_sol_lamports);
        self.push(RecordKind::Frame, Some(frame), RecordOutcome::Accepted);
        self.curves.insert(
            mint.to_string(),
            Mirror::new(CurveState::at_real_sol(real_sol_lamports)),
        );
    }

    /// Somebody else's buy. Returns the tokens the curve gave them, or zero
    /// when it refused or the frame never reached the engine.
    fn flow_buy(
        &mut self,
        mint: &str,
        wallet: &str,
        funder: Option<&str>,
        gross_lamports: u64,
        outcome: RecordOutcome,
    ) -> u64 {
        self.tick();
        let frame = buy_event(mint, self.at_ms, wallet, funder, gross_lamports);
        self.push(RecordKind::Frame, Some(frame), outcome);
        if !is_applied(outcome) {
            return 0;
        }
        let fee = self.fee_bps;
        let mirror = self.mirror(mint);
        match mirror.curve.quote_buy(gross_lamports, fee) {
            Ok(fill) => {
                mirror.curve = mirror.curve.after_buy(&fill);
                *mirror.balances.entry(wallet.to_string()).or_insert(0) += fill.tokens;
                fill.tokens
            }
            Err(_) => 0,
        }
    }

    /// Somebody else's sell, of everything that wallet holds.
    fn flow_sell_all(&mut self, mint: &str, wallet: &str, outcome: RecordOutcome) {
        let tokens = self
            .curves
            .get(mint)
            .and_then(|m| m.balances.get(wallet).copied())
            .unwrap_or(0);
        if tokens == 0 {
            return;
        }
        self.tick();
        let frame = sell_event(mint, self.at_ms, wallet, tokens);
        self.push(RecordKind::Frame, Some(frame), outcome);
        if !is_applied(outcome) {
            return;
        }
        let fee = self.fee_bps;
        let mirror = self.mirror(mint);
        if let Ok(fill) = mirror.curve.quote_sell(tokens, fee) {
            mirror.curve = mirror.curve.after_sell(&fill);
            mirror.balances.insert(wallet.to_string(), 0);
        }
    }

    /// Our buy. The extraction verdict is taken against the curve as it stands
    /// now, which is the state the evaluator will price it against.
    fn entry(&mut self, mint: &str, gross_lamports: u64, tag: &str, outcome: RecordOutcome) {
        self.tick();
        let frame = entry_event(mint, self.at_ms, gross_lamports, tag);
        self.push(RecordKind::Frame, Some(frame), outcome);
        if !is_applied(outcome) {
            return;
        }
        let fee = self.fee_bps;
        let mirror = self.mirror(mint);
        let before = mirror.curve;
        // Recorded only when the quote succeeds, because that is the only case
        // in which the evaluator prices adverse selection at all.
        if let Ok(fill) = before.quote_buy(gross_lamports, fee) {
            mirror.entries_above_threshold.push(sandwich_viable(
                gross_lamports,
                before.virtual_sol_reserves,
                fee,
            ));
            mirror.curve = before.after_buy(&fill);
            mirror.position_tokens = mirror.position_tokens.saturating_add(fill.tokens);
        }
    }

    /// Our sell, of the whole position.
    fn exit_all(&mut self, mint: &str, tag: &str, outcome: RecordOutcome) {
        self.tick();
        let frame = exit_event(mint, self.at_ms, None, tag);
        self.push(RecordKind::Frame, Some(frame), outcome);
        if !is_applied(outcome) {
            return;
        }
        let fee = self.fee_bps;
        let mirror = self.mirror(mint);
        let wanted = mirror.position_tokens;
        if wanted == 0 {
            return;
        }
        if let Ok(fill) = mirror.curve.quote_sell(wanted, fee) {
            mirror.curve = mirror.curve.after_sell(&fill);
            mirror.position_tokens = 0;
        }
    }

    /// A holder snapshot, from the balances the curve actually handed out plus
    /// whatever else the recording claims to have seen.
    fn holders(&mut self, mint: &str, extra: &[(String, u64)]) {
        let mut holders: Vec<(String, u64)> = self
            .curves
            .get(mint)
            .map(|m| {
                m.balances
                    .iter()
                    .filter(|(_, &balance)| balance > 0)
                    .map(|(wallet, &balance)| (wallet.clone(), balance))
                    .collect()
            })
            .unwrap_or_default();
        holders.extend_from_slice(extra);
        // Balance descending, address ascending — the order `decode_event` will
        // put them in anyway. Doing it here as well means the bytes on disk read
        // the way the evaluator reads them.
        holders.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        self.tick();
        let frame = holders_event(mint, self.at_ms, &holders);
        self.push(RecordKind::Frame, Some(frame), RecordOutcome::Accepted);
    }

    /// Liquidity leaving outside the swap path.
    fn pull(&mut self, mint: &str, wallet: &str, lamports: u64) {
        self.tick();
        let frame = pull_event(mint, self.at_ms, wallet, lamports);
        self.push(RecordKind::Frame, Some(frame), RecordOutcome::Accepted);
        let mirror = self.mirror(mint);
        let magnitude = lamports.min(mirror.curve.real_sol_reserves);
        if magnitude == 0 {
            return;
        }
        let signed = i64::try_from(magnitude).unwrap_or(i64::MAX);
        if let Some(next) = mirror.curve.displaced(-signed) {
            mirror.curve = next;
        }
    }

    fn label(&mut self, mint: &str, outcome: RugClass) {
        self.tick();
        let frame = label_event(mint, outcome);
        self.push(RecordKind::Frame, Some(frame), RecordOutcome::Accepted);
    }

    /// Walks the curve up to `target` on ordinary flow.
    ///
    /// Steps are a third of what is left, floored and capped, so the approach is
    /// geometric and terminates: the floor guarantees the last step crosses the
    /// line rather than halving towards it forever, and the cap keeps the ramp
    /// looking like a series of buys instead of one enormous one. A step the
    /// curve refuses — which happens near graduation, where the real token
    /// reserve runs out before the virtual one does — is halved and retried.
    fn ramp_to(&mut self, mint: &str, target_real_sol: u64, label: &str) {
        let mut index = 0u64;
        while self.curve(mint).real_sol_reserves < target_real_sol && index < 200 {
            let curve = self.curve(mint);
            let gap = target_real_sol - curve.real_sol_reserves;
            let mut gross = (gap / 3).clamp(MIN_RAMP_LAMPORTS, MAX_RAMP_LAMPORTS);
            // A little seeded variation, so the ramp is not a metronome and two
            // seeds produce visibly different corpora.
            gross += self.draw(mint, label, index, gross / 4 + 1);

            // Back off until the curve will take it. Bounded, so a curve that
            // will take nothing at all ends the ramp instead of spinning.
            let mut attempts = 0;
            while curve.quote_buy(gross, self.fee_bps).is_err() && attempts < 40 {
                gross /= 2;
                attempts += 1;
            }
            if gross == 0 || curve.quote_buy(gross, self.fee_bps).is_err() {
                return;
            }

            let wallet = format!("wallet-flow-{index:03}");
            self.flow_buy(mint, &wallet, None, gross, RecordOutcome::Accepted);
            index += 1;
        }
    }
}

// ===========================================================================
// Sealing, rotation, and the manifest
// ===========================================================================

/// Seals scripted frames into a hash chain, the way the recorder does.
///
/// `dispatch_latency_us` is a host measurement in a real recording, which is
/// exactly the kind of number that would make a generated fixture differ
/// between runs. It is drawn here instead — addressed by sequence number, so it
/// is stable — because a fixture with the field missing everywhere would not
/// exercise the readers that expect it.
fn seal(
    config: &GeneratorConfig,
    stream_id: &str,
    frames: &[ScriptedFrame],
) -> (Vec<ReplayRecord>, [u8; 32]) {
    let draws = DrawSource::new(&format!("{}::{stream_id}::latency", config.seed));
    let mut writer = ChainWriter::new(stream_id);
    let mut records = Vec::with_capacity(frames.len());
    for (index, frame) in frames.iter().enumerate() {
        let seq = index as u64;
        records.push(writer.seal(RecordDraft {
            event_id: format!("{stream_id}-{seq:06}"),
            slot: frame.slot,
            observed_at_ms: frame.at_ms,
            provider: config.provider.clone(),
            endpoint_index: 0,
            connection: frame.connection,
            kind: frame.kind,
            frame: frame.frame.clone(),
            outcome: frame.outcome,
            dispatch_latency_us: Some(80 + draws.below(stream_id, "dispatch", seq, 240) as u32),
        }));
    }
    let head = writer.head();
    (records, head)
}

/// Splits sealed records into rotated segments.
///
/// The chain is not restarted at the boundary: §3.3's rule is that segmentation
/// is a storage detail and the chain runs across the roll, so the first record
/// of `001.jsonl` still links to the last record of `000.jsonl`. Rotating a
/// fixture must not change a single thing the evaluator concludes, which is
/// what `rotation_changes_the_files_and_nothing_else` checks.
fn split(records: &[ReplayRecord], parts: usize) -> Vec<GeneratedFile> {
    let per = records.len().div_ceil(parts.max(1)).max(1);
    records
        .chunks(per)
        .enumerate()
        .map(|(index, chunk)| GeneratedFile {
            name: format!("{index:03}.jsonl"),
            text: write_stream(chunk),
        })
        .collect()
}

/// Which file a record index lands in once the stream is rotated, and which
/// line of it. The inverse of `split`, and the reason a corruption case can say
/// where its break is before anything has read the file.
fn locate(index: usize, total: usize, parts: usize) -> (String, usize) {
    let per = total.div_ceil(parts.max(1)).max(1);
    (format!("{:03}.jsonl", index / per), index % per + 1)
}

/// The manifest for a stream, describing what the recorder sealed.
///
/// `created_at_ms` comes off the first record rather than the clock. A manifest
/// stamped with the time it was written would make every regeneration of a
/// fixture a different fixture, which is the one thing this module must not do.
fn manifest_for(
    stream_id: &str,
    records: &[ReplayRecord],
    head: [u8; 32],
    files: &[GeneratedFile],
    complete: bool,
) -> Manifest {
    let created = records.first().map(|r| r.observed_at_ms).unwrap_or(0);
    let mut manifest = Manifest::for_records(stream_id, records, head, created);
    manifest.segments = files
        .iter()
        .map(|file| Segment {
            file: file.name.clone(),
            records: file.text.lines().filter(|l| !l.trim().is_empty()).count() as u64,
            sha256: sha256_hex(file.text.as_bytes()),
        })
        .collect();
    manifest.complete = complete;
    manifest
}

// ===========================================================================
// Sizing helpers
// ===========================================================================

/// The largest position doctrine's participation cap allows against the
/// executable liquidity here.
///
/// Floored, because a curve with almost no real SOL in it caps at zero and an
/// entry of zero is not an entry — a fixture that silently skipped our trade
/// would be a fixture that tests nothing about trading.
fn entry_size(curve: &CurveState) -> u64 {
    curve
        .max_position_lamports(DEFAULT_MAX_POOL_SHARE_BPS)
        .max(MIN_RAMP_LAMPORTS)
}

/// The largest size at or below `wanted` this curve will actually fill.
///
/// Near graduation the real token reserve runs out before the virtual one does,
/// so a buy sized off the virtual reserve alone is refused. Halving until it
/// fits is deterministic and terminates; returning zero says the curve will not
/// take a trade at all, which the caller has to decide about rather than
/// discover as a missing event.
fn affordable(curve: &CurveState, wanted: u64, fee_bps: u16) -> u64 {
    let mut size = wanted;
    let mut attempts = 0;
    while size > 0 && curve.quote_buy(size, fee_bps).is_err() && attempts < 64 {
        size /= 2;
        attempts += 1;
    }
    if size == 0 || curve.quote_buy(size, fee_bps).is_err() {
        return 0;
    }
    size
}

// ===========================================================================
// The scenarios
// ===========================================================================

/// Assembles a case from a finished composer.
fn assemble(
    config: &GeneratorConfig,
    scenario: Scenario,
    name: &str,
    stream_id: &str,
    note: &str,
    composer: Composer,
    launches: Vec<ExpectedLaunch>,
) -> FixtureCase {
    let (records, head) = seal(config, stream_id, &composer.frames);
    let files = split(&records, config.segments);
    let manifest = manifest_for(stream_id, &records, head, &files, true);

    let backpressure = composer
        .frames
        .iter()
        .filter(|f| f.outcome.is_backpressure())
        .count() as u64;
    let dropped = composer
        .frames
        .iter()
        .filter(|f| matches!(f.outcome, RecordOutcome::Dropped(_)))
        .count() as u64;

    FixtureCase {
        name: name.to_string(),
        scenario,
        stream_id: stream_id.to_string(),
        expected: Expected {
            schema: EXPECTED_SCHEMA.to_string(),
            scenario,
            case: name.to_string(),
            note: note.to_string(),
            config: config.clone(),
            gate_ready: true,
            records: records.len() as u64,
            frames_backpressure: backpressure,
            frames_dropped_live: dropped,
            break_file: None,
            break_line: None,
            break_status: None,
            refusal_reasons: Vec::new(),
            launches,
        },
        files,
        manifest,
    }
}

/// Where the composer's mirror of one launch's curve ended up.
fn final_real_sol(composer: &Composer, mint: &str) -> u64 {
    composer.curve(mint).real_sol_reserves
}

/// The entries a composer priced for one mint, in order.
fn entries_of(composer: &Composer, mint: &str) -> Vec<bool> {
    composer
        .curves
        .get(mint)
        .map(|m| m.entries_above_threshold.clone())
        .unwrap_or_default()
}

/// The control: an ordinary curve, walked to graduation, with one round trip of
/// ours inside it.
///
/// A corpus of nothing but disasters cannot show a false positive, and every
/// number in the report — precision, avoidance, the confusion matrix — needs a
/// negative case to mean anything. This is that case: no pull, no bundle, no
/// broken line, one closed trade, and a launch the classifier should call
/// `graduated` without being told.
fn graduation(config: &GeneratorConfig) -> FixtureCase {
    let mint = "mint-graduation";
    let mut composer = Composer::new(config, "graduation");

    composer.lifecycle(RecordKind::Connected);
    composer.lifecycle(RecordKind::Ack);
    composer.launch(mint, "creator-graduation", 0);

    composer.ramp_to(mint, 12 * LAMPORTS_PER_SOL, "early");
    let size = entry_size(&composer.curve(mint));
    composer.entry(mint, size, "graduation-entry", RecordOutcome::Accepted);

    composer.ramp_to(mint, 40 * LAMPORTS_PER_SOL, "mid");
    composer.exit_all(mint, "graduation-exit", RecordOutcome::Accepted);

    // The rest of the way. Selling after this point would be quoting against a
    // pool that has moved on, which is a different test.
    composer.ramp_to(mint, PUMP_GRADUATION_LAMPORTS, "late");
    composer.holders(mint, &[]);
    composer.label(mint, RugClass::Graduated);
    composer.lifecycle(RecordKind::Closed);

    let launches = vec![ExpectedLaunch {
        mint: mint.to_string(),
        labelled: RugClass::Graduated,
        classified: RugClass::Graduated,
        entries_above_threshold: entries_of(&composer, mint),
        bundled_wallets: 0,
        final_real_sol_lamports: final_real_sol(&composer, mint),
    }];
    assemble(
        config,
        Scenario::Graduation,
        "graduation",
        "graduation",
        "a clean curve walked to graduation on ordinary flow, with one round trip of ours \
         inside it. Nothing here should be refused and nothing should be flagged.",
        composer,
        launches,
    )
}

/// One funder, several wallets, one slot, and then the floor.
///
/// The shape this is built to reproduce is the one the wallet-level metrics
/// cannot see on their own: every wallet in the bundle is small, so per-wallet
/// concentration looks unremarkable, and it is only when they are grouped by
/// who paid for them that the launch turns into one hand holding most of the
/// flow. The buys are in one slot at one instant, so the synchrony kernel reads
/// exactly one and the geometric mean cannot be talked down.
///
/// It ends stranded on purpose. After the dump and the pull there is a lamport
/// of real SOL left in the curve, so our exit is refused rather than filled and
/// the position lands in `stranded` with `no_executable_exit` set. A rug fixture
/// whose exit fills is a fixture about a launch that went down, not about one
/// that took the door with it.
fn sybil_rug(config: &GeneratorConfig) -> FixtureCase {
    let mint = "mint-sybil-rug";
    let creator = "wallet-creator";
    let funder = "funder-sybil-1";
    let mut composer = Composer::new(config, "sybil-rug");

    composer.lifecycle(RecordKind::Connected);
    composer.lifecycle(RecordKind::Ack);
    composer.launch(mint, creator, 0);

    // The dev bag, bought rather than asserted, so the holder snapshot is a
    // consequence of the curve instead of a number somebody chose.
    composer.flow_buy(
        mint,
        creator,
        None,
        5 * LAMPORTS_PER_SOL,
        RecordOutcome::Accepted,
    );

    // Independent buyers, each behind their own funder, for the bundle to hide
    // among. One wallet per funder is below any cluster floor, so none of these
    // becomes a cluster of its own.
    for index in 0..config.organic_wallets {
        let wallet = format!("wallet-organic-{index:02}");
        let organic_funder = format!("funder-organic-{index:02}");
        let gross =
            200_000_000 + composer.draw(mint, "organic-size", u64::from(index), 400_000_000);
        composer.flow_buy(
            mint,
            &wallet,
            Some(&organic_funder),
            gross,
            RecordOutcome::Accepted,
        );
    }

    // The bundle. One slot, one instant, one funder.
    composer.begin_bundle();
    for index in 0..config.sybil_wallets {
        let wallet = format!("wallet-sybil-{index:02}");
        let gross = 600_000_000 + composer.draw(mint, "sybil-size", u64::from(index), 800_000_000);
        composer.flow_buy(mint, &wallet, Some(funder), gross, RecordOutcome::Accepted);
    }
    composer.end_bundle();

    // Taken here, at peak concentration, and not again. The evaluator carries
    // the last snapshot in the stream, so a second one after the dump would
    // replace this with a picture of an empty curve and the case would be about
    // nothing.
    composer.holders(mint, &[]);

    let size = entry_size(&composer.curve(mint));
    composer.entry(mint, size, "sybil-entry", RecordOutcome::Accepted);

    // Out through the same door, together, one slot.
    composer.wait_slots(4);
    composer.begin_bundle();
    for index in 0..config.sybil_wallets {
        let wallet = format!("wallet-sybil-{index:02}");
        composer.flow_sell_all(mint, &wallet, RecordOutcome::Accepted);
    }
    composer.flow_sell_all(mint, creator, RecordOutcome::Accepted);
    composer.end_bundle();

    // And the floor goes with them. One lamport is left so the curve is still
    // priceable and still cannot pay anybody out.
    let left = composer.curve(mint).real_sol_reserves;
    composer.pull(mint, creator, left.saturating_sub(1));

    composer.exit_all(mint, "sybil-exit", RecordOutcome::Accepted);
    composer.label(mint, RugClass::Rug);
    composer.lifecycle(RecordKind::Closed);

    let launches = vec![ExpectedLaunch {
        mint: mint.to_string(),
        labelled: RugClass::Rug,
        classified: RugClass::Rug,
        entries_above_threshold: entries_of(&composer, mint),
        bundled_wallets: config.sybil_wallets,
        final_real_sol_lamports: final_real_sol(&composer, mint),
    }];
    assemble(
        config,
        Scenario::SybilRug,
        "sybil-rug",
        "sybil-rug",
        "one funder behind every wallet in a same-slot bundle, a same-slot dump, and a pull \
         that leaves one lamport in the curve. Our exit is refused and the position strands.",
        composer,
        launches,
    )
}

/// Our entries laddered across `β = φ / (1 - φ)`, at three points on the curve.
///
/// The ladder is one lamport under the threshold, exactly on it, one lamport
/// over, and then far above with a real three-swap sandwich wrapped around it.
/// The exact-boundary rung is deliberately not asserted either way: §15.2 says
/// there is no sign to claim at the threshold, and `sandwich_breakeven_victim_lamports`
/// rounds up, so whether that rung clears depends on whether the division came
/// out even. What the case does claim is that the generator and the evaluator
/// agree about it, which is the property worth testing.
///
/// Each rung is sized against the virtual reserve *at the instant it is
/// emitted*, not against the reserve at the start of the launch. Every event
/// before it has already moved the curve, and a threshold computed against a
/// curve that has since moved is not a threshold.
fn sandwich_boundary(config: &GeneratorConfig) -> FixtureCase {
    let mut composer = Composer::new(config, "sandwich-boundary");
    let fee = config.fee_bps;
    composer.lifecycle(RecordKind::Connected);
    composer.lifecycle(RecordKind::Ack);

    // The three positions §15.2's table is written at, less the launch's 30 SOL
    // of virtual reserve: an empty curve, the middle of the bond, and one just
    // short of graduation. 115 virtual SOL — the table's third row — is the
    // graduation point exactly, where every quote is refused, so the last rung
    // sits five SOL below it.
    let positions: [u64; 3] = [0, 45 * LAMPORTS_PER_SOL, 80 * LAMPORTS_PER_SOL];
    let mut launches = Vec::with_capacity(positions.len());

    for real_sol in positions {
        let virtual_sol = (real_sol / LAMPORTS_PER_SOL) + 30;
        let mint = format!("mint-sandwich-{virtual_sol:03}");
        composer.launch(&mint, "creator-sandwich", real_sol);
        // One ordinary buy, so the launch has more than the opening observation
        // and the classifier has something to be sure about.
        composer.flow_buy(
            &mint,
            "wallet-organic-00",
            None,
            250_000_000,
            RecordOutcome::Accepted,
        );

        for (index, offset) in [-1i64, 0, 1].into_iter().enumerate() {
            let breakeven =
                sandwich_breakeven_victim_lamports(composer.curve(&mint).virtual_sol_reserves, fee);
            let wanted = if offset < 0 {
                breakeven.saturating_sub(1)
            } else {
                breakeven.saturating_add(offset.unsigned_abs())
            }
            .max(1);
            let size = affordable(&composer.curve(&mint), wanted, fee);
            if size == wanted {
                composer.entry(
                    &mint,
                    size,
                    &format!("rung-{index}"),
                    RecordOutcome::Accepted,
                );
                composer.exit_all(
                    &mint,
                    &format!("rung-{index}-unwind"),
                    RecordOutcome::Accepted,
                );
            }
        }

        // The heavy one: a front-run, our buy, and the front-runner back out,
        // all in one slot. This is what the threshold is a statement about.
        composer.wait_slots(2);
        composer.begin_bundle();
        let front = affordable(
            &composer.curve(&mint),
            sandwich_breakeven_victim_lamports(composer.curve(&mint).virtual_sol_reserves, fee) / 2,
            fee,
        );
        if front > 0 {
            composer.flow_buy(
                &mint,
                "wallet-attacker",
                None,
                front,
                RecordOutcome::Accepted,
            );
        }
        let breakeven =
            sandwich_breakeven_victim_lamports(composer.curve(&mint).virtual_sol_reserves, fee);
        let victim = affordable(&composer.curve(&mint), breakeven.saturating_mul(8), fee);
        if victim > 0 {
            composer.entry(&mint, victim, "sandwiched", RecordOutcome::Accepted);
        }
        composer.flow_sell_all(&mint, "wallet-attacker", RecordOutcome::Accepted);
        composer.end_bundle();
        composer.exit_all(&mint, "sandwiched-unwind", RecordOutcome::Accepted);

        composer.holders(&mint, &[]);
        composer.label(&mint, RugClass::Held);

        launches.push(ExpectedLaunch {
            mint: mint.clone(),
            labelled: RugClass::Held,
            classified: RugClass::Held,
            entries_above_threshold: entries_of(&composer, &mint),
            bundled_wallets: 0,
            final_real_sol_lamports: final_real_sol(&composer, &mint),
        });
    }
    composer.lifecycle(RecordKind::Closed);

    assemble(
        config,
        Scenario::SandwichBoundary,
        "sandwich-boundary",
        "sandwich-boundary",
        "three curve positions, each with our entries one lamport under, on, and one lamport \
         over the extraction threshold, plus a three-swap sandwich around a heavy one.",
        composer,
        launches,
    )
}

/// The same recording seen through an engine that could not keep up.
///
/// Two outcomes that look similar in a log mean opposite things here. A frame
/// the filters rejected is not replayed, so the curve it would have moved stays
/// where it was — reading it would be the filtering bug the fidelity rule exists
/// to catch, arriving by the back door. A frame a bounded channel could not take
/// *is* replayed, because the recorder still wrote it down and recovery means
/// picking it back up. Our entry is one of the latter on purpose: a queue being
/// full is not permission to lose a trade.
fn backpressure(config: &GeneratorConfig) -> FixtureCase {
    let mint = "mint-backpressure";
    let mut composer = Composer::new(config, "backpressure");

    composer.lifecycle(RecordKind::Connected);
    composer.lifecycle(RecordKind::Ack);
    composer.launch(mint, "creator-backpressure", 0);
    composer.ramp_to(mint, 6 * LAMPORTS_PER_SOL, "warmup");

    // A burst none of the three queues had room for.
    for (index, queue) in [Queue::FastPath, Queue::Standard, Queue::Wal]
        .into_iter()
        .enumerate()
    {
        let wallet = format!("wallet-burst-{index:02}");
        let gross = 400_000_000 + composer.draw(mint, "burst-size", index as u64, 600_000_000);
        composer.flow_buy(
            mint,
            &wallet,
            None,
            gross,
            RecordOutcome::Backpressure(queue),
        );
    }

    // And a burst the filters threw away. These carry buys large enough to move
    // the curve visibly, so a replay that wrongly applied them would show up as
    // a different curve rather than as a different counter.
    for (index, reason) in [
        DropClass::TooSmall,
        DropClass::PoolTooThin,
        DropClass::StaleSlot,
    ]
    .into_iter()
    .enumerate()
    {
        let wallet = format!("wallet-filtered-{index:02}");
        composer.flow_buy(
            mint,
            &wallet,
            None,
            2 * LAMPORTS_PER_SOL,
            RecordOutcome::Dropped(reason),
        );
    }

    // Ours, through a full standard queue.
    let size = entry_size(&composer.curve(mint));
    composer.entry(
        mint,
        size,
        "through-a-full-queue",
        RecordOutcome::Backpressure(Queue::Standard),
    );

    composer.reconnect();
    composer.ramp_to(mint, 12 * LAMPORTS_PER_SOL, "recovered");
    composer.exit_all(mint, "backpressure-exit", RecordOutcome::Accepted);
    composer.holders(mint, &[]);
    composer.label(mint, RugClass::Held);
    composer.lifecycle(RecordKind::Closed);

    let launches = vec![ExpectedLaunch {
        mint: mint.to_string(),
        labelled: RugClass::Held,
        classified: RugClass::Held,
        entries_above_threshold: entries_of(&composer, mint),
        bundled_wallets: 0,
        final_real_sol_lamports: final_real_sol(&composer, mint),
    }];
    assemble(
        config,
        Scenario::Backpressure,
        "backpressure",
        "backpressure",
        "three frames a full queue could not take, three the filters rejected, our entry \
         arriving through a full queue, and a reconnect in the middle.",
        composer,
        launches,
    )
}

/// The clean recording every corruption case is an edit of.
///
/// Short on purpose. A tampering case is about one line, and a thousand-line
/// stream around it only makes the line harder to find.
fn corruption_base(config: &GeneratorConfig, mint: &str) -> Composer {
    let mut composer = Composer::new(config, "chain-corruption");
    composer.lifecycle(RecordKind::Connected);
    composer.lifecycle(RecordKind::Ack);
    composer.launch(mint, "creator-corruption", 0);
    for index in 0..4u64 {
        let wallet = format!("wallet-flow-{index:02}");
        let gross = 300_000_000 + composer.draw(mint, "flow-size", index, 500_000_000);
        composer.flow_buy(mint, &wallet, None, gross, RecordOutcome::Accepted);
    }
    let size = entry_size(&composer.curve(mint));
    composer.entry(mint, size, "corruption-entry", RecordOutcome::Accepted);
    for index in 4..6u64 {
        let wallet = format!("wallet-flow-{index:02}");
        let gross = 300_000_000 + composer.draw(mint, "flow-size", index, 500_000_000);
        composer.flow_buy(mint, &wallet, None, gross, RecordOutcome::Accepted);
    }
    composer.exit_all(mint, "corruption-exit", RecordOutcome::Accepted);
    composer.holders(mint, &[]);
    composer.label(mint, RugClass::Held);
    composer.lifecycle(RecordKind::Closed);
    composer
}

/// Relinks and reseals every record from `index` onwards.
///
/// This is what an adversary with the writer's code would do: edit one record,
/// then make the chain agree with the edit. It is also the only edit that
/// produces exactly one broken line — the audit resynchronises from what it
/// read, so a splice that did *not* reseal forward would report a second break
/// on the next line and bury the first.
fn reseal_from(records: &mut [ReplayRecord], index: usize) {
    for position in index..records.len() {
        if position > index {
            records[position].prev_hash = records[position - 1].integrity_hash;
        }
        let prev = records[position].prev_hash;
        records[position].integrity_hash = records[position].compute_integrity(&prev);
    }
}

/// One way of breaking a stream, before it becomes a case.
struct Corruption {
    name: &'static str,
    note: &'static str,
    records: Vec<ReplayRecord>,
    /// Cuts the last line in half after the file is written. The one kind of
    /// damage that is not expressible as a record, because the result is not a
    /// record.
    truncate_last_line: bool,
    break_index: Option<usize>,
    break_status: Option<LineStatus>,
    complete: bool,
    refusals: Vec<&'static str>,
}

/// Six clean chains, each edited one way after it was sealed.
///
/// The manifest in every one of these describes the **pristine** recording: the
/// record count, the chain head and the segment digests the recorder wrote
/// before anybody touched the file. That is the point. A manifest regenerated
/// from tampered bytes would agree with the tampering, and a fixture that
/// certifies its own forgery tests nothing at all.
fn chain_corruption(config: &GeneratorConfig) -> Vec<FixtureCase> {
    let stream_id = "chain-corruption";
    let mint = "mint-corruption";
    let composer = corruption_base(config, mint);
    let (pristine, head) = seal(config, stream_id, &composer.frames);
    let pristine_files = split(&pristine, config.segments);

    let total = pristine.len();
    // Mid-stream, and never the first record: the out-of-order case needs a
    // record in front of it to fail to follow.
    let index = (total / 2).max(1);

    let mut corruptions: Vec<Corruption> = Vec::new();

    // A field edited in place. The record's own hash no longer matches its
    // contents, which is the only thing that catches this — the links on both
    // sides of it are still correct, and the manifest still agrees.
    let mut records = pristine.clone();
    records[index].observed_at_ms += 1;
    corruptions.push(Corruption {
        name: "corruption-self-inconsistent",
        note: "one record's observed_at_ms was edited after sealing. The chain links on both \
               sides still hold and the manifest still agrees; the record's own integrity hash \
               is what catches it.",
        records,
        truncate_last_line: false,
        break_index: Some(index),
        break_status: Some(LineStatus::SelfInconsistent),
        complete: true,
        refusals: vec!["one line's contents are not the contents that were sealed"],
    });

    // A splice, resealed forward. Every record is internally consistent and the
    // sequence is dense; the only thing wrong is that one link points somewhere
    // the previous record did not seal to, and the manifest's chain head no
    // longer matches what the file ends at.
    let mut records = pristine.clone();
    records[index].prev_hash[0] ^= 0xff;
    reseal_from(&mut records, index);
    corruptions.push(Corruption {
        name: "corruption-chain-broken",
        note: "one record's prev_hash was changed and the chain resealed from there on, so \
               every record still verifies against itself. The broken link and the manifest's \
               chain head are what is left to catch it.",
        records,
        truncate_last_line: false,
        break_index: Some(index),
        break_status: Some(LineStatus::ChainBroken),
        complete: true,
        refusals: vec![
            "one link does not point at the record before it",
            "the file does not end at the chain head the manifest declares",
        ],
    });

    // A record removed. The hole is in the sequence numbers, which is checked
    // before the links are, so this reads as a gap rather than as a break.
    let mut records = pristine.clone();
    records.remove(index);
    corruptions.push(Corruption {
        name: "corruption-seq-gap",
        note: "one record was deleted. Sequence density is checked before the links are, so \
               the line that used to follow it reports the hole rather than the broken link.",
        records,
        truncate_last_line: false,
        break_index: Some(index),
        break_status: Some(LineStatus::SeqGap),
        complete: true,
        refusals: vec![
            "a sequence number is missing",
            "the file holds fewer records than the manifest declares",
        ],
    });

    // A reorder, resealed. The chain verifies end to end and the §6 total order
    // does not, which is the shape a plausible forgery has: whoever did it knew
    // how to rebuild the hashes and did not know the order was checked too.
    let mut records = pristine.clone();
    records[index - 1].slot = records[index].slot + 1;
    reseal_from(&mut records, index - 1);
    corruptions.push(Corruption {
        name: "corruption-out-of-order",
        note: "two records were put in the wrong order and the chain resealed over it. Every \
               hash verifies; the slot ordering is what fails.",
        records,
        truncate_last_line: false,
        break_index: Some(index),
        break_status: Some(LineStatus::OutOfOrder),
        complete: true,
        refusals: vec![
            "one record does not follow the one before it in slot order",
            "the file does not end at the chain head the manifest declares",
        ],
    });

    // A half-written line, at the end, which is where a killed process leaves
    // one. Deliberately last: an unparseable line is skipped without the reader
    // learning anything from it, so the sequence it was holding a place for goes
    // missing too, and a line after it would report that hole as a second break.
    corruptions.push(Corruption {
        name: "corruption-unparseable",
        note: "the last line was cut in half, the way a killed process leaves one. It is the \
               last line on purpose: an unparseable record teaches the reader nothing, so \
               anything after it would report the resulting sequence hole as a second break.",
        records: pristine.clone(),
        truncate_last_line: true,
        break_index: Some(total - 1),
        break_status: Some(LineStatus::Unparseable),
        complete: true,
        refusals: vec![
            "the last line is not a record",
            "the file holds fewer records than the manifest declares",
        ],
    });

    // Nothing is wrong with the bytes at all. The recording was stopped before
    // it finished, the manifest says so, and §3.2's rule is that such a fixture
    // may be replayed for debugging and may never back a gate dossier. This is
    // the case that fails for a reason no amount of hashing would find.
    let mut records = pristine.clone();
    records.truncate(total.saturating_sub(3));
    corruptions.push(Corruption {
        name: "corruption-truncated-recording",
        note: "every line verifies. The recording was cut short, the manifest declares more \
               records than the file holds and marks itself incomplete, and that alone is \
               enough to refuse the corpus.",
        records,
        truncate_last_line: false,
        break_index: None,
        break_status: None,
        complete: false,
        refusals: vec![
            "the recording is marked incomplete",
            "the file holds fewer records than the manifest declares",
        ],
    });

    corruptions
        .into_iter()
        .map(|corruption| {
            let mut files = split(&corruption.records, config.segments);
            if corruption.truncate_last_line {
                if let Some(file) = files.last_mut() {
                    let mut lines: Vec<&str> = file.text.lines().collect();
                    if let Some(last) = lines.pop() {
                        let cut = &last[..last.len() / 2];
                        let mut text: String =
                            lines.iter().map(|line| format!("{line}\n")).collect();
                        text.push_str(cut);
                        text.push('\n');
                        file.text = text;
                    }
                }
            }
            let (break_file, break_line) = match corruption.break_index {
                Some(at) => {
                    let (file, line) = locate(at, corruption.records.len(), config.segments);
                    (Some(file), Some(line))
                }
                None => (None, None),
            };
            FixtureCase {
                name: corruption.name.to_string(),
                scenario: Scenario::ChainCorruption,
                stream_id: stream_id.to_string(),
                expected: Expected {
                    schema: EXPECTED_SCHEMA.to_string(),
                    scenario: Scenario::ChainCorruption,
                    case: corruption.name.to_string(),
                    note: corruption.note.to_string(),
                    config: config.clone(),
                    gate_ready: false,
                    // The fragment left by the truncation is a line and not a
                    // record, so it is not counted as one.
                    records: (corruption.records.len() - usize::from(corruption.truncate_last_line))
                        as u64,
                    frames_backpressure: 0,
                    frames_dropped_live: 0,
                    break_file,
                    break_line,
                    break_status: corruption.break_status,
                    refusal_reasons: corruption
                        .refusals
                        .iter()
                        .map(|reason| (*reason).to_string())
                        .collect(),
                    // Deliberately empty. These cases are about the chain, and
                    // what the economics of a stream with a hole in it come to
                    // is not a thing to assert — it is a thing to refuse.
                    launches: Vec::new(),
                },
                files,
                manifest: manifest_for(
                    stream_id,
                    &pristine,
                    head,
                    &pristine_files,
                    corruption.complete,
                ),
            }
        })
        .collect()
}

// ===========================================================================
// The public entry points
// ===========================================================================

/// Every case one scenario expands into.
pub fn generate(
    scenario: Scenario,
    config: &GeneratorConfig,
) -> Result<Vec<FixtureCase>, FixtureError> {
    config.check()?;
    Ok(match scenario {
        Scenario::Graduation => vec![graduation(config)],
        Scenario::SybilRug => vec![sybil_rug(config)],
        Scenario::SandwichBoundary => vec![sandwich_boundary(config)],
        Scenario::Backpressure => vec![backpressure(config)],
        Scenario::ChainCorruption => chain_corruption(config),
    })
}

/// The whole corpus, in `Scenario::ALL` order.
pub fn generate_all(config: &GeneratorConfig) -> Result<Vec<FixtureCase>, FixtureError> {
    config.check()?;
    let mut cases = Vec::new();
    for scenario in Scenario::ALL {
        cases.extend(generate(scenario, config)?);
    }
    Ok(cases)
}

fn pretty<T: Serialize>(value: &T) -> String {
    let mut text = serde_json::to_string_pretty(value)
        .unwrap_or_else(|err| format!("{{\"error\":\"{err}\"}}"));
    text.push('\n');
    text
}

fn io_error(path: &Path, err: std::io::Error) -> FixtureError {
    FixtureError::Io {
        path: path.display().to_string(),
        detail: err.to_string(),
    }
}

/// Writes one case into `root/<case name>/`.
///
/// Each case gets its own directory because that is the unit `evaluate_directory`
/// reads: a directory with a manifest is *one* stream rotated into segments, so
/// two independent streams sharing a directory would be read as segments of
/// whichever stream the manifest happened to name.
///
/// Without `force` an existing file is a refusal rather than an overwrite — a
/// fixture directory is somebody's evidence until they say otherwise. With it,
/// the directory's existing `.jsonl` files are removed before the new ones are
/// written: regenerating with fewer segments would otherwise leave the old
/// tail behind, and `evaluate_directory` would read those leftovers as part of
/// the stream.
pub fn write_case(root: &Path, case: &FixtureCase, force: bool) -> Result<PathBuf, FixtureError> {
    let dir = root.join(&case.name);
    std::fs::create_dir_all(&dir).map_err(|err| io_error(&dir, err))?;

    let mut targets: Vec<(PathBuf, String)> = case
        .files
        .iter()
        .map(|file| (dir.join(&file.name), file.text.clone()))
        .collect();
    targets.push((dir.join("manifest.json"), pretty(&case.manifest)));
    targets.push((dir.join("expected.json"), pretty(&case.expected)));

    if force {
        let entries = std::fs::read_dir(&dir).map_err(|err| io_error(&dir, err))?;
        for entry in entries {
            let entry = entry.map_err(|err| io_error(&dir, err))?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                std::fs::remove_file(&path).map_err(|err| io_error(&path, err))?;
            }
        }
    } else {
        for (path, _) in &targets {
            if path.exists() {
                return Err(FixtureError::Exists {
                    path: path.display().to_string(),
                });
            }
        }
    }

    for (path, text) in targets {
        std::fs::write(&path, text).map_err(|err| io_error(&path, err))?;
    }
    Ok(dir)
}

/// Writes a whole corpus, and hands back the directories it wrote, in order.
pub fn write_corpus(
    root: &Path,
    cases: &[FixtureCase],
    force: bool,
) -> Result<Vec<PathBuf>, FixtureError> {
    std::fs::create_dir_all(root).map_err(|err| io_error(root, err))?;
    let mut written = Vec::with_capacity(cases.len());
    for case in cases {
        written.push(write_case(root, case, force)?);
    }
    Ok(written)
}

// ===========================================================================
// tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::{
        evaluate_streams_with, BacktestConfig, FixtureSource, ForensicReport, StreamReport, MICROS,
    };
    use std::fs;

    /// The evaluation policy the corpus is meant to be read at.
    ///
    /// The fee has to be the one the generator sized against — a boundary case
    /// evaluated at a different fee is a boundary nobody built — so it is taken
    /// off the case rather than defaulted beside it.
    fn policy(case: &FixtureCase) -> BacktestConfig {
        BacktestConfig {
            fee_bps: case.expected.config.fee_bps,
            cents_per_sol: 15_000,
            ..BacktestConfig::default()
        }
    }

    fn sources(case: &FixtureCase) -> Vec<FixtureSource> {
        case.files
            .iter()
            .map(|file| FixtureSource {
                stream_id: case.stream_id.clone(),
                file: file.name.clone(),
                text: file.text.clone(),
            })
            .collect()
    }

    /// Reads a case exactly the way `sts backtest run` would read the directory
    /// it was written to: every segment under the manifest's stream id, with
    /// the manifest attached.
    fn evaluate(case: &FixtureCase) -> ForensicReport {
        evaluate_streams_with(
            &sources(case),
            policy(case),
            &case.name,
            Some(case.manifest.clone()),
        )
    }

    fn case_named(cases: &[FixtureCase], name: &str) -> FixtureCase {
        cases
            .iter()
            .find(|case| case.name == name)
            .unwrap_or_else(|| panic!("the corpus has no case called {name}"))
            .clone()
    }

    fn corpus() -> Vec<FixtureCase> {
        generate_all(&GeneratorConfig::default()).expect("the default knobs describe a corpus")
    }

    fn broken_streams(report: &ForensicReport) -> Vec<&StreamReport> {
        report
            .streams
            .iter()
            .filter(|stream| stream.first_break.is_some())
            .collect()
    }

    /// A scratch directory, cleared going in as well as coming out, so a test
    /// that panicked last run does not poison the next one.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("sts-fixtures-tests/{name}"));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("the scratch directory could not be created");
            Scratch { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    // -----------------------------------------------------------------------
    // determinism
    // -----------------------------------------------------------------------

    #[test]
    fn generation_is_a_function_of_its_seed() {
        let config = GeneratorConfig::default();
        let first = generate_all(&config).expect("a corpus");
        let second = generate_all(&config).expect("a corpus");

        assert_eq!(first.len(), second.len());
        for (left, right) in first.iter().zip(second.iter()) {
            assert_eq!(left.name, right.name);
            // The files, byte for byte. This is the whole property: a case can
            // be cited by name and seed rather than shipped as a blob.
            assert_eq!(
                left.files, right.files,
                "{} differs between runs",
                left.name
            );
            assert_eq!(
                left.manifest, right.manifest,
                "{} manifest differs",
                left.name
            );
            assert_eq!(
                left.expected, right.expected,
                "{} expectations differ",
                left.name
            );
        }
    }

    #[test]
    fn a_different_seed_is_a_different_corpus() {
        let other = GeneratorConfig {
            seed: "0x200x".to_string(),
            ..GeneratorConfig::default()
        };

        let default = corpus();
        let shifted = generate_all(&other).expect("a corpus");

        // Not every case has to move — the corruption cases are edits of a
        // fixed script — but the ones whose sizes are drawn must.
        let left = case_named(&default, "sybil-rug");
        let right = case_named(&shifted, "sybil-rug");
        assert_ne!(
            left.text(),
            right.text(),
            "the seed did not reach the bundle"
        );
    }

    #[test]
    fn this_module_holds_no_floating_point() {
        let source = include_str!("fixtures.rs");
        // Spelled rather than written, so this test does not trip over its own
        // text. Every quantity in here is an integer in a named unit, because
        // the evaluator that has to agree with these fixtures is built the same
        // way and a generator that rounded differently would produce cases
        // whose expected answers are wrong in a way that looks like an engine
        // bug.
        for needle in [format!("{}{}", 'f', 64), format!("{}{}", 'f', 32)] {
            assert!(
                !source.contains(&needle),
                "fixtures.rs mentions {needle}; the curve model here is integer only"
            );
        }
    }

    // -----------------------------------------------------------------------
    // every case, against the file that says what it is
    // -----------------------------------------------------------------------

    #[test]
    fn every_case_reads_the_way_it_says_it_will() {
        for case in corpus() {
            let report = evaluate(&case);
            let expected = &case.expected;

            assert_eq!(
                report.gate_ready, expected.gate_ready,
                "{}: gate readiness disagrees with expected.json; refusals were {:?}",
                case.name, report.refusals
            );
            assert_eq!(
                report.integrity.records as u64, expected.records,
                "{}: record count disagrees with expected.json",
                case.name
            );
            assert_eq!(
                report.refusals.is_empty(),
                expected.refusal_reasons.is_empty(),
                "{}: refusals were {:?}, the case expected {:?}",
                case.name,
                report.refusals,
                expected.refusal_reasons
            );

            let broken = broken_streams(&report);
            match (&expected.break_file, expected.break_line) {
                (Some(file), Some(line)) => {
                    assert_eq!(
                        broken.len(),
                        1,
                        "{}: expected exactly one broken file",
                        case.name
                    );
                    assert_eq!(
                        &broken[0].file, file,
                        "{}: the break is in another file",
                        case.name
                    );
                    assert_eq!(
                        broken[0].first_break,
                        Some(line),
                        "{}: the break is on another line",
                        case.name
                    );
                    assert_eq!(
                        broken[0].verdicts.first().map(|verdict| verdict.status),
                        expected.break_status,
                        "{}: the break is of another kind",
                        case.name
                    );
                }
                _ => assert!(
                    broken.is_empty(),
                    "{}: expected no break, found {:?}",
                    case.name,
                    broken
                        .iter()
                        .map(|s| (&s.file, s.first_break))
                        .collect::<Vec<_>>()
                ),
            }

            for expectation in &expected.launches {
                let launch = report
                    .launches
                    .iter()
                    .find(|launch| launch.mint == expectation.mint)
                    .unwrap_or_else(|| panic!("{}: {} is missing", case.name, expectation.mint));
                assert_eq!(
                    launch.labelled,
                    Some(expectation.labelled),
                    "{}: {} carries another label",
                    case.name,
                    expectation.mint
                );
                assert_eq!(
                    launch.classified, expectation.classified,
                    "{}: {} was classified {} and the case expected {}",
                    case.name, expectation.mint, launch.classified, expectation.classified
                );
                let priced: Vec<bool> = launch
                    .adverse_selection
                    .iter()
                    .map(|verdict| verdict.above_threshold)
                    .collect();
                assert_eq!(
                    priced, expectation.entries_above_threshold,
                    "{}: {} priced its entries against another threshold",
                    case.name, expectation.mint
                );
                // The generator's mirror and the evaluator have to end in the
                // same place. They walk the same events through the same
                // integer curve, so any gap here is a gap in what the generator
                // believes the evaluator does — and every size in the case was
                // computed on that belief.
                assert_eq!(
                    launch.final_real_sol_lamports, expectation.final_real_sol_lamports,
                    "{}: {} ended on another curve than the generator's mirror",
                    case.name, expectation.mint
                );
            }
        }
    }

    #[test]
    fn a_corruption_case_breaks_one_line_and_leaves_the_rest_readable() {
        for case in generate(Scenario::ChainCorruption, &GeneratorConfig::default())
            .expect("the corruption cases")
        {
            let report = evaluate(&case);
            assert!(
                !report.gate_ready,
                "{}: a tampered corpus must not be gate ready",
                case.name
            );
            // One line goes wrong, never two. The audit resynchronises from
            // what it read, so a cascade here would mean the case tampered with
            // more than it claims to and the finding it demonstrates is buried.
            let rejected: usize = report.streams.iter().map(|stream| stream.rejected).sum();
            let expected_rejected = usize::from(case.expected.break_status.is_some());
            assert_eq!(
                rejected, expected_rejected,
                "{}: {rejected} line(s) were rejected",
                case.name
            );
        }
    }

    // -----------------------------------------------------------------------
    // what each scenario is for
    // -----------------------------------------------------------------------

    #[test]
    fn the_bundle_is_found_as_one_hand() {
        let case = case_named(&corpus(), "sybil-rug");
        let report = evaluate(&case);
        let launch = &report.launches[0];
        let sybil = &launch.sybil;

        assert_eq!(sybil.clusters.len(), 1, "the bundle should be one cluster");
        let cluster = &sybil.clusters[0];
        assert_eq!(cluster.wallet_count, case.expected.config.sybil_wallets);
        // One slot, one instant: every gap in the kernel is zero and the
        // exponential is exactly one.
        assert_eq!(cluster.sync_micros, MICROS);
        assert_eq!(cluster.first_buy_span_ms, 0);
        // The bundle is most of the flow, and the recording knows who paid for
        // every wallet in it.
        assert!(
            cluster.flow_share_bps > 4_000,
            "the bundle held {} bps of the flow",
            cluster.flow_share_bps
        );

        // Concentrated at the top, which is the wallet-level reading. The
        // cluster above is the one that says those wallets are one hand.
        assert!(
            sybil.holder_top1_bps > 3_000,
            "top holder had {} bps",
            sybil.holder_top1_bps
        );
        assert!(
            sybil
                .holder_hhi_bps
                .expect("a snapshot with balances in it")
                > 1_500,
            "the snapshot was not concentrated"
        );

        assert_eq!(launch.classified, RugClass::Rug);
        assert_eq!(launch.pulls, 1);
        let stranded = launch
            .stranded
            .as_ref()
            .expect("the position should strand");
        assert!(
            stranded.no_executable_exit,
            "the pull should leave no exit; the mark said {}",
            stranded.reason
        );
        assert!(launch.trades.is_empty(), "nothing should have closed");
    }

    #[test]
    fn the_ladder_straddles_the_extraction_threshold() {
        let case = case_named(&corpus(), "sandwich-boundary");
        let report = evaluate(&case);
        assert_eq!(report.launches.len(), 3, "one launch per curve position");

        for launch in &report.launches {
            let priced: Vec<bool> = launch
                .adverse_selection
                .iter()
                .map(|verdict| verdict.above_threshold)
                .collect();
            assert_eq!(priced.len(), 4, "{}: four rungs", launch.mint);

            // One lamport under the breakeven size cannot clear fees at any
            // attacker size, and one lamport over it can. The middle rung sits
            // exactly on the threshold, where §15.2 says there is no sign to
            // assert, so it is checked against what the generator recorded
            // rather than against a claim.
            assert!(!priced[0], "{}: the rung under b* cleared", launch.mint);
            assert!(priced[2], "{}: the rung over b* did not clear", launch.mint);
            assert!(priced[3], "{}: the heavy rung did not clear", launch.mint);

            // The heavy rung has a sandwich around it, so it should price an
            // attacker who actually makes money rather than one who merely
            // clears the algebra.
            let heavy = &launch.adverse_selection[3];
            assert!(
                heavy.attacker_profit_lamports > 0,
                "{}: the sandwiched entry priced no profitable attacker",
                launch.mint
            );
            assert!(
                heavy.damage_bps > 0,
                "{}: the sandwiched entry took no damage",
                launch.mint
            );
            // §15.1's closed form and the three-swap simulation are two ways to
            // the same number. A fixture that made them disagree would be
            // pointing at the model rather than at the engine.
            assert_eq!(
                heavy.extraction_lamports as u64, heavy.extraction_closed_lamports,
                "{}: the closed form and the simulation disagree",
                launch.mint
            );
        }
    }

    #[test]
    fn a_full_queue_is_recovered_and_a_filtered_frame_is_not() {
        let case = case_named(&corpus(), "backpressure");
        let report = evaluate(&case);
        let expected = &case.expected;

        let recovered: usize = report
            .streams
            .iter()
            .map(|stream| stream.frames_backpressure_recovered)
            .sum();
        let dropped: usize = report
            .streams
            .iter()
            .map(|stream| stream.frames_dropped_live)
            .sum();
        assert_eq!(recovered as u64, expected.frames_backpressure);
        assert_eq!(dropped as u64, expected.frames_dropped_live);
        assert!(expected.frames_backpressure > 0 && expected.frames_dropped_live > 0);

        // `frames` counts what reached the engine, so the filtered ones are
        // already outside it. Every one of those that did reach it carried an
        // event this build could read, and was applied.
        let frames: usize = report.streams.iter().map(|stream| stream.frames).sum();
        let applied: usize = report
            .streams
            .iter()
            .map(|stream| stream.events_applied)
            .sum();
        assert_eq!(
            frames, applied,
            "a frame reached the engine and was not applied"
        );

        // That the filtered frames did not move the curve is checked in
        // `every_case_reads_the_way_it_says_it_will`, against the generator's
        // own mirror: the mirror skips them, so a curve that ended anywhere
        // else would mean they had been replayed after all.

        // And our entry, which arrived through a full standard queue, still
        // traded. A queue being full is not permission to lose a trade.
        let launch = &report.launches[0];
        assert_eq!(launch.entries, 1);
        assert_eq!(launch.trades.len(), 1);
        assert!(launch.stranded.is_none());
        assert!(launch.quote_failures.is_empty());
    }

    #[test]
    fn the_control_graduates_and_closes_its_trade() {
        let case = case_named(&corpus(), "graduation");
        let report = evaluate(&case);
        assert!(report.gate_ready, "refusals: {:?}", report.refusals);

        let launch = &report.launches[0];
        assert!(launch.graduated);
        assert_eq!(launch.classified, RugClass::Graduated);
        assert_eq!(launch.pulls, 0);
        assert!(launch.peak_real_sol_lamports >= PUMP_GRADUATION_LAMPORTS);
        assert_eq!(launch.trades.len(), 1, "one round trip, closed");
        assert!(launch.stranded.is_none());
        assert!(
            launch.quote_failures.is_empty(),
            "the control should not refuse a quote: {:?}",
            launch.quote_failures
        );
        // The negative case the confusion matrix needs: a launch nothing is
        // wrong with, so a false positive has somewhere to show up.
        assert_eq!(report.rug.true_negatives, 1);
        assert_eq!(report.rug.false_positives, 0);
    }

    #[test]
    fn rotation_changes_the_files_and_nothing_else() {
        let rotated = GeneratorConfig {
            segments: 4,
            ..GeneratorConfig::default()
        };

        let whole = case_named(&corpus(), "sybil-rug");
        let split_up = case_named(
            &generate_all(&rotated).expect("a rotated corpus"),
            "sybil-rug",
        );

        assert_eq!(whole.files.len(), 1);
        assert_eq!(split_up.files.len(), 4);
        // §3.3: segmentation is a storage detail. The concatenation is the same
        // stream, and everything the evaluator concludes from it is the same.
        assert_eq!(whole.text(), split_up.text());

        let left = evaluate(&whole);
        let right = evaluate(&split_up);
        assert_eq!(left.launches, right.launches);
        assert_eq!(left.performance, right.performance);
        assert_eq!(left.sybil, right.sybil);
        assert_eq!(left.gate_ready, right.gate_ready);
    }

    // -----------------------------------------------------------------------
    // the knobs and the disk
    // -----------------------------------------------------------------------

    #[test]
    fn a_bundle_below_the_floor_is_refused_rather_than_quietly_raised() {
        let config = GeneratorConfig {
            sybil_wallets: MIN_SYBIL_WALLETS - 1,
            ..GeneratorConfig::default()
        };
        assert_eq!(
            generate_all(&config),
            Err(FixtureError::TooFewWallets {
                asked: MIN_SYBIL_WALLETS - 1,
                needed: MIN_SYBIL_WALLETS,
            })
        );

        let config = GeneratorConfig {
            segments: 0,
            ..GeneratorConfig::default()
        };
        assert_eq!(generate_all(&config), Err(FixtureError::NoSegments));
    }

    #[test]
    fn a_case_directory_is_not_overwritten_without_force() {
        let scratch = Scratch::new("overwrite");
        let cases = generate(Scenario::Graduation, &GeneratorConfig::default()).expect("a case");

        write_corpus(scratch.path(), &cases, false).expect("the first write");
        let again = write_corpus(scratch.path(), &cases, false);
        assert!(
            matches!(again, Err(FixtureError::Exists { .. })),
            "a second write should refuse: {again:?}"
        );

        // With force, and with fewer segments than last time, the stale tail
        // has to go: `evaluate_directory` reads every `.jsonl` in the
        // directory, so a leftover segment would be read as part of the stream.
        let rotated = GeneratorConfig {
            segments: 3,
            ..GeneratorConfig::default()
        };
        let split_up = generate(Scenario::Graduation, &rotated).expect("a rotated case");
        write_corpus(scratch.path(), &split_up, true).expect("the rotated write");
        write_corpus(scratch.path(), &cases, true).expect("the forced write");

        let written: Vec<String> = fs::read_dir(scratch.path().join("graduation"))
            .expect("the case directory")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .to_string()
            })
            .filter(|name| name.ends_with(".jsonl"))
            .collect();
        assert_eq!(
            written,
            vec!["000.jsonl".to_string()],
            "a stale segment was left behind"
        );
    }

    #[test]
    fn the_command_line_writes_a_corpus_the_harness_can_read() {
        let scratch = Scratch::new("cli");
        let root = scratch.path().display().to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = crate::backtest::cli::run(
            &[
                "generate".to_string(),
                "--out".to_string(),
                root.clone(),
                "--scenario".to_string(),
                "sybil-rug".to_string(),
            ],
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0, "generate said: {}", String::from_utf8_lossy(&err));
        assert!(String::from_utf8_lossy(&out).contains("sybil-rug"));

        // And the other end of the same pipe reads it.
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = crate::backtest::cli::run(
            &[
                "verify".to_string(),
                "--fixtures".to_string(),
                scratch.path().join("sybil-rug").display().to_string(),
            ],
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0, "verify said: {}", String::from_utf8_lossy(&err));

        // A corrupted case, through the same path, is refused rather than read.
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = crate::backtest::cli::run(
            &[
                "generate".to_string(),
                "--out".to_string(),
                root.clone(),
                "--scenario".to_string(),
                "chain-corruption".to_string(),
            ],
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0, "generate said: {}", String::from_utf8_lossy(&err));

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = crate::backtest::cli::run(
            &[
                "run".to_string(),
                "--fixtures".to_string(),
                scratch
                    .path()
                    .join("corruption-chain-broken")
                    .display()
                    .to_string(),
                "--gate".to_string(),
            ],
            &mut out,
            &mut err,
        );
        assert_eq!(code, 2, "a gate run over a spliced chain has to be refused");
    }

    #[test]
    fn an_unknown_scenario_is_a_command_line_error() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = crate::backtest::cli::run(
            &[
                "generate".to_string(),
                "--out".to_string(),
                "/nonexistent-and-never-written".to_string(),
                "--scenario".to_string(),
                "nonsense".to_string(),
            ],
            &mut out,
            &mut err,
        );
        assert_eq!(code, 1);
        assert!(String::from_utf8_lossy(&err).contains("unknown scenario"));
        assert!(
            !Path::new("/nonexistent-and-never-written").exists(),
            "nothing should have been written"
        );
    }
}
