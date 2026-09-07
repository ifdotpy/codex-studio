import {
  createStateStore,
  getByPath,
  resolveBindings,
  resolveElementProps,
  resolveRepeatStatePath,
  VisibilityConditionStrictSchema,
  type PropResolutionContext,
  type Spec,
} from "@json-render/core";
import { definitions, panelCatalog, panelContract } from "./catalog";
import type { PanelCallback } from "../components/PanelDocument";

function fail(message: string): never {
  throw new Error(`Panel spec: ${message}`);
}
class UnavailableSelection extends Error {}
const object = (value: unknown): value is Record<string, any> =>
  !!value && typeof value === "object" && !Array.isArray(value);
const forbidden = new Set(["__proto__", "prototype", "constructor"]);
const expressions = new Set([
  "$state",
  "$item",
  "$index",
  "$bindState",
  "$bindItem",
  "$cond",
  "$then",
  "$else",
  "$template",
  "$and",
  "$or",
]);
function inspect(value: unknown, depth = 0) {
  if (depth > 40) fail("JSON nesting exceeds 40 levels");
  if (typeof value === "number" && !Number.isFinite(value))
    fail("numbers must be finite");
  if (value && typeof value === "object")
    for (const [key, child] of Object.entries(value)) {
      if (forbidden.has(key)) fail(`reserved key ${key}`);
      if (key.startsWith("$") && !expressions.has(key))
        fail(`unsupported expression ${key}`);
      if (
        [
          "$state",
          "$bindState",
          "$item",
          "$bindItem",
          "$template",
          "statePath",
        ].includes(key) &&
        typeof child === "string"
      ) {
        if (child.split(/[/.{}]/).some((part) => forbidden.has(part)))
          fail("reserved state path");
      }
      inspect(child, depth + 1);
    }
}
export function validatePanelSpec(
  value: unknown,
  callbacks: PanelCallback[],
): Spec {
  if (
    !object(value) ||
    Object.keys(value).some(
      (key) => !["root", "elements", "state"].includes(key),
    )
  )
    fail("expected root, elements, and optional state");
  inspect(value);
  if (
    !object(value.elements) ||
    Object.keys(value.elements).length > panelContract.limits.maxElements
  )
    fail("expected at most 160 elements");
  if (value.state !== undefined && !object(value.state))
    fail("state must be an object");
  if (
    new TextEncoder().encode(JSON.stringify(value)).length >
    panelContract.limits.maxBytes
  )
    fail("spec exceeds 128 KiB");
  const normalized = {
    ...value,
    elements: Object.fromEntries(
      Object.entries(value.elements).map(([id, element]) => [
        id,
        object(element) ? { children: [], ...element } : element,
      ]),
    ),
  };
  const result = panelCatalog.validate(normalized);
  if (!result.success) fail(JSON.stringify(result.error).slice(0, 1800));
  const spec = normalized as Spec;
  const visited = new Set<string>();
  const visit = (id: string, depth: number, form: boolean) => {
    if (depth > 16) fail("component nesting exceeds 16 levels");
    if (visited.has(id)) fail(`cycle or shared child at ${id}`);
    const element = spec.elements[id];
    if (!element) fail(`missing element ${id}`);
    visited.add(id);
    if (
      Object.keys(element).some(
        (key) =>
          !["type", "props", "children", "visible", "repeat"].includes(key),
      )
    )
      fail(`${id}: unsupported element field`);
    const definition = definitions[element.type as keyof typeof definitions];
    if (!definition) fail(`${id}: unknown component ${element.type}`);
    if (
      Object.keys(element.props).some((key) => !(key in definition.props.shape))
    )
      fail(`${id}: unknown component prop`);
    if (element.visible !== undefined) {
      const condition = VisibilityConditionStrictSchema.safeParse(
        element.visible,
      );
      if (!condition.success) fail(`${id}: invalid visibility condition`);
    }
    if (
      (element.children?.length || element.repeat) &&
      !("slots" in definition)
    )
      fail(`${id}: ${element.type} does not accept children`);
    if (element.type === "Form" && form)
      fail(`${id}: nested forms are not supported`);
    if (element.repeat) {
      if (
        !object(element.repeat) ||
        Object.keys(element.repeat).some(
          (key) => !["statePath", "key"].includes(key),
        ) ||
        typeof element.repeat.statePath !== "string" ||
        !element.repeat.statePath.startsWith("/")
      )
        fail(`${id}: repeat requires an absolute statePath`);
    }
    for (const child of element.children || [])
      visit(child, depth + 1, form || element.type === "Form");
  };
  visit(spec.root, 0, false);
  if (visited.size !== Object.keys(spec.elements).length)
    fail("all elements must be reachable from root");
  validateResolved(spec, spec.state || {}, callbacks);
  return spec;
}

