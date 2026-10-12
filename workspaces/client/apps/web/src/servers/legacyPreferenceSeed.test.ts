import { describe, expect, it } from "vitest";
import { legacyPreferenceSeed } from "./legacyPreferenceSeed";
import type { StudioServer } from "./registry";

const local = {
  id: "local",
  workspaceId: "local-id",
  label: "This computer",
  origin: "http://localhost",
  alias: "LOC",
} satisfies StudioServer;

describe("legacy preference seeding", () => {
  it("does not queue locally derived defaults or server order", () => {
    expect(
      legacyPreferenceSeed([local], new Map([["local", "local-id"]]), {}, null),
    ).toEqual({});
  });

  it("migrates only an existing custom alias and keeps cached order intact", () => {
    const aliasKey = '["server-alias:local-id"]';
    const orderKey = '["server-order"]';
    const existing = {
      [orderKey]: {
        value: ["remote-id", "local-id"],
        timestamp: 0,
        writer: "legacy",
      },
    };
    expect(
      legacyPreferenceSeed(
        [{ ...local, alias: "HOME" }],
        new Map([["local", "local-id"]]),
        existing,
        null,
      ),
    ).toEqual({
      [aliasKey]: { value: "HOME", timestamp: 0, writer: "legacy" },
    });
    expect(existing[orderKey].value).toEqual(["remote-id", "local-id"]);
  });

  it("migrates an explicitly stored local label but not its default", () => {
    expect(
      legacyPreferenceSeed(
        [local],
        new Map([["local", "local-id"]]),
        {},
        "Build machine",
      ),
    ).toEqual({
      '["server-label:local-id"]': {
        value: "Build machine",
        timestamp: 0,
        writer: "legacy",
      },
    });
  });

  it("migrates the configured order of existing paired servers", () => {
    const remote = {
      id: "remote",
      workspaceId: "remote-id",
      label: "Remote",
      origin: "https://remote.invalid",
      alias: "REM",
    } satisfies StudioServer;
    const labelFields = {
      '["server-label:local-id"]': {
        value: "This computer",
        timestamp: 0,
        writer: "legacy",
      },
      '["server-label:remote-id"]': {
        value: "Remote",
        timestamp: 0,
        writer: "legacy",
      },
    };
    expect(
      legacyPreferenceSeed(
        [remote, local],
        new Map([
          ["local", "local-id"],
          ["remote", "remote-id"],
        ]),
        labelFields,
        null,
      ),
    ).toEqual({
      '["server-order"]': {
        value: ["remote-id", "local-id"],
        timestamp: 0,
        writer: "legacy",
      },
    });
  });
});
