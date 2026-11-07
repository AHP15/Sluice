import { describe, expect, test } from "bun:test";
import type { ProfileMeta } from "../../src/lib/contract";
import { withEngine } from "../../src/lib/engine";

const base: ProfileMeta = {
  id: "p",
  label: "x",
  host: "h",
  port: 5432,
  user: "u",
  dbname: "d",
  tls: "verify",
};

describe("T-075: switching a profile's engine", () => {
  test("a port at the old engine's default follows to the new engine's", () => {
    expect(withEngine(base, "sqlserver")).toMatchObject({ engine: "sqlserver", port: 1433 });
    expect(withEngine({ ...base, engine: "sqlserver", port: 1433 }, "mongodb").port).toBe(27017);
  });

  test("a port the user chose is kept", () => {
    expect(withEngine({ ...base, port: 55432 }, "sqlserver")).toMatchObject({ engine: "sqlserver", port: 55432 });
  });

  test("an absent engine is read as postgres", () => {
    expect(withEngine(base, "postgres")).toMatchObject({ engine: "postgres", port: 5432 });
  });
});
