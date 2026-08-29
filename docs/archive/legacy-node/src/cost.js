// What a round trip actually costs.
//
// Measured on 10 Aug 2026 and written down in Log.md. Two costs pull in opposite
// directions: the priority tip is a fixed 0.0046 SOL, so it eats a small trade
// alive, while price impact grows with size, so it eats a large one. There is no
// position size that escapes both. The cheapest point is about 3%, which is the
// number every edge in this project has to clear before it means anything.

export const TIP_SOL = 0.0046;

// Price impact in and out, as a percentage, per SOL of position. Fitted to the
// measured table: 0.5 SOL cost 2.6% and 1.0 SOL cost 5.2%.
export const IMPACT_PCT_PER_SOL = 5.2;

/** Total round-trip cost of a position, as a percentage of the position. */
export function roundTripCostPct(sizeSol) {
  const size = Math.max(0.01, Number(sizeSol) || 0.25);
  const tipPct = (TIP_SOL / size) * 100;
  const impactPct = IMPACT_PCT_PER_SOL * size;
  return {
    sizeSol: size,
    tipPct: round(tipPct),
    impactPct: round(impactPct),
    totalPct: round(tipPct + impactPct),
  };
}

/** The size where the two costs balance — the cheapest a trade can ever be. */
export const CHEAPEST_SIZE_SOL = round(Math.sqrt((TIP_SOL * 100) / IMPACT_PCT_PER_SOL), 3);

/**
 * Turn a gross exit multiple into what actually lands in the wallet.
 * A 1.50x exit on a 3.1% round trip is not 50% — it is 45.4%.
 */
export function netReturnPct(grossMultiple, sizeSol) {
  const cost = roundTripCostPct(sizeSol);
  const gross = (Number(grossMultiple) - 1) * 100;
  return round(gross - cost.totalPct - (gross * cost.totalPct) / 100);
}

/** Sizes offered in the interface, cheapest-first is not the order — smallest is. */
export const SIZES = [0.1, 0.25, 0.5, 1];

function round(n, dp = 4) {
  const f = 10 ** dp;
  return Math.round(n * f) / f;
}
