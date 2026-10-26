import { describe, expect, it, vi } from "vitest";
import { createNativeAttachmentSelections } from "./attachmentSelection";

const target = { stateDir: "project", agent: "agent", workspace: "workspace" };
const file = {
  nativeHandle: "opaque",
  name: "note.txt",
  type: "text/plain",
  size: 4,
};
const batch = () => ({
  id: "selection",
  enqueueRequestId: "enqueue-original",
  serverId: "server",
  workspaceId: "workspace",
  files: [file],
});
function fixture(overrides = {}) {
  const request = vi.fn(async () => file);
  const context = {
    handle: "context",
    serverId: "server",
    workspaceId: "workspace",
    request,
  };
  const queueUploads = vi.fn(async () => [
    {
      id: "upload",
      ...target,
      name: file.name,
      mime: file.type,
      size: file.size,
      created: 1,
    },
  ]);
  const host = {
    pick: vi.fn(async () => batch()),
    drop: vi.fn(async () => batch()),
    paste: vi.fn(() => Promise.resolve(batch())),
    bindTarget: vi.fn(async (_context, _selectionId, target) => ({
      ...batch(),
      target,
    })),
    settle: vi.fn(async () => ({
      selectionId: "selection",
      enqueueRequestId: "enqueue-original",
      acknowledged: true,
    })),
    textIntent: vi.fn(async () => ({
      ...batch(),
      target,
      stageRequestId: "text-original",
    })),
    ...overrides,
  };
  return {
    context,
    request,
    queueUploads,
    host,
    selections: createNativeAttachmentSelections(
      context,
      { queueUploads },
      host,
    ),
  };
}
function gesture() {
  return {
    preventDefault: vi.fn(),
    get dataTransfer() {
      throw Error("DOM bytes forbidden");
    },
    get clipboardData() {
      throw Error("DOM bytes forbidden");
    },
  };
}
describe("native attachment selections", () => {
  it("uses sealed drop and paste without reading DOM bytes", async () => {
    const f = fixture(),
      drop = gesture(),
      paste = gesture();
    expect(await f.selections.drop(drop)).toEqual({
      id: "selection",
      files: [{ name: file.name, type: file.type, size: file.size }],
    });
    const pending = f.selections.paste(paste);
    expect(paste.preventDefault).toHaveBeenCalledOnce();
    await pending;
    expect(drop.preventDefault).toHaveBeenCalledOnce();
    expect(f.host.drop).toHaveBeenCalledWith(f.context);
  });
  it("keeps ordinary text paste with the textarea", () => {
    const f = fixture({ paste: () => null }),
      event = gesture();
    expect(f.selections.paste(event)).toBeNull();
    expect(event.preventDefault).not.toHaveBeenCalled();
  });
  it("retries unknown enqueue with the exact original ID and target", async () => {
    const f = fixture(),
      selected = await f.selections.pick();
    f.queueUploads.mockRejectedValueOnce(Error("Lost response"));
    await expect(f.selections.enqueue(target, selected)).rejects.toThrow(
      "Lost response",
    );
    expect(() =>
      f.selections.enqueue({ ...target, agent: "other" }, selected),
    ).toThrow("original attachment target");
    await f.selections.enqueue(target, selected);
    expect(f.queueUploads.mock.calls[0]).toEqual(f.queueUploads.mock.calls[1]);
    expect(f.queueUploads.mock.calls[0]).toEqual([
      target.stateDir,
      target.agent,
      [file],
      target.workspace,
      "enqueue-original",
    ]);
    await f.selections.enqueue(target, selected);
    expect(f.queueUploads).toHaveBeenCalledTimes(2);
  });
  it("coalesces an in-progress enqueue", async () => {
    const f = fixture(),
      selected = await f.selections.pick();
    let finish;
    f.queueUploads.mockImplementation(
      () =>
        new Promise((resolve) => {
          finish = resolve;
        }),
    );
    const first = f.selections.enqueue(target, selected),
      second = f.selections.enqueue(target, selected);
    expect(first).toBe(second);
    await Promise.resolve();
    await Promise.resolve();
    finish([]);
    await first;
    expect(f.queueUploads).toHaveBeenCalledOnce();
  });
  it("commits the original target before enqueue and preserves an unknown binding", async () => {
    const f = fixture(),
      selected = await f.selections.pick();
    f.host.bindTarget.mockRejectedValueOnce(Error("Unknown binding reply"));
    await expect(f.selections.enqueue(target, selected)).rejects.toThrow(
      "Unknown binding reply",
    );
    expect(f.queueUploads).not.toHaveBeenCalled();
    expect(() =>
      f.selections.enqueue({ ...target, agent: "other" }, selected),
    ).toThrow("original attachment target");
    await f.selections.enqueue(target, selected);
    expect(f.host.bindTarget.mock.calls[0]).toEqual([
      f.context,
      "selection",
      target,
    ]);
    expect(f.host.bindTarget.mock.calls[1]).toEqual(
      f.host.bindTarget.mock.calls[0],
    );
    expect(f.queueUploads).toHaveBeenCalledOnce();
  });
  it("refuses a changed durable target binding before enqueue", async () => {
    const f = fixture(),
      selected = await f.selections.pick();
    f.host.bindTarget.mockResolvedValue({
      ...batch(),
      target,
      files: [{ ...file, nativeHandle: "other" }],
    });
    await expect(f.selections.enqueue(target, selected)).rejects.toThrow(
      "selection changed",
    );
    expect(f.queueUploads).not.toHaveBeenCalled();
  });
  it("retries only the exact settle after its reply is lost", async () => {
    const f = fixture(),
      selected = await f.selections.pick();
    f.host.settle.mockRejectedValueOnce(Error("Unknown settle reply"));
    await expect(f.selections.enqueue(target, selected)).rejects.toThrow(
      "Unknown settle reply",
    );
    expect(f.queueUploads).toHaveBeenCalledOnce();
    await f.selections.enqueue(target, selected);
    expect(f.queueUploads).toHaveBeenCalledOnce();
    expect(f.host.settle).toHaveBeenCalledTimes(2);
    expect(f.host.settle.mock.calls[0]).toEqual(f.host.settle.mock.calls[1]);
  });
  it("keeps the original files.text receipt after a lost reply", async () => {
    const f = fixture();
    f.request.mockRejectedValueOnce(Error("Lost text reply"));
    await expect(f.selections.text(target, "body")).rejects.toThrow(
      "Lost text reply",
    );
    await expect(f.selections.text(target, "changed")).rejects.toThrow(
      "original text attachment",
    );
    const selected = await f.selections.text(target, "body");
    expect(f.request.mock.calls[0]).toEqual([
      "files.text",
      { name: "message.txt", mime: "text/plain", text: "body" },
      "text-original",
    ]);
    expect(f.request.mock.calls[1]).toEqual(f.request.mock.calls[0]);
    expect(f.host.textIntent).toHaveBeenCalledOnce();
    await f.selections.enqueue(target, selected);
    await f.selections.text(target, "next body");
    expect(f.host.textIntent).toHaveBeenCalledTimes(2);
  });
  it.each([
    { serverId: "other" },
    { workspaceId: "other" },
    { enqueueRequestId: "" },
    { files: [{ ...file, nativeHandle: "" }] },
    { files: [{ ...file, size: 0 }] },
    { target: { ...target, workspace: "other" } },
  ])("refuses invalid host ownership or metadata: %j", async (change) => {
    const f = fixture({ pick: async () => ({ ...batch(), ...change }) });
    await expect(f.selections.pick()).rejects.toThrow();
    expect(f.queueUploads).not.toHaveBeenCalled();
  });
  it("refuses a foreign selection or changed context", async () => {
    const f = fixture(),
      other = fixture(),
      selected = await other.selections.pick();
    expect(() => f.selections.enqueue(target, selected)).toThrow(
      "another native owner",
    );
    f.context.handle = "changed";
    await expect(f.selections.pick()).rejects.toThrow("context changed");
    expect(f.queueUploads).not.toHaveBeenCalled();
  });
  it("refuses missing native capture without DOM fallback", async () => {
    const f = fixture(),
      selections = createNativeAttachmentSelections(f.context, {
        queueUploads: f.queueUploads,
      });
    await expect(selections.pick()).rejects.toThrow(
      "gesture port is unavailable",
    );
    expect(() => selections.paste(gesture())).toThrow(
      "gesture port is unavailable",
    );
    await expect(
      selections.input({
        get files() {
          throw Error("DOM bytes forbidden");
        },
      }),
    ).rejects.toThrow("native file picker");
    expect(f.queueUploads).not.toHaveBeenCalled();
  });
});
