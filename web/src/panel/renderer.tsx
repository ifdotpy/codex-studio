import {
  Button as MantineButton,
  Checkbox,
  NativeSelect,
  SegmentedControl as MantineSegmentedControl,
  TextInput as MantineTextInput,
} from "@mantine/core";
import {
  defineRegistry,
  useBoundProp,
  type Components,
} from "@json-render/react";
import { createContext, useContext } from "react";
import { panelCatalog } from "./catalog";

export const PanelActions = createContext({
  busy: true,
  locked: [] as string[],
  send: (_id: string, _form?: HTMLFormElement) => {},
});
const gap = { none: 0, xs: 4, sm: 8, md: 12 };
const toneClass = (tone?: string) => `sp-tone-${tone || "neutral"}`;
const controls: Components<typeof panelCatalog> = {
  Stack: ({ props, children }) => (
    <div
      className="sp-stack"
      style={{
        flexDirection: props.direction || "column",
        gap: gap[props.gap || "sm"],
        alignItems:
          props.align === "start"
            ? "flex-start"
            : props.align === "end"
              ? "flex-end"
              : props.align || "stretch",
        justifyContent:
          props.justify === "between"
            ? "space-between"
            : props.justify || "start",
        flexWrap: props.wrap ? "wrap" : "nowrap",
      }}
    >
      {children}
    </div>
  ),
  Grid: ({ props, children }) => (
    <div
      className="sp-grid"
      style={{
        gridTemplateColumns: `repeat(${props.columns}, minmax(0, 1fr))`,
        gap: gap[props.gap || "md"],
      }}
    >
      {children}
    </div>
  ),
  Surface: ({ props, children }) => (
    <div
      className={`sp-surface sp-surface-${props.tone || "plain"}`}
      style={{ padding: gap[props.padding || "sm"] }}
    >
      {children}
    </div>
  ),
  Text: ({ props }) => (
    <div
      className={`sp-text sp-text-${props.kind || "body"} ${toneClass(props.tone)}`}
    >
      {props.text}
    </div>
  ),
  Metric: ({ props }) => (
    <div className={`sp-metric ${toneClass(props.tone)}`}>
      <span className="sp-label">{props.label}</span>
      <strong>{props.value}</strong>
      {props.detail && <span className="sp-caption">{props.detail}</span>}
    </div>
  ),
  Badge: ({ props }) => (
    <span className={`sp-badge ${toneClass(props.tone)}`}>
      <i />
      {props.label}
    </span>
  ),
  Progress: ({ props }) => (
    <div className={`sp-progress ${toneClass(props.tone)}`}>
      <div className="sp-progress-label">
        <span>{props.label}</span>
        <span>
          {props.value} / {props.total}
        </span>
      </div>
      <div
        role="progressbar"
        aria-label={props.label}
        aria-valuemin={0}
        aria-valuemax={props.total}
        aria-valuenow={props.value}
      >
        <i style={{ width: `${(props.value / props.total) * 100}%` }} />
      </div>
    </div>
  ),
  Steps: ({ props }) => (
    <ol className="sp-steps">
      {props.items.map((item, index) => (
        <li
          key={index}
          aria-label={`${item.label}: ${item.status}`}
          className={`sp-step-${item.status}`}
        >
          <span aria-hidden="true">
            {item.status === "complete"
              ? "✓"
              : item.status === "failed"
                ? "!"
                : index + 1}
          </span>
          <span>{item.label}</span>
        </li>
      ))}
    </ol>
  ),
  Sparkline: ({ props }) => {
    const min = Math.min(...props.values),
      max = Math.max(...props.values),
      span = max - min || 1;
    const points = props.values
      .map(
        (value, index) =>
          `${3 + (index / (props.values.length - 1)) * 194},${37 - ((value - min) / span) * 34}`,
      )
      .join(" ");
    return (
      <figure className={`sp-sparkline ${toneClass(props.tone)}`}>
        <figcaption>{props.label}</figcaption>
        <svg
          viewBox="0 0 200 40"
          preserveAspectRatio="none"
          role="img"
          aria-label={`${props.label}: ${props.values.join(", ")}`}
        >
          <polyline
            points={points}
            fill="none"
            stroke="currentColor"
            strokeWidth="1.5"
            vectorEffect="non-scaling-stroke"
          />
        </svg>
      </figure>
    );
  },
  BarChart: ({ props }) => {
    const max = Math.max(1, ...props.items.map((item) => item.value));
    return (
      <figure className={`sp-bars ${toneClass(props.tone)}`}>
        <figcaption>{props.label}</figcaption>
        {props.items.map((item, index) => (
          <div className="sp-bar-row" key={index}>
            <span>{item.label}</span>
            <span className="sp-bar-track">
              <i style={{ width: `${(item.value / max) * 100}%` }} />
            </span>
            <span>{item.value}</span>
          </div>
        ))}
      </figure>
    );
  },
  Divider: () => <hr className="sp-divider" />,
  Button: ({ props }) => {
    const action = useContext(PanelActions);
    return (
      <MantineButton
        className={`sp-button ${toneClass(props.tone)}`}
        size="compact-xs"
        variant="light"
        disabled={action.busy || action.locked.includes(props.callback)}
        onClick={(event) => {
          if (event.nativeEvent.isTrusted) action.send(props.callback);
        }}
      >
        {props.label}
      </MantineButton>
    );
  },
  Form: ({ props, children }) => {
    const action = useContext(PanelActions);
    return (
      <form
        className={`sp-form sp-form-${props.direction || "row"}`}
        onSubmit={(event) => {
          event.preventDefault();
          if (
            event.nativeEvent.isTrusted &&
            event.currentTarget.reportValidity()
          )
            action.send(props.callback, event.currentTarget);
        }}
      >
        <fieldset
          disabled={action.busy || action.locked.includes(props.callback)}
        >
          {children}
          <MantineButton
            size="compact-xs"
            variant="light"
            type="submit"
            className="sp-submit"
          >
            {props.submitLabel}
          </MantineButton>
        </fieldset>
      </form>
    );
  },
  TextInput: ({ props, bindings }) => {
    const [value, setValue] = useBoundProp(props.value, bindings?.value);
    return (
      <MantineTextInput
        size="xs"
        name={props.name}
        label={props.label}
        value={value || ""}
        required={props.required}
        placeholder={props.placeholder}
        maxLength={2000}
        onChange={(event) => setValue(event.currentTarget.value)}
      />
    );
  },
  Select: ({ props, bindings }) => {
    const [value, setValue] = useBoundProp(props.value, bindings?.value);
    return (
      <NativeSelect
        size="xs"
        name={props.name}
        label={props.label}
        value={value}
        data={props.options}
        onChange={(event) => setValue(event.currentTarget.value)}
      />
    );
  },
  Toggle: ({ props, bindings }) => {
    const [value, setValue] = useBoundProp(props.checked, bindings?.checked);
    return (
      <Checkbox
        size="xs"
        name={props.name}
        label={props.label}
        checked={!!value}
        value="true"
        onChange={(event) => setValue(event.currentTarget.checked)}
      />
    );
  },
  SegmentedControl: ({ props, bindings }) => {
    const [value, setValue] = useBoundProp(props.value, bindings?.value);
    return (
      <MantineSegmentedControl
        size="xs"
        aria-label={props.label}
        value={value}
        data={props.options}
        onChange={setValue}
      />
    );
  },
  Tabs: ({ props, bindings, children }) => {
    const [value, setValue] = useBoundProp(props.value, bindings?.value);
    return (
      <div className="sp-tabs">
        <div role="tablist" aria-label="Panel views">
          {props.options.map((option) => (
            <button
              key={option.value}
              type="button"
              role="tab"
              aria-selected={value === option.value}
              onClick={() => setValue(option.value)}
            >
              {option.label}
            </button>
          ))}
        </div>
        <div className="sp-tab-content">{children}</div>
      </div>
    );
  },
};
export const { registry } = defineRegistry(panelCatalog, {
  components: controls,
});
