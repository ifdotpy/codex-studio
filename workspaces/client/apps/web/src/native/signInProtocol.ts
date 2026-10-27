// Mirrors the desktop.rs method owners. signInProtocol.test.ts checks all consumers.
export const nativeSignInMethods = Object.freeze({
  prepare: "desktop.prepareSignIn",
  open: "desktop.openSignIn",
  cancel: "desktop.cancelSignIn",
});
