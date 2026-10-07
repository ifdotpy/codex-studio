// Operate the visible segmented control or account tiles through real clicks.
export function setupControl(locator) {
  return {
    selectOption: async (value) => {
      const segment = locator.locator(`input[value="${value}"]`);
      if (
        (await locator.getAttribute("role")) === "radiogroup" &&
        (await segment.count())
      ) {
        const id = await segment.getAttribute("id");
        await locator.locator(`label[for="${id}"]`).click();
      } else if (!value)
        await locator
          .getByRole("button", { name: "Automatic", exact: true })
          .click();
      else {
        const tile = locator.locator(`[data-account-key="${value}"]`);
        if (!(await tile.isVisible())) {
          const providers = locator.locator('input[type="radio"]');
          for (const provider of await providers.all()) {
            const id = await provider.getAttribute("id");
            await locator.locator(`label[for="${id}"]`).click();
            if (await tile.isVisible()) break;
          }
        }
        await tile.click();
      }
    },
    inputValue: () => locator.getAttribute("data-value"),
    count: () => locator.count(),
    isEnabled: async () =>
      (await locator.getAttribute("aria-disabled")) !== "true",
    isDisabled: async () =>
      (await locator.getAttribute("aria-disabled")) === "true",
    getAttribute: (attribute) => locator.getAttribute(attribute),
    waitFor: (options) => locator.waitFor(options),
    locator: (...args) => locator.locator(...args),
  };
}
export async function chooseSetupValue(locator, value) {
  await setupControl(locator).selectOption(value);
}
export function setupToggle(locator) {
  const checked = async () =>
    (await locator.getAttribute("aria-pressed")) === "true";
  return {
    check: async () => {
      if (!(await checked())) await locator.click();
    },
    uncheck: async () => {
      if (await checked()) await locator.click();
    },
    isChecked: checked,
    isEnabled: () => locator.isEnabled(),
    isDisabled: () => locator.isDisabled(),
    count: () => locator.count(),
    click: () => locator.click(),
  };
}
