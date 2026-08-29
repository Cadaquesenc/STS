//! Fixed-point arithmetic, and the one transcendental built on it.
//!
//! `backtest.rs` carries `isqrt` and `exp_neg_micros` for the same reason this
//! file exists: a number that is stored, compared and replayed cannot be
//! allowed to depend on whose libm the build linked against. `f64::ln` is not
//! specified to the last bit by IEEE 754, and two machines that disagree in
//! that bit disagree about whether a launch cleared the entry threshold.
//!
//! Everything here is `u128` at `10^-18`. The choice of `10^18` is the same one
//! `backtest.rs` makes: the largest power of ten whose square still leaves room
//! in a `u128` to add.
//!
//! # Three layers
//!
//! The [core](#the-fixed-point-core) is `mul`, `div`, `pow` and the conversions
//! — the arithmetic itself, working in [`ONE`] units and saturating rather than
//! wrapping at every step. [`jito`](crate::jito) prices a tip floor with it and
//! never touches a float.
//!
//! [`Fixed`] is that same arithmetic with the unit carried in the type, for the
//! case the bare core does not cover: a chain of factors that has to compose
//! *without* rounding between the links. The forensics side —
//! [`clustering`](crate::clustering) and [`tracer`](crate::tracer) — scores with
//! it, and [`exp_neg`] is its decay.
//!
//! Above both sit [`ln_fixed`], the two entropy functions and the ratio pair
//! built on it — [`ln_ratio_micros`] and [`growth_score_micros`]. They report
//! millionths because that is the unit the rest of `strategy` scores in. The
//! entropy pair is the syndicate detector's; the ratio pair is
//! [`social`](crate::strategy::social)'s, which needs a log ruler because
//! attention grows by factors rather than by amounts and a doubling has to be
//! the same distance from ten views as from ten thousand.
//!
//! # Saturation, and where it bites
//!
//! Every product here goes through `saturating_mul`, so an overflow clamps to
//! `u128::MAX` instead of wrapping to a small number — a tip floor that reads
//! as astronomically high is refused by the bound it is clamped into, and one
//! that wrapped to nothing would be paid. The practical ceiling is that [`mul`]
//! is exact while both operands stay under about `3.4 x 10^20` in `ONE` units,
//! which is 340 in ordinary terms, and every multiplier in this codebase is a
//! small number near one.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::backtest::{isqrt, mul_div_floor, mul_div_round, MICROS};

// ===========================================================================
// the fixed-point core
// ===========================================================================

/// The unit: one, at `10^-18`.
///
/// Sibling of `backtest::FIXED_ONE`, which stays private to that module. This
/// one is public because [`jito`](crate::jito) computes in these units rather
/// than converting at every step — a tip floor is a chain of six multiplies and
/// a division, and rounding to millionths between each of them would throw away
/// more precision than the whole calculation is allowed to move.
pub const ONE: u128 = 1_000_000_000_000_000_000;

/// The old private name, kept for the entropy code below that reads better with
/// it.
const FIXED_ONE: u128 = ONE;

/// The same unit under the name the tick pipeline spells it.
///
/// [`crate::geyser`] and [`crate::subslot`] work at `10^-18` end to end and
/// convert to millionths only where a score is formed, and they carry a naming
/// rule to keep the two units apart: every value at this precision sits in a
/// binding whose name ends `_e18`, and the only way down to millionths is
/// [`e18_to_micros`]. A raw `u128` crossing a module boundary without that
/// suffix is a bug in review rather than a runtime failure.
///
/// Why the pipeline needs the precision at all: a pump.fun curve prices at
/// roughly `2.8 x 10^-5` lamports per raw token unit, which in millionths is
/// the integer `28` — two significant figures, and a one-unit rounding step
/// worth 3.5% of the price. A price ladder cannot be built on that.
///
/// An alias rather than a second constant, so there is exactly one unit here
/// with two spellings and no way for them to drift apart.
pub const ONE_E18: u128 = ONE;

/// `a x b`, both at `10^-18`, floored.
///
/// Saturating: an overflow clamps rather than wrapping. See the module note on
/// where that bites — in short, exact for any pair under about 340.
pub fn mul(a: u128, b: u128) -> u128 {
    a.saturating_mul(b) / ONE
}

/// `a / b`, both at `10^-18`, floored. Zero when `b` is zero.
///
/// A denominator of zero is a share of nothing, which is the same convention
/// [`crate::backtest::mul_div_floor`] uses and for the same reason: every
/// caller here is taking a proportion, and a proportion of an empty window is
/// zero rather than a panic on the send path.
pub fn div(a: u128, b: u128) -> u128 {
    if b == 0 {
        return 0;
    }
    a.saturating_mul(ONE) / b
}

/// `base^exponent`, at `10^-18`, by squaring.
///
/// The slot-distance weights in [`jito`](crate::jito) are this and nothing
/// else: a decay of `d` per slot, `n` slots back, is `d^n`. Squaring rather
/// than a loop of multiplies because the window is bounded but the exponent is
/// caller-supplied, and `O(log n)` means a misconfigured window costs a few
/// more multiplies instead of a stall.
///
/// Each squaring truncates, so the result drifts below the true power by at
/// most one part in `10^18` per step — about `10^-17` over the five steps a
/// 32-slot window needs, which is nine orders of magnitude below the lamport
/// this eventually rounds to.
pub fn pow(base: u128, exponent: u32) -> u128 {
    let mut result = ONE;
    let mut factor = base;
    let mut remaining = exponent;
    while remaining > 0 {
        if remaining & 1 == 1 {
            result = mul(result, factor);
        }
        remaining >>= 1;
        if remaining > 0 {
            factor = mul(factor, factor);
        }
    }
    result
}

/// `numerator / denominator` as a fixed-point value. Zero when the denominator
/// is zero.
pub fn ratio(numerator: u64, denominator: u64) -> u128 {
    if denominator == 0 {
        return 0;
    }
    u128::from(numerator).saturating_mul(ONE) / u128::from(denominator)
}

/// Millionths in, `10^-18` out. Exact — `10^18` is a whole multiple of `10^6`.
pub fn from_micros(micros: u64) -> u128 {
    u128::from(micros).saturating_mul(ONE / u128::from(MICROS))
}

/// `10^-18` in, millionths out, rounded to nearest.
///
/// Rounded rather than truncated because these are reported numbers: a
/// saturation of 0.9999995 that printed as 0.999999 would read as "not quite
/// full" every time it was in fact full.
pub fn to_micros(value: u128) -> u64 {
    let scaled = value.saturating_mul(u128::from(MICROS));
    let rounded = scaled.saturating_add(ONE / 2) / ONE;
    u64::try_from(rounded).unwrap_or(u64::MAX)
}

/// Scales a whole number — lamports, microseconds — by a fixed-point factor,
/// rounded to nearest and saturating at `u64::MAX`.
///
/// Rounded to nearest for the reason Annex C gives about tips generally: a
/// floor that always truncated would sit one lamport under what the window
/// actually said, on every bundle, forever.
pub fn scale(value: u64, factor: u128) -> u64 {
    let product = u128::from(value).saturating_mul(factor);
    let rounded = product.saturating_add(ONE / 2) / ONE;
    u64::try_from(rounded).unwrap_or(u64::MAX)
}

// ===========================================================================
// the unit-carrying layer
// ===========================================================================

/// A number in `[0, 1]` carried at `10^-18` in a `u128`.
///
/// The module header explains why `FIXED_ONE` is private: a second unit loose in
/// the codebase is how the two get mixed. This type is the answer to the case
/// that argument does not cover — a chain of factors that has to compose
/// *without* rounding between the links.
///
/// `RISK_AND_SYBIL_SPEC.md` §3.3 is that case exactly. Path influence is a
/// product of three factors, each below one, and rounding each to a millionth
/// before multiplying throws away three digits the comparison downstream is
/// made on. So the unit travels in the type instead of in the caller's head:
/// nothing outside can read the raw integer, [`Fixed::to_micros`] is the only
/// way out, and a value in millionths can never be silently passed where one at
/// `10^-18` was meant.
///
/// Saturating at one throughout. Every quantity this represents is a
/// probability, a confidence or a share, and none of them has a meaning above
/// one — so an input that claims otherwise is clamped at the boundary rather
/// than propagating a number the rest of the arithmetic assumes cannot exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Fixed(u128);

impl Fixed {
    pub const ZERO: Fixed = Fixed(0);
    pub const ONE: Fixed = Fixed(FIXED_ONE);

    /// `ln(2)`, the one constant above one this type is allowed to hold.
    ///
    /// It is a multiplier rather than a share — §3.3's `lambda` is
    /// `ln(2) / half_life` — and it is under one anyway, which is why it fits
    /// here at all rather than needing a wider type.
    pub const LN2: Fixed = Fixed(LN2_FIXED);

    /// Widens a value in millionths. Values above one saturate at one.
    pub fn from_micros(micros: u64) -> Fixed {
        Fixed(u128::from(micros).min(u128::from(MICROS)) * 1_000_000_000_000)
    }

    /// Widens a value in basis points. Values above one saturate at one.
    pub fn from_bps(bps: u64) -> Fixed {
        Fixed(u128::from(bps).min(10_000) * 100_000_000_000_000)
    }

    /// `numerator / denominator`, floored, saturating at one.
    ///
    /// A zero denominator is zero, not one and not a panic: every caller here is
    /// taking a share of something, and a denominator of zero means there was
    /// nothing to take a share of. That is the same convention
    /// [`crate::backtest::mul_div_floor`] follows, for the same reason.
    pub fn from_ratio(numerator: u128, denominator: u128) -> Fixed {
        if denominator == 0 {
            return Fixed::ZERO;
        }
        if numerator >= denominator {
            return Fixed::ONE;
        }
        Fixed(numerator.saturating_mul(FIXED_ONE) / denominator)
    }

