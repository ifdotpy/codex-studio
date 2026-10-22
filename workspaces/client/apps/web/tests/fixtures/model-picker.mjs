// The setup fieldset and compact native select both retain data-value.
export const modelValue = (picker) => picker.getAttribute("data-value");
const isSelect = (picker) =>
  picker.evaluate((node) => node.tagName === "SELECT");
const listFor = async (picker) =>
  (await isSelect(picker))
    ? picker.locator("option")
    : picker.getByRole("radio");
export const openModelList = listFor;
export async function closeModelList() {}
export async function modelOptions(picker) {
  return (await listFor(picker)).evaluateAll((nodes) =>
    nodes.map((node) => node.value),
  );
}
export async function modelOptionDisabled(picker, value) {
  return picker.locator(`[value="${value}"]`).isDisabled();
}
export async function selectModel(picker, value) {
  if (await isSelect(picker)) await picker.selectOption(value);
  else await picker.locator(`input[value="${value}"]`).click();
}
