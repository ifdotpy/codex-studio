-- Run only as an explicitly scheduled maintenance operation. Building this
-- index takes SQLite's writer lock (0.9 s on the 902K-row live table, 2026-09-28).
-- Without it, every analytics request scans all rate-limit snapshots.
CREATE INDEX IF NOT EXISTS analytics_limits_account_at ON analytics_limits(account, at);
