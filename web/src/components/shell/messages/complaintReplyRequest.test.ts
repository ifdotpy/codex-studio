import assert from "node:assert/strict";
import { it } from "vitest";
import { complaintReplyRequest } from "./complaintReplyRequest";
import type { UserComplaintResponse } from "./complaintReplyRequest";

it("reuses the exact persisted complaint response after an uncertain attempt", () => {
  const pending: UserComplaintResponse = {
    id: "request-1",
    action: "respond",
    complaint_id: "complaint-1",
    version: 4,
    text: "The original reply",
    status: "in_progress",
  };

  const retry = complaintReplyRequest(
    pending,
    { id: "complaint-1", version: 5 },
    "Changed draft text",
    "resolved",
  );

  assert.strictEqual(retry, pending);
  assert.equal(retry.id, "request-1");
  assert.equal(retry.version, 4);
  assert.equal(retry.text, "The original reply");
  assert.equal(retry.status, "in_progress");
});

it("creates one durable identity and snapshots the current response body", () => {
  const request = complaintReplyRequest(
    undefined,
    { id: "complaint-2", version: 7 },
    "Please review this.",
    "resolved",
  );

  assert.match(request.id, /^[0-9a-f-]{36}$/i);
  assert.equal(request.action, "respond");
  assert.equal(request.complaint_id, "complaint-2");
  assert.equal(request.version, 7);
  assert.equal(request.text, "Please review this.");
  assert.equal(request.status, "resolved");
});