    /// The ratio *without* the clamp at one, for the exponent of [`exp_neg`].
    ///
    /// `lambda × age` is unbounded — funding from a week ago is several
    /// half-lives — and clamping it at one would turn every decay older than one
    /// half-life into the same number. Saturates at the largest representable
    /// value rather than wrapping; `exp_neg` returns zero well before that.
    pub fn ratio_unclamped(numerator: u128, denominator: u128) -> Fixed {
        if denominator == 0 {
            return Fixed::ZERO;
        }
        Fixed(numerator.saturating_mul(FIXED_ONE) / denominator)
    }

    pub fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// The product, floored. Both operands are at `10^-18`, so the raw product
    /// is at `10^-36` and one division brings it back.
    ///
    /// The floor is deliberate and it is the conservative direction: influence
    /// is evidence for blocking an entry, and a product that rounds up is
    /// evidence the traversal did not find.
    ///
    /// Named for what it does rather than `mul`, which is
    /// [`std::ops::Mul::mul`]'s name for an operation that panics on overflow
    /// where this one clamps. It pairs with [`saturating_add`](Fixed::saturating_add)
    /// below, which is the same promise about the other operator.
    pub fn saturating_mul(self, other: Fixed) -> Fixed {
        Fixed(self.0.saturating_mul(other.0) / FIXED_ONE)
    }

    /// The sum, saturating at one.
    pub fn saturating_add(self, other: Fixed) -> Fixed {
        Fixed(self.0.saturating_add(other.0).min(FIXED_ONE))
    }

    /// The sum *without* the clamp at one.
    ///
    /// For a denominator several shares are taken against: §3.3's parent
    /// posterior divides one root's influence by the total over every root, and
    /// that total routinely exceeds one. Clamping it would inflate every share
    /// taken against it.
    pub fn add_unclamped(self, other: Fixed) -> Fixed {
        Fixed(self.0.saturating_add(other.0))
    }

    /// This value's share of `total`, floored, saturating at one.
    ///
    /// The normalising step, kept here so callers never need the raw integer to
    /// perform it — which is what keeps the unit inside the type.
    pub fn share_of(self, total: Fixed) -> Fixed {
        Fixed::from_ratio(self.0, total.0)
    }

    /// This value scaled by `bps` ten-thousandths of itself.
    ///
    /// §3.3's `kappa` discount on corroborating paths, which is a policy number
    /// in basis points and never a `Fixed` of its own.
    pub fn scale_bps(self, bps: u64) -> Fixed {
        Fixed(self.0.saturating_mul(u128::from(bps)) / 10_000).min(Fixed::ONE)
    }

    /// The geometric mean of two values, `sqrt(a × b)`.
    ///
    /// The product is at `10^-36`, which is under `1.1 × 10^36` and so inside a
    /// `u128` with two orders of magnitude to spare, and an integer square root
    /// of a `10^-36` quantity lands back at `10^-18` exactly.
    pub fn geometric_mean(self, other: Fixed) -> Fixed {
        Fixed(isqrt(self.0.saturating_mul(other.0)).min(FIXED_ONE))
    }

    /// Narrows to millionths, rounded to nearest.
    ///
    /// Rounded rather than floored because this is the boundary where a
    /// composed value is finally reported, and a truncation that always points
    /// one way biases every score in the system downwards. The floors inside
    /// [`saturating_mul`](Fixed::saturating_mul) are per-factor and deliberate; this is the one place
    /// the accumulated bias is corrected rather than added to.
    pub fn to_micros(self) -> u64 {
        ((self.0 + 500_000_000_000) / 1_000_000_000_000).min(u128::from(MICROS)) as u64
    }

    /// Narrows to basis points, rounded to nearest.
    pub fn to_bps(self) -> u16 {
        ((self.0 + 50_000_000_000_000) / 100_000_000_000_000).min(10_000) as u16
    }
}

/// `exp(-x)` at `10^-18`.
///
/// The sibling of [`crate::backtest::exp_neg_micros`], and it exists rather than
/// reusing it because §3.3's decay is one factor in a product of three: taking
/// it in millionths and widening back would round away six of the eighteen
/// digits the other two factors are carrying.
///
/// Range reduction by halving to under `1/256`, six Taylor terms there, then
/// repeated squaring back up. Six terms at that argument leave a series residue
/// under `3 × 10^-21`, and the squarings amplify it and the per-division
/// truncations by at most `2^14`, so the result is good to about one part in
/// `10^13` — the same accuracy the millionth-precision sibling reaches, now
/// reported at a precision that does not throw the rest of the product away.
/// What matters more than the accuracy is that every step is integer division:
/// two machines produce the same bits, which `f64::exp` does not promise.
///
/// Monotone non-increasing, exactly one at zero, and zero at and above `x = 42`,
/// where the true value is `5.7 × 10^-19` and rounds to nothing at this
/// precision.
pub fn exp_neg(x: Fixed) -> Fixed {
    if x.0 == 0 {
        return Fixed::ONE;
    }
    if x.0 >= 42 * FIXED_ONE {
        return Fixed::ZERO;
    }

    // Reduce to under 1/256. The loop runs at most fourteen times: the largest
    // argument that got here is just under 42, and 42 / 2^14 is well under the
    // threshold.
    let mut halvings = 0u32;
    let mut u = x.0;
    while u > FIXED_ONE / 256 {
        u >>= 1;
        halvings += 1;
    }

    // exp(-u) = 1 - u + u²/2 - u³/6 + u⁴/24 - u⁵/120 + u⁶/720. Every term is
    // carried at 10^-18 and every division truncates by under one part in 10^18.
    // u <= 1/256 here, so u² is at most 1.6 × 10^31 and nothing overflows.
    let t2 = u * u / FIXED_ONE / 2;
    let t3 = t2 * u / FIXED_ONE / 3;
    let t4 = t3 * u / FIXED_ONE / 4;
    let t5 = t4 * u / FIXED_ONE / 5;
    let t6 = t5 * u / FIXED_ONE / 6;
    let mut value = FIXED_ONE + t2 + t4 + t6;
    value = value
        .saturating_sub(u)
        .saturating_sub(t3)
        .saturating_sub(t5);

    // Square back up. The value never exceeds 10^18, so each square is at most
    // 10^36 and stays inside a u128.
    for _ in 0..halvings {
        value = value * value / FIXED_ONE;
    }

    Fixed(value.min(FIXED_ONE))
}

/// `ln(2)` at `10^-18`, truncated rather than rounded so that the reduction
/// `ln(x) = k·ln2 + ln(m)` never overshoots.
const LN2_FIXED: u128 = 693_147_180_559_945_309;

/// The natural log of `x`, at `10^-18`. Zero for `x <= 1`.
///
/// Range reduction to `m` in `[1, 2)` by the exponent of `x`, then the `artanh`
/// series `ln(m) = 2·(z + z³/3 + z⁵/5 + …)` at `z = (m-1)/(m+1)`, which is
/// bounded by `1/3` after the reduction. Each squaring shrinks the term ninefold
/// so the loop reaches zero in about twenty passes and stops there — the
/// termination is the arithmetic running out of precision, not a fixed count,
/// which is what makes the answer the same on every machine.
///
/// `ln(0)` and `ln(1)` are both reported as zero. That is exactly right for the
/// second and deliberate for the first: the only callers here are entropy sums,
/// where a share of zero contributes nothing and `0·ln 0` is defined as zero
/// (`RISK_AND_SYBIL_SPEC.md` §2.3), so a zero is skipped before it ever gets
/// here and a zero returned is a belt on top of that brace.
pub fn ln_fixed(x: u64) -> u128 {
    if x <= 1 {
        return 0;
    }

    // floor(log2 x). `x >= 2` here, so `k >= 1`.
    let k = 63 - x.leading_zeros();

    // m = x / 2^k, in [1, 2) at 10^-18. `x` is at most 2^64, so the product is
    // at most 1.9 x 10^37 and the shift only makes it smaller.
    let m = (u128::from(x) * FIXED_ONE) >> k;

    // z = (m - 1)/(m + 1), in [0, 1/3).
    let z = (m - FIXED_ONE) * FIXED_ONE / (m + FIXED_ONE);
    let z_squared = z * z / FIXED_ONE;

    let mut term = z;
    let mut sum: u128 = 0;
    let mut odd: u128 = 1;
    while term > 0 {
        sum += term / odd;
        term = term * z_squared / FIXED_ONE;
        odd += 2;
    }

    u128::from(k) * LN2_FIXED + 2 * sum
}

/// Shannon entropy over a partition, normalised by `ln(n)`, in millionths.
///
/// `group_sizes` are the sizes of the parts and `total` is the number of items
/// across all of them. One part holding everything is zero; every item in its
/// own part is one.
///
/// The identity it computes is
///
/// ```text
/// H / ln(n)  =  1 - (Σ_g g·ln g) / (n·ln n)
/// ```
///
/// which is the same number as `-Σ p·ln p / ln n` and needs no division inside
/// the log, so every argument to `ln_fixed` is a whole count and the shares
/// never have to be represented at all.
///
/// Fewer than two items is not a low-entropy population, it is an unmeasurable
/// one, and the answer is one — the same convention `cluster.js` used, chosen
/// because this feeds a term that *rewards* variety and a lone buyer must not
/// be scored as if it were a script.
pub fn normalised_entropy_micros(group_sizes: &[usize], total: usize) -> u64 {
    if total < 2 {
        return MICROS;
    }
    let Ok(total_u64) = u64::try_from(total) else {
        return MICROS;
    };

    let denominator = (total as u128) * ln_fixed(total_u64);
    if denominator == 0 {
        return MICROS;
    }

    let numerator: u128 = group_sizes
        .iter()
        .filter_map(|&size| u64::try_from(size).ok())
        .map(|size| u128::from(size) * ln_fixed(size))
        .sum();

    let ratio = mul_div_round(numerator, u128::from(MICROS), denominator);
    MICROS.saturating_sub(ratio.min(u128::from(MICROS)) as u64)
}

