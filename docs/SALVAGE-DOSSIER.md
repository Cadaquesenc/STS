# The deletion dossier

**Written 2026-08-27 so that binning ~40,000 lines is one review rather than an
afternoon of archaeology. Nothing has been deleted and nothing here recommends
deleting. This is the map; the decision is Ethan's.**

`docs/SALVAGE.md` says what is worth keeping and why. This says what depends on what,
so a split can be checked before it is made — a split that does not compile is not a
split. Every number below was recomputed from the tree rather than read off SALVAGE,
and the disagreements are flagged inline. Recovery from any of it is
`git checkout pre-salvage-2026-08-27`.

---

# STS deletion dossier — src-tauri/ and ui/

Everything below was recomputed from the snapshot tree, not read off SALVAGE. Where SALVAGE, W42 or W47 state a number I re-derived it; disagreements are flagged inline. Nothing was modified.

**Universe (recomputed):** Rust `105,987` lines = `src-tauri/src` 92,627 (incl. `build.rs` 3) + `src-tauri/tests` 13,360. W42's "106,000 lines of Rust" ✅ reconciles exactly. `ui/` = 17,619. Tauri/npm config = 152. **Total in scope: 123,758 lines.** Test attributes (`#[test]` + `#[tokio::test]`): **1,678** — 1,446 in `src/`, 232 in `tests/`. W42's "I counted 1,678 test attributes" ✅ reproduces to the digit.

---

## 1. Complete inventory

