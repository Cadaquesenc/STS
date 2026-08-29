// The 0x100x rules, checked against what the layout engine actually computed.
//
// Every assertion here is made over the live document with rows, badges, the
// replay bar and both modals on screen, because a rule that only holds on an
// empty shell is a rule that holds nowhere it matters.

import { LAMPORTS, observation, push, selectFirst, enableReplay } from "../seed.mjs";

/// The ground, and the spec's hairline.
const GROUND = [10, 10, 10];
const HAIRLINE = 0x1a;

/// How far a composited hairline may sit from #1a1a1a and still be one.
///
/// The two border tokens land at #19 (`--border-soft`, 6% white) and #23
/// (`--border`, 10% white) once composited on the ground. Both read as the same
/// line at any size a person looks at them; ten levels is the band that admits
/// exactly those two and rejects a third that is visibly lighter or darker.
const HAIRLINE_TOLERANCE = 10;

/// The four colours that are allowed to mean something, and white.
const PALETTE = [
  [255, 255, 255],
  [74, 222, 128], // --live
  [251, 191, 36], // --warn
  [248, 113, 113], // --halt
  [96, 165, 250], // --accent
  [252, 165, 165], // the kill switch's active fill
  [10, 10, 10], // the ground, as text on a filled control
];

/// The separators. Everything in this list draws a line between two regions of
/// the layout rather than around a piece of state, and it is those lines the
/// hairline rule is about.
const STRUCTURAL = [
  ".topbar",
  ".statusbar",
  ".pane",
  ".pane-head",
  ".pane-tools",
  ".col-head",
  ".row",
  ".section-head",
  ".curve-module",
  ".metric",
  ".gov-row",
  ".subject",
  ".tick",
  ".modal-head",
  ".modal-actions",
  ".detail-grid dt",
  ".detail-grid dd",
  ".filter-input",
];

