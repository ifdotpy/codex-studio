# Studio redesign brief: dispatch board

Status: experiment on branch `ui/redesign`. Not for `main` until the owner
approves it after a visual review.

## Subject, audience, job

Layr Studio is a dispatch console for a fleet of AI coding agents that work in
parallel across several projects. One operator runs it for long sessions on a
desktop and checks it from a phone.

Primary job: in one glance answer "what is running where, who waits for me,
what broke, what it costs", then steer the agent that needs it.

## Concept

Every chat and every agent is a strip on a dispatch board. The leading edge of
a strip is a state rail. Strips are ordered by what needs the operator. The
state rail and the "Needs you" lane are the one bold element. Everything around
them stays quiet: hierarchy comes from type weight and spacing, not from boxes.

## Palette

State is the only saturated color. Nothing decorative is colored.

| Name      | Light     | Dark      | Meaning                                |
| --------- | --------- | --------- | -------------------------------------- |
| Porcelain | `#F3F5F7` | —         | Light page background                  |
| Graphite  | —         | `#12161C` | Dark page background                   |
| Surface   | `#FFFFFF` | `#1A2028` | Panels, dialogs                        |
| Ink       | `#18202A` | `#E6EAF0` | Primary text                           |
| Signal    | `#08777A` | `#3FC1C3` | Primary action and the `working` state |
| Beacon    | `#875800` | `#F0B43C` | `answer`: the operator must act        |
| Fault     | `#B52E25` | `#F0776B` | `error`                                |
| Settled   | `#237448` | `#62C08E` | `unread`: new result to read           |
| Idle      | `#5B6675` | `#95A1B0` | `paused` and `none`                    |

The final light state shades are darker than the initial proposal so they can
also label small text. Against both page and panel surfaces, the minimum WCAG
contrast is 4.89:1 for light states and 5.90:1 for dark states. Muted text reaches
5.79:1 in light and 6.25:1 in dark. Primary filled actions use white on Signal 7
in light and Mantine auto-contrast on Signal 4 in dark. Token owners are
`src/theme.ts` (Mantine palettes), `src/studio-theme.css` (surfaces/type aliases),
and `src/design/state.css` (semantic state colors). The table records their
reviewed values; consume the owners, not copied hex values.

Signal is the Mantine primary color (a 10-shade `signal` palette). The state
names are the existing `ChatIndicatorKind` values from
`src/components/chat-status/chatStatusModel.ts`; that type owns the state set.

## Type

One family: Inter (already the theme font), weights 400, 500 and 600 only.
Scale in rem: meta 0.75, body 0.875, title 1, section 1.25. Counters, limits,
costs and times use tabular numerals. Sentence case everywhere. No all-caps
labels, no monospace for labels (monospace is for code and commands only).

## Layout

Desktop:

```
┌ Fleet ───────────────┬ Chat ─────────────────────────────┬ Team ───────────┐
│ Needs you        2   │ Title                state  team ▮▮▮│ ▮ Lead  Working │
│ ▮ arb2  Review docs  │───────────────────────────────────│ ▮ W1    Tests   │
│ ▮ attar Approve run  │ transcript                        │ ▮ W2    Done    │
│ Projects             │                                   │ ▮ Review Idle   │
│ af-utils    ▮2 ▮1    │                                   │                 │
│  ▮ Обновление…   3m  │ composer                          │                 │
│ codex-studio         │ Lead Astra 6 High   Workers Luna 6 │                 │
└──────────────────────┴───────────────────────────────────┴─────────────────┘
```

Phone: the fleet board is the home screen. A chat opens as a detail screen
with Back. "Needs you" stays on top.

Alignment: text left aligned; numbers right aligned in their column.

## Principles

1. State is the only saturated color; Signal also marks the one primary action.
2. Hierarchy by type and spacing. Borders only mark a grouping boundary;
   shadows only on overlays.
3. Order by urgency: `answer`, `error`, `working`, `unread`, `paused`, `none`.
4. Dense and legible: one line per strip; a second line only for live activity.
5. Mantine components and native controls; no custom keyboard handlers.
6. Component CSS uses tokens only. Raw sizes live in the token files.
7. Copy names things by what the operator does, in plain sentence case. Show
   separate facts as separate elements, not as `A · B · C` strings.

## Review against generic defaults

- Near-black with one acid accent: rejected. Graphite base carries a state
  scale where each color has a meaning; Signal teal is mid-saturation.
- Identical rounded cards with shadows: rejected for strips with a rail.
- Middle-dot meta strings: replaced with columns and separate elements.
- Monospace data labels: rejected; tabular Inter numerals instead.

## Contract for all workstreams

- Tokens: `src/design/state.css` (state colors, rail size) and the Mantine
  theme in `src/theme.ts`. Only the foundation workstream edits them.
- A state rail: any element with `data-studio-state="<ChatIndicatorKind>"`
  gets a leading rail in that state's color. An inline marker is
  `<span class="studio-state-dot" data-studio-state="...">`. No JS.
- State colors: `var(--studio-state-<kind>)`; soft backgrounds:
  `var(--studio-state-<kind>-soft)`.

## Shared geometry

`src/design/layout.css` owns the rem geometry, imported by `studio-theme.css`
for the application and Storybook. Current reviewed values:

| Token                        |    rem | Purpose                            |
| ---------------------------- | -----: | ---------------------------------- |
| `--studio-border-width`      | 0.0625 | Group separator                    |
| `--studio-control-size`      |      2 | Desktop icon control               |
| `--studio-touch-target`      |   2.75 | Standalone coarse-pointer control  |
| `--studio-strip-min-height`  |   2.75 | Dense title/activity strip         |
| `--studio-strip-indent`      |   0.75 | One hierarchy level                |
| `--studio-team-size`         |     20 | Team dock inline size              |
| `--studio-form-max`          |     40 | Readable form column               |
| `--studio-settings-nav-size` |     12 | Settings navigation inline size    |
| `--studio-room-list-size`    |     16 | Messages room selector inline size |
| `--studio-meter-width`       |      3 | Context meter length               |
| `--studio-meter-size`        | 0.1875 | Context meter thickness            |

`src/studio-preferences.css` alone owns the custom transcript column through
`--studio-content-width-ratio` and its computed `--studio-content-column`.
Full uses the entire available conversation width minus its gutters. Original
layout retains the geometry from `workspace-layout.css`. User-selected font
families and sizes still override the Inter theme defaults.

## Workstreams and file ownership

| Stream                | Owns                                                                                                                                  |
| --------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| Foundation            | `theme.ts`, `studio-theme.css`, `src/design/*`, global rules in `style.css` and `appearance.css`, `realtime-voice.css`                |
| Execution and models  | `agents/ExecutionSettings*`, `AgentSetupPicker`, `models/*`, `UnifiedAgentSettings`, `AccountExclusionsControl`, chat settings dialog |
| Fleet board           | `Sidebar*`, `SidebarRow`, `sidebar/*`, `sidebar-projects.css`                                                                         |
| Conversation chrome   | conversation header, empty chat, composer footer layout, usage footer                                                                 |
| Team and activity     | team panel, `WorkerCard`, `team-navigation.css`, background tasks                                                                     |
| Settings and messages | Studio settings, project settings, Messages drawer                                                                                    |

A stream that needs a change in a file it does not own asks the owner through
the lead.