"Prod importers" = files under `src/` that reference `crate::<mod>`/`sts_lib::<mod>` in code **outside** a `#[cfg(test)]` module. Comment/doc-link mentions are excluded (this is where SALVAGE's import claims go wrong — see §3). "TM" = test-module-only importers. "IT" = integration-test files under `tests/`.

### `src-tauri/src/` — 35 files

| File | Lines | Tests | Prod importers (src/) | TM | IT | Bucket |
|---|---|---|---|---|---|---|
| `backtest.rs` | 7,429 | 100 | attribution, bundle, clustering, daemon, **fixed**, fixtures, jito, lib, main, mev_sim, entry, social, syndicate, **walkforward** (14) | 5 | 3 | **UNDECIDED** |
| `execution.rs` | 6,822 | 70 | attribution, **backtest**, bundle, daemon, engine, jito, journal, lib, entry (9) | 3 | 5 | BIN |
| `replay.rs` | 6,357 | 123 | attribution, backtest, daemon, error, execution, fixtures, forensics, lib, mev_sim, entry, social, syndicate, walkforward (13) | 7 | 4 | **KEEP** |
| `ingestion.rs` | 5,032 | 72 | daemon, execution, geyser, lib, loadgen (5) | 1 | 2 | **UNDECIDED** |
| `geyser.rs` | 4,878 | 70 | daemon, lib, loadgen (3) | 1 | 3 | **UNDECIDED** |
| `daemon.rs` | 4,334 | 44 | main (1) | 0 | 2 | BIN |
| `strategy/syndicate.rs` | 4,272 | 82 | via `strategy/mod.rs:99,116` (see §3) | — | — | BIN (freeze) |
| `forensics.rs` | 4,124 | 52 | daemon, **db**, lib (3) | engine | 2 | BIN |
| `attribution.rs` | 3,990 | 69 | **none** | 0 | replay_tests | BIN |
| `mev_sim.rs` | 3,405 | 59 | attribution (1) | attribution | replay_tests | BIN |
| `db.rs` | 3,349 | 33 | alerting, daemon, engine, execution, forensics, ingestion, journal, lib (8) | 3 | 6 | **UNDECIDED** |
| `alerting.rs` | 2,826 | 37 | engine, execution, lib (3) | 0 | 3 | BIN |
| `engine.rs` | 2,732 | 26 | daemon, lib (2) | 0 | 2 | **UNDECIDED** |
| `metrics.rs` | 2,688 | 53 | bundle, daemon, engine, execution, lib, **prometheus** (6) | prometheus | 3 | **UNDECIDED** |
| `clustering.rs` | 2,634 | 44 | lib (1) | 0 | forensics_tests | BIN (freeze) |
| `fixtures.rs` | 2,483 | 14 | **backtest** (1, production — `backtest.rs:3963`) | 0 | 3 | BIN |
| `journal.rs` | 2,441 | 30 | alerting, db, execution, forensics, lib (5) | 3 | 4 | **UNDECIDED** |
| `tracer.rs` | 2,331 | 51 | chainproof, clustering, lib (3) | clustering | forensics_tests | **KEEP** |
| `strategy/entry.rs` | 2,291 | 49 | via `strategy` (11 prod importers) | 3 | 7 | **UNDECIDED** |
| `types.rs` | 2,220 | 53 | 15 files incl. **`replay.rs:1792`** | 5 | 6 | **UNDECIDED** |
| `walkforward.rs` | 2,201 | 28 | **backtest** (1) | 0 | 0 | **KEEP core ~600 / BIN 1,601** |
| `fixed.rs` | 1,880 | 79 | jito, strategy/mod, social, syndicate (4) | 0 | sprint2 | **KEEP** |
| `bundle.rs` | 1,759 | 42 | lib (1) | 0 | 0 | BIN |
| `lib.rs` | 1,575 | 7 | — (crate root; `pub mod` ×29 at L75–103) | — | — | **UNDECIDED** |
| `chainproof.rs` | 1,526 | 30 | clustering, lib (2) | 0 | forensics_tests | BIN |
| `jito.rs` | 1,476 | 40 | bundle (1) | bundle | 0 | BIN |
| `subslot.rs` | 1,456 | 31 | geyser, loadgen (2) | lib, loadgen | 2 | **KEEP** |
| `loadgen.rs` | 1,390 | 7 | **none** | 0 | geyser_tests | BIN |
| `prometheus.rs` | 1,126 | 25 | metrics (1) | metrics | 0 | BIN |
| `strategy/social.rs` | 891 | 23 | via `strategy` | — | — | **UNDECIDED** |
| `telemetry.rs` | 466 | 3 | alerting, clustering, daemon, db, engine, forensics, geyser, ingestion, lib, metrics (10) | 4 | 6 | BIN |
| `strategy/mod.rs` | 123 | 0 | 11 prod importers incl. **`tracer.rs:75`**, **`walkforward.rs:59`** | 3 | 7 | **UNDECIDED** |
| `main.rs` | 60 | 0 | — (binary entry) | — | — | **UNDECIDED** (shell) |
| `error.rs` | 57 | 0 | chainproof, clustering, db, engine, execution, forensics, journal, lib (8) | 0 | 0 | **UNDECIDED** |
| `build.rs` | 3 | 0 | — | — | — | UNDECIDED (shell) |

### `src-tauri/tests/` — 12 files, 13,360 lines, 232 tests. **Nothing imports any of them** (cargo builds each as its own binary). SALVAGE assigns none of them to any bucket.

| File | Lines | Tests | File | Lines | Tests |
|---|---|---|---|---|---|
| `e2e_integration.rs` | 3,013 | 43 | `journal_execution.rs` | 1,048 | 8 |
| `replay_tests.rs` | 1,857 | 49 | `strategy_entry.rs` | 698 | 12 |
| `geyser_tests.rs` | 1,430 | 22 | `sprint2_contracts.rs` | 623 | 13 |
| `forensics_tests.rs` | 1,247 | 25 | `emergency_unwind.rs` | 608 | 10 |
| `journal_alerting.rs` | 1,036 | 12 | `strategy_tests.rs` | 604 | 14 |
| | | | `cursor_tests.rs` / `journal_forensics.rs` | 598 / 598 | 14 / 10 |

### `ui/` — 26 files, 17,619 lines. "Tests" = static `.ok/.eq/.near/.every` call sites.

| File | Lines | Asserts | Imported by | Bucket |
|---|---|---|---|---|
| `app.js` | 5,898 | 0 | `index.html:1519` | BIN |
| `styles.css` | 2,521 | 0 | `index.html:7` | BIN |
| `index.html` | 1,521 | 0 | **nothing** (served) | BIN |
| `test/engine.mjs` | 1,134 | 0 | `run.mjs:16` | BIN |
| `test/suites/cluster.mjs` | 731 | 71 | `run.mjs:35` | BIN |
| `test/suites/layout.mjs` | 562 | 22 | `run.mjs:20` | BIN |
| `test/suites/transport.mjs` | 469 | 56 | `run.mjs:27` | BIN |
| `test/suites/revisions.mjs` | 427 | 46 | `run.mjs:30` | BIN |
| `test/suites/ticks.mjs` | 369 | 62 | `run.mjs:23` | BIN |
| `test/suites/geyser.mjs` | 368 | 71 | `run.mjs:29` | BIN |
| `test/suites/journal.mjs` | 342 | 63 | `run.mjs:28` | BIN |
| `test/suites/aria.mjs` | 321 | 58 | `run.mjs:21` | BIN |
| `test/suites/unwind.mjs` | 295 | 37 | `run.mjs:32` | BIN |
| `test/suites/sorting.mjs` | 289 | 34 | `run.mjs:25` | BIN |
| `test/suites/bundles.mjs` | 288 | 39 | `run.mjs:34` | BIN |
| `test/suites/design.mjs` | 270 | 22 | `run.mjs:19` | BIN |
| `test/suites/replay.mjs` | 258 | 35 | `run.mjs:26` | BIN |
| **`test/cdp.mjs`** | **248** | 0 | `run.mjs:14` | **KEEP** |
| `test/suites/radar.mjs` | 247 | 26 | `run.mjs:31` | BIN |
| `test/suites/metrics.mjs` | 231 | 35 | `run.mjs:33` | BIN |
| `test/suites/curve.mjs` | 230 | 28 | `run.mjs:22` | BIN |
| `test/suites/sandwich.mjs` | 172 | 31 | `run.mjs:24` | BIN |
| `test/run.mjs` | 163 | 2 | **nothing** (`package.json` `"test"`) | BIN |
| `test/seed.mjs` | 147 | 0 | 13 suites | BIN |
| **`test/assert.mjs`** | **67** | 0 | `run.mjs:17` | **KEEP** |
| **`test/server.mjs`** | **51** | 0 | `run.mjs:15` | **KEEP** |

Static assert call sites total **738**. W42 says it counted 723; `run.mjs:137` says 812. **Could not reconcile any of the three** — 812 is a runtime count (loops), 723 I could not reproduce.

---

## 2. The three-way split, and where my count differs

| Bucket | Recomputed | SALVAGE says | Delta |
|---|---|---|---|
| **KEEP** | **12,990** — `replay.rs` 6,357 + `fixed.rs` 1,880 + `subslot.rs` 1,456 + `tracer.rs` 2,331 (=12,024) + walkforward core 600 + ui harness 366 | "about 9,000" + "~450" | **+3,990 (+44%)** |
| **BIN** | **61,642** — 15 named Rust files 42,633 + walkforward remainder 1,601 (=44,234) + `ui/` less harness 17,253 + shell config 152 + `build.rs` 3 | "roughly 40,000" | **+21,642 (+54%)** |
| **UNDECIDED** | **49,126** — 14 Rust `src/` files 35,766 + all 12 `tests/` files 13,360 | *no bucket at all* | — |

**The KEEP gap reconciles, and the reconciliation is the finding.** SALVAGE's "~9,000" is the sum of the *sub-components* it praises, not the files it says to keep: `CurveState` ~550 + record layer ~1,400 + walkforward core 600 + `fixed.rs` 1,880 + `subslot.rs` 1,456 + `tracer.rs` 2,331 + ui harness 366 = **8,583 ≈ 9,000**. But `SALVAGE.md:56` says of `replay.rs` "**Keep whole.**" — and whole is 6,357, not ~1,950. The heading and the instruction four lines below it disagree by 4,407 lines.

**The BIN gap is an internal contradiction in SALVAGE.** Its own bin table's Rust rows sum to **42,633** (42,587 at the tag, before W47's header comments), and the table then adds a `Tauri shell + ui/ | ~12,000` row — so the table as written totals **54,587** under a heading that says "roughly 40,000". Separately, that `~12,000` row is itself low: `ui/` alone is **17,619**, of which 366 is kept, leaving **17,253** plus 152 of shell config.

