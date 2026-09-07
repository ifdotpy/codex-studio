#!/usr/bin/env node
// Run with: node --experimental-strip-types tests/structured-panel-validation.mjs
// Import the production validator directly. No browser, build, or runtime state.
import assert from "node:assert/strict";
import { registerHooks } from "node:module";

const hooks = registerHooks({
  resolve(specifier, context, next) {
    if (
      specifier === "./catalog" &&
      context.parentURL?.endsWith("/panel/validate.ts")
    ) {
      specifier = "./catalog.ts";
    }
    return next(specifier, context);
  },
});
const {
  validatePanelSpec,
  validateResolved,
  panelStates,
  validateFeedBinding,
} = await import("../web/src/panel/validate.ts");
const { createStateStore } = await import(
  "../web/node_modules/@json-render/core/dist/index.js"
);
hooks.deregister();

let passed = 0;
function check(name, body) {
  body();
  passed++;
  console.log(`PASS ${name}`);
}
const simple = () => ({
  root: "root",
  elements: { root: { type: "Text", props: { text: "Ready" } } },
});
const rejects = (spec, pattern, callbacks = []) =>
  assert.throws(() => validatePanelSpec(spec, callbacks), pattern);

check("accepts the catalog and preserves the submitted content", () => {
  const spec = simple();
  assert.deepEqual(
    validatePanelSpec(spec, []).elements.root.props,
    spec.elements.root.props,
  );
  assert.deepEqual(panelStates(spec, []), [{}]);
});
check("rejects arbitrary props and malformed catalog values", () => {
  const extra = simple();
  extra.elements.root.props.style = { color: "red" };
  rejects(extra, /unknown component prop|unrecognized/i);
  const malformed = simple();
  malformed.elements.root.props.text = 42;
  rejects(malformed, /string|invalid/i);
});
check("rejects unknown components and event handlers", () => {
  const component = simple();
  component.elements.root.type = "iframe";
  rejects(component, /unknown|invalid/i);
  const event = simple();
  event.elements.root.on = { click: { action: "send" } };
  rejects(event, /unsupported element field|unrecognized/i);
});
check("rejects cycles, shared children, and unreachable elements", () => {
  rejects(
    {
      root: "r",
      elements: {
        r: { type: "Stack", props: {}, children: ["r"] },
      },
    },
    /cycle|shared/i,
  );
  rejects(
    {
      root: "r",
      elements: {
        r: { type: "Stack", props: {}, children: ["text", "text"] },
        text: { type: "Text", props: { text: "One owner" } },
      },
    },
    /cycle|shared/i,
  );
  const orphan = simple();
  orphan.elements.orphan = { type: "Text", props: { text: "Unreachable" } };
  rejects(orphan, /reachable/i);
});
check("rejects prototype keys and state paths", () => {
  const key = JSON.parse(
    '{"root":"root","state":{"__proto__":{}},"elements":{"root":{"type":"Text","props":{"text":"Blocked"}}}}',
  );
  rejects(key, /reserved key/i);
  for (const path of ["/__proto__/polluted", "/constructor/prototype/x"]) {
    const spec = simple();
    spec.elements.root.props.text = { $state: path };
    rejects(spec, /reserved state path/i);
  }
  assert.equal({}.polluted, undefined);
});
check("requires declared callbacks and form fields", () => {
  const button = {
    root: "b",
    elements: {
      b: { type: "Button", props: { label: "Inspect", callback: "inspect" } },
    },
  };
  rejects(button, /not declared/i);
  assert.equal(
    validatePanelSpec(button, [{ id: "inspect", label: "Inspect" }]).elements.b
      .props.callback,
    "inspect",
  );
  rejects(
    {
      root: "f",
      state: { answer: "" },
      elements: {
        f: {
          type: "Form",
          props: { callback: "send", submitLabel: "Send" },
          children: ["input"],
        },
        input: {
          type: "TextInput",
          props: {
            name: "answer",
            label: "Answer",
            value: { $bindState: "/answer" },
          },
        },
      },
    },
    /callback.fields/i,
    [{ id: "send", label: "Send", fields: [] }],
  );
});
check("requires initialized writable control bindings", () => {
  rejects(
    {
      root: "t",
      elements: {
        t: {
          type: "Toggle",
          props: { name: "enabled", label: "Enabled", checked: false },
        },
      },
    },
    /state binding/i,
  );
  rejects(
    {
      root: "t",
      state: {},
      elements: {
        t: {
          type: "Toggle",
          props: {
            name: "enabled",
            label: "Enabled",
            checked: { $bindState: "/enabled" },
          },
        },
      },
    },
    /initialize|string|boolean|invalid/i,
  );
});
check("checks repeated instances against the rendered component limit", () => {
  const spec = {
    root: "r",
    state: { rows: Array.from({ length: 100 }, (_, id) => ({ id })) },
    elements: {
      r: {
        type: "Stack",
        props: {},
        repeat: { statePath: "/rows", key: "id" },
        children: ["row"],
      },
      row: { type: "Stack", props: {}, children: ["a", "b", "c"] },
      a: { type: "Text", props: { text: "A" } },
      b: { type: "Text", props: { text: "B" } },
      c: { type: "Text", props: { text: "C" } },
    },
  };
  rejects(spec, /400 components/i);
});

