//! The story behind a launch, and what it is allowed to do to a position.
//!
//! Every pump.fun launch carries a link to its own metadata file, and three
//! quarters of them point at a social post — nine in ten of those at *one
//! specific post* rather than an account. So a launch is usually not a project
//! with a following; it is a bet on a piece of news with the receipt attached,
//! and the receipt is free to read. `docs/archive/legacy-node/src/social.js` is
//! the scanner that reads it. This module is what the reading is worth.
//!
//! **It is only ever worth a reduction.** [`weigh`] returns a multiplier in
//! basis points that is never above `10_000`, so no story can add a lamport to
//! a position. That is doctrine — `STS_CORE_IDEOLOGY.md` §1, "social hype cannot
//! override on-chain forensic risk", and §5, "social corroboration, never as a
//! safety override" — and it is also what the measurement says. The archived
//! grading in `docs/archive/Log.md` (11 Aug 2026, 2,100 launches) found that
//! sorting by story looked strongly predictive and was a ruler artefact: fresh
//! posts arrive with eleven times more money already in at the three-second
//! mark, and every multiple is counted from that mark. Compared like with like —
//! same crowd size at three seconds — a fresh post was worth 8.2% against a
//! no-link launch's 8.9% in one bucket and 13.2% against 12.7% in the other.
//! The story adds nothing. The prototype's `+6 for a readable post, +4 for a
//! recent one, +4 for a thousand followers` in `dash.js` is not carried across,
//! because a term worth nothing that is added to a score is worth something to
//! the score.
//!
//! **Two things did survive that check, and both point down.** A dead attention
//! curve: split into thirds by whether views were actually growing, the flat
//! third reached 2x 4.3% of the time against the accelerating third's 7.5% —
//! and the accelerating third had *fewer* wallets at three seconds, so for once
//! the result is not the ruler. And a reused story: 18 of 46 linked launches
//! were racing a post somebody had already launched on, which is a farm rather
//! than a following. So a flat curve and a repeated link take size off, and an
//! accelerating curve on an unused story simply takes none.
//!
//! **Not looking and looking-and-failing are different.** No scan at all leaves
//! the multiplier at `10_000`: a launch judged before the scanner ran must come
//! out where it always did, which is the same argument
//! [`crate::strategy::syndicate`] makes for a record with no funding edges. A
//! scan that came back unreadable is UNKNOWN, and §1's "unknown, stale,
//! contradictory or unverifiable data is never silently treated as safe" makes
//! that a haircut. It is never a block — a story cannot stop a trade any more
//! than it can start one, and a system that blocked on an unreadable metadata
//! file would refuse a quarter of the market on the strength of a web request.
//!
//! Everything here is an integer. Growth is multiplicative, so it is measured on
//! a log ruler by [`crate::fixed::growth_score_micros`], where a
//! doubling is the same distance from ten views as from ten thousand.

use serde::{Deserialize, Serialize};

use crate::backtest::{mul_div_floor, MICROS};
use crate::fixed::growth_score_micros;
use crate::replay::BPS_DENOMINATOR;

// ===========================================================================
// Policy constants
// ===========================================================================

/// The rise the attention ruler is measured against.
///
/// The accelerating third of the archived corpus grew its views ×16.02 over the
/// follow window. Sixteen is that number, and it is a ruler rather than a
/// threshold: a launch that beat it scores one and stops there.
pub const ATTENTION_FULL_SCALE: u64 = 16;

/// Below this much growth, the story is dead.
///
/// 17 200 bps is ×1.72, the geometric midpoint of the archived flat third's
/// ×1.15 and middle third's ×2.56. A midpoint rather than either edge because
/// the two thirds are medians of overlapping distributions and there is no
/// measured boundary between them — putting it at one of the medians would
/// claim a precision the grading did not have.
pub const DEAD_BELOW_GROWTH_BPS: u32 = 17_200;

/// What a dead attention curve costs. 25% off.
pub const DEAD_STORY_BPS: u16 = 7_500;