**The largest single fact: 49,126 lines — 40% of the tree — are in neither bucket.** SALVAGE never mentions `backtest.rs` (7,429, the second-largest file in the crate), `ingestion.rs`, `geyser.rs`, `db.rs`, `engine.rs`, `metrics.rs`, `journal.rs`, `types.rs`, `lib.rs`, `error.rs`, the `strategy/` facade, `entry.rs`, `social.rs`, or **any of the 12 integration-test files** (13,360 lines, 232 of the 1,678 tests). W42 does say the walkforward remainder is "welded to `backtest.rs` and go[es] wherever it goes" — but never says where `backtest.rs` goes.

**Anchor that does reconcile:** the tree total. 92,627 src + 13,360 tests = **105,987**, W42's "about 106,000 lines of Rust" ✅.

---

## 3. BIN blast radius — what breaks, and the four places the split does not compile

### 3a. Per-file: what would break

| BIN file | Lines | Breaks in `src/` (production) | Breaks in `tests/` | Notes |
|---|---|---|---|---|
| `attribution.rs` | 3,990 | **nothing** | `replay_tests.rs:40` | + `lib.rs:76` decl |
| `loadgen.rs` | 1,390 | **nothing** | `geyser_tests.rs:21,102,103` | + `lib.rs:93` decl |
| `mev_sim.rs` | 3,405 | `attribution.rs:107` (itself unreachable) | `replay_tests.rs:56` | + `lib.rs:95` decl |
| `jito.rs` | 1,476 | `bundle.rs:60` (also BIN) | none | + `bundle.rs:953` TM |
| `bundle.rs` | 1,759 | `lib.rs:114` | none | UI panel `bundles.mjs` loses its feed |
| `prometheus.rs` | 1,126 | `metrics.rs:1572,1693` — **metrics is UNDECIDED** | none | + `metrics.rs:2566` TM |
| `clustering.rs` | 2,634 | `lib.rs:116` | `forensics_tests.rs:13` | |
| `chainproof.rs` | 1,526 | `clustering.rs:82` (BIN), `lib.rs:115,557` | `forensics_tests.rs:12` | |
| `daemon.rs` | 4,334 | `main.rs:41,42` | `e2e_integration.rs:55,195,1398`, `journal_forensics.rs:30` | |
| `fixtures.rs` | 2,483 | `backtest.rs:3963` — **production**, inside `pub mod cli {` at :3959; the file's only `#[cfg(test)]` is :4799 | `e2e_integration.rs:61`, `journal_forensics.rs:35`, `replay_tests.rs:50,101` | **W47's claim verified** |
| `alerting.rs` | 2,826 | `engine.rs:19` (UND), `execution.rs:60` (BIN), `lib.rs:112` | 3 files | |
| `forensics.rs` | 4,124 | `daemon.rs:76,2284` (BIN), **`db.rs:639` (UND)**, `lib.rs:124,1156` | 2 files | |
| `strategy/syndicate.rs` | 4,272 | `strategy/mod.rs:99` `pub mod syndicate;` + `:116` `pub use syndicate::{…}` (30 names) | 7 files via `strategy` | **See break #1** |
| `execution.rs` | 6,822 | attribution(BIN), **`backtest.rs:51`**, bundle(BIN), daemon(BIN), **`engine.rs:22`**, jito(BIN), **`journal.rs:68`**, **`lib.rs:123`**, **`strategy/entry.rs:62`** | 5 files | 5 UNDECIDED modules break |
| `telemetry.rs` | 466 | **10 prod importers**: alerting, clustering, daemon, forensics (BIN) + **`db.rs:1256`, `engine.rs:24`, `geyser.rs:90`, `ingestion.rs:48`, `lib.rs:139,1159`, `metrics.rs:63`** (UND) | 6 files | 466 lines, widest reach in the crate |
| walkforward remainder | 1,601 | `backtest.rs:59,3964` | none | inseparable from the kept 600 without a file split |

