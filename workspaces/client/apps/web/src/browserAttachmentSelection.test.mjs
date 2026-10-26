import { describe, expect, it, vi } from "vitest";
vi.mock("./uuid", () => ({ randomUUID: () => "selection-id" }));
vi.mock("./sync/uploads", () => ({
  queueUploads: vi.fn(),
  createUploadBatch: vi.fn(),
}));
vi.mock("./native/fileDialogs", () => ({
  canPickFiles: () => false,
  pickFiles: vi.fn(),
}));
import { createBrowserAttachmentSelections } from "./browserAttachmentSelection";

const target = { stateDir: "state", agent: "agent", workspace: "workspace" };
function fixture() {
  const identities = [{ id: "original-upload", created: 123 }];
  const uploads = {
    createUploadBatch: vi.fn(() => identities),
    queueUploads: vi.fn(async () => []),
  };
  const files = [new File(["body"], "note.txt", { type: "text/plain" })];
  const fileDialogs = {
    canPickFiles: () => true,
    pickFiles: vi.fn(async () => files),
  };
  return {
    uploads,
    files,
    selections: createBrowserAttachmentSelections({ uploads, fileDialogs }),
  };
}
describe("browser attachment selections", () => {
  it("keeps the original upload IDs, File bytes and target after an unknown reply", async () => {
    const f = fixture(),
      batch = await f.selections.pick();
    f.uploads.queueUploads.mockRejectedValueOnce(Error("Unknown reply"));
    await expect(f.selections.enqueue(target, batch)).rejects.toThrow(
      "Unknown reply",
    );
    expect(() =>
      f.selections.enqueue({ ...target, agent: "other" }, batch),
    ).toThrow("original attachment target");
    await f.selections.enqueue(target, batch);
    expect(f.uploads.createUploadBatch).toHaveBeenCalledOnce();
    expect(f.uploads.queueUploads.mock.calls[0]).toEqual(
      f.uploads.queueUploads.mock.calls[1],
    );
    expect(f.uploads.queueUploads.mock.calls[0][2][0]).toBe(f.files[0]);
    await f.selections.enqueue(target, batch);
    expect(f.uploads.queueUploads).toHaveBeenCalledTimes(2);
  });
  it("preserves text attachment bytes until the original enqueue succeeds", async () => {
    const f = fixture(),
      first = await f.selections.text(target, "body");
    expect(await f.selections.text(target, "body")).toBe(first);
    await expect(f.selections.text(target, "changed")).rejects.toThrow(
      "original text attachment",
    );
    await f.selections.enqueue(target, first);
    expect(await f.uploads.queueUploads.mock.calls[0][2][0].text()).toBe(
      "body",
    );
    expect(f.uploads.queueUploads.mock.calls[0][2][0].name).toBe("message.txt");
    expect(await f.selections.text(target, "changed")).not.toBe(first);
  });
  it("preserves default text paste and accepts file paste synchronously", async () => {
    const f = fixture(),
      preventDefault = vi.fn();
    expect(
      f.selections.paste({ clipboardData: { files: [] }, preventDefault }),
    ).toBeNull();
    expect(preventDefault).not.toHaveBeenCalled();
    const selected = f.selections.paste({
      clipboardData: { files: f.files },
      preventDefault,
    });
    expect(preventDefault).toHaveBeenCalledOnce();
    expect((await selected).files).toEqual([
      { name: "note.txt", type: "text/plain", size: 4 },
    ]);
    expect(
      await f.selections.drop({ dataTransfer: { files: [] }, preventDefault }),
    ).toBeNull();
  });
});
