//! Whether the funding graph is a claim about the chain, or only a claim.
//!
//! `tracer.rs` traverses edges and `clustering.rs` scores what the traversal
//! found, and both of them take the edges as given. That is the right split and
//! it leaves a hole exactly one module wide: a [`TraceEdge`] is an *assertion*
//! that some signature moved some lamports between two addresses at some slot,
//! and until something compares it against the chain, a forensic report is a
//! rigorous derivation from whatever the message said. Every number downstream
//! — the posterior, the cluster, the insider score, the flag an operator acts
//! on — inherits the truth of that assertion and none of them can check it.
//!
//! This module is the check. It is not a source of edges and it does not fetch
//! anything: it takes the edges a request asserted, takes what providers say
//! about the signatures on them, and returns a verdict per edge plus the edges
//! that survived it.
//!
//! # Three answers, not two
//!
//! The temptation is `verified: bool`, and it is wrong in the way the whole
//! project is written to avoid. There are three states and they license
//! different actions:
//!
//! - **Confirmed.** Enough providers found the transaction and it carries the
//!   transfer claimed. The edge stands at the confidence it was asserted with.
//! - **UNKNOWN** — [`EdgeVerdict::Unverified`] when nobody could answer,
//!   [`EdgeVerdict::SingleSource`] when fewer providers answered than the
//!   quorum wants. The edge is kept and discounted. It is *not* dropped: an
//!   unverifiable funding edge is still the best evidence available and
//!   throwing it away would clear a wallet by failing to look at it, which is
//!   the one direction the conventions forbid.
//! - **Contradicted** — [`EdgeVerdict::Absent`], [`Failed`](EdgeVerdict::Failed),
//!   [`Mismatched`](EdgeVerdict::Mismatched), [`Split`](EdgeVerdict::Split).
//!   The chain says the edge did not happen, or did not happen this way. It is
//!   dropped from the graph and reported, because a request that asserts
//!   transfers the chain does not have is itself the finding.
//!
//! # What a proof licenses
//!
//! The same asymmetry `WalletTrace::may_clear` enforces for truncation, for the
//! same reason: **an unverified lineage may block an entry and may never clear
//! one.** [`LineageProof::may_clear`] is false unless every edge came back
//! confirmed. A discount is a lower bound on how much a wallet is implicated;
//! more verification could only find more, so acting on it defensively is
//! sound and acting on it permissively is not.
//!
//! # Two providers or UNKNOWN
//!
//! [`VerificationPolicy::quorum`] defaults to two, which is the roadmap's
//! Phase 1 rule — critical facts require two consistent providers or they are
//! UNKNOWN — applied to the one class of fact that had escaped it. A single
//! provider confirming an edge is [`EdgeVerdict::SingleSource`]: better than
//! nothing, not a confirmation, and never a clearance. Providers that disagree
//! with *each other* are [`EdgeVerdict::Split`], which is a contradiction
//! rather than a tie broken by whoever answered first.
//!
//! # Nothing here reaches the network
//!
//! [`ChainWitness`] is a port, the same shape as
//! [`LeaderSchedule`](crate::execution::LeaderSchedule): this crate has no RPC
//! client and acquiring one is a Phase 1 decision, not a forensic one. What the
//! seam buys is that verification is already asked for on the path that needs
//! it, so a live adapter adds an answer rather than a branch.
//!
//! It also buys something better than convenience. Attestations travel *in the
//! request*, next to the edges they are about, exactly as `clustering.rs`
//! carries its graph — so a verified report is reproducible from the message
//! that produced it, replayable from a fixture, and provable after the fact.
//! A verifier that dialled out mid-analysis would make the same report depend
//! on when it was asked for, which is the property this whole tree is built to
//! keep.
//!
//! # Determinism
//!
//! Attestations are grouped in a `BTreeMap`, every provider list is sorted,
//! every proof is sorted on a key ending in a signature, and the per-provider
//! disagreement checks run in a fixed order so that an edge failing two of them
//! reports the same one on every machine. No floating point: the confidence
//! discount is basis points and integer division.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::EngineError;
use crate::tracer::{Asset, TraceEdge};
use crate::types::BPS_DENOMINATOR;

/// The schema tag every stored proof carries.
pub const PROOF_SCHEMA: &str = "sts.chainproof.lineage.v1";

/// The most edges one verification request may carry.
///
/// A ceiling on work rather than a policy: the check is linear in edges times
/// attestations and both arrive over IPC, so a window that asks about a hundred
/// thousand edges has not asked a question.
pub const MAX_EDGES: usize = 16_384;

// ===========================================================================
// Policy
// ===========================================================================

/// How strict the comparison is, versioned with everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationPolicy {
    pub version: u32,
    /// Providers that must independently confirm an edge before it counts as
    /// confirmed. Two, per the Phase 1 rule.
    pub quorum: u32,
    /// Slots the chain may differ from the claim by.
    ///
    /// Zero by default, and that is not strictness for its own sake: a
    /// transaction lands in exactly one slot, so a claim naming a different one
    /// is a claim about a different transaction. The knob exists because a
    /// re-org window is a real reason for two honest readings to differ, and
    /// `RISK_AND_SYBIL_SPEC.md` §3.2 already treats provider disagreement as a
    /// thing to bound rather than a thing to be shocked by.
    pub slot_tolerance: u64,
    /// Milliseconds the chain's block time may differ from the claim by.
    ///
    /// A second by default. §3.2 notes that two providers' block times disagree
    /// by a few hundred milliseconds, and an edge rejected for that would be
    /// rejected for the disagreement the slot field exists to resolve.
    pub time_tolerance_ms: i64,
    /// Base units the amounts may differ by. Zero: an amount is an integer.
    pub amount_tolerance: u64,
    /// What an edge the quorum did not confirm is carried at, as a fraction of
    /// the confidence it was asserted with, in basis points.
    ///
    /// Half. The number is a policy dial and the *shape* is the doctrine: not
    /// one, because an unchecked assertion is not a checked one; not zero,
    /// because zero is `UNKNOWN` rendered as "did not happen", and the whole
    /// module exists to stop that particular collapse.
    pub unverified_confidence_bps: u64,
}

impl Default for VerificationPolicy {
    fn default() -> Self {
        VerificationPolicy {
            version: 1,
            quorum: 2,
            slot_tolerance: 0,
            time_tolerance_ms: 1_000,
            amount_tolerance: 0,
            unverified_confidence_bps: 5_000,
        }
    }
}

