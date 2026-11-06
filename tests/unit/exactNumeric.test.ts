/**
 * T-019 / R-005: the exact-numeric edit path keeps every digit.
 *
 * Run with `bun test tests/unit`. The headline cases are values a JS number
 * cannot hold — each assertion would fail if the validator round-tripped
 * through `parseFloat`.
 */
import { describe, expect, test } from "bun:test";

import type { ExactDomain } from "../../src/lib/contract";
import { checkExact, compareIntegerText, normalizeInteger } from "../../src/lib/exactNumeric";

const INT8: ExactDomain = { kind: "integer", min: "-9223372036854775808", max: "9223372036854775807" };
const DECIMAL: ExactDomain = { kind: "decimal", precision: null, scale: null };
const NUMERIC_10_2: ExactDomain = { kind: "decimal", precision: 10, scale: 2 };

const okValue = (input: string, d: ExactDomain) => {
  const r = checkExact(input, d);
  if (!r.ok) throw new Error(`expected ok for ${input}: ${r.reason}`);
  return r.value;
};
const reason = (input: string, d: ExactDomain) => {
  const r = checkExact(input, d);
  if (r.ok) throw new Error(`expected rejection for ${input}`);
  return r.reason;
};

describe("int8 round-trips digit for digit", () => {
  test("the extremes survive exactly", () => {
    // parseFloat("9223372036854775807") === 9223372036854775808
    expect(okValue("9223372036854775807", INT8)).toEqual({ t: "numeric", v: "9223372036854775807" });
    expect(okValue("-9223372036854775808", INT8).v).toBe("-9223372036854775808");
    // 2^53 + 1: the first integer an f64 cannot represent
    expect(okValue("9007199254740993", INT8).v).toBe("9007199254740993");
  });

  test("one past either bound is rejected, not wrapped or rounded", () => {
    expect(reason("9223372036854775808", INT8)).toContain("Out of range");
    expect(reason("-9223372036854775809", INT8)).toContain("Out of range");
    expect(reason("99999999999999999999", INT8)).toContain("Out of range");
  });

  test("shape is validated by regex", () => {
    for (const bad of ["1.5", "1e3", "12a", "--1", "0x10", "1 000", "Infinity"]) {
      expect(reason(bad, INT8)).toContain("whole number");
    }
  });

  test("sign and leading zeros are normalized, not misread", () => {
    expect(okValue("+007", INT8).v).toBe("7");
    expect(okValue("-0", INT8).v).toBe("0");
    expect(okValue("  42  ", INT8).v).toBe("42");
  });

  test("empty is not a value", () => {
    expect(reason("", INT8)).toContain("NULL");
    expect(reason("   ", DECIMAL)).toContain("NULL");
  });
});

describe("numeric keeps every digit the user typed", () => {
  test("precision beyond f64 survives", () => {
    // parseFloat collapses this to 1
    expect(okValue("1.00000000000000000001", DECIMAL).v).toBe("1.00000000000000000001");
    // The T-072 MONEY value tiberius rounded to 922337203685477.6
    expect(okValue("922337203685477.5807", DECIMAL).v).toBe("922337203685477.5807");
  });

  test("unconstrained decimals accept the forms Postgres does", () => {
    for (const v of ["0", "-1.5", ".5", "5.", "+3.25", "1e-30", "6.02E23"]) {
      expect(checkExact(v, DECIMAL).ok).toBe(true);
    }
    expect(okValue("nan", DECIMAL).v).toBe("NaN");
    expect(okValue("-infinity", DECIMAL).v).toBe("-Infinity");
    expect(okValue("inf", DECIMAL).v).toBe("Infinity");
  });

  test("malformed decimals are rejected", () => {
    for (const bad of ["1.2.3", "abc", "1,5", "e5", ".", "1e", "$1.00"]) {
      expect(reason(bad, DECIMAL)).toContain("decimal number");
    }
  });

  test("a scale overflow is rejected because the database would silently round it", () => {
    expect(reason("1.234", NUMERIC_10_2)).toContain("round");
    // Trailing zeros do not change the value, so they are not a rounding.
    expect(okValue("1.2300", NUMERIC_10_2).v).toBe("1.2300");
  });

  test("integer digits are bounded by precision minus scale", () => {
    expect(okValue("99999999.99", NUMERIC_10_2).v).toBe("99999999.99");
    expect(reason("100000000", NUMERIC_10_2)).toContain("at most 8 digit");
    expect(okValue("00000012.5", NUMERIC_10_2).v).toBe("00000012.5");
  });

  test("constrained columns refuse exponent notation and infinity", () => {
    expect(reason("1e3", NUMERIC_10_2)).toContain("exponent");
    expect(reason("Infinity", NUMERIC_10_2)).toContain("infinity");
    expect(okValue("NaN", NUMERIC_10_2).v).toBe("NaN");
  });
});

describe("digit-wise comparison", () => {
  test("orders by sign, then length, then digits", () => {
    expect(compareIntegerText("-5", "3")).toBeLessThan(0);
    expect(compareIntegerText("10", "9")).toBeGreaterThan(0);
    expect(compareIntegerText("-10", "-9")).toBeLessThan(0);
    expect(compareIntegerText("9223372036854775807", "9223372036854775807")).toBe(0);
    expect(compareIntegerText("9223372036854775808", "9223372036854775807")).toBeGreaterThan(0);
  });

  test("normalization", () => {
    expect(normalizeInteger("000")).toBe("0");
    expect(normalizeInteger("-00012")).toBe("-12");
  });
});