function dynamicChoices() {
  const option = (value) => ({ label: value.toUpperCase(), value });
  return {
    root: "root",
    state: { expanded: false, view: "a" },
    elements: {
      root: {
        type: "Stack",
        props: {},
        children: ["toggle", "select", "detail"],
      },
      toggle: {
        type: "Toggle",
        props: {
          name: "expanded",
          label: "More",
          checked: { $bindState: "/expanded" },
        },
      },
      select: {
        type: "Select",
        props: {
          name: "view",
          label: "View",
          value: { $bindState: "/view" },
          options: {
            $cond: { $state: "/expanded", eq: true },
            $then: ["a", "b", "c"].map(option),
            $else: ["a", "b"].map(option),
          },
        },
      },
      detail: {
        type: "Text",
        props: {
          text: {
            $cond: { $state: "/view", eq: "c" },
            $then: "The third view",
            $else: "Summary",
          },
        },
      },
    },
  };
}
check(
  "enumerates dynamic options to closure without invalid transitions",
  () => {
    const spec = validatePanelSpec(dynamicChoices(), []);
    const choices = panelStates(spec, []);
    const snapshots = choices.map((choice) => {
      const state = createStateStore(structuredClone(spec.state));
      state.update(choice);
      validateResolved(spec, state.getSnapshot(), []);
      return state.getSnapshot();
    });
    assert.equal(snapshots.length, 5);
    assert(
      snapshots.some((state) => state.expanded === true && state.view === "c"),
    );
    assert(
      !snapshots.some(
        (state) => state.expanded === false && state.view === "c",
      ),
    );
  },
);
check("rejects invalid callbacks exposed by later local states", () => {
  const spec = dynamicChoices();
  spec.elements.root.children.push("action");
  spec.elements.action = {
    type: "Button",
    props: {
      label: "Inspect",
      callback: {
        $cond: { $state: "/expanded", eq: true },
        $then: "undeclared",
        $else: "inspect",
      },
    },
  };
  const callbacks = [{ id: "inspect", label: "Inspect" }];
  validatePanelSpec(spec, callbacks);
  assert.throws(() => panelStates(spec, callbacks), /not declared/i);
});

function toggles(count) {
  const spec = {
    root: "root",
    state: {},
    elements: {
      root: { type: "Stack", props: {}, children: [] },
    },
  };
  for (let index = 0; index < count; index++) {
    const id = `toggle${index}`;
    spec.state[id] = false;
    spec.elements.root.children.push(id);
    spec.elements[id] = {
      type: "Toggle",
      props: { name: id, label: id, checked: { $bindState: `/${id}` } },
    };
  }
  return spec;
}
check("allows 32 reachable states and rejects a larger state space", () => {
  assert.equal(panelStates(validatePanelSpec(toggles(5), []), []).length, 32);
  assert.throws(
    () => panelStates(validatePanelSpec(toggles(6), []), []),
    /32.*states/i,
  );
});

check("accepts allowlisted icons and rejects arbitrary icon names", () => {
  const spec = {
    root: "icon",
    elements: {
      icon: { type: "Icon", props: { name: "server", label: "EC2" } },
    },
  };
  validatePanelSpec(spec, []);
  spec.elements.icon.props.name = "external-url";
  rejects(spec, /invalid|option/i);
});
check("feed bindings cannot write the collector subtree", () => {
  const spec = validatePanelSpec(
    {
      root: "input",
      state: { live: { note: "seed" }, note: "local" },
      elements: {
        input: {
          type: "TextInput",
          props: {
            name: "note",
            label: "Note",
            value: { $bindState: "/live/note" },
          },
        },
      },
    },
    [],
  );
  assert.throws(() => validateFeedBinding(spec, "/live", []), /read-only/);
  spec.elements.input.props.value = { $bindState: "/note" };
  validateFeedBinding(spec, "/live", []);
  for (const path of ["/", "/constructor", "/live/nested", "/missing"])
    assert.throws(() => validateFeedBinding(spec, path, []), /feed statePath/);
});
check("feed binding validation resolves writable repeated item paths", () => {
  const spec = validatePanelSpec(
    {
      root: "rows",
      state: { live: { rows: [{ id: "one", note: "seed" }] } },
      elements: {
        rows: {
          type: "Stack",
          props: {},
          repeat: { statePath: "/live/rows", key: "id" },
          children: ["input"],
        },
        input: {
          type: "TextInput",
          props: { name: "note", label: "Note", value: { $bindItem: "note" } },
        },
      },
    },
    [],
  );
  assert.throws(() => validateFeedBinding(spec, "/live", []), /read-only/);
});

console.log(`Structured panel validation: ${passed} checks passed.`);