impl VerificationPolicy {
    /// Checks the policy describes a comparison that could fail.
    pub fn validate(&self) -> Result<(), EngineError> {
        if self.quorum == 0 {
            return Err(EngineError::Forensics(
                "a quorum of zero would confirm every edge nobody looked at".to_string(),
            ));
        }
        if self.unverified_confidence_bps > BPS_DENOMINATOR as u64 {
            return Err(EngineError::Forensics(
                "an unverified edge cannot be worth more than a verified one".to_string(),
            ));
        }
        if self.time_tolerance_ms < 0 {
            return Err(EngineError::Forensics(
                "a negative time tolerance would reject every edge".to_string(),
            ));
        }
        Ok(())
    }
}

// ===========================================================================
// What the chain says
// ===========================================================================

/// One transfer inside a transaction, as a provider read it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainTransfer {
    pub from: String,
    pub to: String,
    /// Lamports for SOL, base units for a token — the same unit
    /// [`TraceEdge::lamports`] is in.
    pub lamports: u64,
    pub asset: Asset,
}

/// One transaction, as one provider reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainTransaction {
    pub signature: String,
    pub slot: u64,
    /// `None` when the provider served the transaction without one, which
    /// happens for old blocks. Absent is not zero: the time check is skipped
    /// rather than failed.
    pub block_time_ms: Option<i64>,
    /// False for a transaction that landed and reverted. It is on chain and it
    /// moved nothing, which is a different finding from not being there.
    pub succeeded: bool,
    /// Every transfer the provider decoded, in any order.
    pub transfers: Vec<ChainTransfer>,
}

/// What one provider said when asked about one signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum WitnessAnswer {
    /// The provider served the transaction.
    Found(Box<ChainTransaction>),
    /// The provider looked and the chain has no transaction under that
    /// signature.
    Absent,
    /// The provider could not answer: no credentials, a timeout, a quota, a
    /// pruned ledger. UNKNOWN, and never confused with `Absent` — "I did not
    /// look" and "it is not there" license opposite actions.
    Unavailable { reason: String },
}

/// One provider's answer about one signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attestation {
    pub provider: String,
    pub signature: String,
    pub answer: WitnessAnswer,
}

impl Attestation {
    pub fn found(provider: &str, transaction: ChainTransaction) -> Attestation {
        Attestation {
            provider: provider.to_string(),
            signature: transaction.signature.clone(),
            answer: WitnessAnswer::Found(Box::new(transaction)),
        }
    }

    pub fn absent(provider: &str, signature: &str) -> Attestation {
        Attestation {
            provider: provider.to_string(),
            signature: signature.to_string(),
            answer: WitnessAnswer::Absent,
        }
    }

    pub fn unavailable(provider: &str, signature: &str, reason: &str) -> Attestation {
        Attestation {
            provider: provider.to_string(),
            signature: signature.to_string(),
            answer: WitnessAnswer::Unavailable {
                reason: reason.to_string(),
            },
        }
    }
}

/// Where a lineage's edges are checked against the chain.
///
/// A port with nothing behind it in this build, deliberately: answering it is a
/// `getTransaction` per signature and this crate has no RPC client in its
/// dependencies — see the module header. An implementation belongs beside the
/// provider adapters, where the quota counters and the circuit breakers already
/// live, and it returns [`Attestation`]s that then travel in the request like
/// every other piece of evidence.
pub trait ChainWitness: Send + Sync {
    /// Answers about every signature given, in any order.
    ///
    /// One call rather than one per signature because a real implementation
    /// batches, and because a per-signature port would invite a verifier that
    /// makes one network round trip per edge inside a loop it does not own.
    fn attest(&self, signatures: &[String]) -> Vec<Attestation>;
}

/// A witness that answers from a table it was handed.
///
/// What a fixture, a replay and every test in this module use, and the shape a
/// live adapter's cache would take. Signatures it was not told about come back
/// [`WitnessAnswer::Unavailable`] rather than [`WitnessAnswer::Absent`]: a
/// table that does not contain something has not established that the chain
/// does not either.
#[derive(Debug, Clone, Default)]
pub struct RecordedWitness {
    provider: String,
    answers: BTreeMap<String, WitnessAnswer>,
}

impl RecordedWitness {
    pub fn new(provider: &str) -> RecordedWitness {
        RecordedWitness {
            provider: provider.to_string(),
            answers: BTreeMap::new(),
        }
    }

    /// Records a transaction the chain has.
    pub fn with_transaction(mut self, transaction: ChainTransaction) -> RecordedWitness {
        self.answers.insert(
            transaction.signature.clone(),
            WitnessAnswer::Found(Box::new(transaction)),
        );
        self
    }

    /// Records that the chain has nothing under this signature.
    pub fn with_absent(mut self, signature: &str) -> RecordedWitness {
        self.answers
            .insert(signature.to_string(), WitnessAnswer::Absent);
        self
    }

    pub fn len(&self) -> usize {
        self.answers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.answers.is_empty()
    }
}

impl ChainWitness for RecordedWitness {
    fn attest(&self, signatures: &[String]) -> Vec<Attestation> {
        let mut seen: BTreeSet<&String> = BTreeSet::new();
        let mut out = Vec::new();
        for signature in signatures {
            if !seen.insert(signature) {
                continue;
            }
            let answer = self.answers.get(signature).cloned().unwrap_or_else(|| {
                WitnessAnswer::Unavailable {
                    reason: "not in the recorded slice".to_string(),
                }
            });
            out.push(Attestation {
                provider: self.provider.clone(),
                signature: signature.clone(),
                answer,
            });
        }
        out
    }
}

/// Collects attestations for every signature an edge set claims.
///
/// The one function that touches a witness, so that everything downstream is a
/// pure function of the attestations it produced. Signatures are deduplicated
/// and asked in sorted order: one signature can carry several edges, and asking
/// twice would double a provider's weight in the quorum.
pub fn collect_attestations(
    edges: &[TraceEdge],
    witnesses: &[&dyn ChainWitness],
) -> Vec<Attestation> {
    let signatures: Vec<String> = edges
        .iter()
        .map(|edge| edge.signature.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let mut out = Vec::new();
    for witness in witnesses {
        out.extend(witness.attest(&signatures));
    }
    out.sort_by(|a, b| {
        a.signature
            .cmp(&b.signature)
            .then_with(|| a.provider.cmp(&b.provider))
    });
    out
}

// ===========================================================================
// Verdicts
// ===========================================================================

/// What the chain had to say about one claimed edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeVerdict {
    /// The quorum found the transaction and it carries this transfer.
    Confirmed,
    /// Fewer providers found it than the quorum wants, and none disagreed.
    /// UNKNOWN.
    SingleSource,
    /// Nobody could answer. UNKNOWN.
    Unverified,
    /// Every provider that answered says the chain has no such transaction.
    Absent,
    /// The transaction is on chain and it failed, so it moved nothing.
    Failed,
    /// The transaction is on chain and does not carry this transfer.
    Mismatched,
    /// The providers that answered do not agree with one another.
    Split,
}

