import type { Locator } from "@playwright/test";
// The setup fieldset and compact native select both retain data-value.
export const modelValue = (picker: Locator) =>
  picker.evaluate(
    (node) =>
      node.getAttribute("data-value") ??
      node.parentElement?.getAttribute("data-value"),
  );
const isSelect = (picker: Locator) =>
  picker.evaluate((node) => node.tagName === "SELECT");
const listFor = async (picker: Locator) =>
  (await isSelect(picker))
    ? picker.locator("option")
    : picker.getByRole("option");
export const openModelList = listFor;
export async function closeModelList() {}
export async function modelOptions(picker: Locator) {
  return (await listFor(picker)).evaluateAll((nodes) =>
    nodes.map(
      (node) =>
        node.getAttribute("data-value") ??
        (node instanceof HTMLOptionElement ? node.value : undefined),
    ),
  );
}
export async function modelOptionDisabled(picker: Locator, value: string) {
  return picker
    .locator(`[data-value="${value}"], option[value="${value}"]`)
    .isDisabled();
}
export async function selectModel(picker: Locator, value: string) {
  if (await isSelect(picker)) await picker.selectOption(value);
  else await picker.locator(`[role="option"][data-value="${value}"]`).click();
}
