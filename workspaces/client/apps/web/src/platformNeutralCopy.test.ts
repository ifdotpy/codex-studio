import { expect, test } from "vitest";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { SignInPushSettings } from "./components/SignInPushSettings";
import { nativeErrorView } from "./nativeErrors";
import { layrSyncText } from "./components/sidebar/MacSync";
import { workspaceBadgeInfo } from "./components/shell/WorkspaceBadge";
import { resultTarget } from "./components/questions/signInCardModel";
import { deviceName } from "./push/signInPush";
import type { Agent } from "./types";
import {
  LEGACY_VM_PROVIDER_DISCONNECTED_TURN_ERROR,
  VM_PROVIDER_DISCONNECTED_TURN_ERROR,
} from "./generated/sharedValues";

test("unsupported push alerts give platform-neutral setup guidance", () => {
  const markup = renderToStaticMarkup(
    createElement(SignInPushSettings, { active: true }),
  );
  expect(markup).toContain("This window cannot get push alerts.");
  expect(markup).toContain(
    "Try adding Studio to your home screen and opening it from there.",
  );
  expect(markup).not.toContain("iPhone");
});

test("disk recovery, folder sync, workspaces and sign-in use platform-neutral product words", () => {
  expect(nativeErrorView("ENOSPC: no space left on device").hint).toBe(
    "Free disk space on the server. Queued work starts again on its own.",
  );
  expect(
    layrSyncText({
      projectId: "project",
      state: "paused",
      enabled: true,
      conflicts: [],
      choices: [],
      withAgent: [],
      deletes: 2,
      reason: null,
    }),
  ).toBe("Folder sync paused: confirm 2 files deleted locally");
  expect(
    workspaceBadgeInfo({
      isLead: true,
      workspaceMode: "image",
    } as Agent),
  ).toBeNull();
  expect(
    workspaceBadgeInfo({ isLead: true, workspaceMode: "layr" } as Agent),
  ).toEqual({ label: "Layr VM", title: "Virtual machine with Layr" });
  expect(
    resultTarget(
      {
        host: "id.example.com",
        url: "https://id.example.com/",
        source: "vm",
        port: 8000,
      },
      "Build server",
    ),
  ).toBe("The VM on Build server, port 8000");
  expect(deviceName("Mozilla/5.0 (Macintosh; Intel Mac OS X 15_0)")).toBe(
    "Computer",
  );
});

test("user-supplied server labels and raw error details stay intact", () => {
  const details = "ENOENT: /projects/Linux tools/Mac notes.txt";
  expect(nativeErrorView(details).message).toBe(details);
  expect(
    resultTarget(
      {
        host: "id.example.com",
        url: "https://id.example.com/",
        source: "host",
        port: 8000,
      },
      "Linux build server",
    ),
  ).toBe("Linux build server, port 8000");
});

test("saved provider errors use current product wording while retaining diagnostic details", () => {
  for (const value of [
    LEGACY_VM_PROVIDER_DISCONNECTED_TURN_ERROR,
    {
      message: LEGACY_VM_PROVIDER_DISCONNECTED_TURN_ERROR,
      codexErrorInfo: "other",
    },
  ]) {
    const view = nativeErrorView(value);
    expect(view.message).toBe(VM_PROVIDER_DISCONNECTED_TURN_ERROR);
    expect(view.details).toContain(LEGACY_VM_PROVIDER_DISCONNECTED_TURN_ERROR);
  }
});