export function validateResolved(
  spec: Spec,
  state: Record<string, unknown>,
  callbacks: PanelCallback[],
) {
  let count = 0;
  const domains = new Map<string, (string | boolean)[]>();
  const callbackMap = new Map(
    callbacks.map((callback) => [callback.id, callback]),
  );
  const visit = (
    id: string,
    ctx: PropResolutionContext,
    form?: PanelCallback,
  ) => {
    if (++count > 400) fail("repeat expands to more than 400 components");
    const element = spec.elements[id];
    const definition = definitions[element.type as keyof typeof definitions];
    const resolved = resolveElementProps(element.props, ctx);
    const result = definition.props.safeParse(resolved);
    if (!result.success)
      fail(
        `${id}: ${result.error.issues.map((issue) => `${issue.path.join(".")}: ${issue.message}`).join("; ")}`,
      );
    const props = resolved as Record<string, any>;
    const bindings = resolveBindings(element.props, ctx) || {};
    if (element.type === "Progress" && props.value > props.total)
      fail(`${id}: progress value exceeds total`);
    if (["Button", "Form"].includes(element.type)) {
      const callback = callbackMap.get(props.callback);
      if (!callback) fail(`${id}: callback ${props.callback} is not declared`);
      if (element.type === "Form") form = callback;
    }
    if (
      ["TextInput", "Select", "Toggle"].includes(element.type) &&
      form &&
      !form.fields?.includes(props.name)
    )
      fail(`${id}: input ${props.name} is not declared in callback.fields`);
    if (
      ["TextInput", "Select", "Toggle", "SegmentedControl", "Tabs"].includes(
        element.type,
      )
    ) {
      const prop = element.type === "Toggle" ? "checked" : "value";
      const binding = bindings[prop];
      if (!binding?.startsWith("/"))
        fail(`${id}: ${prop} requires a state binding`);
      if (Object.keys(bindings).some((key) => key !== prop))
        fail(`${id}: only ${prop} may be writable`);
      if (getByPath(state, binding) === undefined)
        fail(`${id}: initialize state at ${binding}`);
      if (element.type !== "TextInput") {
        const values: (string | boolean)[] =
          element.type === "Toggle"
            ? [false, true]
            : props.options.map((option: { value: string }) => option.value);
        if (new Set(values).size !== values.length)
          fail(`${id}: options must be unique`);
        if (!values.includes(props[prop]))
          throw new UnavailableSelection(
            `Panel spec: ${id}: options do not include the current value`,
          );
        const prior = domains.get(binding);
        if (prior && JSON.stringify(prior) !== JSON.stringify(values))
          fail(
            `${id}: controls bound to ${binding} must have the same options`,
          );
        domains.set(binding, values);
      }
    } else if (Object.keys(bindings).length)
      fail(`${id}: only input controls accept writable bindings`);
    if (element.repeat) {
      const path = resolveRepeatStatePath(
        element.repeat.statePath,
        ctx.repeatBasePath,
      );
      if (!path) fail(`${id}: invalid repeat path`);
      const items = getByPath(state, path);
      if (!Array.isArray(items) || items.length > 100)
        fail(`${id}: repeat must read an array with at most 100 entries`);
      if (element.repeat.key) {
        const keys = items.map((item) =>
          object(item) ? item[element.repeat!.key!] : undefined,
        );
        if (
          keys.some(
            (key) => typeof key !== "string" && typeof key !== "number",
          ) ||
          new Set(keys).size !== keys.length
        )
          fail(`${id}: repeat keys must be unique strings or numbers`);
      }
      items.forEach((item, index) => {
        for (const child of element.children || [])
          visit(
            child,
            {
              stateModel: state,
              repeatItem: item,
              repeatIndex: index,
              repeatBasePath: `${path}/${index}`,
            },
            form,
          );
      });
    } else for (const child of element.children || []) visit(child, ctx, form);
  };
  visit(spec.root, { stateModel: state });
  return domains;
}

export function panelStates(spec: Spec, callbacks: PanelCallback[]) {
  const initial = spec.state || {};
  const states: Record<string, unknown>[] = [{}];
  const seen = new Set([JSON.stringify(initial)]);
  // A choice may change another control's options. Explore reachable states
  // until no new domain appears, rather than using only the initial options.
  for (let index = 0; index < states.length; index++) {
    const choice = states[index];
    const current = createStateStore(structuredClone(initial));
    current.update(choice);
    const domains = validateResolved(spec, current.getSnapshot(), callbacks);
    for (const [path, values] of domains)
      for (const value of values) {
        const next = { ...choice, [path]: value };
        const candidate = createStateStore(structuredClone(initial));
        candidate.update(next);
        try {
          validateResolved(spec, candidate.getSnapshot(), callbacks);
        } catch (error) {
          // The same guard rejects this local transition in the visible panel.
          // Other validation failures are real spec errors and must reject set.
          if (error instanceof UnavailableSelection) continue;
          throw error;
        }
        const signature = JSON.stringify(candidate.getSnapshot());
        if (seen.has(signature)) continue;
        seen.add(signature);
        states.push(next);
        if (states.length > 32)
          fail(
            "more than 32 reachable states of local controls; simplify the panel",
          );
      }
  }
  return states;
}