/// Shannon entropy over weights, normalised by `ln(count)`, in millionths.
///
/// `RISK_AND_SYBIL_SPEC.md` §5.1's interaction entropy: the weights are edge
/// volumes and the count is how many edges there are. Weights of zero are
/// skipped rather than evaluated, because `ln(0)` is not a limit to take at
/// runtime.
///
/// `None` when fewer than two non-zero weights survive. §5.1 is explicit that
/// this is the right answer and zero is not: one edge has a defined entropy of
/// zero that means nothing at all, and a zero in this column reads as "one
/// funder pays everyone", which is a claim the data has not made.
///
/// The per-term product is `w·ln(W/w)`, which is bounded by `W/e` however the
/// weights are arranged, so the accumulator cannot overflow for any `W` that
/// fits a `u64` — which every lamport total does, the whole supply of SOL being
/// under `10^18`.
pub fn weighted_entropy_micros(weights: &[u64]) -> Option<u64> {
    let non_zero: Vec<u64> = weights.iter().copied().filter(|&w| w > 0).collect();
    if non_zero.len() < 2 {
        return None;
    }

    let total: u64 = non_zero
        .iter()
        .try_fold(0u64, |acc, &w| acc.checked_add(w))?;
    let log_total = ln_fixed(total);

    // H = Σ (w/W)·ln(W/w), accumulated at 10^-18.
    let entropy: u128 = non_zero
        .iter()
        .map(|&w| mul_div_floor(u128::from(w), log_total - ln_fixed(w), u128::from(total)))
        .sum();

    let log_count = ln_fixed(non_zero.len() as u64);
    if log_count == 0 {
        return None;
    }

    Some(mul_div_round(entropy, u128::from(MICROS), log_count).min(u128::from(MICROS)) as u64)
}

/// The natural log of a ratio, in millionths, signed.
///
/// `ln(n/d) = ln(n) - ln(d)`, which is why this is here rather than a division
/// followed by a log: the division would have to happen in fixed point and its
/// rounding would land inside the log, where the series amplifies it. Two whole
/// counts subtracted after the fact is exact to the last unit either side.
///
/// `None` when either side is zero. A ratio against nothing is not a ratio of
/// zero, and `ln(0)` is not a limit to take at runtime — the same convention
/// [`weighted_entropy_micros`] uses for a zero weight.
///
/// Not to be confused with [`ratio`], which is the plain quotient in [`ONE`]
/// units. This is its logarithm, in millionths, and the two are not
/// interchangeable in either direction.
///
/// The range is bounded by `+/- ln(u64::MAX)`, about 44.4, so the millionths fit
/// an `i64` with thirteen orders of magnitude to spare.
pub fn ln_ratio_micros(numerator: u64, denominator: u64) -> Option<i64> {
    if numerator == 0 || denominator == 0 {
        return None;
    }
    let difference = ln_fixed(numerator) as i128 - ln_fixed(denominator) as i128;
    let scale = (FIXED_ONE / u128::from(MICROS)) as i128;
    // Rounded half away from zero, so a growth and the shrink that undoes it
    // report the same magnitude. Rounding half up would make the pair asymmetric
    // by one millionth, which is the sort of thing that decides a threshold.
    let magnitude = (difference.abs() + scale / 2) / scale;
    Some(if difference < 0 {
        -(magnitude as i64)
    } else {
        magnitude as i64
    })
}

/// Multiplicative growth on a log ruler, as a share of `full_scale_ratio`, in
/// millionths.
///
/// `ln(to/from) / ln(full_scale_ratio)`, clamped to `[0, 1]`. A ruler for
/// quantities that grow by factors rather than by amounts: on it, doubling is
/// the same distance whether it happened from ten or from ten thousand, which
/// is the only way a view counter on a post with four hundred followers and one
/// with four hundred thousand can be compared at all.
///
/// Shrinking reports zero rather than a negative. The callers here are bounded
/// scores where "it went backwards" and "it did not grow" have the same
/// consequence, and a signed score would have to be clamped anyway;
/// [`ln_ratio_micros`] is the one to reach for when the sign matters.
///
/// `None` when either endpoint is zero — nothing can be measured from nothing —
/// or when `full_scale_ratio` is below two, which is a ruler with no length.
pub fn growth_score_micros(from: u64, to: u64, full_scale_ratio: u64) -> Option<u64> {
    if from == 0 || to == 0 || full_scale_ratio < 2 {
        return None;
    }
    let start = ln_fixed(from);
    let end = ln_fixed(to);
    if end <= start {
        return Some(0);
    }
    let full_scale = ln_fixed(full_scale_ratio);
    if full_scale == 0 {
        return None;
    }
    Some(mul_div_round(end - start, u128::from(MICROS), full_scale).min(u128::from(MICROS)) as u64)
}
// ===========================================================================
// the checked 10^-18 layer
// ===========================================================================
//
// The core above saturates: an overflow clamps to the ceiling, which is what a
// tip floor wants, because a floor that wrapped to nothing would be paid. The
// tick pipeline wants the opposite. A saturated price is a wrong price that
// looks like a real one, and a ladder built on it would act; so everything
// below reports `None` instead and the pipeline reports a gap.
//
// The two layers are not duplicates of each other. They divide by magnitude:
// the core is exact while both operands stay under about 340 in ordinary
// terms, which every multiplier near one satisfies, and the layer below splits
// each operation into whole and fractional parts so that reserves lifted whole
// into `10^-18` — `from_integer` makes a lamport count `10^18` times bigger —
// still divide exactly. Reach for the core when scaling by a factor near one,
// and for this layer when the operands are quantities rather than multipliers.

/// `numerator / denominator`, at `10^-18`.
///
/// `None` when the denominator is zero or when the answer will not fit. The
/// division is done in two pieces — whole part, then remainder — so that a
/// ratio whose numerator would overflow `numerator * 10^18` still computes
/// exactly. Only the remainder term needs the scaling headroom, and the
/// remainder is smaller than the denominator by construction.
///
/// Both curve reserves are `u64`, so the remainder term is at most
/// `1.8 x 10^19 * 10^18 = 1.8 x 10^37`, comfortably inside `u128`. The checked
/// arithmetic is there for callers this module does not know about yet.
pub fn ratio_e18(numerator: u128, denominator: u128) -> Option<u128> {
    if denominator == 0 {
        return None;
    }
    let whole = numerator / denominator;
    let remainder = numerator % denominator;

    let scaled_whole = whole.checked_mul(ONE_E18)?;
    // `remainder < denominator`, so this quotient is below `ONE_E18` and the
    // sum can only overflow if `scaled_whole` was already near the ceiling.
    let scaled_remainder = remainder.checked_mul(ONE_E18)? / denominator;
    scaled_whole.checked_add(scaled_remainder)
}

/// `a * b` where both sides are already at `10^-18`, floored.
///
/// The product carries `10^-36`, so one factor of [`ONE_E18`] comes back out.
/// Split the same way as [`ratio_e18`] so that two large prices multiplied do
/// not overflow before the scale is removed.
pub fn mul_e18(a_e18: u128, b_e18: u128) -> Option<u128> {
    let whole = b_e18 / ONE_E18;
    let fraction = b_e18 % ONE_E18;
    let from_whole = a_e18.checked_mul(whole)?;
    // `fraction < ONE_E18`, so this term never needs more room than `a_e18`.
    let from_fraction = a_e18.checked_mul(fraction)? / ONE_E18;
    from_whole.checked_add(from_fraction)
}

/// `a / b` where both sides are already at `10^-18`, floored.
pub fn div_e18(a_e18: u128, b_e18: u128) -> Option<u128> {
    ratio_e18(a_e18, b_e18)
}

/// A raw integer amount lifted to `10^-18`.
///
/// The identity conversion for reserves: lamports, or raw token units, become
/// the same quantity at this precision so they can be divided by each other.
pub fn from_integer(amount: u64) -> Option<u128> {
    u128::from(amount).checked_mul(ONE_E18)
}

/// A raw token amount scaled down by its mint's decimals, at `10^-18`.
///
/// SPL balances arrive as an integer and a decimal count, never as a decimal
/// number, and this is the only place the two are put together. `decimals`
/// above 18 has no representation here and reports `None` — an SPL mint may
/// declare up to 255 and this precision cannot hold the tail of one.
pub fn from_token_amount(raw: u128, decimals: u8) -> Option<u128> {
    if decimals > 18 {
        return None;
    }
    // 10^(18 - decimals), which is exact and always fits.
    let scale = 10u128.checked_pow(u32::from(18 - decimals))?;
    raw.checked_mul(scale)
}

/// The signed difference `to - from`, at `10^-18`.
///
/// Both inputs are unsigned because a price is; the answer is signed because a
/// price change is. `None` only if a value sits above `i128::MAX`, which no
/// reachable price does.
pub fn delta_e18(from_e18: u128, to_e18: u128) -> Option<i128> {
    let from = i128::try_from(from_e18).ok()?;
    let to = i128::try_from(to_e18).ok()?;
    to.checked_sub(from)
}

/// The relative change from `from_e18` to `to_e18`, in basis points.
///
/// Truncated towards zero, which is the direction that never overstates a move.
/// `None` when the baseline is zero — the first tick for a curve has no
/// previous price, and a first tick reported as "up 0 bps" is a lie a ladder
/// would act on.
pub fn delta_bps(from_e18: u128, to_e18: u128) -> Option<i64> {
    if from_e18 == 0 {
        return None;
    }
    let difference = delta_e18(from_e18, to_e18)?;
    let magnitude = difference.unsigned_abs();
    // 10_000 basis points to the whole.
    let scaled = magnitude.checked_mul(10_000)? / from_e18;
    let bounded = i64::try_from(scaled).unwrap_or(i64::MAX);
    Some(if difference < 0 { -bounded } else { bounded })
}

