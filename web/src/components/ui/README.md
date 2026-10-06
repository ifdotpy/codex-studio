# Shared UI foundation

Import primitives from `./components/ui/primitives`. Preserve each caller's
labels, shortcuts, disabled states, and callbacks.

## Text and size tokens

- `--studio-text-body`: 14px.
- `--studio-text-meta`: 12px minimum for dates, status, help, and counts.
- `--studio-text-code`: 13px for code and command output.
- `--studio-action-size`: 32px for desktop header and icon buttons.
- `--studio-warning` and `--studio-warning-hover`: theme-aware warning colors.

Mantine `xs` is 12px; `sm` and `md` are 14px. Existing custom font preferences
still apply. Mobile controls retain the existing 44px minimum touch target.

## Buttons

Use `ActionButton` with `actionRole`:

- `primary`: filled indigo, for the main completion action.
- `secondary` (default): bordered, for other actions.
- `quiet`: subtle text, for navigation and low-emphasis actions.
- `destructive`: filled red, for a confirmed destructive action.

Native Mantine equivalents are `variant="filled"`, `variant="default"`, and
`variant="subtle"`. Use `color="red"` for destructive actions. The global Button
default is now bordered. Explicit variants remain valid. Existing `IconAction`
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

Mantine Menu dropdowns have a 220px minimum width. Group related actions with
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