/// The launch number, on one story, at which the link is shared.
///
/// The scanner counts launches per link and this launch is included, so the
/// third launch on a post is `3`. `dash.js` penalised `nth > 2`, which is the
/// same boundary.
pub const SHARED_STORY_NTH: u32 = 3;

/// What a shared story costs. 25% off, matching `dash.js`'s -10 of 100.
pub const SHARED_STORY_BPS: u16 = 7_500;

/// The launch number at which the story is a farm rather than a race.
pub const FARMED_STORY_NTH: u32 = 4;

/// What a farmed story costs. Half, matching `dash.js`'s -15 of 100.
pub const FARMED_STORY_BPS: u16 = 5_000;

/// What a scan that came back unreadable costs. 25% off.
pub const UNREADABLE_BPS: u16 = 7_500;

/// How many view samples the attention curve needs before it is a curve.
pub const MIN_VIEW_SAMPLES: usize = 2;

/// How far apart the first and last samples have to be before the growth
/// between them means anything. A minute, which is the follow window the
/// archived grading measured its curves over.
pub const MIN_VIEW_SPAN_MS: i64 = 60_000;

// ===========================================================================
// Vocabulary
// ===========================================================================

/// What the scanner found where the story should be.
///
/// [`StoryKind::Unreadable`] is the only one of these that is a fact about the
/// scan rather than about the launch, and it is the only one that costs
/// anything: the rest are all "we read it and this is what it said", and what it
/// said does not predict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StoryKind {
    /// The metadata file or the post behind it could not be read. UNKNOWN.
    Unreadable,
    /// The metadata was read and carried no social link at all.
    NoLink,
    /// A link to somewhere that is not the social host the scanner knows.
    OtherLink,
    /// An account rather than a post.
    Profile,
    /// One specific post. The usual case, and the only one with a curve to read.
    Tweet,
}

impl StoryKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            StoryKind::Unreadable => "UNREADABLE",
            StoryKind::NoLink => "NO_LINK",
            StoryKind::OtherLink => "OTHER_LINK",
            StoryKind::Profile => "PROFILE",
            StoryKind::Tweet => "TWEET",
        }
    }

    /// Whether the scan produced facts to read, as opposed to a failure.
    pub const fn is_readable(self) -> bool {
        !matches!(self, StoryKind::Unreadable)
    }
}

/// Every reason a story can take size off a position, worst first.
///
/// Worst first so a caller printing a funnel over a corpus gets the same column
/// order whatever the corpus contained — the same reason
/// [`crate::strategy::syndicate::GateReason`] is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SocialCaution {
    /// Several launches have already run on this story. A farm.
    FarmedStory,
    /// This is not the first launch on this story.
    SharedStory,
    /// Attention was flat over the window. Nobody was actually reading it.
    DeadStory,
    /// The scanner looked and could not read the story. UNKNOWN, not clean.
    Unreadable,
}

impl SocialCaution {
    pub const fn as_str(self) -> &'static str {
        match self {
            SocialCaution::FarmedStory => "farmed-story",
            SocialCaution::SharedStory => "shared-story",
            SocialCaution::DeadStory => "dead-story",
            SocialCaution::Unreadable => "unreadable",
        }
    }

    /// Every caution, worst first.
    pub const ALL: [SocialCaution; 4] = [
        SocialCaution::FarmedStory,
        SocialCaution::SharedStory,
        SocialCaution::DeadStory,
        SocialCaution::Unreadable,
    ];

    /// What this caution multiplies the position by, in basis points.
    pub const fn haircut_bps(self, params: &SocialParams) -> u16 {
        match self {
            SocialCaution::FarmedStory => params.farmed_story_bps,
            SocialCaution::SharedStory => params.shared_story_bps,
            SocialCaution::DeadStory => params.dead_story_bps,
            SocialCaution::Unreadable => params.unreadable_bps,
        }
    }
}

// ===========================================================================
// Inputs
// ===========================================================================

/// One reading of the story's view counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewSample {
    pub at_ms: i64,
    pub views: u64,
}

