import { expect, it } from "vitest";
import type { ServerAccount } from "./navigation";
import type { StudioServer } from "./registry";
import { rowServers, serverAccountRows } from "./serverAccountRows";

const server = (id: string, label: string) =>
  ({ id, label, origin: `https://${id}.example` }) as StudioServer;
const account = (fields: Partial<ServerAccount>): ServerAccount => ({
  provider: "claude",
  email: null,
  plan: null,
  status: "ready",
  label: "Claude Code",
  isDefault: false,
  ...fields,
});
const mac = server("local", "Lumina Mac");
const mbp = server("mbp", "Igor MBP");

it("keeps an account without an email on its own server", () => {
  const rows = serverAccountRows([mac, mbp], {
    local: [
      account({ email: "igos.321@gmail.com", label: "Personal", plan: "max" }),
    ],
    mbp: [
      account({ email: "igos.321@gmail.com", label: "Personal" }),
      // Its Keychain blocks the sign-in, so the server cannot name the person.
      account({ status: "error" }),
    ],
  });

  expect(rows.map((row) => row.label)).toEqual(["Personal", "Claude Code"]);
  const [person, unknown] = rows;
  expect(Object.keys(person!.byServer)).toEqual(["local", "mbp"]);
  expect(person!.plan).toBe("max");
  expect(rowServers(person!, [mac, mbp])).toEqual([mac, mbp]);
  // No "+ Lumina Mac" chip: Studio cannot add an unknown person to a server.
  expect(rowServers(unknown!, [mac, mbp])).toEqual([mbp]);
});

it("does not merge unknown accounts of two servers that share a label", () => {
  const rows = serverAccountRows([mac, mbp], {
    local: [account({})],
    mbp: [account({})],
  });

  expect(rows).toHaveLength(2);
  expect(rows.map((row) => Object.keys(row.byServer))).toEqual([
    ["local"],
    ["mbp"],
  ]);
});

it("keeps unique email rows and their existing keys across servers", () => {
  const rows = serverAccountRows([mac, mbp], {
    local: [
      account({
        accountKey: "personal-local",
        email: "Person@example.test",
        label: "Personal",
      }),
    ],
    mbp: [
      account({
        accountKey: "personal-remote",
        email: "person@example.test",
        label: "Remote",
      }),
    ],
  });
  expect(rows).toHaveLength(1);
  expect(rows[0]?.key).toBe("claude:person@example.test");
  expect(Object.keys(rows[0]!.byServer)).toEqual(["local", "mbp"]);
  expect(rowServers(rows[0]!, [mac, mbp])).toEqual([mac, mbp]);
});

it("keeps each exact profile when one host has duplicate provider emails", () => {
  const personal = account({
    accountKey: "personal",
    email: "same@example.test",
    label: "Personal",
    plan: "Max",
  });
  const team = account({
    accountKey: "team",
    email: "SAME@example.test",
    label: "Team",
    plan: "Team",
  });
  const remote = account({
    accountKey: "remote",
    email: "same@example.test",
    label: "Remote",
    plan: "Pro",
  });
  const rows = serverAccountRows([mac, mbp], {
    local: [personal, team],
    mbp: [remote],
  });
  expect(
    rows.map((row) => [
      row.label,
      row.plan,
      Object.values(row.byServer)[0]?.accountKey,
    ]),
  ).toEqual([
    ["Personal", "Max", "personal"],
    ["Team", "Team", "team"],
    ["Remote", "Pro", "remote"],
  ]);
  expect(new Set(rows.map((row) => row.key)).size).toBe(3);
  expect(rows.map((row) => rowServers(row, [mac, mbp]))).toEqual([
    [mac],
    [mac],
    [mbp],
  ]);
  const reordered = serverAccountRows([mac, mbp], {
    local: [team, personal],
    mbp: [remote],
  });
  expect(new Set(reordered.map((row) => row.key))).toEqual(
    new Set(rows.map((row) => row.key)),
  );
});

it.each([null, "same@example.test"])(
  "does not overwrite same-label legacy profiles with email %s",
  (email) => {
    const first = account({ email });
    const second = account({ email });
    const rows = serverAccountRows([mac, mbp], { local: [first, second] });
    expect(rows).toHaveLength(2);
    expect(rows[0]?.key).not.toBe(rows[1]?.key);
    expect(rows.map((row) => Object.values(row.byServer)[0])).toEqual([
      first,
      second,
    ]);
    expect(rows.map((row) => rowServers(row, [mac, mbp]))).toEqual([
      [mac],
      [mac],
    ]);
    expect(
      rows.every(
        (row) => Object.values(row.byServer)[0]?.accountKey === undefined,
      ),
    ).toBe(true);
  },
);

it("does not overwrite exact profiles that have no email and the same label", () => {
  const rows = serverAccountRows([mac, mbp], {
    local: [
      account({ accountKey: "first" }),
      account({ accountKey: "second" }),
    ],
  });
  expect(rows).toHaveLength(2);
  expect(rows.map((row) => Object.values(row.byServer)[0]?.accountKey)).toEqual(
    ["first", "second"],
  );
  expect(rows[0]?.key).not.toBe(rows[1]?.key);
});
