import { ComplaintResponseStatus as GeneratedApiComplaintResponseStatus } from "../../../generated/api.ts";
import assert from "node:assert/strict";
import { it } from "vitest";
import {
  complaintReplyRequest,
  submitComplaintResponse,
} from "./complaintReplyRequest";
import type { UserComplaintResponse } from "./complaintReplyRequest";

// A response must snapshot every field used for version checks and replay.
type RequiredResponseFields = Pick<
  UserComplaintResponse,
  "action" | "complaint_id" | "status" | "version"
>;
// @ts-expect-error A response without status is not a complete command.
const missingStatus: RequiredResponseFields = {
  action: "respond",
  complaint_id: "complaint-1",
  version: 1,
};
// @ts-expect-error A response without version is not a complete command.
const missingVersion: RequiredResponseFields = {
  action: "respond",
  complaint_id: "complaint-1",
  status: GeneratedApiComplaintResponseStatus.resolved,
};
void missingStatus;
void missingVersion;

it("reuses the exact persisted complaint response after an uncertain attempt", () => {
  const pending: UserComplaintResponse = {
    id: "request-1",
    action: "respond",
    complaint_id: "complaint-1",
    version: 4,
    text: "The original reply",
    status: GeneratedApiComplaintResponseStatus.in_progress,
  };

  const retry = complaintReplyRequest(
    pending,
    { id: "complaint-1", version: 5 },
    "Changed draft text",
    GeneratedApiComplaintResponseStatus.resolved,
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
    GeneratedApiComplaintResponseStatus.resolved,
  );

  assert.match(request.id, /^[0-9a-f-]{36}$/i);
  assert.equal(request.action, "respond");
  assert.equal(request.complaint_id, "complaint-2");
  assert.equal(request.version, 7);
  assert.equal(request.text, "Please review this.");
  assert.equal(request.status, "resolved");
});

it("does not turn a successful response into a retry when detail reload fails", async () => {
  const request = complaintReplyRequest(
    undefined,
    { id: "complaint-3", version: 2 },
    "Thanks",
    GeneratedApiComplaintResponseStatus.resolved,
  );
  const writes: UserComplaintResponse[] = [];
  const reads: string[] = [];
  const outcome = await submitComplaintResponse(
    request,
    "session-token",
    async (body, token) => {
      assert.strictEqual(body, request);
      assert.equal(token, "session-token");
      writes.push(body);
    },
    async (id) => {
      reads.push(id);
      throw new Error("detail unavailable");
    },
  );

  assert.deepEqual(writes, [request]);
  assert.deepEqual(reads, ["complaint-3"]);
  assert.equal("reloadError" in outcome, true);
});