impl EdgeVerdict {
    /// Whether the quorum stood behind this edge.
    pub fn is_confirmed(self) -> bool {
        self == EdgeVerdict::Confirmed
    }

    /// Whether the chain says this edge did not happen, or did not happen this
    /// way. A contradicted edge is dropped from the graph.
    pub fn contradicts(self) -> bool {
        matches!(
            self,
            EdgeVerdict::Absent
                | EdgeVerdict::Failed
                | EdgeVerdict::Mismatched
                | EdgeVerdict::Split
        )
    }

    /// Whether nothing was established either way.
    pub fn is_unknown(self) -> bool {
        matches!(self, EdgeVerdict::SingleSource | EdgeVerdict::Unverified)
    }
}

/// Which claimed field the chain disagrees with.
///
/// Named rather than counted: "the chain has this transaction and it moved
/// 4 SOL, not 40" and "the chain has this transaction and these two addresses
/// are not in it" are different findings about a request, and a report that
/// said only `mismatched` would make an operator go and look them up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Disagreement {
    Slot {
        claimed: u64,
        observed: u64,
    },
    BlockTime {
        claimed_ms: i64,
        observed_ms: i64,
    },
    Amount {
        claimed: u64,
        observed: u64,
    },
    /// The transaction moved money, but not between these two addresses.
    Endpoints,
    /// These two addresses exchanged something, and it was a different asset.
    Asset,
    /// The transaction landed and reverted.
    Failed,
    /// The chain has no transaction under this signature.
    Missing,
    /// Two providers reported different things about one signature.
    Providers,
}

/// One claimed edge, judged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeProof {
    pub signature: String,
    pub from: String,
    pub to: String,
    pub lamports: u64,
    pub asset: Asset,
    pub claimed_slot: u64,
    pub claimed_at_ms: i64,
    pub verdict: EdgeVerdict,
    /// Providers that served the transaction and agreed this edge is in it.
    pub providers_confirming: u32,
    /// Providers that served the transaction and disagreed.
    pub providers_disagreeing: u32,
    /// Providers that say the chain has no such transaction.
    pub providers_absent: u32,
    /// Providers that could not answer.
    pub providers_unavailable: u32,
    /// Every provider that was asked and said something, sorted.
    pub providers: Vec<String>,
    /// The confidence this edge is carried at after the verdict, in millionths.
    /// Zero for a contradicted edge, which is dropped rather than carried.
    pub confidence_micros: u32,
    /// Named only when the verdict is a contradiction.
    pub disagreement: Option<Disagreement>,
}

/// Every claimed edge, judged, plus what the set adds up to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineageProof {
    pub schema: String,
    pub policy: VerificationPolicy,
    /// One row per distinct claim, in `(signature, from, to, asset, amount)`
    /// order.
    pub edges: Vec<EdgeProof>,
    pub claimed: u32,
    pub confirmed: u32,
    pub single_source: u32,
    pub unverified: u32,
    pub contradicted: u32,
    /// Attestations about signatures no edge claimed. Evidence supplied and not
    /// used, which is worth a number: a witness answering about nothing in the
    /// request usually means the two were assembled from different slices.
    pub unclaimed_attestations: u32,
    /// Every claimed edge came back confirmed.
    pub complete: bool,
}

impl LineageProof {
    /// Whether this proof may be used to *clear* anything.
    ///
    /// The same predicate `WalletTrace::may_clear` publishes and the same
    /// asymmetry behind it. A lineage with one unverified edge in it is a lower
    /// bound on what the chain would show; a lower bound may raise risk and may
    /// never lower it.
    pub fn may_clear(&self) -> bool {
        self.complete
    }

    /// The edges the chain contradicted, in report order.
    pub fn contradictions(&self) -> impl Iterator<Item = &EdgeProof> {
        self.edges.iter().filter(|edge| edge.verdict.contradicts())
    }

    /// A one-line summary for a log or an alert.
    pub fn headline(&self) -> String {
        format!(
            "{} claimed, {} confirmed, {} unknown, {} contradicted",
            self.claimed,
            self.confirmed,
            self.single_source + self.unverified,
            self.contradicted
        )
    }
}

/// A proof and the edges that survived it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedLineage {
    pub proof: LineageProof,
    /// The claimed edges minus the contradicted ones, each carried at the
    /// confidence its verdict licenses. In the order they were claimed, because
    /// `FundingGraph::build` is order-independent and preserving the caller's
    /// order keeps a diff of the request against the survivors readable.
    pub edges: Vec<TraceEdge>,
}

// ===========================================================================
// The comparison
// ===========================================================================

/// The identity of a claim: what makes two asserted edges the same assertion.
///
/// The same tuple `FundingGraph::build` deduplicates on, minus the endpoints'
/// interning. One signature legitimately carries several transfers, so the
/// signature alone is not the identity.
type ClaimKey = (String, String, String, Asset, u64, i64, u64);

fn claim_key(edge: &TraceEdge) -> ClaimKey {
    (
        edge.signature.clone(),
        edge.from.clone(),
        edge.to.clone(),
        edge.asset.clone(),
        edge.lamports,
        edge.at_ms,
        edge.slot,
    )
}