export default {
  name: "design",
  async run(t, page) {
    // Fill the window: rows in every pane, a subject in the inspector, the
    // replay bar up, and the tick detail open.
    await enableReplay(page);
    await push(page, [
      observation({ index: 0, slot: 312_905_100, realSol: 62.65 * LAMPORTS, mcap: 92.65 * LAMPORTS }),
      observation({ index: 0, slot: 312_905_140, realSol: 63.96 * LAMPORTS, mcap: 94.58 * LAMPORTS }),
      observation({ index: 1, slot: 312_905_150, realSol: 8.2 * LAMPORTS, mcap: 38.2 * LAMPORTS }),
    ]);
    await selectFirst(page);
    await page.evaluate(() => {
      document.querySelector('[data-region="tick-rows"] .row')?.dispatchEvent(
        new MouseEvent("dblclick", { bubbles: true }),
      );
    });
    await page.settle();

    // --- the ground and the type -------------------------------------------
    const base = await page.evaluate(() => {
      const style = getComputedStyle(document.body);
      return {
        background: style.backgroundColor,
        fontFamily: style.fontFamily,
        fontSize: style.fontSize,
        letterSpacing: style.letterSpacing,
        numeric: style.fontVariantNumeric,
        colorScheme: getComputedStyle(document.documentElement).colorScheme,
      };
    });

    t.eq("the ground is #0a0a0a", base.background, "rgb(10, 10, 10)");
    t.ok(
      "Inter is the first face asked for",
      /^Inter\b/.test(base.fontFamily),
      base.fontFamily,
    );
    t.eq("body type is 13px", base.fontSize, "13px");
    t.near(
      "tracking is -0.04em",
      Number.parseFloat(base.letterSpacing),
      -0.04 * Number.parseFloat(base.fontSize),
      0.01,
      `${base.letterSpacing} on ${base.fontSize}`,
    );
    t.ok(
      "numerals are tabular",
      base.numeric.includes("tabular-nums"),
      base.numeric,
    );
    t.eq("the window declares a dark colour scheme", base.colorScheme, "dark");

    // --- radius, shadow, and everything drawn to lift off the page ---------
    const surfaces = await page.evaluate(() => {
      const offenders = { radius: [], shadow: [], textShadow: [], filter: [], width: [] };
      const describe = (el) =>
        `${el.tagName.toLowerCase()}${el.className && typeof el.className === "string" ? "." + el.className.trim().split(/\s+/).join(".") : ""}`;

      for (const el of document.querySelectorAll("*")) {
        const style = getComputedStyle(el);

        for (const corner of [
          "borderTopLeftRadius",
          "borderTopRightRadius",
          "borderBottomLeftRadius",
          "borderBottomRightRadius",
        ]) {
          if (Number.parseFloat(style[corner]) !== 0) {
            offenders.radius.push(`${describe(el)} ${corner}=${style[corner]}`);
            break;
          }
        }

        // An inset shadow is a marker drawn inside the box and cannot change
        // its size. An outer one is the drop shadow the design does not have.
        const shadow = style.boxShadow;
        if (shadow && shadow !== "none") {
          const layers = shadow.split(/,(?![^(]*\))/);
          if (layers.some((layer) => !layer.includes("inset"))) {
            offenders.shadow.push(`${describe(el)} ${shadow}`);
          }
        }

        if (style.textShadow && style.textShadow !== "none") {
          offenders.textShadow.push(`${describe(el)} ${style.textShadow}`);
        }
        if (style.filter && style.filter.includes("drop-shadow")) {
          offenders.filter.push(`${describe(el)} ${style.filter}`);
        }

        for (const side of ["Top", "Right", "Bottom", "Left"]) {
          const width = Number.parseFloat(style[`border${side}Width`]);
          if (width !== 0 && width !== 1) {
            offenders.width.push(`${describe(el)} border${side}=${style[`border${side}Width`]}`);
          }
        }
      }
      return offenders;
    });

    t.every("no element has a border radius", surfaces.radius, () => false, (x) => x);
    t.every("no element casts an outer shadow", surfaces.shadow, () => false, (x) => x);
    t.every("no element has a text shadow", surfaces.textShadow, () => false, (x) => x);
    t.every("no element has a drop-shadow filter", surfaces.filter, () => false, (x) => x);
    t.every("every border is 0 or exactly 1px", surfaces.width, () => false, (x) => x);

    // --- the palette --------------------------------------------------------
    const borders = await page.evaluate(() => {
      const seen = [];
      const describe = (el) =>
        `${el.tagName.toLowerCase()}${el.className && typeof el.className === "string" ? "." + el.className.trim().split(/\s+/).join(".") : ""}`;
      for (const el of document.querySelectorAll("*")) {
        const style = getComputedStyle(el);
        for (const side of ["Top", "Right", "Bottom", "Left"]) {
          if (Number.parseFloat(style[`border${side}Width`]) === 0) continue;
          seen.push({ el: describe(el), color: style[`border${side}Color`], side });
        }
      }
      return seen;
    });

    const parse = (value) => {
      const parts = value.match(/[\d.]+/g)?.map(Number) ?? [];
      return { r: parts[0] ?? 0, g: parts[1] ?? 0, b: parts[2] ?? 0, a: parts[3] ?? 1 };
    };
    const inPalette = ({ r, g, b }) =>
      PALETTE.some(([pr, pg, pb]) => Math.abs(pr - r) <= 2 && Math.abs(pg - g) <= 2 && Math.abs(pb - b) <= 2);

    // A fully transparent border draws no line. It is there so that a control
    // gaining a real border when it is pressed does not gain a pixel of height
    // with it, which is the same discipline the rest of this suite is about.
    const drawn = borders.filter((border) => parse(border.color).a > 0);
    t.ok(
      "transparent borders are reserved space, not lines",
      borders.length - drawn.length > 0,
      `${borders.length - drawn.length} of ${borders.length} sides draw nothing`,
    );
    t.every(
      "every border colour that draws comes from the palette",
      drawn,
      (border) => inPalette(parse(border.color)),
      (border) => `${border.el} ${border.side}=${border.color}`,
    );

    // --- the hairline -------------------------------------------------------
    const structural = await page.evaluate((selectors) => {
      const seen = [];
      const describe = (el) =>
        `${el.tagName.toLowerCase()}${el.className && typeof el.className === "string" ? "." + el.className.trim().split(/\s+/).join(".") : ""}`;
      for (const selector of selectors) {
        for (const el of document.querySelectorAll(selector)) {
          const style = getComputedStyle(el);
          for (const side of ["Top", "Right", "Bottom", "Left"]) {
            const width = Number.parseFloat(style[`border${side}Width`]);
            if (width === 0) continue;
            if (style[`border${side}Color`].endsWith(", 0)")) continue;
            seen.push({
              el: describe(el),
              selector,
              side,
              width,
              color: style[`border${side}Color`],
              style: style[`border${side}Style`],
            });
          }
        }
      }
      return seen;
    }, STRUCTURAL);

    t.ok("structural separators were found to check", structural.length > 20, `${structural.length} sides`);
    t.every(
      "every structural separator is 1px",
      structural,
      (border) => border.width === 1,
      (border) => `${border.el} ${border.side}=${border.width}px`,
    );

    const composite = ({ r, g, b, a }) => [
      a * r + (1 - a) * GROUND[0],
      a * g + (1 - a) * GROUND[1],
      a * b + (1 - a) * GROUND[2],
    ];

    t.every(
      `every structural separator composites within ±${HAIRLINE_TOLERANCE} of #1a1a1a`,
      structural,
      (border) => {
        const [r, g, b] = composite(parse(border.color));
        return [r, g, b].every((channel) => Math.abs(channel - HAIRLINE) <= HAIRLINE_TOLERANCE);
      },
      (border) =>
        `${border.el} ${border.side} ${border.color} → ${composite(parse(border.color)).map((c) => Math.round(c)).join(",")}`,
    );

    // --- the modal is flat too ---------------------------------------------
    const modal = await page.evaluate(() => {
      const panel = document.querySelector('[data-region="tick-modal"] .modal-panel');
      if (!panel) return null;
      const style = getComputedStyle(panel);
      return {
        shadow: style.boxShadow,
        radius: style.borderTopLeftRadius,
        width: style.borderTopWidth,
        background: style.backgroundColor,
      };
    });
    t.ok("the tick detail is on screen to check", modal !== null);
    t.eq("the modal panel casts no shadow", modal?.shadow, "none");
    t.eq("the modal panel has no radius", modal?.radius, "0px");
    t.eq("the modal panel is a 1px hairline", modal?.width, "1px");
    t.eq("the modal panel sits on the ground colour", modal?.background, "rgb(10, 10, 10)");
  },
};