/// `10^-18` down to millionths, rounded to nearest.
///
/// The one sanctioned exit from this precision, and the reason every other
/// function here keeps its `_e18` suffix. Rounding rather than truncating
/// because the destination is a score that gets compared against a threshold,
/// and a truncation biases every such comparison one way.
///
/// [`to_micros`] above is the same conversion under the core's naming; this is
/// the spelling the `_e18` convention asks for, and it delegates rather than
/// repeating the arithmetic.
pub fn e18_to_micros(value_e18: u128) -> u64 {
    to_micros(value_e18)
}

/// Millionths lifted to `10^-18`. Exact in this direction.
///
/// The inverse of [`e18_to_micros`], and [`from_micros`] under the other name.
pub fn micros_to_e18(value_micros: u64) -> u128 {
    from_micros(value_micros)
}

/// A `10^-18` value written out with `decimals` places after the point.
///
/// For the tabular readouts in the UI, which need a fixed column width and a
/// fixed number of places to line up. No float is involved: the whole part and
/// the fraction are formatted as integers and joined. `decimals` above 18 is
/// clamped, since there is nothing beyond 18 to print.
pub fn format_e18(value_e18: u128, decimals: u8) -> String {
    let places = u32::from(decimals.min(18));
    let whole = value_e18 / ONE_E18;
    if places == 0 {
        return whole.to_string();
    }
    // Keep `places` digits of the fraction by dropping the rest.
    let divisor = 10u128.pow(18 - places);
    let fraction = (value_e18 % ONE_E18) / divisor;
    format!("{whole}.{fraction:0width$}", width = places as usize)
}

// ===========================================================================
// the storage unit
// ===========================================================================
//
// The fourth face, and the one that leaves the process. `journal.rs` records
// what a fill was priced at, and a price is a ratio — lamports per token base
// unit — that millionths cannot hold: a pump.fun launch prices around twenty-
// eight millionths, so fifty basis points of slippage is a seventh of a
// millionth, which is zero in the coarser unit for every fill that ever
// mattered.
//
// `Q18` is that number with a `Display`, a `FromStr`, a serde pair and an
// `i64` view, because it has to survive a SQLite INTEGER column and a JSON
// boundary and come back the same. It is the reason the journal has no REAL
// column anywhere in it.

// ---------------------------------------------------------------------------
// the storage unit
// ---------------------------------------------------------------------------

/// One, at `10^-18`, under the name the journal spells it.
///
/// [`ONE`] and [`ONE_E18`] are this same number. Three names because three
/// layers each read better with their own, one constant because there is one
/// unit.
pub const Q18_ONE: u128 = ONE;

/// A non-negative quantity at `10^-18`, in a `u128`.
///
/// The header above says `FIXED_ONE` is private because "callers work in
/// millionths, and a second unit loose in the codebase is how the two get
/// mixed". This is the exception that keeps that rule rather than breaking it.
///
/// `journal.rs` has to record what a fill was priced at, and a price is a ratio
/// — lamports per token base unit — that millionths do not have the resolution
/// for. A pump.fun launch prices around `2.8 x 10^-5` lamports per base unit,
/// which is twenty-eight millionths: two significant figures for the number the
/// whole book is denominated in. Slippage makes it worse, because slippage is
/// the *difference* between two of those, and fifty basis points of twenty-eight
/// millionths is a seventh of a millionth — zero, in the coarser unit, for every
/// fill that ever mattered. So the journal needs the finer one, and the choice
/// was between exporting the bare constant and exporting a type.
///
/// A type, because the mixing the header warns about is a mixing of *numbers*.
/// A `Q18` is not a `u128`: it does not add to one, does not compare to one,
/// and every way in is named for what it converts from. There is no way to
/// spell "this millionth is a `Q18`" by accident, which a bare `10^18` sitting
/// in scope next to `MICROS` would make easy. [`Q18::to_micros_floor`] and
/// [`Q18::from_micros`] are the crossing, and they are spelled out at the call
/// site where somebody can see the unit change happen.
///
/// **Nothing here rounds silently.** Every constructor that can lose a digit
/// says so in its name, and every conversion that can overflow returns
/// `Option`. That is `mul_div_floor`'s contract and it is here for the same
/// reason: a price that saturated quietly is a row that reads as a real number
/// somebody computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct Q18(u128);

impl Q18 {
    pub const ZERO: Q18 = Q18(0);
    pub const ONE: Q18 = Q18(Q18_ONE);

    /// The raw `10^-18` count, for arithmetic this type does not offer.
    pub const fn raw(self) -> u128 {
        self.0
    }

    /// Wraps a raw `10^-18` count. For deserialisation and for tests; callers
    /// with a real quantity in hand want one of the conversions below.
    pub const fn from_raw(raw: u128) -> Self {
        Q18(raw)
    }

    /// A whole number of something. `None` past what a `u128` can scale, which
    /// no `u64` reaches — `u64::MAX * 10^18` is `1.8 x 10^37` against a ceiling
    /// of `3.4 x 10^38` — and is checked anyway because the ceiling is not
    /// obvious from the call site.
    pub const fn from_integer(units: u64) -> Option<Self> {
        match (units as u128).checked_mul(Q18_ONE) {
            Some(raw) => Some(Q18(raw)),
            None => None,
        }
    }

    /// `numerator / denominator`, floored to the last `10^-18`.
    ///
    /// The price of a fill: lamports over token base units. `None` on a zero
    /// denominator, which is a fill of nothing and not a price of infinity, and
    /// `None` on a numerator too large to scale.
    ///
    /// `mul_div_floor` saturates rather than wrapping, which would turn an
    /// out-of-range price into the largest one instead of into an error. The
    /// `checked_mul` above it is what makes reusing it safe here.
    pub fn ratio_floor(numerator: u128, denominator: u128) -> Option<Self> {
        if denominator == 0 {
            return None;
        }
        numerator.checked_mul(Q18_ONE)?;
        Some(Q18(mul_div_floor(numerator, Q18_ONE, denominator)))
    }

    /// `ratio_floor`, rounded to nearest instead. For a quantity that is
    /// averaged rather than compared, where always truncating would drift.
    pub fn ratio_round(numerator: u128, denominator: u128) -> Option<Self> {
        if denominator == 0 {
            return None;
        }
        numerator.checked_mul(Q18_ONE)?;
        Some(Q18(mul_div_round(numerator, Q18_ONE, denominator)))
    }

    pub const fn checked_add(self, other: Q18) -> Option<Q18> {
        match self.0.checked_add(other.0) {
            Some(raw) => Some(Q18(raw)),
            None => None,
        }
    }

    /// Saturating at zero rather than `None`, because every caller is asking
    /// "how much bigger is this than that" and a negative answer to that is
    /// none at all. [`Q18::abs_diff`] is the one to reach for when the sign
    /// matters.
    pub const fn saturating_sub(self, other: Q18) -> Q18 {
        Q18(self.0.saturating_sub(other.0))
    }

    pub const fn abs_diff(self, other: Q18) -> Q18 {
        Q18(self.0.abs_diff(other.0))
    }

    /// This many of something. `None` on overflow.
    pub const fn checked_mul_integer(self, units: u64) -> Option<Q18> {
        match self.0.checked_mul(units as u128) {
            Some(raw) => Some(Q18(raw)),
            None => None,
        }
    }

    /// How far below `reference` this sits, in basis points of `reference`,
    /// floored. Zero when this is at or above it.
    ///
    /// The shape every slippage bound in the codebase is written in: a bound is
    /// `u16` basis points, and comparing a price to one has to end in the same
    /// unit or the comparison is not the one the policy meant.
    pub fn shortfall_bps_floor(self, reference: Q18) -> u64 {
        if reference.0 == 0 || self.0 >= reference.0 {
            return 0;
        }
        mul_div_floor(reference.0 - self.0, 10_000, reference.0) as u64
    }

    /// Down into the strategy module's unit. Floored, and named for it.
    pub const fn to_micros_floor(self) -> u64 {
        (self.0 / 1_000_000_000_000) as u64
    }

    /// Up from the strategy module's unit. Exact — every millionth is a whole
    /// number of `10^-18`s.
    pub const fn from_micros(micros: u64) -> Self {
        Q18(micros as u128 * 1_000_000_000_000)
    }

    /// The raw count as the `i64` SQLite stores.
    ///
    /// `None` past `i64::MAX`, which is `9.22` at this scale. That is not a
    /// price any token on either venue this build trades reaches — it is nine
    /// lamports for one base unit of a six-decimal token, or nine million SOL
    /// for one whole token — and the day something does reach it, a refused
    /// insert is the right outcome. A saturated one would be a lie in the
    /// column the journal exists to be trusted about.
    pub fn to_i64_raw(self) -> Option<i64> {
        i64::try_from(self.0).ok()
    }

    /// Reads back what [`Q18::to_i64_raw`] wrote. `None` on a negative, which
    /// nothing in this build can have written.
    pub fn from_i64_raw(raw: i64) -> Option<Self> {
        u128::try_from(raw).ok().map(Q18)
    }
}

impl fmt::Display for Q18 {
    /// The decimal, with no trailing zeros and no exponent.
    ///
    /// This is the form that crosses IPC. It is a string rather than a number
    /// because JavaScript has one numeric type and it is a `f64`: `3.0e-5`
    /// survives that trip and the eighteenth digit of it does not, and the
    /// whole point of storing the digit was to be able to show it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.0 / Q18_ONE;
        let fraction = self.0 % Q18_ONE;
        if fraction == 0 {
            return write!(f, "{whole}");
        }
        let digits = format!("{fraction:018}");
        write!(f, "{whole}.{}", digits.trim_end_matches('0'))
    }
}