/// What the scanner found, as it found it.
///
/// `followers`, `account_age_days` and `post_age_ms` are carried and **not
/// weighed**. They are on the record because the audit trail and the operator
/// want them and because a corpus cannot be re-graded on a field nobody stored;
/// they are not in the arithmetic because the archived grading measured them
/// against a matched crowd size and found nothing. If a later holdout says
/// otherwise, the place to change is [`weigh`] and the evidence goes next to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SocialScan {
    pub kind: StoryKind,
    /// The account the story is on, when there is one.
    pub handle: Option<String>,
    pub followers: Option<u64>,
    pub account_age_days: Option<u32>,
    /// How old the post was when the launch appeared, in milliseconds. Negative
    /// means the launch came first, which happens and means the link was chosen
    /// after the fact.
    pub post_age_ms: Option<i64>,
    /// Launches that have pointed at this exact story, this one included. One is
    /// "nobody else has used it".
    pub reuse_nth: u32,
    /// The view counter over time. Order is not assumed.
    pub views: Vec<ViewSample>,
}

impl SocialScan {
    /// A scan that failed. The kind that costs something.
    pub fn unreadable() -> Self {
        SocialScan {
            kind: StoryKind::Unreadable,
            handle: None,
            followers: None,
            account_age_days: None,
            post_age_ms: None,
            reuse_nth: 1,
            views: Vec::new(),
        }
    }

    /// A launch whose metadata was read and carried no story.
    pub fn no_link() -> Self {
        SocialScan {
            kind: StoryKind::NoLink,
            ..SocialScan::unreadable()
        }
    }
}

/// What a story is allowed to cost. Every field is policy and versioned with the
/// rest; [`weigh`] knows none of these numbers by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SocialParams {
    /// Off leaves every multiplier at `10_000`, which is how the rule before
    /// this one stays runnable rather than quoted from an old checkout.
    pub enabled: bool,
    pub unreadable_bps: u16,
    pub shared_story_nth: u32,
    pub shared_story_bps: u16,
    pub farmed_story_nth: u32,
    pub farmed_story_bps: u16,
    pub dead_below_growth_bps: u32,
    pub dead_story_bps: u16,
    pub attention_full_scale: u64,
    pub min_view_samples: usize,
    pub min_view_span_ms: i64,
}

impl Default for SocialParams {
    fn default() -> Self {
        SocialParams {
            enabled: true,
            unreadable_bps: UNREADABLE_BPS,
            shared_story_nth: SHARED_STORY_NTH,
            shared_story_bps: SHARED_STORY_BPS,
            farmed_story_nth: FARMED_STORY_NTH,
            farmed_story_bps: FARMED_STORY_BPS,
            dead_below_growth_bps: DEAD_BELOW_GROWTH_BPS,
            dead_story_bps: DEAD_STORY_BPS,
            attention_full_scale: ATTENTION_FULL_SCALE,
            min_view_samples: MIN_VIEW_SAMPLES,
            min_view_span_ms: MIN_VIEW_SPAN_MS,
        }
    }
}

impl SocialParams {
    /// The scan is recorded and nothing is charged for it. What to run when a
    /// holdout is being collected and the weighting must not be in the loop it
    /// is being graded against.
    pub fn observe_only() -> Self {
        SocialParams {
            enabled: false,
            ..SocialParams::default()
        }
    }

    /// The growth score a story has to clear to be alive, on the same ruler
    /// [`weigh`] measures the story with.
    ///
    /// `None` when the parameters describe a ruler that cannot measure, which
    /// turns the attention test off rather than failing every launch on it.
    pub fn dead_below_micros(&self) -> Option<u64> {
        growth_score_micros(
            u64::from(BPS_DENOMINATOR),
            u64::from(self.dead_below_growth_bps),
            self.attention_full_scale,
        )
    }
}

// ===========================================================================
// The weighing
// ===========================================================================

