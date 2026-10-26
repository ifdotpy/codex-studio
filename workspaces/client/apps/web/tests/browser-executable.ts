import { chromium } from "playwright";

export const browserExecutablePath =
  process.env.CHROME_BIN?.trim() || chromium.executablePath();
