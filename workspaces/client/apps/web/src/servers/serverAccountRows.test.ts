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
