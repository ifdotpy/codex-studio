import { nativeRequest } from "./client";
import { nativeSignInMethods } from "./signInProtocol";

export async function openNativeLink(
  value: string,
  signIn = false,
): Promise<void> {
  const url = new URL(value);
  if (
    !["http:", "https:"].includes(url.protocol) ||
    url.username ||
    url.password
  )
    throw new Error(
      "Only HTTP and HTTPS links without credentials are supported.",
    );
  await nativeRequest(
    signIn ? nativeSignInMethods.open : "desktop.openExternal",
    {
      value: url.href,
    },
  );
}