**The chokepoint is `lib.rs` (1,575 lines, UNDECIDED).** It `use`s **8 of the 15 BIN modules** in production code — `alerting:112`, `bundle:114`, `chainproof:115`, `clustering:116`, `execution:123`, `forensics:124`, `telemetry:139` — plus `pub mod` declarations for all 29 at L75–103. **Deleting the bin is not a file-deletion operation; it is a rewrite of `lib.rs`'s Tauri command surface.** SALVAGE does not say this.

### 3b. Four places a KEEP file depends on a BIN file — the split does not compile as written

| # | KEEP file | Exact line | Reaches | Consequence |
|---|---|---|---|---|
| **1** | `tracer.rs` (2,331) | `src-tauri/src/tracer.rs:75` — `use crate::strategy::fixed::{exp_neg, Fixed};` | `strategy/mod.rs:107` `pub use crate::fixed;` — but `mod.rs:99` declares `pub mod syndicate;` and `:116` `pub use syndicate::{…}` | **Deleting `syndicate.rs` (BIN, 4,272) breaks `strategy/mod.rs`, which breaks `tracer.rs` (KEEP).** One-line fix: repoint :75 to `use crate::fixed::{exp_neg, Fixed};` |
| **2** | `fixed.rs` (1,880) | `src-tauri/src/fixed.rs:48` — `use crate::backtest::{isqrt, mul_div_floor, mul_div_round, MICROS};` (+ `:1691` `crate::backtest::exp_neg_micros`, test module) | `backtest.rs:51` `use crate::execution::TipPolicy;` and `backtest.rs:3963` `use crate::fixtures::{…}` | **`fixed.rs` (KEEP) → `backtest.rs` (UNDECIDED) → `execution.rs` (BIN, 6,822) + `fixtures.rs` (BIN, 2,483).** W42's "copy the file plus four small integer helpers" is 4 in the `use` + 1 more in the test mod = **5 symbols** |
| **3** | `walkforward.rs` (KEEP core 600) | `:52` imports **23** symbols from `crate::backtest`; `:58` `crate::replay::BPS_DENOMINATOR`; `:59` `use crate::strategy::entry::{GAP_BUCKETS_BPS, SLIPPAGE_BUCKETS_BPS};` | same `backtest`→BIN chain, plus the `strategy/mod.rs`→`syndicate.rs` chain of break #1 | The **file** needs 23 backtest symbols. W42's "the split core needs only `mul_div_floor` and one constant" describes the ~600-line subset, which does not exist as a file |
| **4** | `replay.rs` (6,357) | `src-tauri/src/replay.rs:1792` — `pub const DEFAULT_MAX_POOL_SHARE_BPS: u16 = crate::types::MAX_POOL_SHARE_BPS;` | `types.rs` (2,220, **UNDECIDED**) | Not a break if `types.rs` travels with it — **but it falsifies `SALVAGE.md:55`.** `replay.rs` has no `use crate::` *statement*; it does have a crate path reference in code |