/// What a decimal that is not one of these looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Q18ParseError {
    /// Not digits, or more than one point, or empty.
    NotADecimal,
    /// More than eighteen digits after the point. Refused rather than
    /// truncated: the caller wrote a precision this cannot hold, and quietly
    /// dropping the digits it did not ask to lose is how a price arrives wrong.
    TooPrecise,
    /// A whole part past what `u128` can scale.
    TooLarge,
}

impl fmt::Display for Q18ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Q18ParseError::NotADecimal => "not a decimal",
            Q18ParseError::TooPrecise => "more than eighteen digits after the point",
            Q18ParseError::TooLarge => "too large to hold at 10^-18",
        })
    }
}

impl std::str::FromStr for Q18 {
    type Err = Q18ParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (whole_text, fraction_text) = match text.split_once('.') {
            Some((whole, fraction)) => (whole, fraction),
            None => (text, ""),
        };
        if whole_text.is_empty() && fraction_text.is_empty() {
            return Err(Q18ParseError::NotADecimal);
        }
        if !whole_text.bytes().all(|b| b.is_ascii_digit())
            || !fraction_text.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(Q18ParseError::NotADecimal);
        }
        if fraction_text.len() > 18 {
            return Err(Q18ParseError::TooPrecise);
        }

        let whole: u128 = if whole_text.is_empty() {
            0
        } else {
            whole_text.parse().map_err(|_| Q18ParseError::TooLarge)?
        };
        let scaled = whole.checked_mul(Q18_ONE).ok_or(Q18ParseError::TooLarge)?;

        let mut fraction: u128 = 0;
        for index in 0..18 {
            let digit = fraction_text
                .as_bytes()
                .get(index)
                .map_or(0, |b| u128::from(b - b'0'));
            fraction = fraction * 10 + digit;
        }

        scaled
            .checked_add(fraction)
            .map(Q18)
            .ok_or(Q18ParseError::TooLarge)
    }
}

