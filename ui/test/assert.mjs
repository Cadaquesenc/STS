// The assertion record.
//
// Every check made by every suite lands here with the label it was made under,
// so the run ends with a list of what was actually verified rather than a count
// of how many times nothing went wrong. A suite that silently skips half its
// checks looks identical to one that ran them all if the only output is "ok".

export class Recorder {
  constructor(suite) {
    this.suite = suite;
    this.results = [];
  }

  #record(label, passed, detail) {
    this.results.push({ suite: this.suite, label, passed, detail });
    return passed;
  }

  ok(label, condition, detail = "") {
    return this.#record(label, condition === true, condition === true ? detail : detail);
  }

  eq(label, actual, expected, detail = "") {
    const passed = Object.is(actual, expected);
    return this.#record(
      label,
      passed,
      passed ? detail : `expected ${format(expected)}, got ${format(actual)}${detail ? ` — ${detail}` : ""}`,
    );
  }

  /// Equality with a tolerance, for anything that came out of a layout engine.
  near(label, actual, expected, tolerance, detail = "") {
    const passed =
      Number.isFinite(actual) && Math.abs(actual - expected) <= tolerance;
    return this.#record(
      label,
      passed,
      passed
        ? detail
        : `expected ${format(expected)} ±${tolerance}, got ${format(actual)}${detail ? ` — ${detail}` : ""}`,
    );
  }

  /// Every member of a list has to hold. The failure names the members that did
  /// not, because "12 of 40 headings clipped" is not an actionable sentence.
  every(label, items, predicate, describe = (item) => JSON.stringify(item)) {
    const failures = items.filter((item) => !predicate(item));
    return this.#record(
      label,
      failures.length === 0,
      failures.length === 0
        ? `${items.length} checked`
        : `${failures.length} of ${items.length} failed: ${failures.slice(0, 6).map(describe).join("; ")}${failures.length > 6 ? " …" : ""}`,
    );
  }

  get failed() {
    return this.results.filter((result) => !result.passed);
  }
}

function format(value) {
  if (typeof value === "string") return JSON.stringify(value);
  if (typeof value === "number") return String(value);
  return JSON.stringify(value);
}