Verified clean in the other direction: `subslot.rs` and `types.rs` have **zero** `crate::`/`sts_lib::` references of any kind, comments included. `ui/test/{cdp,assert,server}.mjs` import **only `node:` builtins** — no project import at all; that 366-line lift is genuinely free.

BIN→KEEP references (harmless — they vanish with the deleted file): `jito.rs:71`→fixed, `loadgen.rs:81`→subslot, `clustering.rs:87`+`chainproof.rs:85`→tracer, and `replay` is imported by attribution/execution/fixtures/mev_sim/daemon/forensics.

### 3c. Exact drop-in substitutions for SALVAGE.md

`SALVAGE.md:55` — replace
`### \`replay.rs\` — 6,357 lines, 123 tests, zero imports from the rest of the crate`
with
`### \`replay.rs\` — 6,357 lines, 123 tests, one reference to the rest of the crate`

`SALVAGE.md:276–279` — replace
```
1. **Extract the keep pile first, delete nothing yet.** `replay.rs`, `subslot.rs`
   and `types.rs` have no crate imports at all; `tracer.rs` and `fixed.rs` have one
   each. The audit prices the whole extraction at under two days. Do it while the
   tree still builds.
```
with
```
1. **Extract the keep pile first, delete nothing yet.** `subslot.rs` and `types.rs`
   have no crate references at all; `replay.rs`, `tracer.rs` and `fixed.rs` have one
   each. Two of those three point into code this document bins, so the split does not
   compile as written: `tracer.rs:75` reaches `crate::fixed` through
   `strategy/mod.rs`, which `pub use`s `syndicate.rs`, and `fixed.rs:48` takes four
   symbols from `backtest.rs`, which imports `execution.rs` and `fixtures.rs`.
   `replay.rs:1792` reads one constant from `types.rs`. Repoint all three first. The
   audit prices the whole extraction at under two days. Do it while the tree still
   builds.
```

`SALVAGE.md:53` — replace `## Keep — about 9,000 lines` with `## Keep — 12,990 lines as files, ~8,600 as named components`

`SALVAGE.md:169` — replace `## Bin — roughly 40,000 lines` with `## Bin — 44,234 lines of Rust, plus 17,405 of \`ui/\` and shell`

`SALVAGE.md:186` — replace `| Tauri shell + \`ui/\` | ~12,000 | Template-grade config; keep only the 450-line test harness |` with `| Tauri shell + \`ui/\` | 17,405 | Template-grade config; keep only the 366-line test harness |`

---

## 4. The cheapest deletions

**No module in `src/` has zero inbound references.** Two have zero *production* references, which is the meaningful bar:

