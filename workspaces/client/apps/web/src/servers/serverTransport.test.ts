import { describe, expect, it, vi } from "vitest";
import { createServerTransport } from "./serverTransport";
import type { StudioServer } from "./registry";

const server = {
  id: "a",
  label: "A",
  origin: "https://a.example",
  credentialId: "key-a",
} as StudioServer;
describe("server ownership without a frame", () => {
  it("keeps a submitted mutation bound to its original server when a view closes", async () => {
    let reply!: (value: Response) => void;
    const send = vi.fn(
      (_owner: StudioServer, _request: Request) =>
        new Promise<Response>((resolve) => {
          reply = resolve;
        }),
    );
    const first = createServerTransport(server, send);
    const second = createServerTransport(
      {
        ...server,
        id: "b",
        origin: "https://b.example",
        credentialId: "key-b",
      },
      send,
    );
    const pending = first.fetch(
      new Request(server.origin + "/api/action", {
        method: "POST",
        body: "{}",
        headers: { "X-Studio-Request-Id": "original-request" },
      }),
    );
    first.close();
    reply(new Response("committed"));
    await expect(pending).rejects.toMatchObject({ name: "AbortError" });
    expect(send).toHaveBeenCalledTimes(1);
    expect(send.mock.calls[0]![0]).toMatchObject({
      id: "a",
      credentialId: "key-a",
    });
    expect(
      (send.mock.calls[0]![1] as Request).headers.get("X-Studio-Request-Id"),
    ).toBe("original-request");
    await expect(
      second.fetch(new Request(server.origin + "/api/action")),
    ).rejects.toThrow("does not belong");
    expect(send).toHaveBeenCalledTimes(1);
  });
  it("captures the owner before external registry objects can change", async () => {
    const mutable = { ...server };
    const send = vi.fn(
      async (_owner: StudioServer, _request: Request) => new Response("ok"),
    );
    const transport = createServerTransport(mutable, send);
    mutable.id = "other";
    mutable.origin = "https://other.example";
    await transport.fetch(new Request(server.origin + "/api/health"));
    expect(send.mock.calls[0]![0]).toMatchObject({
      id: "a",
      origin: server.origin,
    });
    transport.close();
    await expect(
      transport.fetch(new Request(server.origin + "/api/health")),
    ).rejects.toMatchObject({ name: "AbortError" });
    expect(send).toHaveBeenCalledTimes(1);
  });
});