/// Judges every claimed edge against what providers said, and returns the
/// survivors.
///
/// Pure: same edges, same attestations, same policy, same bytes. The only
/// input that reaches the network is `attestations`, and it reached it before
/// this was called.
pub fn verify_edges(
    edges: &[TraceEdge],
    attestations: &[Attestation],
    policy: &VerificationPolicy,
) -> VerifiedLineage {
    // Group answers by signature, and by provider inside it. A provider that
    // answered twice about one signature is one voice: if the two answers agree
    // it is one answer, and if they do not, the provider disagrees with itself
    // and that is a split like any other.
    let mut by_signature: BTreeMap<&str, BTreeMap<&str, Vec<&WitnessAnswer>>> = BTreeMap::new();
    for attestation in attestations {
        by_signature
            .entry(attestation.signature.as_str())
            .or_default()
            .entry(attestation.provider.as_str())
            .or_default()
            .push(&attestation.answer);
    }

    let claimed_signatures: BTreeSet<&str> =
        edges.iter().map(|edge| edge.signature.as_str()).collect();
    let unclaimed_attestations = by_signature
        .keys()
        .filter(|signature| !claimed_signatures.contains(*signature))
        .count() as u32;

    // One proof per distinct claim, so that two identical asserted edges are
    // judged once and counted once — the graph would have collapsed them
    // anyway, and counting both would let one assertion corroborate itself.
    let mut proofs: BTreeMap<ClaimKey, EdgeProof> = BTreeMap::new();
    for edge in edges {
        let key = claim_key(edge);
        if proofs.contains_key(&key) {
            continue;
        }
        let answers = by_signature.get(edge.signature.as_str());
        proofs.insert(key, judge_edge(edge, answers, policy));
    }

    let mut counts = (0u32, 0u32, 0u32, 0u32);
    for proof in proofs.values() {
        match proof.verdict {
            EdgeVerdict::Confirmed => counts.0 += 1,
            EdgeVerdict::SingleSource => counts.1 += 1,
            EdgeVerdict::Unverified => counts.2 += 1,
            _ => counts.3 += 1,
        }
    }

    // Every claimed edge carried at what its verdict licenses, minus the ones
    // the chain contradicted. Every edge has a proof by construction — the loop
    // above inserted one for each distinct claim — so a miss here would be a
    // bug rather than a state, and the edge is dropped rather than passed
    // through unjudged.
    let mut survivors = Vec::with_capacity(edges.len());
    for edge in edges {
        let Some(proof) = proofs.get(&claim_key(edge)) else {
            continue;
        };
        if proof.verdict.contradicts() {
            continue;
        }
        let mut kept = edge.clone();
        kept.confidence_micros = proof.confidence_micros;
        survivors.push(kept);
    }

    let claimed = proofs.len() as u32;
    let rows: Vec<EdgeProof> = proofs.into_values().collect();

    VerifiedLineage {
        proof: LineageProof {
            schema: PROOF_SCHEMA.to_string(),
            policy: *policy,
            edges: rows,
            claimed,
            confirmed: counts.0,
            single_source: counts.1,
            unverified: counts.2,
            contradicted: counts.3,
            unclaimed_attestations,
            complete: claimed > 0 && counts.0 == claimed,
        },
        edges: survivors,
    }
}

/// One claimed edge against every provider that said something about its
/// signature.
fn judge_edge(
    edge: &TraceEdge,
    answers: Option<&BTreeMap<&str, Vec<&WitnessAnswer>>>,
    policy: &VerificationPolicy,
) -> EdgeProof {
    let mut providers: Vec<String> = Vec::new();
    let mut confirming = 0u32;
    let mut reverted = 0u32;
    let mut disagreeing = 0u32;
    let mut absent = 0u32;
    let mut unavailable = 0u32;
    // The first disagreement in provider order, so that an edge failing two
    // checks names the same one on every machine.
    let mut disagreement: Option<Disagreement> = None;

    if let Some(answers) = answers {
        for (provider, provider_answers) in answers {
            providers.push((*provider).to_string());

            // What this one provider concluded, folded across the answers it
            // gave. A provider that said two different things about one
            // signature is inconsistent with itself.
            let mut verdicts: BTreeSet<ProviderVerdict> = BTreeSet::new();
            for answer in provider_answers {
                verdicts.insert(match answer {
                    WitnessAnswer::Unavailable { .. } => ProviderVerdict::Silent,
                    WitnessAnswer::Absent => ProviderVerdict::Missing,
                    WitnessAnswer::Found(transaction) => match compare(edge, transaction, policy) {
                        None => ProviderVerdict::Agrees,
                        Some(Disagreement::Failed) => {
                            disagreement.get_or_insert(Disagreement::Failed);
                            ProviderVerdict::Reverted
                        }
                        Some(found) => {
                            disagreement.get_or_insert(found);
                            ProviderVerdict::Disagrees
                        }
                    },
                });
            }

            // "I could not answer" is the absence of an answer, not an answer,
            // so it never conflicts with one. A provider that timed out on the
            // first attempt and served the transaction on the second has not
            // contradicted itself, and treating it as though it had would mark
            // the edge SPLIT and *drop* it — which removes evidence, and
            // removing evidence is not the conservative direction. It can clear
            // a wallet by unlinking it.
            if verdicts.len() > 1 {
                verdicts.remove(&ProviderVerdict::Silent);
            }

            match verdicts.len() {
                0 => {}
                1 => match verdicts.into_iter().next().expect("one verdict") {
                    ProviderVerdict::Agrees => confirming += 1,
                    ProviderVerdict::Reverted => reverted += 1,
                    ProviderVerdict::Disagrees => disagreeing += 1,
                    ProviderVerdict::Missing => absent += 1,
                    ProviderVerdict::Silent => unavailable += 1,
                },
                _ => {
                    // The provider contradicted itself. Counted on the
                    // disagreeing side, which is the side that blocks.
                    disagreeing += 1;
                    disagreement.get_or_insert(Disagreement::Providers);
                }
            }
        }
    }

    let verdict = decide(confirming, reverted, disagreeing, absent, policy);
    if verdict == EdgeVerdict::Split {
        disagreement.get_or_insert(Disagreement::Providers);
    }
    if verdict == EdgeVerdict::Absent {
        disagreement.get_or_insert(Disagreement::Missing);
    }

    let confidence_micros = if verdict.contradicts() {
        0
    } else if verdict.is_confirmed() {
        edge.confidence_micros
    } else {
        discount(edge.confidence_micros, policy.unverified_confidence_bps)
    };

    EdgeProof {
        signature: edge.signature.clone(),
        from: edge.from.clone(),
        to: edge.to.clone(),
        lamports: edge.lamports,
        asset: edge.asset.clone(),
        claimed_slot: edge.slot,
        claimed_at_ms: edge.at_ms,
        verdict,
        providers_confirming: confirming,
        // Reverted and mismatched together: the field says how many providers
        // served the transaction and did not stand behind this edge.
        providers_disagreeing: reverted + disagreeing,
        providers_absent: absent,
        providers_unavailable: unavailable,
        providers,
        confidence_micros,
        disagreement: if verdict.contradicts() {
            disagreement
        } else {
            None
        },
    }
}