| Rank | What | Lines | Cost to delete |
|---|---|---|---|
| 1 | `attribution.rs` + `mev_sim.rs` + `loadgen.rs` | **8,785** (8,739 at the tag, pre-header) | 3 `pub mod` lines out of `lib.rs` (L76, 93, 95); 2 test files edited (`replay_tests.rs:40,56`; `geyser_tests.rs:21,102,103`). **Zero production references anywhere.** Costs 135 test attributes. **W47's 8,739 independently reproduced** |
| 2 | `jito.rs` + `bundle.rs` | 3,235 | 1 line out of `lib.rs` (L114). No `tests/` file touches either. `ui/test/suites/bundles.mjs` (288 lines, 39 asserts) tests the orphaned panel |
| 3 | `prometheus.rs` | 1,126 | 3 sites in `metrics.rs` (:1572, :1693, :2566) |
| 4 | `clustering.rs` | 2,634 | `lib.rs:116` + `forensics_tests.rs:13`; but `chainproof.rs:82` also imports it, so pair them (4,160 together, + `lib.rs:115,557`) |

**True leaves — nothing in the tree imports them:** all **12 `src-tauri/tests/*.rs`** (13,360 lines, 232 tests — cargo compiles each as its own binary), `ui/test/run.mjs` (163, invoked by `package.json` `"test"`), `ui/index.html` (1,521, served). Deleting any of these cannot break a compile. They are the largest zero-risk block in the tree and **SALVAGE assigns none of them.**

---

## 5. Recovery path — verified

| Check | Result |
|---|---|
| `git rev-parse pre-salvage-2026-08-27` | `3a67b182fe06b8eb3ee0c2063840718676fc0e0f` — **annotated tag object** |
| Peels to commit | `29260467255f759cdaf525943a987cba4177ef05` — SALVAGE's `2926046` ✅ |
| Tagger / message | `Ethan Giannaros 2026-08-27 05:06:39 +0200` — "Full tree before any salvage action (W47, 2026-08-27)." |
| Pushed to origin | ✅ `git ls-remote --tags origin` returns both `refs/tags/pre-salvage-2026-08-27` and its `^{}` peel, matching the local SHAs |
| Ancestor of `origin/main` | ✅ `git merge-base --is-ancestor` returns 0 |
| Files in tree | **168** |
| Contains every file in this dossier | ✅ **34/34** `src-tauri/src/*.rs`, **12/12** `src-tauri/tests/*.rs`, **26/26** `ui/` files |
| Line counts in tag vs snapshot | Identical for **every file** except `attribution.rs` 3,976→3,990, `mev_sim.rs` 3,387→3,405, `loadgen.rs` 1,376→1,390 |
| `git diff --stat pre-salvage-2026-08-27 origin/main -- src-tauri ui` | **3 files changed, 46 insertions(+), 0 deletions** — exactly W47's three header comments. **No other change to `src-tauri/` or `ui/` since the tag.** |

**Recovery is sound.** `git checkout pre-salvage-2026-08-27` restores the complete pre-salvage tree; every file listed in §1 is in it. The only content in the current tree that the tag does *not* hold is the 46 lines of header comment and, outside this dossier's scope, `docs/` and `tools/capture/` work (587 files, +65,899 lines across the whole diff).

**One caveat on the recovery path.** The tag preserves the tree, not the working state — `data/` is gitignored and is not in it (168 files total; W78/W82's 106 MB of captures is on one disk with a partial archive). Deleting Rust is fully reversible; that is a separate exposure and is unchanged by any decision here.

---

## Could not verify

- **Nothing was compiled.** No cargo was run (hard rule). Every "breaks" in §3 is derived from grepping code-only `crate::`/`sts_lib::` references and reading the exact import lines; the three named breaks are read off `use` statements and `pub use` re-exports, not from a failing build. A `cargo check` after each cut is the only thing that settles it.
- **W42's 723 UI assertion count and `run.mjs:137`'s 812** — I get **738** static call sites; none of the three reconcile.
- **W42's per-file "tests" figures for the bin pile** (`jito`+`bundle` 82, `mev_sim` 59, `execution` 70) — I get 40+42=82 ✅, 59 ✅, 70 ✅. But W42's `walkforward` "28 tests" ✅ and `replay` "123" ✅, `fixed` "79" ✅, `subslot` "31" ✅, `tracer` "51" ✅ all reproduce, so its counting method is sound.
- **`tools/capture/`** (26 files, 9,688 JS lines) is out of the requested scope and is not in any bucket above. W42 §6 keeps "the method, not the recorder"; `SALVAGE.md` drops that section entirely. It has moved substantially since the tag (W70's `906e1e7`).