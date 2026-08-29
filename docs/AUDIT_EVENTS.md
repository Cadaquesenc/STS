# STS audit events

> **STATUS NOTE — 27 August 2026.** This document describes the audit logger of
> the archived Node implementation (`src/audit.js`), which now lives under
> `docs/archive/legacy-node/`. It is not a description of the Rust engine's audit
> trail. It is kept for provenance. Note also that no audit event here records
> decode validity: 18.4% of captured coins carry a corrupt `virtualSolReserves`
> and nothing in this scheme would have said so. See
> [`VERDICT-2026-08-27.md`](VERDICT-2026-08-27.md).

All cross-module and contributor activity that needs operational or provenance tracking belongs in the shared `AuditLogger` (`src/audit.js`). Do not create per-feature log files.

## Format

Audit output is newline-delimited JSON in `$STS_HOME` (or the configured data directory), named `audit-YYYY-MM-DD[-N].ndjson`. Each line is one object:

`schema` (`sts.audit`), `version`, unique `id`, UTC `ts`, `level`, `type`, `action`, `actor`, and JSON `data`.

Required fields are stable; new information goes under `data`. Never put secrets, tokens, or unbounded raw payloads in audit data. Keep raw blockchain payloads in the existing data records.

## Event types

Use `socket` for connection lifecycle and gaps, `record` for durable record writes, `decode` for parse/decode failures or schema drift, `dashboard` for server lifecycle, and `error` for failures that do not fit another type. Actions should be short past-tense or noun phrases such as `connect`, `disconnect`, `gap`, `append`, and `json_error`.

### On the Rust side

`db.rs` has no `AuditLogger` of its own yet, so it writes the mirror directly:
one row in `sts.db`'s `audit_log` table, with an `event_type` naming what
happened and a JSON `payload`. The NDJSON file above remains the record of first
resort; these rows are what a person greps after an incident, and the list is
short enough to state in full.

| `event_type` | written by | means |
| --- | --- | --- |
| `kill_switch` | `Engine::arm_kill_switch` | the switch was pulled, and when |
| `emergency_unwind` | `Engine::emergency_unwind` | an unwind was asked for, and what was held when it was |
| `warm_start_unclean` | `forensics::verify_on_start` | the book does not match its own checkpoints — a broken chain link, or a divergence under a standing revision |

`warm_start_unclean` is the one nothing in this build can cause. Every write path
into `journal_snapshots` is append-only behind a trigger and every digest is
recomputed from the row it covers, so a row that does not verify was edited by
something outside this process. The payload carries the mode, the counter's
current value, and every broken link with the reason it broke; the check repairs
nothing, because a guess at what the numbers should have been is worth less than
an accurate record that they are wrong.

## Rotation and writing rules

The logger rotates at UTC midnight and at 50 MiB by default. Use `emit(type, action, data, options)`; do not call `appendFileSync`, write a second audit format, or silently swallow write errors. Pass the logger into modules using their `audit` option. Close it during shutdown. Event data must be JSON-serializable and bounded.

## Performance review notes

- `Records.write` uses one serialization and a persistent stream, and now exposes backpressure rather than doing synchronous file I/O.
- `Socket.stop` cancels pending reconnects, avoiding orphan timers and duplicate connections; socket lifecycle and gaps are auditable.
- `Store` already keeps `byMint` and `byWallet` indexes. Its remaining synchronous tail reads are intentionally bounded by file growth and refresh throttling; a future async Store migration should preserve those indexes and partial-line handling.
- `borsh` already uses zero-copy `subarray` for byte fields and decodes each payload once; avoid converting raw buffers to hex/base64 more than once.

Contributors should add tests for event schema, rotation, partial writes, reconnect cancellation, and backpressure before changing these contracts.
