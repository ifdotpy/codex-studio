import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { expect, it, vi } from "vitest";
import { SidebarBackendProvider, useSidebarBackend } from "./sidebarServices";
import { NativeServicesProvider } from "./servicesContext";
import { createNativeServerServices } from "./serverServices";
import { openNativePreferences } from "./preferenceStore";
import type { NativeServerContext } from "./client";
vi.mock(
  "../components/sidebar/services",
  async () => await import("./sidebarServices"),
);
import { useProjectSave } from "../components/useProjectSave";

async function fixture(id: string) {
  const calls: string[] = [];
  const context: NativeServerContext = {
    handle: `context:${id}`,
    serverId: id,
    origin: `https://${id}.example.test`,
    credentialId: id === "local" ? null : `credential:${id}`,
    workspaceId: (id === "local" ? "a" : "b").repeat(32),
    async request<T>(method: string): Promise<T> {
      calls.push(method);
      if (method === "preferences.raw_all") return {} as T;
      if (method === "preferences.view")
        return {
          values: {},
          fields: { user: {}, server: {} },
          revision: "1",
          sync: { user: { state: "saved" }, server: { state: "saved" } },
        } as T;
      if (method === "uploads.snapshot")
        return {
          workspaceId: context.workspaceId,
          entries: [],
          tombstones: {},
          revision: "1",
        } as T;
      throw new Error(
        `Fixture stops before native business execution: ${method}`,
      );
    },
    async subscribe() {
      return async () => {};
    },
  };
  const storage = await openNativePreferences(context, vi.fn());
  const owner = {
    id,
    origin: context.origin,
    label: id,
    ...(context.credentialId ? { credentialId: context.credentialId } : {}),
  };
  return {
    services: createNativeServerServices(
      owner,
      { context, storage },
      { report: vi.fn() },
    ),
    calls,
  };
}

it("native sidebar helpers refuse an absent server owner", () => {
  function Consumer() {
    useSidebarBackend();
    return null;
  }
  expect(() => renderToStaticMarkup(createElement(Consumer))).toThrow(
    /requires.*server services/i,
  );
});

it("the real project-save hook retains its native owner after another sidebar renders", async () => {
  const first = await fixture("local");
  const second = await fixture("remote");
  const submits: Array<
    ReturnType<typeof useProjectSave<"/api/organization">>["submit"]
  > = [];
  function Consumer({ expected }: { expected: typeof first }) {
    expect(useSidebarBackend()).toBe(expected.services.sidebarServices.cache);
    submits.push(useProjectSave("/api/organization", async () => {}).submit);
    return null;
  }
  for (const expected of [first, second])
    renderToStaticMarkup(
      createElement(NativeServicesProvider, {
        services: expected.services,
        children: createElement(Consumer, { expected }),
      }),
    );
  await submits[0]({ id: "same-chat", pinned: true });
  expect(first.calls).toContain("mutation.begin");
  expect(second.calls).toEqual(["preferences.raw_all"]);
  await first.services.close();
  await second.services.close();
});

it("an explicit combined row backend keeps its own owner inside another native screen", async () => {
  const first = await fixture("local");
  const second = await fixture("remote");
  function Consumer() {
    expect(useSidebarBackend()).toBe(second.services.sidebarServices.cache);
    return null;
  }
  renderToStaticMarkup(
    createElement(NativeServicesProvider, {
      services: first.services,
      children: createElement(SidebarBackendProvider, {
        value: second.services.sidebarServices.cache,
        children: createElement(Consumer),
      }),
    }),
  );
  expect(first.calls).toEqual(["preferences.raw_all"]);
  expect(second.calls).toEqual(["preferences.raw_all"]);
  await first.services.close();
  await second.services.close();
});
