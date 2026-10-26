import { hasNativeClient, nativeRequest } from "../../native/client";
import { openNativeLink } from "../../native/externalLinks";
import type { SignInForward } from "../../desktop";
import { nativeSignInMethods } from "../../native/signInProtocol";

type Preparation = Readonly<{ id: string; requestId: string }>;
type Approval = { openUrl?: string | null; forward?: SignInForward | null };

/** Capture native permission before the approval request can outlive the click. */
export async function approveSignInPage<T extends Approval>(options: {
  url: string;
  requestId: string;
  owner?: string;
  approve(): Promise<T>;
  onOpenError(error: unknown): void;
}): Promise<T> {
  let preparation: Preparation | undefined;
  if (hasNativeClient() && !window.codexDesktop) {
    const id = await nativeRequest<unknown>(nativeSignInMethods.prepare, {
      value: { url: new URL(options.url).href, requestId: options.requestId },
      ...(options.owner ? { signInOwner: options.owner } : {}),
    });
    if (typeof id !== "string" || !id)
      throw new Error("The sign-in preparation was not confirmed.");
    preparation = { id, requestId: options.requestId };
  }
  try {
    const result = await options.approve();
    if (result.openUrl) {
      try {
        await openSignInPage(
          result.openUrl,
          result.forward,
          options.owner,
          preparation,
        );
      } catch (error) {
        options.onOpenError(error);
      }
    }
    return result;
  } finally {
    if (preparation) {
      // Cancellation only releases an intent. It cannot approve or open a link.
      await nativeRequest(nativeSignInMethods.cancel, {
        value: {
          preparation: preparation.id,
          requestId: preparation.requestId,
        },
        ...(options.owner ? { signInOwner: options.owner } : {}),
      }).catch(() => {});
    }
  }
}

/** True when this window can hold the redirect port of a sign-in that another server
 * approved: the desktop asks this Mac's Studio server to hold it. */
export function canForwardSignIn(): boolean {
  return window.codexDesktop?.signInForward === true || hasNativeClient();
}

/** Open a sign-in link that the person approved in a request card
 * (docs/vm-browser-bridge.md). The desktop brings the browser to the front; other links
 * keep opening in the background. A plain browser window opens a new tab. With `forward`,
 * the command waits on another computer: this Mac first holds its redirect port. */
export async function openSignInPage(
  url: string,
  forward?: SignInForward | null,
  owner?: string,
  preparation?: Preparation,
): Promise<void> {
  const parsed = new URL(url);
  if (!["http:", "https:"].includes(parsed.protocol))
    throw new Error("Only HTTP and HTTPS links are supported.");
  const desktop = window.codexDesktop;
  if (preparation && hasNativeClient() && !desktop) {
    await nativeRequest(nativeSignInMethods.open, {
      value: {
        url: parsed.href,
        ...(forward ? { forward } : {}),
        preparation: preparation.id,
        requestId: preparation.requestId,
      },
      ...(owner ? { signInOwner: owner } : {}),
    });
    return;
  }
  if (forward) {
    if (hasNativeClient() && !desktop) {
      await nativeRequest(nativeSignInMethods.open, {
        value: { url: parsed.href, forward },
        ...(owner ? { signInOwner: owner } : {}),
      });
      return;
    }
    if (!desktop?.openSignIn || !canForwardSignIn())
      throw new Error(
        "Update LAYR Studio on this device to sign in to a command on another server.",
      );
    return desktop.openSignIn({ url: parsed.href, forward });
  }
  if (desktop?.openSignIn) return desktop.openSignIn(parsed.href);
  if (desktop) return desktop.openExternal(parsed.href);
  if (hasNativeClient()) return openNativeLink(parsed.href, true);
  // "noopener" makes window.open return null, so detach the opener instead.
  const opened = window.open(parsed.href, "_blank");
  if (!opened)
    throw new Error(`Allow pop-ups, or open this link: ${parsed.href}`);
  opened.opener = null;
}
