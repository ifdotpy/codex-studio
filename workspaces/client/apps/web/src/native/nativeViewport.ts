// Web owner of the native iOS shell viewport API. The shell side is
// apps/native/src-tauri/src/ios_viewport.js; see apps/native/README.md.
// The shell injects the receiver at document start, sets the
// --native-safe-top|right|bottom|left CSS lengths on the document element
// (they replace env(safe-area-inset-*) in hooks/viewport-surface.css),
// and dispatches nativeViewportEvent on window. Pages without the shell
// (browser, PWA, desktop host) see neither and keep their own paths.

/** CustomEvent name on window. Its detail is a NativeViewportState. */
export const nativeViewportEvent = "studio-native-viewport";

/** Keyboard state measured by UIKit before its animation starts. */
export interface NativeKeyboard {
  /** Keyboard overlap with the bottom of the web view, in CSS px. */
  overlap: number;
  /** UIKit animation duration, in ms. */
  duration: number;
  /** UIViewAnimationCurve raw value. */
  curve: number;
}

export interface NativeViewportState {
  /** UIKit safe area insets of the web view, in CSS px. */
  insets: { top: number; right: number; bottom: number; left: number } | null;
  keyboard: NativeKeyboard | null;
}

type NativeViewportHost = { version: number; state: NativeViewportState };

/** The current shell state, or undefined outside the native iOS shell. */
export function nativeViewport(): NativeViewportState | undefined {
  const host = (
    globalThis as typeof globalThis & {
      __studioNativeViewport?: NativeViewportHost;
    }
  ).__studioNativeViewport;
  return host?.version === 1 ? host.state : undefined;
}

// UIKit's keyboard uses the private curve 7. The others are the public
// UIViewAnimationCurve values easeInOut, easeIn, easeOut, and linear.
const keyboardEasing: Record<number, string> = {
  0: "cubic-bezier(0.42, 0, 0.58, 1)",
  1: "cubic-bezier(0.42, 0, 1, 1)",
  2: "cubic-bezier(0, 0, 0.58, 1)",
  3: "linear",
  7: "cubic-bezier(0.38, 0.7, 0.125, 1)",
};

/** CSS easing that matches one UIViewAnimationCurve value. */
export function nativeKeyboardEasing(curve: number) {
  return keyboardEasing[curve] ?? keyboardEasing[7];
}

/** UIKit keyboard animation duration, in ms. */
export function nativeKeyboardDuration(keyboard: NativeKeyboard) {
  return Number.isFinite(keyboard.duration)
    ? Math.round(Math.max(0, keyboard.duration))
    : 0;
}

/** CSS transition timing that matches one UIKit keyboard animation. */
export function nativeKeyboardMotion(keyboard: NativeKeyboard) {
  return `${nativeKeyboardDuration(keyboard)}ms ${nativeKeyboardEasing(keyboard.curve)}`;
}
