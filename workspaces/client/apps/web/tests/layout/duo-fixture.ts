import type { Page } from "playwright";

export const duoPhoneUserAgent =
  "Mozilla/5.0 (iPhone; CPU iPhone OS 27_2 like Mac OS X) AppleWebKit/605.1.15 Mobile/15E148 Safari/604.1";

/** Native rail and the older PWA menu expose the same Team action. */
export async function openDuoTeam(page: Page) {
  const rail = page.locator('.edge-rail button[aria-label^="Team,"]');
  if (await rail.isVisible()) await rail.click();
  else {
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await page.getByRole("menuitem", { name: "Team", exact: true }).click();
  }
}
