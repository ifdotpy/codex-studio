# Shared UI foundation

## Change contract

This directory provides presentation primitives. Preserve native controls, accessible
names, callbacks, disabled states, and all caller behavior. `theme.ts` owns the
Mantine palette, `studio-theme.css` owns type and surface aliases, and
`design/layout.css` owns shared geometry. Check the Foundation Storybook stories
and focused unit tests when changing these contracts.

Import primitives from `./primitives.tsx`. Preserve each caller's
labels, shortcuts, disabled states, and callbacks.

## Text and size tokens

Use the rem scale owned by [`studio-theme.css`](../../studio-theme.css):
`--studio-text-meta`, `--studio-text-body`, `--studio-text-title`,
`--studio-text-section`, and `--studio-text-code`. Use weights 400, 500, and 600.
Apply `.studio-tabular` to counts, costs, limits, and times.

[`design/layout.css`](../../design/layout.css) owns desktop control sizes and
the coarse-pointer touch target. Mantine size variables enlarge standalone
controls; compound NumberInput steppers retain their native proportions.
`--studio-warning` and `--studio-warning-hover` are scheme-aware warning text.

Mantine `xs` maps to meta; `sm` and `md` map to body. Explicit user font choices
and sizes still apply. See the [redesign brief](../../../../../../../docs/design/redesign-brief.md)
for the palette and state contract.

## Buttons

Use `ActionButton` with `actionRole`:

- `primary`: filled primary theme color, for one main completion action per surface.
- `secondary`: bordered, opt-in for other form and dialog actions.
- `quiet` (default): subtle text, for toolbars, headers, sidebars, and navigation.
- `destructive`: filled red, for a confirmed destructive action.

Native Mantine equivalents are `variant="filled"`, `variant="default"`, and
`variant="subtle"`. Use `color="red"` for destructive actions. The global Button
default remains subtle and gray. Existing `IconAction`
under `actions/` preserves the accessible label for icon-only buttons.

```tsx
<ActionButton actionRole="primary" onClick={save}>Save</ActionButton>
<ActionButton actionRole="quiet" onClick={close}>Back</ActionButton>
```

## Layout

- `SettingsSection`: `title`, optional `help` and `action`, body as `children`.
- `SettingsRow`: `label`, optional `help`, control as `children`.
- `PanelHeader`: `title`, optional `help` and `actions`.
- `EmptyState`: inline `title`, optional `help` and `action`.

Settings rows place controls on the right on desktop. They stack on mobile.
Use a real `<label htmlFor>` for the row label, or keep the control's existing
accessible label. The components do not change control names or add live regions.
PanelHeader uses an h3. Choose a parent heading that keeps the page hierarchy.

```tsx
<SettingsSection title="Appearance" help="Choose the chat text.">
  <SettingsRow label={<label htmlFor="font">Font</label>} help="Used in this browser.">
    <NativeSelect id="font" data={fonts} value={font} onChange={changeFont} />
  </SettingsRow>
</SettingsSection>
<PanelHeader title="Changes" actions={<ActionButton onClick={refresh}>Refresh</ActionButton>} />
<EmptyState title="No changes" help="Changed files appear here." />
```

## Menus and dialogs

Mantine Menu dropdowns use `--studio-menu-min` from the shared layout tokens. Group related actions with
`Menu.Label` when useful. Put `Menu.Divider` between groups. Put destructive
items in the final group. Keep item roles and accessible names.

```tsx
<Menu.Dropdown>
  <Menu.Label>Chat</Menu.Label>
  <Menu.Item onClick={rename}>Rename</Menu.Item>
  <Menu.Divider />
  <Menu.Item color="red" onClick={remove}>
    Delete
  </Menu.Item>
</Menu.Dropdown>
```

Import `modalSizes` from `theme.ts`. Use `<Modal size={modalSizes.settings}>` for
settings dialogs (720px desktop, Mantine viewport limit on mobile). The shared
modal overlay blur is 1px to reduce blur when dialogs overlap. Dialog selection
and confirmation behavior remain with each caller.
