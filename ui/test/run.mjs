#!/usr/bin/env node
// The headless UI suite.
//
//   node ui/test/run.mjs            every suite
//   node ui/test/run.mjs curve aria  the named ones
//
// Exit code is the number of failed assertions, capped at 1, so this is usable
// as a gate rather than only as a report.

import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

import { Browser } from "./cdp.mjs";
import { serve } from "./server.mjs";
import { FAKE_ENGINE } from "./engine.mjs";
import { Recorder } from "./assert.mjs";

import design from "./suites/design.mjs";
import layout from "./suites/layout.mjs";
import aria from "./suites/aria.mjs";
import curve from "./suites/curve.mjs";
import ticks from "./suites/ticks.mjs";
import sandwich from "./suites/sandwich.mjs";
import sorting from "./suites/sorting.mjs";
import replay from "./suites/replay.mjs";
import transport from "./suites/transport.mjs";
import journal from "./suites/journal.mjs";
import geyser from "./suites/geyser.mjs";
import revisions from "./suites/revisions.mjs";
import radar from "./suites/radar.mjs";
import unwind from "./suites/unwind.mjs";
import metrics from "./suites/metrics.mjs";
import bundles from "./suites/bundles.mjs";
import cluster from "./suites/cluster.mjs";

const SUITES = [
  design,
  layout,
  aria,
  radar,
  curve,
  ticks,
  sandwich,
  sorting,
  replay,
  transport,
  journal,
  geyser,
  revisions,
  unwind,
  metrics,
  bundles,
  cluster,
];

const here = dirname(fileURLToPath(import.meta.url));
const uiRoot = resolve(here, "..");

const wanted = process.argv.slice(2);
const selected = wanted.length
  ? SUITES.filter((suite) => wanted.includes(suite.name))
  : SUITES;

if (selected.length === 0) {
  console.error(`no such suite. known: ${SUITES.map((s) => s.name).join(", ")}`);
  process.exit(1);
}

const server = await serve(uiRoot);
const browser = await Browser.launch();
const all = [];
let crashed = 0;

for (const suite of selected) {
  const recorder = new Recorder(suite.name);
  const page = await browser.newPage();
  try {
    await page.onNewDocument(FAKE_ENGINE);
    await page.setViewport(1440, 900);
    page.origin = server.origin;
    await page.goto(server.origin);
    await page.settle();
    await suite.run(recorder, page);

    // An uncaught exception in the window is a failure of the window whatever
    // the assertions above concluded.
    recorder.every(
      "the window threw nothing",
      page.exceptions,
      () => false,
      (message) => String(message).split("\n")[0],
    );
  } catch (error) {
    crashed += 1;
    recorder.ok(`suite ran to completion`, false, String(error?.stack ?? error));
  } finally {
    await page.close();
  }
  all.push(...recorder.results);
}

await browser.close();
await server.close();

// --- the report ------------------------------------------------------------

let currentSuite = null;
for (const result of all) {
  if (result.suite !== currentSuite) {
    currentSuite = result.suite;
    console.log(`\n  ${currentSuite}`);
  }
  const mark = result.passed ? "  ok  " : "  FAIL";
  console.log(`${mark}  ${result.label}${result.detail ? `\n          ${result.detail}` : ""}`);
}

const failed = all.filter((result) => !result.passed);
console.log(
  `\n  ${all.length - failed.length}/${all.length} assertions passed` +
    (failed.length ? `, ${failed.length} failed` : "") +
    (crashed ? `, ${crashed} suite(s) crashed` : ""),
);

/// Which tree produced the number above.
///
/// This runner builds the working tree and says nothing about it, which makes
/// every figure it prints a silent claim about an uncommitted snapshot. On
/// 2026-08-25 three sessions in this repo independently quoted working-tree
/// counts as though they were properties of a commit — not three lapses, but
/// one missing line, because nothing was visible at the moment the number was
/// copied into a report.
///
/// Scoped to `ui/` deliberately. This suite exercises the window and nothing
/// else, so `ui/` being clean is exactly the condition under which its total is
/// reproducible from the named commit; a dirty `src-tauri/` says nothing about
/// whether 812 is 812 again tomorrow.
function provenance(root) {
  const git = (...args) =>
    execFileSync("git", args, {
      cwd: root,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }).trim();
  try {
    const commit = git("rev-parse", "--short", "HEAD");
    const dirty = git("status", "--porcelain", "--", root).split("\n").filter(Boolean);
    return dirty.length === 0
      ? `at ${commit}, ui/ clean — reproducible from that commit`
      : `at ${commit} plus ${dirty.length} uncommitted file${dirty.length === 1 ? "" : "s"}` +
          ` under ui/ — this number describes the working tree, not the commit`;
  } catch {
    // No git, no repo, a tarball. The suite still ran and its total still
    // stands; it just cannot say where from, and an unanswerable question is
    // not a reason to fail a green run.
    return null;
  }
}

const where = provenance(uiRoot);
if (where) console.log(`  ${where}`);

process.exit(failed.length === 0 ? 0 : 1);
