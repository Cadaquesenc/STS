//! Entry rules, and the forensics an entry rule is allowed to read.
//!
//! This is the Rust home of the syndicate sniper: the rule that says a launch
//! is worth following when the wallets that opened it are one operator rather
//! than a crowd. It arrives here from the Node prototype — `cluster.js`'s
//! launch analyser and `backtest.js`'s entry gate, with the refusal thresholds
//! `score.js` grew around them — and three things changed on the way across.
//!
//! **There is no floating point in it.** The prototype scored in `f64` and
//! compared against `0.6`. Every number here is an integer in a named unit:
//! times in milliseconds, money in lamports, scores in millionths, tolerances
//! and shares in basis points. The one transcendental the entropy terms need
//! carries its own fixed-point implementation in [`fixed`], for the reason
//! `backtest.rs` gives about its own: a score that is stored, compared and
//! replayed must not depend on whose libm the build linked against.
//!
//! **Every order is total.** The prototype leaned on JavaScript's insertion
//! ordering for maps and on a sort key that could tie. A tie decided by
//! iteration order is a metric that changes when the input is reshuffled, and
//! the fan-out caps mean the *order* decides which evidence survives. So every
//! comparison here falls through to the wallet address, every map that is
//! iterated is a `BTreeMap`, and two runs over the same record produce the same
//! bytes.
//!
//! **What is not measured says so.** The concentration index is
//! `RISK_AND_SYBIL_SPEC.md` §2.2 exactly, reusing `backtest::hhi_bps`. Buy
//! synchrony is §3.5's kernel, reusing `backtest::sync_micros`. Interaction
//! entropy is §5.1. Spectral separation is §4, it needs the Lanczos solver that
//! section specifies, and this module does not have one — so
//! [`syndicate::Cluster`] does not carry a `spectral_separation` field and does
//! not build a [`crate::types::SybilClusterMetrics`] on its own. A zero in that
//! column would read as "this cluster does not separate from the market", which
//! is the opposite of "nobody looked". [`syndicate::Cluster::metrics_with`] is
//! how a caller that *has* run the solver assembles the full row.
//!
//! The funding half of §3.5's temporal influence is likewise an approximation
//! and is named for what it is: [`syndicate::Cluster::funder_share_bps`] is
//! reachability within `funding_depth` hops, not §3.3's path posterior, which
//! needs a traversal with decay, bottleneck flow and edge-disjoint corroboration
//! that belongs on the async worker.
//!
//! # Two refusals that are not about coordination
//!
//! The rule above finds a group and follows it. Two checks sit after it and can
//! throw out a launch it liked, and both are here rather than in the analyser
//! because both are thresholds.
//!
//! **The ring check** asks whether the group is a group. §14 publishes one
//! population, `[0.9, 0.1]`, and two numbers for it — an index of 8 200 and an
//! entropy of 0.4690 — and [`syndicate::Cluster::ring_finding`] refuses a
//! cluster that reaches both. Both, because the index is a sum of squares and
//! feels almost only the largest holder, while the entropy is a sum of logs and
//! counts the tail: splitting the small end into more addresses moves one and
//! not the other, and that split is exactly the edit an operator makes. A
//! cluster at that shape is one wallet and some costumes, and the exit this rule
//! was going to be early to does not exist.
//!
//! **The sandwich guard** asks whether we can get on at the size we want.
//! `REPLAY_AND_SIMULATION_SPEC.md` §15.2 gives the condition under which any
//! front-run of a buy clears its own fees, `β > φ / (1 - φ)`, and
//! [`syndicate::SandwichCheck`] is that comparison done without dividing. It
//! refuses only a public send, because a send nobody can read first is outside
//! what §15.1 models — but it is computed and reported either way, since §15.4's
//! use for the number is justifying a bundle tip against the adverse selection
//! it buys out of, and a tip larger than the exposure is buying nothing.
//!
//! Neither is a measurement of anything that happened. The first is arithmetic
//! on the opening positions and the second is arithmetic on the curve, and STS
//! does not sandwich anyone.
//!
//! # The two halves of a decision
//!
//! [`syndicate`] answers "is this launch one operator". [`entry`] answers "and
//! what is the position", which is a different question with different inputs —
//! the account, the pool, the mode the engine is in, and what a bad day costs.
//! Keeping them apart is what lets the analyser be re-run over a recorded corpus
//! without the current sizing policy leaking into what it reports, and it is why
//! [`entry::decide`] is the only function here that needs all of them at once.
//!
//! [`social`] is the third input and the smallest. It reads what
//! `docs/archive/legacy-node/src/social.js` scanned — the story a launch links
//! to — and turns it into a multiplier that is **never above one**. Doctrine
//! puts it there (`STS_CORE_IDEOLOGY.md` §1: social hype cannot override
//! on-chain forensic risk) and so does the measurement: the archived grading in
//! `docs/archive/Log.md` found that every apparent edge in a launch's story
//! disappeared once launches were compared at the same crowd size, and that the
//! only two things which survived that check — a dead attention curve and a
//! reused story — both point downwards. So a story can take size off a position
//! and can never put any on, and [`social::weigh`] carries the reasons for
//! showing a person.
//!
//! Nothing in this module reads a clock, a socket or a disk. [`entry::decide`]
//! takes the instant it is deciding at as an argument for that reason: a replay
//! has to produce the decision the live run produced, and a function that reads
//! `now` cannot.

pub mod entry;
pub mod social;
pub mod syndicate;

/// The fixed-point kernel, which used to live here.
///
/// It moved to [`crate::fixed`] when the execution side started pricing Jito
/// tips with it: a numeric primitive that two subsystems depend on does not
/// belong inside either of them. Re-exported under the old path because
/// `strategy` is still its oldest caller and the module doc above names it.
pub use crate::fixed;

pub use entry::{
    decide, plan_entry, stress, Account, EntryDecision, EntryParams, EntryReason, EvReport,
    ExitReadiness, Policy, SizeCap, SizeCaps, StressReport, Tier,
};
pub use social::{
    weigh, SocialCaution, SocialParams, SocialScan, SocialWeight, StoryKind, ViewSample,
};
pub use syndicate::{
    analyse_launch, coordinated_cohort, evaluate, syndicate_gate, Cluster, ClusterParams,
    ClusterReport, Cohort, Concentration, DevSignal, EntryQuote, FundingEdge, FundingSignal,
    GateParams, GateReason, GateVerdict, LaunchRecord, LinkKind, OpeningBuyer, Participant,
    Relation, RingFinding, RiskTag, SandwichCheck, SandwichGuard, SharedFunderRow, SizeGroup,
    SizingSignal, TimingBundle, TimingSignal, WalletFlag, Window, RING_ENTROPY_MICROS,
    RING_HHI_BPS,
};
