/**
 * String-only validation for exact numeric cells (T-019 / R-005, D-006).
 *
 * D-006 ships `int8` and `numeric` to the webview as tagged strings because a
 * JavaScript number silently rounds them. R-005 is the observation that the
 * read-side protection is worthless if the *edit* side routes the value through
 * `parseFloat` or `<input type="number">` on its way back — the value survives
 * display and is corrupted on write.
 *
 * So everything here operates on the text. Validation is a regex for shape plus
 * a digit-wise comparison for range; no value is ever converted to a `number`
 * (or a `BigInt` — it would be exact, but the rule is simpler to audit as "the
 * digits are never converted"). `scripts/check-exact-numeric.mjs` enforces that
 * mechanically on this file and on the editor.
 */
import type { ExactDomain, WireValue } from "./contract";

export type NumericWire = Extract<WireValue, { t: "numeric" }>;

export type ExactCheck = { ok: true; value: NumericWire } | { ok: false; reason: string };

const INTEGER_RE = /^[+-]?\d+$/;
const DECIMAL_RE = /^[+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?$/;
const NAN_RE = /^nan$/i;
const INFINITY_RE = /^[+-]?inf(?:inity)?$/i;

/**
 * Validate editor text against a column's exact domain. On success, returns the
 * `numeric` wire value to bind — still a string.
 */
export function checkExact(input: string, domain: ExactDomain): ExactCheck {
  const text = input.trim();
  if (text === "") {
    // NULL is a separate, explicit action; an empty box is not a value.
    return { ok: false, reason: "Enter a number, or set the cell to NULL." };
  }
  return domain.kind === "integer" ? checkInteger(text, domain) : checkDecimal(text, domain);
}

function checkInteger(text: string, d: Extract<ExactDomain, { kind: "integer" }>): ExactCheck {
  if (!INTEGER_RE.test(text)) {
    return { ok: false, reason: "Must be a whole number (digits, optional leading sign)." };
  }
  const v = normalizeInteger(text);
  if (compareIntegerText(v, d.min) < 0 || compareIntegerText(v, d.max) > 0) {
    return { ok: false, reason: `Out of range: must be between ${d.min} and ${d.max}.` };
  }
  return { ok: true, value: { t: "numeric", v } };
}

function checkDecimal(text: string, d: Extract<ExactDomain, { kind: "decimal" }>): ExactCheck {
  const constrained = d.precision !== null || d.scale !== null;

  // Postgres numeric accepts NaN in any numeric column, Infinity only in an
  // unconstrained one.
  if (NAN_RE.test(text)) return { ok: true, value: { t: "numeric", v: "NaN" } };
  if (INFINITY_RE.test(text)) {
    if (constrained) return { ok: false, reason: "This column cannot hold infinity." };
    return { ok: true, value: { t: "numeric", v: text.startsWith("-") ? "-Infinity" : "Infinity" } };
  }

  if (!DECIMAL_RE.test(text)) {
    return { ok: false, reason: "Must be a decimal number, like 1234.5678." };
  }

  if (constrained) {
    if (/[eE]/.test(text)) {
      // Range-checking exponent notation means expanding it, which is where
      // mistakes hide. Plain notation is unambiguous.
      return { ok: false, reason: "Write this value without exponent notation." };
    }
    const unsigned = text.replace(/^[+-]/, "");
    const [intRaw, fracRaw = ""] = unsigned.split(".");
    const intDigits = intRaw.replace(/^0+/, "");
    // Trailing fractional zeros do not change the value, so the database
    // dropping them is not a rounding.
    const fracDigits = fracRaw.replace(/0+$/, "");

    if (d.scale !== null && fracDigits.length > d.scale) {
      // The silent-change case: the database would round, not reject.
      return {
        ok: false,
        reason: `At most ${d.scale} decimal place(s); the database would round this value.`,
      };
    }
    if (d.precision !== null) {
      const maxInt = d.precision - (d.scale ?? 0);
      if (intDigits.length > maxInt) {
        return {
          ok: false,
          reason: `Too large: at most ${maxInt} digit(s) before the decimal point.`,
        };
      }
    }
  }

  // Sent as typed (trimmed): the database canonicalizes it, and the digits the
  // user wrote are exactly the digits that are bound.
  return { ok: true, value: { t: "numeric", v: text } };
}

/** Strip a leading `+` and leading zeros; `-0` becomes `0`. */
export function normalizeInteger(text: string): string {
  const negative = text.startsWith("-");
  const digits = text.replace(/^[+-]/, "").replace(/^0+(?=\d)/, "");
  return negative && digits !== "0" ? `-${digits}` : digits;
}

/**
 * Compare two normalized integer strings by their digits. Returns <0, 0, or >0.
 */
export function compareIntegerText(a: string, b: string): number {
  const an = a.startsWith("-");
  const bn = b.startsWith("-");
  if (an !== bn) return an ? -1 : 1;
  const am = an ? a.slice(1) : a;
  const bm = bn ? b.slice(1) : b;
  let mag: number;
  if (am.length !== bm.length) mag = am.length < bm.length ? -1 : 1;
  else mag = am < bm ? -1 : am > bm ? 1 : 0;
  return an ? -mag : mag;
}
