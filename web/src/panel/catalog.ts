import { defineCatalog } from "@json-render/core";
import { schema } from "@json-render/react/schema";
import { z } from "zod";

const label = z.string().max(120);
const name = z.string().regex(/^[A-Za-z][A-Za-z0-9_-]{0,63}$/);
const tone = z.enum(["neutral", "accent", "success", "warning", "danger"]);
const space = z.enum(["none", "xs", "sm", "md"]);
const option = z.strictObject({
  label: label.max(40),
  value: z.string().max(80),
});
const options = z.array(option).min(1).max(12);
const container = ["default"];

export const definitions = {
  Stack: {
    props: z.strictObject({
      direction: z.enum(["row", "column"]).optional(),
      gap: space.optional(),
      align: z.enum(["start", "center", "end", "stretch"]).optional(),
      justify: z.enum(["start", "center", "end", "between"]).optional(),
      wrap: z.boolean().optional(),
    }),
    slots: container,
    description:
      "Compose children in a row or column. Studio spacing tokens only.",
  },
  Grid: {
    props: z.strictObject({
      columns: z.number().int().min(1).max(6),
      gap: space.optional(),
    }),
    slots: container,
    description:
      "Equal-width columns, full available width. Must fit at 320, 640 and 1000px. Use Tabs when columns need more room.",
  },
  Surface: {
    props: z.strictObject({
      tone: z.enum(["plain", "subtle"]).optional(),
      padding: space.optional(),
    }),
    slots: container,
    description:
      "Group related content. Plain is transparent; subtle uses the Studio surface.",
  },
  Text: {
    props: z.strictObject({
      text: z.string().max(500),
      kind: z.enum(["body", "title", "label", "caption"]).optional(),
      tone: tone.optional(),
    }),
    description: "Short text in Studio typography. Plain text, no markup.",
  },
  Metric: {
    props: z.strictObject({
      label,
      value: z.union([z.string().max(40), z.number()]),
      detail: label.optional(),
      tone: tone.optional(),
    }),
    description:
      "Measured value with a label and optional short detail. Never invent measurements.",
  },
  Badge: {
    props: z.strictObject({ label: label.max(60), tone: tone.optional() }),
    description: "Small status label with a colored dot.",
  },
  Progress: {
    props: z.strictObject({
      label,
      value: z.number().min(0),
      total: z.number().positive(),
      tone: tone.optional(),
    }),
    description: "Measured progress bar. Value cannot exceed total.",
  },
  Steps: {
    props: z.strictObject({
      items: z
        .array(
          z.strictObject({
            label: label.max(45),
            status: z.enum(["pending", "active", "complete", "failed"]),
          }),
        )
        .min(1)
        .max(8),
    }),
    description: "Connected stage indicators. Each step has an explicit state.",
  },
  Sparkline: {
    props: z.strictObject({
      label,
      values: z.array(z.number()).min(2).max(100),
      tone: tone.optional(),
    }),
    description:
      "Compact line chart with accessible label. Supply actual measured values.",
  },
  BarChart: {
    props: z.strictObject({
      label,
      items: z
        .array(
          z.strictObject({ label: label.max(45), value: z.number().min(0) }),
        )
        .min(1)
        .max(8),
      tone: tone.optional(),
    }),
    description: "Compare up to eight measured values using horizontal bars.",
  },
  Divider: {
    props: z.strictObject({}),
    description: "Subtle horizontal separator.",
  },
  Button: {
    props: z.strictObject({
      label: label.max(60),
      callback: name,
      tone: tone.optional(),
    }),
    description:
      "Explicit user action. Callback must be declared in the tool callbacks array. No automatic invocation.",
  },
  Form: {
    props: z.strictObject({
      callback: name,
      submitLabel: label.max(60),
      direction: z.enum(["row", "column"]).optional(),
    }),
    slots: container,
    description:
      "Form with a submit button. Input names must be declared in callback.fields. Submit sends arrays of strings once per panel revision.",
  },
  TextInput: {
    props: z.strictObject({
      name,
      label: label.max(60),
      value: z.string().max(2000),
      placeholder: label.optional(),
      required: z.boolean().optional(),
    }),
    description:
      "Text field. Bind value with {$bindState:'/path'} or {$bindItem:'field'} for local edits.",
  },
  Select: {
    props: z.strictObject({
      name,
      label: label.max(60),
      value: z.string().max(80),
      options,
    }),
    description:
      "Native select. Bind value for local state. Choice changes do not call the agent.",
  },
  Toggle: {
    props: z.strictObject({ name, label: label.max(60), checked: z.boolean() }),
    description:
      "Checkbox. Bind checked for local state. Form submission returns true when checked.",
  },
  SegmentedControl: {
    props: z.strictObject({
      label: label.max(60),
      value: z.string().max(80),
      options: options.max(6),
    }),
    description:
      "Local view selector. Bind value to state and use visible conditions on content.",
  },
  Tabs: {
    props: z.strictObject({
      value: z.string().max(80),
      options: options.max(6),
    }),
    slots: container,
    description:
      "Local tabs. Bind value to state. Children use visible conditions matching each option. Every selectable state must fit.",
  },
};

export const panelCatalog = defineCatalog(schema, {
  components: definitions,
  actions: {},
});
export const panelContract = {
  version: 1,
  format: "json-render",
  library: "@json-render/react@0.20.0",
  limits: {
    height: 150,
    widths: [320, 640, 1000],
    maxElements: 160,
    maxDepth: 16,
    maxRenderedElements: 400,
    maxLocalStates: 32,
    maxBytes: 131072,
  },
  instructions: [
    "Submit orchestration_panel action=set with spec {root,elements,state?} and optional callbacks. Do not combine spec with html or css.",
    "Use the listed components and compose children freely. Studio owns colors, typography and spacing. Arbitrary style, className, HTML, JavaScript, links and external resources are not spec props.",
    "Elements use type, props, children, visible and repeat. State expressions: $state, $bindState, $item, $bindItem, $index, $cond/$then/$else, $template. on, watch, computed functions, slots and directives are not supported in this Studio catalog.",
    "State and repeat use json-render JSON Pointer paths. Do not use prototype-related keys. Keep all elements reachable from root with no cycles or shared children.",
    "Bind TextInput.value, Select.value, SegmentedControl.value, Tabs.value or Toggle.checked to local state. Controls update locally without a model request. Up to 32 combinations of selectable states are validated.",
    "Buttons and forms reference callbacks declared outside spec: [{id,label,fields:[inputNames]}]. Only a real click or submit sends the callback. Each callback is accepted once per published version.",
    "Rendering and every finite selectable state must fit at 320, 640 and 1000px wide by 150px high. Overflow and invalid specs reject the write and preserve the previous panel. set/get return a PNG for inspection.",
    "Use meaningful shapes, measured progress, short labels and controls. Choose Tabs for several detailed views in the fixed-height panel. Use HTML only when this catalog cannot express the required visual.",
  ],
};
