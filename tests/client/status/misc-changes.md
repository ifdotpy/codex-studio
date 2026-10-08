# Misc browser spec changes

- `terminal/terminal-dock-ui.spec.mjs` — Class 1 (stale fixture / timing): supplied the current API schema hash and valid sync identity/stream contract; kept the truncation flag active until the expected notice is rendered so an in-flight reconnect read cannot consume the one-shot fixture state; waited for the dock to detach after switching to phone viewport before checking it is absent. Evidence: solo pass and 3/3 consecutive solo repeats, including mock dock and live PTY flows.
