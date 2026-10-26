import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
vi.mock("../servers/ServerServicesContext", () => ({
  useServerApi: () => ({ post: vi.fn() }),
}));
import LimitRecoveryNotice from "./LimitRecoveryNotice";
vi.mock("../local-time", () => ({ localDateTime: () => "Fixture reset time" }));
import type { LimitRecovery } from "../usage/limitRecovery";

const NOW = 1_800_000_000;
const RESET = NOW + 3600;
const recovery: LimitRecovery = {
  title: "Usage limit reached",
  message: "View account limits for reset times and available credits.",
  resetAt: RESET,
  eligibility: {
    agentId: "a",
    accountKey: "default",
    model: "sonnet",
    provider: "claude",
    allowed: false,
    canRun: false,
    fresh: false,
    checkedAt: NOW - 600,
    validUntil: NOW - 300,
    resetAt: RESET,
    buckets: [{ id: "claude-sonnet", overageAllowed: false }],
    overage: "rejected",
  },
};
const scheduled = {
  id: "resume-1",
  status: "scheduled",
  cause: "usage",
  plannedAt: RESET + 60,
  updatedAt: NOW - 10,
} as never;

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(NOW * 1000);
});
afterEach(() => vi.useRealTimers());

function render(props: Record<string, unknown>) {
  return renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(LimitRecoveryNotice, {
        recovery,
        onRefresh: async () => {},
        ...props,
      }),
    ),
  );
}

function hidesDetails(html: string) {
  for (const detail of [
    "Applicable buckets",
    "Overage",
    "Current eligibility is unknown",
    "Cannot run now",
    "View account limits for reset times",
    "Reported reset",
  ])
    expect(html).not.toContain(detail);
}

test("a limit shows one line with the local reset time and a Refresh button", () => {
  const html = render({});
  expect(html).toContain("Limit reached. Resets at Fixture reset time.");
  expect(html).toContain("Refresh");
  hidesDetails(html);
});

test("a chat that resumes by itself says so and needs no Refresh", () => {
  const html = render({ agentId: "a", usageResume: scheduled });
  expect(html).toContain(
    "Limit reached. Studio resumes this chat after the reset.",
  );
  expect(html).not.toContain(">Refresh<");
  hidesDetails(html);
});

test("after the reset time a waiting chat says that Studio checks the account", () => {
  vi.setSystemTime((RESET + 1) * 1000);
  const waiting = render({ agentId: "a", usageResume: scheduled });
  expect(waiting).toContain(
    "The limit has reset. Studio is checking the account.",
  );
  hidesDetails(waiting);
  // A chat without automatic resume asks for a refresh instead.
  const manual = render({});
  expect(manual).toContain(
    "The limit has reset. Refresh to check the account.",
  );
  expect(manual).toContain("Refresh");
});

test("a limit without a known reset time stays one line", () => {
  const html = render({
    recovery: { ...recovery, resetAt: undefined, eligibility: undefined },
  });
  expect(html).toContain("Limit reached.");
  expect(html).not.toContain("Resets at");
  hidesDetails(html);
});