/// What one provider concluded about one edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ProviderVerdict {
    Agrees,
    /// Served the transaction, and it landed and reverted. Kept apart from
    /// `Disagrees` so that [`decide`] can report *why* the chain contradicts
    /// the edge — a reverted transaction and a transaction carrying different
    /// numbers are both contradictions and they send an operator to different
    /// places.
    Reverted,
    Disagrees,
    Missing,
    Silent,
}

/// The quorum rule, in one place.
///
/// Read in the order the answers bind: disagreement first, because a provider
/// that looked and found something different is the strongest thing anybody
/// said; then absence; then the count against the quorum.
fn decide(
    confirming: u32,
    reverted: u32,
    disagreeing: u32,
    absent: u32,
    policy: &VerificationPolicy,
) -> EdgeVerdict {
    let contradicting = reverted + disagreeing;

    // Providers that looked and saw different things. A tie broken by whoever
    // answered first would be exactly the silent overwrite the roadmap forbids.
    if contradicting > 0 && (confirming > 0 || absent > 0) {
        return EdgeVerdict::Split;
    }
    if absent > 0 && confirming > 0 {
        return EdgeVerdict::Split;
    }
    // Every provider that served it contradicts the edge. Which contradiction
    // is reported is decided here rather than by whichever check fired first:
    // a transaction carrying the wrong numbers is the more specific finding, so
    // a mix reports that rather than the revert.
    if disagreeing > 0 {
        return EdgeVerdict::Mismatched;
    }
    if reverted > 0 {
        return EdgeVerdict::Failed;
    }
    if absent > 0 {
        return EdgeVerdict::Absent;
    }
    if confirming == 0 {
        // Either nobody was asked, or everybody who was could not answer.
        return EdgeVerdict::Unverified;
    }
    if confirming >= policy.quorum {
        EdgeVerdict::Confirmed
    } else {
        EdgeVerdict::SingleSource
    }
}

/// One claimed edge against one served transaction.
///
/// `None` when the transaction carries the transfer claimed. The checks run
/// most-specific-first and stop at the first failure, so the reported
/// disagreement is the same on every machine.
fn compare(
    edge: &TraceEdge,
    transaction: &ChainTransaction,
    policy: &VerificationPolicy,
) -> Option<Disagreement> {
    if !transaction.succeeded {
        // On chain and reverted. It moved nothing, so no transfer inside it is
        // evidence of funding — and this is emphatically not `Absent`: the
        // signature is real and a report that called it missing would be
        // wrong in a way somebody would spend an hour on.
        return Some(Disagreement::Failed);
    }

    if transaction.slot.abs_diff(edge.slot) > policy.slot_tolerance {
        return Some(Disagreement::Slot {
            claimed: edge.slot,
            observed: transaction.slot,
        });
    }

    if let Some(block_time_ms) = transaction.block_time_ms {
        if (block_time_ms - edge.at_ms).abs() > policy.time_tolerance_ms {
            return Some(Disagreement::BlockTime {
                claimed_ms: edge.at_ms,
                observed_ms: block_time_ms,
            });
        }
    }

    // Everything the transaction moved between these two addresses.
    let between: Vec<&ChainTransfer> = transaction
        .transfers
        .iter()
        .filter(|transfer| transfer.from == edge.from && transfer.to == edge.to)
        .collect();
    if between.is_empty() {
        return Some(Disagreement::Endpoints);
    }

    let same_asset: Vec<&ChainTransfer> = between
        .into_iter()
        .filter(|transfer| transfer.asset == edge.asset)
        .collect();
    if same_asset.is_empty() {
        return Some(Disagreement::Asset);
    }

    if same_asset
        .iter()
        .any(|transfer| transfer.lamports.abs_diff(edge.lamports) <= policy.amount_tolerance)
    {
        return None;
    }

    // The closest one, so the report names the amount somebody would recognise
    // rather than whichever happened to be decoded first. Ties fall to the
    // smaller amount, which is total because the values are integers.
    let observed = same_asset
        .iter()
        .min_by_key(|transfer| (transfer.lamports.abs_diff(edge.lamports), transfer.lamports))
        .map(|transfer| transfer.lamports)
        .unwrap_or(0);

    Some(Disagreement::Amount {
        claimed: edge.lamports,
        observed,
    })
}

/// A confidence scaled by basis points, rounding down.
///
/// Down, because the rounding direction of a discount on unverified evidence is
/// a policy question with one defensible answer.
fn discount(confidence_micros: u32, bps: u64) -> u32 {
    let scaled = u64::from(confidence_micros) * bps / BPS_DENOMINATOR as u64;
    scaled.min(u64::from(u32::MAX)) as u32
}

