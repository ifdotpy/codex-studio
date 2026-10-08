import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(
  fileURLToPath(new URL("../../web/package.json", import.meta.url)),
);
const { chromium } = require("playwright");

export const browserExecutablePath =
  process.env.CHROME_BIN?.trim() || chromium.executablePath();
