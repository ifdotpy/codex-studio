import type { UsageAccount } from "./Usage";

const now = Math.floor(Date.now() / 1000);
const limitWindow = (remaining: number, minutes: number, reset: number) => ({
  usedPercent: 100 - remaining,
  windowDurationMins: minutes,
  resetsAt: now + reset,
});
export const accountLimitFixtures: UsageAccount[] = [
  {
    key: "codex-main",
    label: "Codex",
    email: "main@example.test",
    provider: "codex",
    plan: "pro",
    accountId: "main",
    loading: false,
    reload: () => {},
    limits: {
      accountKey: "codex-main",
      at: now,
      data: {
        accountId: "main",
        rateLimitsByLimitId: {
          codex: {
            limitId: "codex",
            primary: limitWindow(72, 300, 3600),
            secondary: limitWindow(38, 10080, 172800),
          },
          spark: {
            limitId: "spark",
            limitName: "Spark",
            primary: limitWindow(84, 300, 7200),
            secondary: limitWindow(62, 10080, 259200),
          },
        },
        rateLimitResetCredits: {
          availableCount: 1,
          credits: [
            {
              id: "reset",
              title: "Session reset",
              status: "available",
              resetType: "codexRateLimits",
            },
          ],
        },
      },
    },
  },
  {
    key: "codex-review",
    label: "Review",
    email: "review@example.test",
    provider: "codex",
    plan: "pro",
    accountId: "review",
    loading: false,
    reload: () => {},
    limits: {
      accountKey: "codex-review",
      at: now,
      data: {
        accountId: "review",
        rateLimits: {
          limitId: "codex",
          primary: limitWindow(0.5, 300, 600),
          secondary: limitWindow(12, 10080, 86400),
        },
      },
    },
  },
  {
    key: "codex-stale",
    label: "Documentation",
    email: "docs@example.test",
    provider: "codex",
    plan: "pro",
    loading: false,
    reload: () => {},
    limits: {
      accountKey: "codex-stale",
      at: now - 7200,
      data: null,
      error: "Limits temporarily unavailable",
    },
  },
  {
    key: "claude-main",
    label: "Claude code 2",
    email: "claude-two@example.test",
    provider: "claude",
    plan: "max",
    loading: false,
    reload: () => {},
    limits: {
      accountKey: "claude-main",
      at: now,
      data: {
        rateLimitsByLimitId: {
          claude: {
            limitId: "claude",
            primary: limitWindow(64, 300, 10800),
            secondary: limitWindow(48, 10080, 345600),
          },
          opus: {
            limitId: "opus",
            limitName: "Opus",
            secondary: limitWindow(8, 10080, 432000),
          },
        },
      },
    },
  },
  {
    key: "claude-review",
    label: "Claude code 1",
    email: "claude-one@example.test",
    provider: "claude",
    plan: "max",
    loading: false,
    reload: () => {},
    limits: {
      accountKey: "claude-review",
      at: now,
      data: {
        rateLimits: {
          limitId: "claude",
          primary: limitWindow(88, 300, 3600),
          secondary: limitWindow(79, 10080, 259200),
        },
      },
    },
  },
];
