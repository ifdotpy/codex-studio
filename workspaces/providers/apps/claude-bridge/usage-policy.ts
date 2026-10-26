// Shared JSON contract for the Claude bridge and Rust resume policy.
export default `{
  "modelBuckets": {
    "opus": "claude-opus",
    "sonnet": "claude-sonnet",
    "haiku": "claude-haiku"
  },
  "allowedOverageStatuses": ["allowed", "allowed_warning"]
}`;