/// What the story is worth to the position.
///
/// `multiplier_bps` is the whole answer and it is never above `10_000`. The rest
/// is why, kept beside it so a refusal to size up can be shown to somebody
/// rather than asserted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SocialWeight {
    /// Whether anybody looked. False means the multiplier is one because there
    /// was no scan, not because the scan came back clean.
    pub scanned: bool,
    pub kind: Option<StoryKind>,
    /// Never above `10_000`.
    pub multiplier_bps: u16,
    /// Where the attention curve landed on the ruler, in millionths. `None` when
    /// there were too few samples, too little time between them, or no curve to
    /// read at all — which is not a flat curve and is not charged for.
    pub attention_micros: Option<u64>,
    pub reuse_nth: u32,
    /// Worst first.
    pub cautions: Vec<SocialCaution>,
    pub notes: Vec<String>,
}

impl SocialWeight {
    /// Nobody looked. The multiplier is one and says so.
    pub fn unscanned() -> Self {
        SocialWeight {
            scanned: false,
            kind: None,
            multiplier_bps: BPS_DENOMINATOR as u16,
            attention_micros: None,
            reuse_nth: 0,
            cautions: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// The multiplier that is actually applied, which is never above `10_000`.
    ///
    /// [`weigh`] cannot produce one that is, but `weigh` is not the only way a
    /// value of this type comes to exist: the fields are public and the type is
    /// `Deserialize`, so a weight also arrives from a replay file, an IPC
    /// projection, or a corpus somebody edited. The invariant this module is
    /// *for* — a story can take size off and can never put any on — has to hold
    /// for those too, and a clamp here is the only place it holds for all of
    /// them at once. A forged `65_535` would otherwise size a position at six
    /// and a half times what the risk chain allowed.
    pub const fn effective_bps(&self) -> u16 {
        if self.multiplier_bps > BPS_DENOMINATOR as u16 {
            BPS_DENOMINATOR as u16
        } else {
            self.multiplier_bps
        }
    }

    /// This many lamports after the story has had its say.
    ///
    /// Floor division: the residual goes to the house, so a haircut can never
    /// round a position up.
    pub fn apply(&self, lamports: u64) -> u64 {
        mul_div_floor(
            u128::from(lamports),
            u128::from(self.effective_bps()),
            u128::from(BPS_DENOMINATOR),
        ) as u64
    }

    /// Whether the story took anything off.
    pub const fn reduced(&self) -> bool {
        self.effective_bps() < BPS_DENOMINATOR as u16
    }

    pub fn has(&self, caution: SocialCaution) -> bool {
        self.cautions.contains(&caution)
    }
}

/// The growth of a view curve on the attention ruler, in millionths.
///
/// `None` when there is no curve to read: too few samples, or a first and last
/// sample too close together for growth between them to mean anything. That is
/// UNKNOWN and it is deliberately not zero — a zero here would be indexed
/// against [`SocialParams::dead_below_micros`] and charge every launch the
/// scanner reached late for being dead.
///
/// The samples are sorted rather than assumed sorted, and the sort falls through
/// to the view count so two readings at one millisecond cannot swap between
/// runs. A counter that went backwards between the ends scores zero, which is
/// the pessimistic direction: view counts are monotone in reality, so the only
/// thing that produces a fall is a provider disagreeing with itself.
pub fn attention_micros(samples: &[ViewSample], params: &SocialParams) -> Option<u64> {
    if samples.len() < params.min_view_samples.max(2) {
        return None;
    }
    let mut ordered: Vec<ViewSample> = samples.to_vec();
    ordered.sort_by(|a, b| a.at_ms.cmp(&b.at_ms).then_with(|| a.views.cmp(&b.views)));

    let first = ordered.first().copied()?;
    let last = ordered.last().copied()?;
    if last.at_ms.saturating_sub(first.at_ms) < params.min_view_span_ms {
        return None;
    }
    growth_score_micros(first.views, last.views, params.attention_full_scale)
}

/// What the story is worth to the position.
///
/// Pure: same scan and same params, same answer, nothing fetched and nothing
/// written. The scan itself is the network's job and happens long before this.
///
/// `None` is "nobody looked" and leaves the multiplier at one. Every other
/// answer is the product of the cautions that fired, each of which is a
/// documented haircut and none of which is above `10_000`, so the product cannot
/// be either.
pub fn weigh(scan: Option<&SocialScan>, params: &SocialParams) -> SocialWeight {
    let Some(scan) = scan else {
        return SocialWeight::unscanned();
    };

    let attention = if scan.kind.is_readable() {
        attention_micros(&scan.views, params)
    } else {
        None
    };

    let mut weight = SocialWeight {
        scanned: true,
        kind: Some(scan.kind),
        multiplier_bps: BPS_DENOMINATOR as u16,
        attention_micros: attention,
        reuse_nth: scan.reuse_nth,
        cautions: Vec::new(),
        notes: Vec::new(),
    };
    if !params.enabled {
        weight
            .notes
            .push("the story was recorded and not weighed".to_string());
        return weight;
    }

    // Reuse first: it is the loudest of the three and the two thresholds are
    // exclusive, so the worse of them fires rather than both.
    if scan.reuse_nth >= params.farmed_story_nth {
        weight.cautions.push(SocialCaution::FarmedStory);
        weight.notes.push(format!(
            "launch number {} on the same story — a farm, not a following",
            scan.reuse_nth
        ));
    } else if scan.reuse_nth >= params.shared_story_nth {
        weight.cautions.push(SocialCaution::SharedStory);
        weight.notes.push(format!(
            "launch number {} on the same story",
            scan.reuse_nth
        ));
    }

    match (attention, params.dead_below_micros()) {
        (Some(growth), Some(floor)) if growth < floor => {
            weight.cautions.push(SocialCaution::DeadStory);
            weight.notes.push(format!(
                "attention was flat over the window — {} of the way to a {}x rise",
                share_note(growth),
                params.attention_full_scale
            ));
        }
        (Some(growth), Some(_)) => {
            weight.notes.push(format!(
                "attention was growing — {} of the way to a {}x rise",
                share_note(growth),
                params.attention_full_scale
            ));
        }
        _ => {
            weight.notes.push("no attention curve to read".to_string());
        }
    }

    if !scan.kind.is_readable() {
        weight.cautions.push(SocialCaution::Unreadable);
        weight
            .notes
            .push("the story could not be read — unknown, not clean".to_string());
    }

    weight.cautions.sort_unstable();
    weight.multiplier_bps = weight
        .cautions
        .iter()
        .fold(BPS_DENOMINATOR as u16, |acc, c| {
            mul_div_floor(
                u128::from(acc),
                u128::from(c.haircut_bps(params).min(BPS_DENOMINATOR as u16)),
                u128::from(BPS_DENOMINATOR),
            ) as u16
        });
    weight
}

/// A millionths score as a percentage, for the sentence a person reads.
fn share_note(micros: u64) -> String {
    format!("{}%", micros.min(MICROS) / 10_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: i64 = 60_000;

    fn samples(points: &[(i64, u64)]) -> Vec<ViewSample> {
        points
            .iter()
            .map(|&(at_ms, views)| ViewSample { at_ms, views })
            .collect()
    }

    /// A post nobody else has launched on, whose views quadrupled over a minute.
    /// The best case the measurement supports, and it is worth exactly nothing.
    fn a_live_story() -> SocialScan {
        SocialScan {
            kind: StoryKind::Tweet,
            handle: Some("someone".to_string()),
            followers: Some(40_000),
            account_age_days: Some(365),
            post_age_ms: Some(30_000),
            reuse_nth: 1,
            views: samples(&[(0, 1_000), (MINUTE, 4_000)]),
        }
    }

    /// The same post, on a curve that did not move.
    fn a_dead_story() -> SocialScan {
        SocialScan {
            views: samples(&[(0, 1_000), (MINUTE, 1_050)]),
            ..a_live_story()
        }
    }

    fn weigh_default(scan: &SocialScan) -> SocialWeight {
        weigh(Some(scan), &SocialParams::default())
    }

    // -----------------------------------------------------------------------
    // The invariant the whole module exists for
    // -----------------------------------------------------------------------

    #[test]
    fn no_story_can_ever_add_to_a_position() {
        let params = SocialParams::default();
        let mut scans = vec![
            a_live_story(),
            a_dead_story(),
            SocialScan::unreadable(),
            SocialScan::no_link(),
        ];
        for nth in 1..=6 {
            scans.push(SocialScan {
                reuse_nth: nth,
                ..a_live_story()
            });
        }
        for followers in [0u64, 1, 1_000, 10_000_000] {
            scans.push(SocialScan {
                followers: Some(followers),
                ..a_live_story()
            });
        }
        for scan in &scans {
            let weight = weigh(Some(scan), &params);
            assert!(
                weight.multiplier_bps <= BPS_DENOMINATOR as u16,
                "{:?} was allowed to size a position up",
                scan.kind
            );
            assert!(
                weight.apply(10 * crate::replay::LAMPORTS_PER_SOL)
                    <= 10 * crate::replay::LAMPORTS_PER_SOL
            );
        }
    }

    #[test]
    fn the_best_possible_story_is_worth_nothing() {
        let weight = weigh_default(&a_live_story());
        assert_eq!(weight.multiplier_bps, BPS_DENOMINATOR as u16);
        assert!(!weight.reduced());
        assert!(weight.cautions.is_empty());
    }

    #[test]
    fn followers_do_not_change_the_weight() {
        // The archived grading compared like with like and found the story adds
        // nothing. A follower count that moved this number would be a claim the
        // measurement does not support.
        let poor = SocialScan {
            followers: Some(3),
            account_age_days: Some(0),
            ..a_live_story()
        };
        let famous = SocialScan {
            followers: Some(4_000_000),
            account_age_days: Some(4_000),
            ..a_live_story()
        };
        assert_eq!(
            weigh_default(&poor).multiplier_bps,
            weigh_default(&famous).multiplier_bps
        );
    }

    #[test]
    fn a_freshly_posted_story_does_not_change_the_weight_either() {
        let stale = SocialScan {
            post_age_ms: Some(66 * MINUTE),
            ..a_live_story()
        };
        let fresh = SocialScan {
            post_age_ms: Some(11_000),
            ..a_live_story()
        };
        assert_eq!(
            weigh_default(&stale).multiplier_bps,
            weigh_default(&fresh).multiplier_bps
        );
    }

    // -----------------------------------------------------------------------
    // Not looking, and looking and failing
    // -----------------------------------------------------------------------

    #[test]
    fn nobody_looked_is_not_a_clean_scan_and_costs_nothing() {
        let weight = weigh(None, &SocialParams::default());
        assert!(!weight.scanned);
        assert_eq!(weight.kind, None);
        assert_eq!(weight.multiplier_bps, BPS_DENOMINATOR as u16);
        assert!(weight.cautions.is_empty());
    }

    #[test]
    fn a_scan_that_failed_is_unknown_and_costs_something() {
        let weight = weigh_default(&SocialScan::unreadable());
        assert!(weight.scanned);
        assert!(weight.has(SocialCaution::Unreadable));
        assert_eq!(weight.multiplier_bps, UNREADABLE_BPS);
    }

    #[test]
    fn a_launch_with_no_story_at_all_is_read_and_costs_nothing() {
        // 852 of the archived corpus had no link and outperformed every linked
        // bucket. Charging for the absence would be backwards.
        let weight = weigh_default(&SocialScan::no_link());
        assert!(weight.scanned);
        assert_eq!(weight.kind, Some(StoryKind::NoLink));
        assert_eq!(weight.multiplier_bps, BPS_DENOMINATOR as u16);
    }

    #[test]
    fn an_unreadable_scan_has_no_attention_curve_to_read() {
        let scan = SocialScan {
            views: samples(&[(0, 1_000), (MINUTE, 4_000)]),
            ..SocialScan::unreadable()
        };
        let weight = weigh_default(&scan);
        assert_eq!(weight.attention_micros, None);
        assert!(!weight.has(SocialCaution::DeadStory));
    }

    // -----------------------------------------------------------------------
    // Reuse
    // -----------------------------------------------------------------------

    #[test]
    fn the_first_two_launches_on_a_story_are_a_race_and_the_fourth_is_a_farm() {
        let costs = |nth: u32| {
            weigh_default(&SocialScan {
                reuse_nth: nth,
                ..a_live_story()
            })
        };
        assert_eq!(costs(1).multiplier_bps, BPS_DENOMINATOR as u16);
        assert_eq!(costs(2).multiplier_bps, BPS_DENOMINATOR as u16);
        assert_eq!(costs(3).multiplier_bps, SHARED_STORY_BPS);
        assert_eq!(costs(4).multiplier_bps, FARMED_STORY_BPS);
        assert_eq!(costs(40).multiplier_bps, FARMED_STORY_BPS);
    }

    #[test]
    fn a_farm_is_charged_once_rather_than_twice() {
        let weight = weigh_default(&SocialScan {
            reuse_nth: 9,
            ..a_live_story()
        });
        assert!(weight.has(SocialCaution::FarmedStory));
        assert!(!weight.has(SocialCaution::SharedStory));
    }

    // -----------------------------------------------------------------------
    // Attention
    // -----------------------------------------------------------------------

    #[test]
    fn a_flat_curve_is_a_dead_story() {
        let weight = weigh_default(&a_dead_story());
        assert!(weight.has(SocialCaution::DeadStory));
        assert_eq!(weight.multiplier_bps, DEAD_STORY_BPS);
    }

    #[test]
    fn a_growing_curve_is_not_charged_for() {
        let weight = weigh_default(&a_live_story());
        assert!(!weight.has(SocialCaution::DeadStory));
    }

    #[test]
    fn a_curve_too_short_to_read_is_unknown_rather_than_dead() {
        let scan = SocialScan {
            views: samples(&[(0, 1_000), (MINUTE / 4, 1_000)]),
            ..a_live_story()
        };
        let weight = weigh_default(&scan);
        assert_eq!(weight.attention_micros, None);
        assert!(!weight.has(SocialCaution::DeadStory));
        assert_eq!(weight.multiplier_bps, BPS_DENOMINATOR as u16);
    }

    #[test]
    fn one_sample_is_not_a_curve() {
        let scan = SocialScan {
            views: samples(&[(0, 1_000)]),
            ..a_live_story()
        };
        assert_eq!(weigh_default(&scan).attention_micros, None);
    }

    #[test]
    fn a_curve_reads_the_same_however_the_samples_arrive() {
        let params = SocialParams::default();
        let forwards = samples(&[(0, 1_000), (MINUTE, 2_000), (2 * MINUTE, 8_000)]);
        let mut shuffled = forwards.clone();
        shuffled.swap(0, 2);
        shuffled.swap(1, 2);
        assert_eq!(
            attention_micros(&forwards, &params),
            attention_micros(&shuffled, &params)
        );
    }

    #[test]
    fn a_counter_that_went_backwards_is_flat_rather_than_negative() {
        let scan = SocialScan {
            views: samples(&[(0, 5_000), (MINUTE, 1_000)]),
            ..a_live_story()
        };
        let weight = weigh_default(&scan);
        assert_eq!(weight.attention_micros, Some(0));
        assert!(weight.has(SocialCaution::DeadStory));
    }

    #[test]
    fn the_dead_boundary_sits_where_the_policy_says_it_does() {
        let params = SocialParams::default();
        let floor = params.dead_below_micros().expect("a usable ruler");
        // x1.72 is the boundary: just under is dead, just over is not.
        let under = growth_score_micros(10_000, 17_100, ATTENTION_FULL_SCALE).expect("measurable");
        let over = growth_score_micros(10_000, 17_300, ATTENTION_FULL_SCALE).expect("measurable");
        assert!(under < floor, "x1.71 should be dead");
        assert!(over >= floor, "x1.73 should not be");
    }

    // -----------------------------------------------------------------------
    // Compounding, and the switch
    // -----------------------------------------------------------------------

    #[test]
    fn two_cautions_compound_rather_than_replace() {
        let scan = SocialScan {
            reuse_nth: 4,
            ..a_dead_story()
        };
        let weight = weigh_default(&scan);
        assert!(weight.has(SocialCaution::FarmedStory));
        assert!(weight.has(SocialCaution::DeadStory));
        // 5_000 of 10_000, then 7_500 of that.
        assert_eq!(weight.multiplier_bps, 3_750);
    }

    #[test]
    fn the_cautions_come_out_worst_first_whatever_order_they_fired_in() {
        let scan = SocialScan {
            reuse_nth: 3,
            views: samples(&[(0, 1_000), (MINUTE, 1_000)]),
            ..SocialScan::unreadable()
        };
        let mut weight = weigh_default(&scan);
        let listed = weight.cautions.clone();
        weight.cautions.sort_unstable();
        assert_eq!(listed, weight.cautions);
    }

    #[test]
    fn observing_records_the_story_and_charges_nothing_for_it() {
        let params = SocialParams::observe_only();
        for scan in [a_dead_story(), SocialScan::unreadable()] {
            let weight = weigh(Some(&scan), &params);
            assert!(weight.scanned);
            assert_eq!(weight.multiplier_bps, BPS_DENOMINATOR as u16);
            assert!(weight.cautions.is_empty());
        }
    }

    #[test]
    fn the_haircut_never_rounds_a_position_up() {
        let weight = weigh_default(&a_dead_story());
        // 7_500 of three lamports is 2.25, and the quarter goes to the house.
        assert_eq!(weight.apply(3), 2);
        assert_eq!(weight.apply(0), 0);
    }

    #[test]
    fn a_weight_that_did_not_come_from_weigh_still_cannot_add() {
        // `weigh` cannot produce a multiplier above one, but it is not the only
        // way one of these comes to exist: the fields are public and the type is
        // `Deserialize`, so a weight also arrives from a replay file, an IPC
        // projection, or a corpus somebody hand-edited. The invariant belongs to
        // the module rather than to the constructor.
        let forged: SocialWeight = serde_json::from_str(
            r#"{"scanned":true,"kind":"TWEET","multiplier_bps":65535,
                "attention_micros":null,"reuse_nth":1,"cautions":[],"notes":[]}"#,
        )
        .expect("a weight can arrive from a file");
        assert_eq!(
            forged.multiplier_bps,
            u16::MAX,
            "the field is kept as it was read"
        );
        assert_eq!(forged.effective_bps(), BPS_DENOMINATOR as u16);
        assert_eq!(forged.apply(1_000_000_000), 1_000_000_000);
        assert!(
            !forged.reduced(),
            "an inflated multiplier is not a reduction"
        );

        // One basis point over is the interesting case, not just the absurd one.
        let barely = SocialWeight {
            multiplier_bps: BPS_DENOMINATOR as u16 + 1,
            ..forged
        };
        assert_eq!(barely.apply(1_000_000_000), 1_000_000_000);
    }

    #[test]
    fn nothing_here_panics_on_a_nonsense_scan() {
        let params = SocialParams {
            attention_full_scale: 1,
            min_view_samples: 0,
            min_view_span_ms: i64::MIN,
            dead_below_growth_bps: u32::MAX,
            ..SocialParams::default()
        };
        let scan = SocialScan {
            reuse_nth: u32::MAX,
            followers: Some(u64::MAX),
            post_age_ms: Some(i64::MIN),
            views: samples(&[(i64::MIN, 0), (i64::MAX, u64::MAX)]),
            ..a_live_story()
        };
        let weight = weigh(Some(&scan), &params);
        assert!(weight.multiplier_bps <= BPS_DENOMINATOR as u16);
    }
}
