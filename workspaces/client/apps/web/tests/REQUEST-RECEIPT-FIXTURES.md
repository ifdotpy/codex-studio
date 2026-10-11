# Request keyed browser fixture receipts

This inventory is a starting point for validating browser fixture receipts
against the generated API schema. It lists fixtures that construct a response
or stored receipt using a request body field such as `id`, `request_id`, or
`login_id`; it does not claim that each fixture has already been schema-audited.

- `accounts/account-transfer-ui.spec.mjs`
- `accounts/accounts-ui-smoke.spec.mjs`
- `claude/claude-login-ui.spec.mjs`
- `drafts/dialog-drafts-ui.spec.mjs`
- `drafts/draft-push-coalescing-browser.spec.mjs`
- `files/upload-recovery-browser.spec.mjs`
- `messages/message-delivery-ui.spec.mjs`
- `messages/mobile-send-reliability-browser.spec.mjs`
- `messages/send-session-token-browser.spec.mjs`
- `mobile/mobile-send-reliability-ui.spec.mjs`
- `outbox/send-preflight-browser.spec.mjs`
- `projects/new-chat-workspace-ui.spec.mjs`
- `queue/queue-mutation-deadline-browser.spec.mjs`
- `radio/radio-create-browser.spec.mjs`
- `servers/multi-server-fixture.mjs`
- `servers/project-locations-ui.spec.mjs`
- `settings/claude-settings-auth-ui.spec.mjs`
- `settings/execution-settings-canonical-browser.spec.mjs`
- `settings/settings-ux-ui.spec.mjs`
- `settings/unified-agent-picker-browser.spec.mjs`
- `shell/shell-ux-ui.spec.mjs`
- `sidebar/sidebar-parity-fixture.mjs`
- `teams/peer-teams-browser.spec.mjs`
- `teams/subagent-concurrency-ui.spec.mjs`
- `terminal/terminal-dock-ui.spec.mjs`
- `terminal/terminal-input-recovery-browser.spec.mjs`