/// Verifies a set of edges, refusing a request too large to be a question.
pub fn verify_request(
    edges: &[TraceEdge],
    attestations: &[Attestation],
    policy: &VerificationPolicy,
) -> Result<VerifiedLineage, EngineError> {
    policy.validate()?;
    if edges.len() > MAX_EDGES {
        return Err(EngineError::Forensics(format!(
            "a verification request may carry at most {MAX_EDGES} edges, and this one has {}",
            edges.len()
        )));
    }
    Ok(verify_edges(edges, attestations, policy))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLOT: u64 = 250_000_000;
    const AT_MS: i64 = 1_700_000_000_000;
    const SOL: u64 = 1_000_000_000;

    fn edge() -> TraceEdge {
        TraceEdge {
            from: "operator".to_string(),
            to: "puppet".to_string(),
            lamports: 5 * SOL,
            at_ms: AT_MS,
            slot: SLOT,
            signature: "sig1".to_string(),
            asset: Asset::Sol,
            confidence_micros: 1_000_000,
        }
    }

    /// The transaction the chain would serve if the edge were true.
    fn truthful() -> ChainTransaction {
        ChainTransaction {
            signature: "sig1".to_string(),
            slot: SLOT,
            block_time_ms: Some(AT_MS),
            succeeded: true,
            transfers: vec![ChainTransfer {
                from: "operator".to_string(),
                to: "puppet".to_string(),
                lamports: 5 * SOL,
                asset: Asset::Sol,
            }],
        }
    }

    fn verdict(attestations: Vec<Attestation>) -> EdgeProof {
        let verified = verify_edges(&[edge()], &attestations, &VerificationPolicy::default());
        verified.proof.edges.into_iter().next().expect("one proof")
    }

    // -----------------------------------------------------------------
    // The quorum
    // -----------------------------------------------------------------

    #[test]
    fn two_providers_that_agree_confirm_an_edge_and_leave_its_confidence_alone() {
        let verified = verify_edges(
            &[edge()],
            &[
                Attestation::found("helius", truthful()),
                Attestation::found("quicknode", truthful()),
            ],
            &VerificationPolicy::default(),
        );

        let proof = &verified.proof.edges[0];
        assert_eq!(proof.verdict, EdgeVerdict::Confirmed);
        assert_eq!(proof.providers_confirming, 2);
        assert_eq!(proof.confidence_micros, 1_000_000);
        assert_eq!(proof.disagreement, None);
        assert_eq!(proof.providers, vec!["helius", "quicknode"]);

        assert!(verified.proof.complete);
        assert!(verified.proof.may_clear());
        assert_eq!(verified.edges.len(), 1);
        assert_eq!(verified.edges[0], edge());
    }

    #[test]
    fn one_provider_is_not_a_quorum_and_the_edge_is_discounted_rather_than_dropped() {
        let verified = verify_edges(
            &[edge()],
            &[Attestation::found("helius", truthful())],
            &VerificationPolicy::default(),
        );

        let proof = &verified.proof.edges[0];
        assert_eq!(proof.verdict, EdgeVerdict::SingleSource);
        assert!(proof.verdict.is_unknown());
        assert!(!proof.verdict.contradicts());
        // Half, and the edge survives carrying it. Not dropped: an edge one
        // provider stands behind is the best evidence there is.
        assert_eq!(proof.confidence_micros, 500_000);
        assert_eq!(verified.edges.len(), 1);
        assert_eq!(verified.edges[0].confidence_micros, 500_000);

        // One unconfirmed edge is enough to stop the whole lineage clearing
        // anything, which is the asymmetry the module exists to enforce.
        assert!(!verified.proof.complete);
        assert!(!verified.proof.may_clear());
    }

    #[test]
    fn a_relaxed_quorum_confirms_what_one_provider_saw() {
        let policy = VerificationPolicy {
            quorum: 1,
            ..VerificationPolicy::default()
        };
        let verified = verify_edges(
            &[edge()],
            &[Attestation::found("helius", truthful())],
            &policy,
        );
        assert_eq!(verified.proof.edges[0].verdict, EdgeVerdict::Confirmed);
        assert!(verified.proof.may_clear());
    }

    #[test]
    fn nobody_asked_and_nobody_knowing_are_both_unknown_and_neither_is_a_pass() {
        let silent = verdict(Vec::new());
        assert_eq!(silent.verdict, EdgeVerdict::Unverified);
        assert_eq!(silent.confidence_micros, 500_000);
        assert!(silent.providers.is_empty());

        let asked = verdict(vec![
            Attestation::unavailable("helius", "sig1", "quota exhausted"),
            Attestation::unavailable("quicknode", "sig1", "timeout"),
        ]);
        assert_eq!(asked.verdict, EdgeVerdict::Unverified);
        assert_eq!(asked.providers_unavailable, 2);
        assert_eq!(asked.confidence_micros, 500_000);

        // The distinction survives into the report: one was asked and one was
        // not, and the provider counts are how a reader tells them apart.
        assert_eq!(silent.providers_unavailable, 0);
    }

    // -----------------------------------------------------------------
    // Contradictions
    // -----------------------------------------------------------------

    #[test]
    fn a_transaction_the_chain_does_not_have_is_absent_and_the_edge_is_dropped() {
        let verified = verify_edges(
            &[edge()],
            &[
                Attestation::absent("helius", "sig1"),
                Attestation::absent("quicknode", "sig1"),
            ],
            &VerificationPolicy::default(),
        );

        let proof = &verified.proof.edges[0];
        assert_eq!(proof.verdict, EdgeVerdict::Absent);
        assert!(proof.verdict.contradicts());
        assert_eq!(proof.disagreement, Some(Disagreement::Missing));
        assert_eq!(proof.confidence_micros, 0);
        assert!(verified.edges.is_empty(), "a contradicted edge is dropped");
        assert_eq!(verified.proof.contradicted, 1);
    }

    #[test]
    fn a_transaction_that_landed_and_reverted_moved_nothing_and_is_not_missing() {
        let mut reverted = truthful();
        reverted.succeeded = false;
        let proof = verdict(vec![
            Attestation::found("helius", reverted.clone()),
            Attestation::found("quicknode", reverted),
        ]);

        assert_eq!(proof.verdict, EdgeVerdict::Failed);
        assert!(proof.verdict.contradicts());
        // The signature is real. Calling it missing would send somebody looking
        // for a transaction that is right there.
        assert_eq!(proof.disagreement, Some(Disagreement::Failed));
    }

    #[test]
    fn the_chain_naming_a_different_amount_says_which_amount() {
        let mut wrong = truthful();
        wrong.transfers[0].lamports = SOL / 2;
        let proof = verdict(vec![
            Attestation::found("helius", wrong.clone()),
            Attestation::found("quicknode", wrong),
        ]);

        assert_eq!(proof.verdict, EdgeVerdict::Mismatched);
        assert_eq!(
            proof.disagreement,
            Some(Disagreement::Amount {
                claimed: 5 * SOL,
                observed: SOL / 2,
            })
        );
    }

    #[test]
    fn a_transaction_between_other_addresses_is_an_endpoint_disagreement() {
        let mut elsewhere = truthful();
        elsewhere.transfers[0].to = "somebody-else".to_string();
        let proof = verdict(vec![Attestation::found("helius", elsewhere)]);
        assert_eq!(proof.verdict, EdgeVerdict::Mismatched);
        assert_eq!(proof.disagreement, Some(Disagreement::Endpoints));
    }

    #[test]
    fn the_same_pair_moving_a_different_asset_is_not_the_same_transfer() {
        let mut token = truthful();
        token.transfers[0].asset = Asset::Token("mint".to_string());
        let proof = verdict(vec![Attestation::found("helius", token)]);
        assert_eq!(proof.verdict, EdgeVerdict::Mismatched);
        assert_eq!(proof.disagreement, Some(Disagreement::Asset));
    }

    #[test]
    fn a_transaction_in_another_slot_is_a_transaction_about_something_else() {
        let mut moved = truthful();
        moved.slot = SLOT + 1;
        let proof = verdict(vec![Attestation::found("helius", moved.clone())]);
        assert_eq!(
            proof.disagreement,
            Some(Disagreement::Slot {
                claimed: SLOT,
                observed: SLOT + 1,
            })
        );

        // The tolerance is a knob, and one slot inside it is agreement again.
        let policy = VerificationPolicy {
            slot_tolerance: 1,
            quorum: 1,
            ..VerificationPolicy::default()
        };
        let verified = verify_edges(&[edge()], &[Attestation::found("helius", moved)], &policy);
        assert_eq!(verified.proof.edges[0].verdict, EdgeVerdict::Confirmed);
    }

    #[test]
    fn block_times_may_disagree_by_the_tolerance_and_a_missing_one_is_not_checked() {
        let mut close = truthful();
        close.block_time_ms = Some(AT_MS + 900);
        assert_eq!(
            verdict(vec![Attestation::found("helius", close)]).verdict,
            EdgeVerdict::SingleSource,
            "§3.2's few hundred milliseconds of provider disagreement is not a contradiction"
        );

        let mut far = truthful();
        far.block_time_ms = Some(AT_MS + 5_000);
        assert_eq!(
            verdict(vec![Attestation::found("helius", far)]).disagreement,
            Some(Disagreement::BlockTime {
                claimed_ms: AT_MS,
                observed_ms: AT_MS + 5_000,
            })
        );

        // A provider that served no block time has not disagreed about one.
        let mut timeless = truthful();
        timeless.block_time_ms = None;
        assert_eq!(
            verdict(vec![Attestation::found("helius", timeless)]).verdict,
            EdgeVerdict::SingleSource
        );
    }

    // -----------------------------------------------------------------
    // Providers that disagree with each other
    // -----------------------------------------------------------------

    #[test]
    fn one_provider_finding_it_and_another_not_is_a_split_rather_than_a_tie_broken() {
        let proof = verdict(vec![
            Attestation::found("helius", truthful()),
            Attestation::absent("quicknode", "sig1"),
        ]);

        assert_eq!(proof.verdict, EdgeVerdict::Split);
        assert!(proof.verdict.contradicts());
        assert_eq!(proof.providers_confirming, 1);
        assert_eq!(proof.providers_absent, 1);
    }

    #[test]
    fn a_confirming_provider_does_not_outvote_a_disagreeing_one() {
        let mut wrong = truthful();
        wrong.transfers[0].lamports = SOL;
        let proof = verdict(vec![
            Attestation::found("helius", truthful()),
            Attestation::found("quicknode", wrong),
        ]);

        // Emphatically not `Confirmed` on a count of one-all, and not
        // `Mismatched` either: the finding is that the providers do not agree.
        assert_eq!(proof.verdict, EdgeVerdict::Split);
        assert_eq!(proof.providers_confirming, 1);
        assert_eq!(proof.providers_disagreeing, 1);
    }

    #[test]
    fn a_provider_that_contradicts_itself_counts_against_the_edge() {
        let mut wrong = truthful();
        wrong.transfers[0].lamports = SOL;
        let proof = verdict(vec![
            Attestation::found("helius", truthful()),
            Attestation::found("helius", wrong),
        ]);

        assert_eq!(proof.verdict, EdgeVerdict::Mismatched);
        assert_eq!(proof.providers_disagreeing, 1, "one provider, one voice");
        assert_eq!(proof.providers_confirming, 0);
    }

    #[test]
    fn a_provider_that_timed_out_and_then_answered_has_not_contradicted_itself() {
        // The retry, which is the ordinary shape of a real provider call.
        // Marking this SPLIT would drop the edge, and dropping an edge removes
        // a link — so the "conservative" reading here is the one that can clear
        // a wallet by failing to connect it to anything.
        let proof = verdict(vec![
            Attestation::unavailable("helius", "sig1", "timeout"),
            Attestation::found("helius", truthful()),
        ]);

        assert_eq!(proof.verdict, EdgeVerdict::SingleSource);
        assert_eq!(proof.providers_confirming, 1);
        assert_eq!(proof.providers_disagreeing, 0);
        assert_eq!(proof.providers_unavailable, 0);

        // And the same when the substantive answer is the contradicting one.
        let mut wrong = truthful();
        wrong.transfers[0].lamports = SOL;
        let proof = verdict(vec![
            Attestation::unavailable("helius", "sig1", "timeout"),
            Attestation::found("helius", wrong),
        ]);
        assert_eq!(proof.verdict, EdgeVerdict::Mismatched);
        assert_eq!(proof.providers_disagreeing, 1);
    }

    #[test]
    fn one_provider_answering_twice_the_same_way_is_still_one_provider() {
        let proof = verdict(vec![
            Attestation::found("helius", truthful()),
            Attestation::found("helius", truthful()),
        ]);
        assert_eq!(proof.verdict, EdgeVerdict::SingleSource);
        assert_eq!(proof.providers_confirming, 1);
    }

    // -----------------------------------------------------------------
    // The shape of the report
    // -----------------------------------------------------------------

    #[test]
    fn one_signature_carrying_two_transfers_is_two_claims_and_both_are_judged() {
        let mut second = edge();
        second.to = "puppet2".to_string();
        second.lamports = 2 * SOL;

        let mut transaction = truthful();
        transaction.transfers.push(ChainTransfer {
            from: "operator".to_string(),
            to: "puppet2".to_string(),
            lamports: 2 * SOL,
            asset: Asset::Sol,
        });

        let verified = verify_edges(
            &[edge(), second],
            &[
                Attestation::found("helius", transaction.clone()),
                Attestation::found("quicknode", transaction),
            ],
            &VerificationPolicy::default(),
        );

        assert_eq!(verified.proof.claimed, 2);
        assert_eq!(verified.proof.confirmed, 2);
        assert!(verified.proof.complete);
    }

    #[test]
    fn the_same_edge_asserted_twice_is_judged_once() {
        let verified = verify_edges(
            &[edge(), edge()],
            &[
                Attestation::found("helius", truthful()),
                Attestation::found("quicknode", truthful()),
            ],
            &VerificationPolicy::default(),
        );
        // One claim, one row — otherwise an assertion could corroborate itself
        // by being made twice.
        assert_eq!(verified.proof.claimed, 1);
        assert_eq!(verified.proof.edges.len(), 1);
        // Both copies survive, because the graph is what deduplicates vertices
        // and it is entitled to see what it was sent.
        assert_eq!(verified.edges.len(), 2);
    }

    #[test]
    fn evidence_about_nothing_in_the_request_is_counted_rather_than_ignored() {
        let verified = verify_edges(
            &[edge()],
            &[
                Attestation::found("helius", truthful()),
                Attestation::found("quicknode", truthful()),
                Attestation::absent("helius", "some-other-signature"),
            ],
            &VerificationPolicy::default(),
        );
        assert_eq!(verified.proof.unclaimed_attestations, 1);
        assert!(verified.proof.complete, "and it does not spoil the proof");
    }

    #[test]
    fn an_empty_request_is_not_a_complete_proof() {
        // Nothing was claimed, so nothing was confirmed, so there is nothing
        // here that could clear anything. `0 == 0` would have said otherwise.
        let verified = verify_edges(&[], &[], &VerificationPolicy::default());
        assert_eq!(verified.proof.claimed, 0);
        assert!(!verified.proof.complete);
        assert!(!verified.proof.may_clear());
    }

    #[test]
    fn the_headline_says_what_happened() {
        let verified = verify_edges(
            &[edge()],
            &[Attestation::found("helius", truthful())],
            &VerificationPolicy::default(),
        );
        assert_eq!(
            verified.proof.headline(),
            "1 claimed, 0 confirmed, 1 unknown, 0 contradicted"
        );
    }

    // -----------------------------------------------------------------
    // Determinism, and the wire
    // -----------------------------------------------------------------

    #[test]
    fn the_order_the_attestations_arrived_in_does_not_move_a_verdict() {
        let mut second = edge();
        second.signature = "sig2".to_string();
        second.to = "puppet2".to_string();
        let mut other = truthful();
        other.signature = "sig2".to_string();
        other.transfers[0].to = "puppet2".to_string();

        let forwards = vec![
            Attestation::found("helius", truthful()),
            Attestation::found("quicknode", truthful()),
            Attestation::found("helius", other.clone()),
            Attestation::absent("quicknode", "sig2"),
        ];
        let mut backwards = forwards.clone();
        backwards.reverse();

        let edges = [edge(), second];
        let policy = VerificationPolicy::default();
        let a = verify_edges(&edges, &forwards, &policy);
        let b = verify_edges(&edges, &backwards, &policy);
        assert_eq!(a, b);

        // And two runs of one input agree to the byte.
        let json_a = serde_json::to_string(&a.proof).expect("serialises");
        let json_b = serde_json::to_string(&verify_edges(&edges, &forwards, &policy).proof)
            .expect("serialises");
        assert_eq!(json_a, json_b);
    }

    #[test]
    fn a_proof_survives_the_wire_in_the_shape_it_left() {
        let verified = verify_edges(
            &[edge()],
            &[
                Attestation::found("helius", truthful()),
                Attestation::absent("quicknode", "sig1"),
            ],
            &VerificationPolicy::default(),
        );
        let json = serde_json::to_string(&verified.proof).expect("serialises");
        let back: LineageProof = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(back, verified.proof);
        assert!(json.contains("\"SPLIT\""), "{json}");
        assert_eq!(back.schema, PROOF_SCHEMA);
    }

    #[test]
    fn an_attestation_survives_the_wire_too() {
        let attestation = Attestation::found("helius", truthful());
        let json = serde_json::to_string(&attestation).expect("serialises");
        let back: Attestation = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(back, attestation);

        let unavailable = Attestation::unavailable("quicknode", "sig1", "quota");
        let json = serde_json::to_string(&unavailable).expect("serialises");
        assert_eq!(
            serde_json::from_str::<Attestation>(&json).expect("deserialises"),
            unavailable
        );
    }

    // -----------------------------------------------------------------
    // The witness
    // -----------------------------------------------------------------

    #[test]
    fn a_recorded_witness_says_unavailable_for_what_it_was_never_told() {
        let witness = RecordedWitness::new("fixture").with_transaction(truthful());
        let answers = witness.attest(&["sig1".to_string(), "sig2".to_string()]);

        assert_eq!(answers.len(), 2);
        assert!(matches!(answers[0].answer, WitnessAnswer::Found(_)));
        // Not `Absent`. A table that does not contain something has not
        // established that the chain does not either.
        assert!(matches!(
            answers[1].answer,
            WitnessAnswer::Unavailable { .. }
        ));
    }

    #[test]
    fn a_witness_is_asked_once_per_signature_however_many_edges_carry_it() {
        let mut second = edge();
        second.to = "puppet2".to_string();
        let witness = RecordedWitness::new("fixture").with_transaction(truthful());

        let attestations = collect_attestations(&[edge(), second], &[&witness]);
        assert_eq!(attestations.len(), 1, "one signature, one question");
    }

    #[test]
    fn collected_attestations_come_back_in_a_total_order() {
        let mut second = edge();
        second.signature = "sig0".to_string();
        let helius = RecordedWitness::new("helius").with_transaction(truthful());
        let quicknode = RecordedWitness::new("quicknode").with_absent("sig0");

        let attestations = collect_attestations(&[edge(), second], &[&helius, &quicknode]);
        let order: Vec<(&str, &str)> = attestations
            .iter()
            .map(|a| (a.signature.as_str(), a.provider.as_str()))
            .collect();
        assert_eq!(
            order,
            vec![
                ("sig0", "helius"),
                ("sig0", "quicknode"),
                ("sig1", "helius"),
                ("sig1", "quicknode"),
            ]
        );
    }

    // -----------------------------------------------------------------
    // Refusals
    // -----------------------------------------------------------------

    #[test]
    fn a_policy_that_could_not_fail_is_refused() {
        let no_quorum = VerificationPolicy {
            quorum: 0,
            ..VerificationPolicy::default()
        };
        assert!(matches!(
            verify_request(&[edge()], &[], &no_quorum),
            Err(EngineError::Forensics(_))
        ));

        let generous = VerificationPolicy {
            unverified_confidence_bps: 10_001,
            ..VerificationPolicy::default()
        };
        assert!(matches!(
            verify_request(&[edge()], &[], &generous),
            Err(EngineError::Forensics(_))
        ));
    }

    #[test]
    fn a_request_too_large_to_be_a_question_is_refused() {
        let edges = vec![edge(); MAX_EDGES + 1];
        assert!(matches!(
            verify_request(&edges, &[], &VerificationPolicy::default()),
            Err(EngineError::Forensics(_))
        ));
    }

    #[test]
    fn the_discount_is_integer_arithmetic_and_rounds_down() {
        // 999_999 millionths at half is 499_999.5, and the half goes to the
        // side that blocks.
        assert_eq!(discount(999_999, 5_000), 499_999);
        assert_eq!(discount(1_000_000, 10_000), 1_000_000);
        assert_eq!(discount(1_000_000, 0), 0);
    }
}
