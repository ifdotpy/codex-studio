// Helpers for the shared model picker. The trigger is a button labelled by the
// field label. It keeps the chosen value in data-value. The open list renders
// one option per model with data-value.
export const modelValue = (picker) => picker.getAttribute("data-value");
const isSetup = async (picker) =>
  (await picker.getAttribute("role")) === "listbox";

const listFor = (picker) =>
  picker.page().locator('[role="listbox"]:visible [role="option"]');

export async function openModelList(picker) {
  if (await isSetup(picker)) {
    await listFor(picker).first().waitFor();
    return listFor(picker);
  }
  if ((await picker.getAttribute("aria-expanded")) !== "true")
    await picker.click();
  await listFor(picker).first().waitFor();
  return listFor(picker);
}

export async function closeModelList(picker) {
  if (await isSetup(picker)) return;
  if ((await picker.getAttribute("aria-expanded")) === "true")
    await picker.press("Escape");
  if (!(await isSetup(picker)))
    await listFor(picker).first().waitFor({ state: "hidden" });
}

export async function modelOptions(picker) {
  const options = await openModelList(picker);
  const values = await options.evaluateAll((nodes) =>
    nodes.map((node) => node.getAttribute("data-value")),
  );
  await closeModelList(picker);
  return values;
}

const optionFor = (picker, value) =>
  picker
    .page()
    .locator(`[role="listbox"]:visible [role="option"][data-value="${value}"]`);

export async function modelOptionDisabled(picker, value) {
  await openModelList(picker);
  const option = optionFor(picker, value);
  const disabled = (await option.getAttribute("aria-disabled")) === "true";
  await closeModelList(picker);
  return disabled;
}

export async function selectModel(picker, value) {
  await openModelList(picker);
  await optionFor(picker, value).click();
  if (!(await isSetup(picker)))
    await listFor(picker).first().waitFor({ state: "hidden" });
}
