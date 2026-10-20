import { existsSync } from "node:fs";
import { serverBinary } from "./tests/fixtures/rust-ui-fixture.mjs";

// Fail once, before any test, when the Rust server the fixtures run is missing.
export default function globalSetup() {
  const binary = serverBinary();
  if (!existsSync(binary))
    throw new Error(`Browser fixtures need the server binary ${binary}.`);
  process.env.CODEX_STUDIO_RUNTIME_SERVER = binary;
  console.log(`Playwright fixture server ${binary}`);
}