impl Serialize for Q18 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Q18 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How far a fixed-point log is allowed to sit from the real one. The series
    /// truncates about twenty times at 10^-18, so a tolerance of 10^-15 is three
    /// orders of magnitude of slack and still tight enough to catch a wrong
    /// reduction, a wrong constant, or an off-by-one in the exponent.
    const LOG_TOLERANCE: f64 = 1e-15;

    fn as_f64(fixed: u128) -> f64 {
        fixed as f64 / FIXED_ONE as f64
    }

    // -- the 10^-18 layer ---------------------------------------------------

    #[test]
    fn a_ratio_is_exact_when_it_divides_evenly() {
        assert_eq!(ratio_e18(1, 2), Some(ONE_E18 / 2));
        assert_eq!(ratio_e18(3, 4), Some(750_000_000_000_000_000));
        assert_eq!(ratio_e18(7, 7), Some(ONE_E18));
        assert_eq!(ratio_e18(0, 5), Some(0));
    }

    #[test]
    fn a_ratio_floors_rather_than_rounding() {
        // 1/3 at 10^-18 is a repeating decimal, and it truncates. Rounding up
        // would let a price tick over a threshold it did not reach.
        assert_eq!(ratio_e18(1, 3), Some(333_333_333_333_333_333));
        assert_eq!(ratio_e18(2, 3), Some(666_666_666_666_666_666));
    }

    #[test]
    fn a_ratio_by_zero_is_absent_rather_than_zero() {
        // Zero would be a price, and a price is exactly what there is not.
        assert_eq!(ratio_e18(5, 0), None);
    }

    #[test]
    fn a_ratio_survives_a_numerator_that_could_not_be_scaled_directly() {
        // `u64::MAX` lamports over a single unit. Scaling the numerator first
        // would be 1.8e19 x 1e18 = 1.8e37, which fits — but the whole/remainder
        // split is what keeps that true, and this is the case that proves it.
        let huge = u128::from(u64::MAX);
        assert_eq!(ratio_e18(huge, 1), Some(huge * ONE_E18));
        // A numerator past the point where a direct scale would overflow.
        assert_eq!(
            ratio_e18(u128::MAX, 1),
            None,
            "an unrepresentable answer says so"
        );
        assert_eq!(ratio_e18(u128::MAX, u128::MAX), Some(ONE_E18));
    }

    #[test]
    fn multiplication_and_division_undo_each_other() {
        let price = ratio_e18(30_000_000_000, 1_073_000_000_000_000).unwrap();
        let two = 2 * ONE_E18;
        let doubled = mul_e18(price, two).unwrap();
        assert_eq!(doubled, 2 * price);
        assert_eq!(div_e18(doubled, two), Some(price));
    }

    #[test]
    fn a_token_amount_is_scaled_out_of_its_own_decimals() {
        // One whole token, at three different decimal counts, is the same
        // quantity once normalised.
        assert_eq!(from_token_amount(1_000_000, 6), Some(ONE_E18));
        assert_eq!(from_token_amount(1_000_000_000, 9), Some(ONE_E18));
        assert_eq!(from_token_amount(1, 0), Some(ONE_E18));
        // And a raw integer is the same thing at zero decimals.
        assert_eq!(from_integer(1), from_token_amount(1, 0));
    }

    #[test]
    fn a_mint_with_more_decimals_than_this_precision_is_refused() {
        // SPL allows up to 255 decimals. Past 18 there is nothing here to hold
        // the tail, and a silent truncation would be a wrong balance.
        assert_eq!(from_token_amount(1, 18), Some(1));
        assert_eq!(from_token_amount(1, 19), None);
        assert_eq!(from_token_amount(1, 255), None);
    }

    #[test]
    fn a_delta_carries_its_sign() {
        assert_eq!(delta_e18(100, 250), Some(150));
        assert_eq!(delta_e18(250, 100), Some(-150));
        assert_eq!(delta_e18(100, 100), Some(0));
    }

    #[test]
    fn a_relative_delta_is_basis_points_of_the_baseline() {
        assert_eq!(delta_bps(ONE_E18, 2 * ONE_E18), Some(10_000));
        assert_eq!(delta_bps(2 * ONE_E18, ONE_E18), Some(-5_000));
        assert_eq!(delta_bps(ONE_E18, ONE_E18), Some(0));
        // One basis point up, exactly.
        assert_eq!(delta_bps(10_000, 10_001), Some(1));
    }

    #[test]
    fn a_relative_delta_from_nothing_is_absent_not_zero() {
        // The first observation of a curve has no baseline. Reporting "0 bps"
        // would be a move a ladder could act on.
        assert_eq!(delta_bps(0, ONE_E18), None);
    }

    #[test]
    fn a_relative_delta_truncates_towards_zero_in_both_directions() {
        // 1.99 bps up and 1.99 bps down both report 1, not 2. Truncation that
        // followed the sign would overstate one side.
        assert_eq!(delta_bps(1_000_000, 1_000_199), Some(1));
        assert_eq!(delta_bps(1_000_000, 999_801), Some(-1));
    }

    #[test]
    fn the_bridge_to_millionths_rounds_to_nearest() {
        assert_eq!(e18_to_micros(ONE_E18), MICROS);
        assert_eq!(e18_to_micros(0), 0);
        // Half a millionth rounds up; just under it rounds down.
        assert_eq!(e18_to_micros(500_000_000_000), 1);
        assert_eq!(e18_to_micros(499_999_999_999), 0);
    }

    #[test]
    fn millionths_survive_a_round_trip_through_the_finer_unit() {
        for micros in [0u64, 1, 7, 999_999, MICROS, 3 * MICROS + 17] {
            assert_eq!(e18_to_micros(micros_to_e18(micros)), micros);
        }
    }

    #[test]
    fn formatting_produces_a_fixed_width_column_with_no_float_in_sight() {
        assert_eq!(format_e18(ONE_E18, 8), "1.00000000");
        assert_eq!(format_e18(0, 8), "0.00000000");
        assert_eq!(format_e18(3 * ONE_E18 / 2, 4), "1.5000");
        assert_eq!(format_e18(ONE_E18 - 1, 18), "0.999999999999999999");
        assert_eq!(format_e18(ONE_E18 + ONE_E18 / 2, 0), "1");
        // Places past the precision are clamped rather than padded with lies.
        assert_eq!(format_e18(1, 18), "0.000000000000000001");
        assert_eq!(format_e18(1, 200), format_e18(1, 18));
    }

    #[test]
    fn a_formatted_column_lines_up_across_wildly_different_magnitudes() {
        // What the readout actually needs: same width, whatever the number.
        let rows = [ONE_E18, ONE_E18 / 1_000, 27_958_993_476_234, 0];
        let widths: Vec<usize> = rows.iter().map(|&v| format_e18(v, 18).len()).collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "ragged column: {widths:?}"
        );
    }

    /// How far the fixed-point `exp` is allowed to sit from the real one. The
    /// reduction and the squarings put the achievable accuracy near 10^-13; a
    /// tolerance of 10^-12 is a decade of slack over that and still tight
    /// enough to catch a wrong term, a wrong threshold or a lost halving.
    const EXP_TOLERANCE: f64 = 1e-12;

    fn fixed_as_f64(value: Fixed) -> f64 {
        value.0 as f64 / FIXED_ONE as f64
    }

    #[test]
    fn the_log_of_one_and_below_is_zero() {
        assert_eq!(ln_fixed(0), 0);
        assert_eq!(ln_fixed(1), 0);
    }

    #[test]
    fn the_log_of_a_power_of_two_is_that_many_log_twos() {
        assert_eq!(ln_fixed(2), LN2_FIXED);
        assert_eq!(ln_fixed(4), 2 * LN2_FIXED);
        assert_eq!(ln_fixed(1 << 32), 32 * LN2_FIXED);
    }

    #[test]
    fn the_log_agrees_with_the_real_one_across_the_whole_range() {
        let cases = [
            2u64,
            3,
            5,
            7,
            10,
            50,
            99,
            1_000,
            65_535,
            1_000_000,
            1_000_000_007,
            1_000_000_000_000,
            u64::MAX / 3,
            u64::MAX,
        ];
        for x in cases {
            let got = as_f64(ln_fixed(x));
            let want = (x as f64).ln();
            assert!(
                (got - want).abs() < LOG_TOLERANCE * want.max(1.0),
                "ln({x}): got {got}, want {want}",
            );
        }
    }

    #[test]
    fn the_log_never_decreases() {
        let mut previous = 0u128;
        for x in 1u64..2_000 {
            let current = ln_fixed(x);
            assert!(current >= previous, "ln({x}) went backwards");
            previous = current;
        }
    }

    #[test]
    fn one_group_holding_everything_has_no_entropy() {
        assert_eq!(normalised_entropy_micros(&[8], 8), 0);
    }

    #[test]
    fn every_item_alone_is_full_entropy() {
        assert_eq!(normalised_entropy_micros(&[1, 1, 1, 1], 4), MICROS);
    }

    #[test]
    fn a_population_too_small_to_read_reports_one() {
        assert_eq!(normalised_entropy_micros(&[], 0), MICROS);
        assert_eq!(normalised_entropy_micros(&[1], 1), MICROS);
    }

    #[test]
    fn an_even_split_lands_where_the_real_entropy_does() {
        // Four items in two groups of two: H = ln 2, ln n = ln 4, so a half.
        assert_eq!(normalised_entropy_micros(&[2, 2], 4), 500_000);
    }

    #[test]
    fn entropy_matches_the_float_formula_it_replaces() {
        let partitions: [(&[usize], usize); 5] = [
            (&[3, 1], 4),
            (&[5, 3, 2], 10),
            (&[1, 1, 1, 7], 10),
            (&[2, 2, 2, 2, 2], 10),
            (&[47, 1, 1, 1], 50),
        ];
        for (groups, total) in partitions {
            let want: f64 = -groups
                .iter()
                .map(|&g| {
                    let p = g as f64 / total as f64;
                    p * p.ln()
                })
                .sum::<f64>()
                / (total as f64).ln();
            let got = normalised_entropy_micros(groups, total) as f64 / MICROS as f64;
            assert!(
                (got - want).abs() < 1e-6,
                "{groups:?}/{total}: got {got}, want {want}",
            );
        }
    }

    #[test]
    fn weighted_entropy_needs_two_edges_to_mean_anything() {
        assert_eq!(weighted_entropy_micros(&[]), None);
        assert_eq!(weighted_entropy_micros(&[5]), None);
        // A zero weight is skipped, not counted, so this is still one edge.
        assert_eq!(weighted_entropy_micros(&[5, 0]), None);
    }

    #[test]
    fn equal_edges_are_maximum_entropy() {
        assert_eq!(weighted_entropy_micros(&[7, 7, 7, 7]), Some(MICROS));
    }

    #[test]
    fn a_star_of_volume_is_near_zero_entropy() {
        let star = [1_000_000_000u64, 1, 1, 1, 1];
        let entropy = weighted_entropy_micros(&star).expect("five edges");
        assert!(
            entropy < 10_000,
            "a star should be near zero, got {entropy}"
        );
    }

    #[test]
    fn the_published_entropy_vector_lands_where_the_spec_says() {
        // `RISK_AND_SYBIL_SPEC.md` §14: shares of 0.9 and 0.1 give H = 0.3251
        // and H_norm = 0.4690. The weights are the shares; the normaliser is
        // ln(2), which is what two edges gives.
        let normalised = weighted_entropy_micros(&[90, 10]).expect("two weights");
        assert_eq!((normalised + 50) / 100, 4_690);

        // The same section's `[0.5, 0.5]` and four-equal cases are exactly one.
        assert_eq!(weighted_entropy_micros(&[50, 50]), Some(MICROS));
        assert_eq!(weighted_entropy_micros(&[25, 25, 25, 25]), Some(MICROS));
    }

    #[test]
    fn weighted_entropy_matches_the_float_formula() {
        let cases: [&[u64]; 4] = [
            &[1, 1, 2],
            &[100, 200, 300, 400],
            &[1_000_000_000, 500_000_000, 250_000_000],
            &[9, 9, 9, 1],
        ];
        for weights in cases {
            let total: f64 = weights.iter().map(|&w| w as f64).sum();
            let want: f64 = -weights
                .iter()
                .map(|&w| {
                    let p = w as f64 / total;
                    p * p.ln()
                })
                .sum::<f64>()
                / (weights.len() as f64).ln();
            let got =
                weighted_entropy_micros(weights).expect("enough edges") as f64 / MICROS as f64;
            assert!(
                (got - want).abs() < 1e-6,
                "{weights:?}: got {got}, want {want}",
            );
        }
    }

    #[test]
    fn nothing_here_leaves_the_unit_interval() {
        for total in 2usize..40 {
            for first in 1..total {
                let groups = [first, total - first];
                let entropy = normalised_entropy_micros(&groups, total);
                assert!(entropy <= MICROS, "{groups:?} left the interval");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Q18
    // -----------------------------------------------------------------------

    #[test]
    fn a_whole_number_scales_and_comes_back() {
        assert_eq!(Q18::from_integer(0), Some(Q18::ZERO));
        assert_eq!(Q18::from_integer(1), Some(Q18::ONE));
        assert_eq!(Q18::from_integer(7).unwrap().raw(), 7 * Q18_ONE);
        // The largest u64 still scales: the check is not load-bearing today and
        // is here so it stays that way if the type ever widens.
        assert!(Q18::from_integer(u64::MAX).is_some());
    }

    #[test]
    fn a_price_is_the_ratio_it_was_built_from() {
        // 30 lamports for 10^6 base units: 3 x 10^-5, exactly.
        let price = Q18::ratio_floor(30, 1_000_000).expect("a real fill");
        assert_eq!(price.raw(), 30_000_000_000_000);
        assert_eq!(price.to_string(), "0.00003");
    }

    #[test]
    fn a_fill_of_nothing_has_no_price_rather_than_an_infinite_one() {
        assert_eq!(Q18::ratio_floor(1_000, 0), None);
        assert_eq!(Q18::ratio_round(1_000, 0), None);
    }

    #[test]
    fn a_numerator_too_large_to_scale_is_refused_rather_than_saturated() {
        // `mul_div_floor` would saturate this into the largest u128 and divide
        // it, which is a number nobody computed. The constructor says no.
        let past_it = u128::MAX / 1_000;
        assert_eq!(Q18::ratio_floor(past_it, 2), None);
    }

    #[test]
    fn the_two_roundings_differ_only_in_the_last_digit() {
        // 2/3 at 10^-18 is 0.666...6 with a 6 falling off the end, so rounding
        // lands one higher. 1/3 is the case where they agree, and both are
        // here so a change that collapsed the two would be noticed.
        assert_eq!(
            Q18::ratio_round(2, 3).unwrap().raw() - Q18::ratio_floor(2, 3).unwrap().raw(),
            1,
        );
        assert_eq!(Q18::ratio_round(1, 3), Q18::ratio_floor(1, 3));
    }

    #[test]
    fn a_shortfall_is_measured_in_the_unit_the_policy_is_written_in() {
        let quoted = Q18::ratio_floor(1_000, 1).expect("a quote");
        let filled = Q18::ratio_floor(950, 1).expect("a fill");
        assert_eq!(filled.shortfall_bps_floor(quoted), 500);
        // A fill at or above the quote is not slippage.
        assert_eq!(quoted.shortfall_bps_floor(filled), 0);
        assert_eq!(quoted.shortfall_bps_floor(quoted), 0);
        // Nothing to be short of.
        assert_eq!(filled.shortfall_bps_floor(Q18::ZERO), 0);
    }

    #[test]
    fn the_crossing_into_millionths_is_the_one_the_strategy_module_uses() {
        assert_eq!(Q18::from_micros(MICROS), Q18::ONE);
        assert_eq!(Q18::ONE.to_micros_floor(), MICROS);
        // A launch price and the same price fifty basis points worse are one
        // number in millionths and two here. That collapse is why the journal
        // does not store prices in the strategy module's unit.
        let quoted = Q18::ratio_floor(28_400, 1_000_000_000).expect("a quote");
        let filled = Q18::ratio_floor(28_258, 1_000_000_000).expect("a fill");
        assert_eq!(filled.shortfall_bps_floor(quoted), 50);
        assert_eq!(quoted.to_micros_floor(), filled.to_micros_floor());
        assert_ne!(quoted, filled);
    }

    #[test]
    fn the_sqlite_column_refuses_what_it_cannot_hold() {
        let fits = Q18::from_raw(i64::MAX as u128);
        assert_eq!(fits.to_i64_raw(), Some(i64::MAX));
        assert_eq!(Q18::from_i64_raw(i64::MAX), Some(fits));
        assert_eq!(Q18::from_raw(i64::MAX as u128 + 1).to_i64_raw(), None);
        assert_eq!(Q18::from_i64_raw(-1), None);
    }

    #[test]
    fn the_decimal_survives_the_round_trip_it_crosses_ipc_as() {
        let cases = [
            Q18::ZERO,
            Q18::ONE,
            Q18::from_raw(1),
            Q18::from_raw(999_999_999_999_999_999),
            Q18::ratio_floor(30, 1_000_000).unwrap(),
            Q18::ratio_floor(1, 3).unwrap(),
            Q18::from_integer(1_000_000_000).unwrap(),
        ];
        for value in cases {
            let text = value.to_string();
            let back: Q18 = text.parse().expect("its own output parses");
            assert_eq!(back, value, "{text}");
            let json = serde_json::to_string(&value).expect("serialises");
            assert!(json.starts_with('"'), "{json} is a string, not a number");
            let decoded: Q18 = serde_json::from_str(&json).expect("deserialises");
            assert_eq!(decoded, value);
        }
    }

    #[test]
    fn a_decimal_this_cannot_hold_is_refused_rather_than_truncated() {
        assert_eq!("".parse::<Q18>(), Err(Q18ParseError::NotADecimal));
        assert_eq!("1.2.3".parse::<Q18>(), Err(Q18ParseError::NotADecimal));
        assert_eq!("-1".parse::<Q18>(), Err(Q18ParseError::NotADecimal));
        assert_eq!("1e-5".parse::<Q18>(), Err(Q18ParseError::NotADecimal));
        // Nineteen digits after the point is a precision this cannot keep.
        assert_eq!(
            "0.1234567890123456789".parse::<Q18>(),
            Err(Q18ParseError::TooPrecise),
        );
        // Eighteen is exactly what it can.
        assert_eq!(
            "0.123456789012345678".parse::<Q18>().map(|q| q.raw()),
            Ok(123_456_789_012_345_678),
        );
    }

    #[test]
    fn the_short_forms_parse_to_the_same_place() {
        assert_eq!("1".parse::<Q18>(), Ok(Q18::ONE));
        assert_eq!("1.".parse::<Q18>(), Ok(Q18::ONE));
        assert_eq!(".5".parse::<Q18>(), Ok(Q18::from_raw(Q18_ONE / 2)));
        assert_eq!("0.50".parse::<Q18>(), Ok(Q18::from_raw(Q18_ONE / 2)));
    }

    #[test]
    fn the_arithmetic_says_no_rather_than_wrapping() {
        let top = Q18::from_raw(u128::MAX);
        assert_eq!(top.checked_add(Q18::from_raw(1)), None);
        assert_eq!(top.checked_mul_integer(2), None);
        assert_eq!(Q18::ONE.checked_mul_integer(3).unwrap().raw(), 3 * Q18_ONE);
        // Subtraction floors at zero, and `abs_diff` is how the other direction
        // is asked for.
        assert_eq!(Q18::ONE.saturating_sub(top), Q18::ZERO);
        assert_eq!(Q18::ONE.abs_diff(Q18::ZERO), Q18::ONE);
        assert_eq!(Q18::ZERO.abs_diff(Q18::ONE), Q18::ONE);
    }

    // --- the fixed-point core ---------------------------------------------

    #[test]
    fn one_is_the_multiplicative_identity() {
        for value in [0u128, 1, ONE / 3, ONE, 7 * ONE, u64::MAX as u128] {
            assert_eq!(mul(value, ONE), value, "{value} x 1");
            assert_eq!(mul(ONE, value), value, "1 x {value}");
            assert_eq!(div(value, ONE), value, "{value} / 1");
        }
    }

    #[test]
    fn multiplication_and_division_are_inverses_on_exact_values() {
        // Halves, quarters and eighths divide 10^18 exactly, so the round trip
        // is exact rather than merely close.
        let half = ONE / 2;
        assert_eq!(mul(half, half), ONE / 4);
        assert_eq!(div(ONE / 4, half), half);
        assert_eq!(mul(ONE / 8, 8 * ONE), ONE);
    }

    #[test]
    fn dividing_by_nothing_is_a_share_of_nothing() {
        assert_eq!(div(ONE, 0), 0);
        assert_eq!(ratio(5, 0), 0);
    }

    #[test]
    fn a_product_that_would_overflow_clamps_instead_of_wrapping() {
        // The whole point of saturating: a wrapped product would come back
        // small and be paid, where a clamped one is caught by a bound.
        assert_eq!(mul(u128::MAX, u128::MAX), u128::MAX / ONE);
        assert_eq!(div(u128::MAX, 1), u128::MAX);
    }

    #[test]
    fn a_power_of_zero_is_one_and_a_power_of_one_is_itself() {
        let decay = from_micros(850_000);
        assert_eq!(pow(decay, 0), ONE);
        assert_eq!(pow(decay, 1), decay);
        assert_eq!(pow(ONE, 4_000), ONE, "one to any power is one");
    }

    #[test]
    fn powers_of_a_half_are_exact() {
        let half = ONE / 2;
        assert_eq!(pow(half, 1), ONE / 2);
        assert_eq!(pow(half, 2), ONE / 4);
        assert_eq!(pow(half, 3), ONE / 8);
        assert_eq!(pow(half, 10), ONE / 1024);
    }

    #[test]
    fn a_decay_never_climbs_and_reaches_nothing() {
        let decay = from_micros(850_000);
        let mut previous = ONE;
        for exponent in 1u32..200 {
            let current = pow(decay, exponent);
            assert!(current <= previous, "0.85^{exponent} went up");
            previous = current;
        }
        assert_eq!(pow(decay, 4_000), 0, "a decay run far enough is nothing");
    }

    #[test]
    fn the_squaring_ladder_tracks_repeated_multiplication_to_the_last_digit() {
        // `pow` is O(log n) and this is O(n), and the two are *not* required to
        // be bit-identical: both truncate, and they truncate at different
        // points, so they drift apart by an ulp or two. That is the documented
        // behaviour rather than a defect — what matters is that `pow` is the
        // one definition every caller uses, so every machine gets the same
        // answer from it.
        //
        // The bound is one ulp per multiply the naive form does, which is the
        // most the two can diverge and still be computing the same power. A
        // ladder with a wrong bit in it misses by orders of magnitude and this
        // catches it in the first few exponents.
        let decay = from_micros(850_000);
        for exponent in 0u32..40 {
            let mut naive = ONE;
            for _ in 0..exponent {
                naive = mul(naive, decay);
            }
            let ladder = pow(decay, exponent);
            let drift = ladder.abs_diff(naive);
            assert!(
                drift <= u128::from(exponent).max(1),
                "0.85^{exponent}: ladder {ladder}, naive {naive}, {drift} apart",
            );
        }
    }

    #[test]
    fn a_power_is_the_same_answer_every_time_it_is_asked_for() {
        // The property the ladder actually owes its callers, and the one a
        // replay depends on: `pow` is a pure function, so a floor priced from
        // the same window prices the same lamport on every machine and every
        // run. Bit-identical to itself is the whole requirement.
        let decay = from_micros(850_000);
        for exponent in 0u32..64 {
            let first = pow(decay, exponent);
            for _ in 0..4 {
                assert_eq!(pow(decay, exponent), first, "0.85^{exponent} moved");
            }
        }
    }

    #[test]
    fn a_ratio_is_the_share_it_names() {
        assert_eq!(ratio(1, 2), ONE / 2);
        assert_eq!(ratio(1, 4), ONE / 4);
        assert_eq!(ratio(3, 3), ONE);
        assert_eq!(ratio(0, 9), 0);
        assert_eq!(ratio(u64::MAX, u64::MAX), ONE);
    }

    #[test]
    fn millionths_survive_the_round_trip() {
        for micros in [0u64, 1, 500_000, 850_000, MICROS, 2 * MICROS, 1_234_567] {
            assert_eq!(to_micros(from_micros(micros)), micros, "{micros}");
        }
    }

    #[test]
    fn reporting_rounds_to_nearest_rather_than_down() {
        // A hair under a millionth reports as that millionth, not the one below
        // it: the saturation case the doc names.
        assert_eq!(to_micros(ONE - 1), MICROS);
        assert_eq!(to_micros(ONE / 2 - 1), 500_000);
        // And a hair over still reports the same millionth.
        assert_eq!(to_micros(ONE + 1), MICROS);
    }

    #[test]
    fn scaling_rounds_to_nearest_and_saturates() {
        assert_eq!(scale(100_000, ONE), 100_000, "one leaves it alone");
        assert_eq!(scale(100_000, ONE / 2), 50_000);
        assert_eq!(scale(100_000, 3 * ONE / 2), 150_000);
        assert_eq!(scale(0, 5 * ONE), 0);
        // 1 x 1.5 is 1.5, which rounds to 2 rather than truncating to 1.
        assert_eq!(scale(1, 3 * ONE / 2), 2);
        assert_eq!(scale(u64::MAX, 2 * ONE), u64::MAX, "clamped, not wrapped");
    }

    #[test]
    fn a_multiplier_of_one_moves_no_money() {
        // The property the tip floor leans on: an unfitted window multiplies by
        // exactly one and the lamport count is untouched.
        for lamports in [1u64, 10_000, 25_000, 10_000_000, u32::MAX as u64] {
            assert_eq!(scale(lamports, ONE), lamports, "{lamports}");
        }
    }

    #[test]
    fn a_share_of_nothing_is_zero_and_not_one() {
        // The denominator convention: no population means no share, which is
        // the opposite of a full share.
        assert_eq!(Fixed::from_ratio(0, 0), Fixed::ZERO);
        assert_eq!(Fixed::from_ratio(5, 0), Fixed::ZERO);
        assert_eq!(Fixed::ratio_unclamped(5, 0), Fixed::ZERO);
    }

    #[test]
    fn multiplication_composes_without_rounding_between_the_links() {
        // Three factors that each round to the same millionth but whose product
        // does not: 0.999999_4 cubed is 0.999998_2, and a caller that rounded
        // each factor to a millionth first would get 0.999999 cubed instead.
        let factor = Fixed::from_ratio(9_999_994, 10_000_000);
        let product = factor.saturating_mul(factor).saturating_mul(factor);
        assert_eq!(product.to_micros(), 999_998);
        assert_eq!(Fixed::ONE.saturating_mul(Fixed::ONE), Fixed::ONE);
        assert_eq!(Fixed::ZERO.saturating_mul(Fixed::ONE), Fixed::ZERO);
    }

    #[test]
    fn nothing_above_one_survives_the_constructors() {
        assert_eq!(Fixed::from_micros(u64::MAX), Fixed::ONE);
        assert_eq!(Fixed::from_bps(u64::MAX), Fixed::ONE);
        assert_eq!(Fixed::from_ratio(9, 4), Fixed::ONE);
        assert_eq!(Fixed::ONE.saturating_add(Fixed::ONE), Fixed::ONE);
    }

    #[test]
    fn the_exponent_ratio_is_allowed_past_one() {
        // Three half-lives is an argument of 3, which the clamped constructor
        // would have flattened to 1 and turned every old edge into one number.
        let three = Fixed::ratio_unclamped(3, 1);
        assert!(three > Fixed::ONE);
        assert!(exp_neg(three) < exp_neg(Fixed::ONE));
    }

    #[test]
    fn the_exponential_agrees_with_its_millionth_precision_sibling() {
        // Both are the same series; the wide one is the one that keeps the
        // digits. They must not disagree about the millionth they share.
        for x_micros in [0u64, 1, 1_000, 100_000, 693_147, 1_000_000, 5_000_000] {
            let wide = exp_neg(Fixed::ratio_unclamped(
                u128::from(x_micros),
                u128::from(MICROS),
            ))
            .to_micros();
            let narrow = crate::backtest::exp_neg_micros(x_micros);
            assert!(
                wide.abs_diff(narrow) <= 1,
                "exp(-{x_micros}/1e6): wide {wide}, narrow {narrow}",
            );
        }
    }

    #[test]
    fn the_exponential_agrees_with_the_real_one_across_the_whole_range() {
        // Numerators over a denominator of 1000, so the cases land on and
        // between the halving thresholds rather than only on round arguments.
        let cases = [
            1u128, 3, 39, 100, 693, 1_000, 2_500, 5_000, 10_000, 20_000, 41_000,
        ];
        for numerator in cases {
            let x = Fixed::ratio_unclamped(numerator, 1_000);
            let got = fixed_as_f64(exp_neg(x));
            let want = (-(numerator as f64) / 1_000.0).exp();
            assert!(
                (got - want).abs() < EXP_TOLERANCE.max(want * EXP_TOLERANCE),
                "exp(-{numerator}/1000): got {got}, want {want}",
            );
        }
    }

    #[test]
    fn the_exponential_bottoms_out_where_the_precision_does() {
        // exp(-42) is 5.7e-19, under the last digit this carries.
        assert_eq!(exp_neg(Fixed::ratio_unclamped(42, 1)), Fixed::ZERO);
        assert_eq!(exp_neg(Fixed::ratio_unclamped(1_000, 1)), Fixed::ZERO);
    }

    #[test]
    fn the_exponential_never_increases() {
        let mut previous = Fixed::ONE;
        for numerator in 0u128..3_000 {
            let current = exp_neg(Fixed::ratio_unclamped(numerator, 100));
            assert!(current <= previous, "exp(-{numerator}/100) went up");
            previous = current;
        }
    }

    #[test]
    fn the_exponential_of_zero_is_exactly_one() {
        assert_eq!(exp_neg(Fixed::ZERO), Fixed::ONE);
    }

    #[test]
    fn the_geometric_mean_is_the_square_root_of_the_product() {
        assert_eq!(Fixed::ONE.geometric_mean(Fixed::ONE), Fixed::ONE);
        assert_eq!(Fixed::ZERO.geometric_mean(Fixed::ONE), Fixed::ZERO);
        // sqrt(0.25 x 0.64) = 0.4
        let mean = Fixed::from_micros(250_000).geometric_mean(Fixed::from_micros(640_000));
        assert_eq!(mean.to_micros(), 400_000);
        // The property §3.5 leans on: one half being zero takes the whole score
        // to zero, which an arithmetic mean would not do.
        assert_eq!(
            Fixed::from_micros(1_000_000).geometric_mean(Fixed::ZERO),
            Fixed::ZERO
        );
    }

    #[test]
    fn the_half_life_decay_halves_at_the_half_life() {
        // §3.3's lambda = ln(2)/half_life, so exp(-lambda x half_life) = 1/2 and
        // each further half-life halves it again. This is the identity the
        // tracer's age decay rests on, and LN2 is carried to 10^-18 exactly so
        // that it lands on the half rather than near it.
        let half_life_ms: u128 = 24 * 60 * 60 * 1_000;
        for (multiple, want_micros) in [(1u128, 500_000u64), (2, 250_000), (3, 125_000)] {
            let age = half_life_ms * multiple;
            let exponent = Fixed::LN2.saturating_mul(Fixed::ratio_unclamped(age, half_life_ms));
            assert_eq!(exp_neg(exponent).to_micros(), want_micros);
        }
    }

    #[test]
    fn the_kappa_discount_is_a_share_of_the_value() {
        let value = Fixed::from_micros(800_000);
        assert_eq!(value.scale_bps(2_500).to_micros(), 200_000);
        assert_eq!(value.scale_bps(0), Fixed::ZERO);
        assert_eq!(value.scale_bps(10_000), value);
    }

    #[test]
    fn the_unit_conversions_round_trip() {
        assert_eq!(Fixed::from_micros(0), Fixed::ZERO);
        assert_eq!(Fixed::from_micros(MICROS), Fixed::ONE);
        assert_eq!(Fixed::from_micros(500_000).to_micros(), 500_000);
        assert_eq!(Fixed::from_bps(10_000), Fixed::ONE);
        assert_eq!(Fixed::from_bps(2_500).to_bps(), 2_500);
        // Millionths and basis points are the same value read two ways.
        assert_eq!(Fixed::from_micros(250_000).to_bps(), 2_500);
    }

    #[test]
    fn a_ratio_against_nothing_is_not_a_ratio_of_zero() {
        assert_eq!(ln_ratio_micros(0, 5), None);
        assert_eq!(ln_ratio_micros(5, 0), None);
        assert_eq!(growth_score_micros(0, 5, 16), None);
        assert_eq!(growth_score_micros(5, 0, 16), None);
    }

    #[test]
    fn a_ratio_of_one_is_zero_however_it_is_written() {
        for x in [1u64, 7, 1_000, u64::MAX] {
            assert_eq!(ln_ratio_micros(x, x), Some(0), "ln({x}/{x})");
        }
    }

    #[test]
    fn a_ratio_and_its_reciprocal_are_the_same_distance_apart() {
        for (n, d) in [(2u64, 1u64), (16, 1), (1_000, 7), (u64::MAX, 3)] {
            let up = ln_ratio_micros(n, d).expect("both non-zero");
            let down = ln_ratio_micros(d, n).expect("both non-zero");
            assert_eq!(up, -down, "ln({n}/{d}) against its reciprocal");
        }
    }

    #[test]
    fn the_log_ratio_agrees_with_the_real_one() {
        let cases = [
            (2u64, 1u64),
            (16, 1),
            (1_602, 100),
            (256, 278),
            (1_000_000_000, 3),
            (u64::MAX, 2),
        ];
        for (n, d) in cases {
            let got = ln_ratio_micros(n, d).expect("both non-zero") as f64 / MICROS as f64;
            let want = (n as f64 / d as f64).ln();
            assert!(
                (got - want).abs() < 1e-5,
                "ln({n}/{d}): got {got}, want {want}"
            );
        }
    }

    #[test]
    fn growth_is_a_share_of_the_ruler_it_is_measured_against() {
        // The ruler is a sixteenfold rise, which is what the archived grading
        // measured the accelerating third of its corpus at.
        assert_eq!(growth_score_micros(100, 100, 16), Some(0));
        assert_eq!(growth_score_micros(100, 1_600, 16), Some(MICROS));
        // Four is the square root of sixteen, so on a log ruler it is halfway.
        assert_eq!(growth_score_micros(100, 400, 16), Some(500_000));
        // And a doubling is a quarter of the way, sixteen being two to the four.
        assert_eq!(growth_score_micros(100, 200, 16), Some(250_000));
    }

    #[test]
    fn growth_past_the_ruler_stops_at_the_end_of_it() {
        assert_eq!(growth_score_micros(1, 1_000_000_000, 16), Some(MICROS));
    }

    #[test]
    fn a_counter_that_went_backwards_reports_no_growth_rather_than_a_negative() {
        assert_eq!(growth_score_micros(500, 100, 16), Some(0));
        assert_eq!(ln_ratio_micros(100, 500), Some(-1_609_438));
    }

    #[test]
    fn a_ruler_with_no_length_cannot_measure() {
        assert_eq!(growth_score_micros(100, 400, 0), None);
        assert_eq!(growth_score_micros(100, 400, 1), None);
    }

    #[test]
    fn growth_is_scale_free_which_is_the_whole_point_of_the_log_ruler() {
        // The same fourfold rise, four orders of magnitude apart, is the same
        // score. A ruler in raw views would call the second one four thousand
        // times the story.
        let small = growth_score_micros(10, 40, 16).expect("measurable");
        let large = growth_score_micros(100_000, 400_000, 16).expect("measurable");
        assert_eq!(small, large);
    }

    #[test]
    fn growth_never_leaves_the_unit_interval() {
        for from in [1u64, 3, 97, 10_000, u64::MAX / 4] {
            for factor in [1u64, 2, 5, 17, 1_000] {
                let to = from.saturating_mul(factor);
                let score = growth_score_micros(from, to, 16).expect("measurable");
                assert!(score <= MICROS, "{from} -> {to} left the interval");
            }
        }
    }
}
