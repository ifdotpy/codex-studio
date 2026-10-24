# Responsive layout port from t3code

Status: proposal. This document changes no application behavior.

## Scope and evidence

The port takes layout rules from t3code and keeps the Studio application model.
Mantine remains the owner of dialogs, drawers, focus traps, and portals.
The port does not require Tailwind, Base UI, React Native, or a new native host.

Source revisions:

- t3code: `5bb54a0f77c6655fdb2ab551612ca1ca1fcd3667`, read-only clone at `/tmp/t3src/t3code`.
- Studio: `28a9cbde525f3ee2b474cd38332f05f22179e798`.
- Inspection date: 2026-10-23. Evidence: source inspection, not a device test.

Path prefixes in the tables:

- `T`: [t3code web source](https://github.com/pingdotgg/t3code/tree/5bb54a0f77c6655fdb2ab551612ca1ca1fcd3667/apps/web/src).
- `N`: [t3code mobile source](https://github.com/pingdotgg/t3code/tree/5bb54a0f77c6655fdb2ab551612ca1ca1fcd3667/apps/mobile/src).
- `S`: [Studio web source](../../workspaces/client/apps/web/src).
- Paths without these prefixes start at the repository root of the named project.

The owner lifted the stabilization freeze on 2026-10-23, according to task coordination.
The removal of the stale freeze section in AGENTS.md awaits its separate pull request.
Features are permitted. Bug fixes still need a failing witness first.
The existing test gates remain in force.
This task authorizes the design document only, with no application changes or push.

Separate work:

- iOS input zoom: task `work-1792783775233041152-186`, branch `fix/ios-touch-input-zoom`.
  That task owns the 16px input floor for touch devices at all widths.
  This plan consumes that fix and does not repeat its font rules.
- iPhone Duo research: task `work-1792783781369213952-187`, branch `research/iphone-duo-web`.
  Expected document: `docs/research/2026-10-23-iphone-duo-web.md`.
  That task owns evidence about viewport segments, posture, safe areas, and the right status bar.
  Slice 6 depends on that document. Native t3code geometry does not prove browser support.

## Intended result

The user can keep the conversation visible while the available space changes.
The sidebar, Workspace panel, and terminal use the space that their actual host provides.
The interface keeps the user preference separate from a temporary size limit.

- At phone widths, navigation and Workspace content use a modal drawer.
- At wider widths, the sidebar uses a bounded width that the user can change.
- The Workspace panel appears beside the conversation only when both columns fit.
- The team panel does not consume the space reserved for a Workspace panel.
- The terminal keeps the header and composer reachable in short windows.
- The keyboard and device safe areas reduce the usable space once.
- A smaller host does not overwrite a preferred width or a visibility preference.
- A mode change preserves the selected section, drafts, terminal sessions, and scroll position.

The design keeps Studio's integer viewport boundaries at first.
It uses local container widths for content decisions.
It does not replace all component thresholds with the t3code breakpoint scale.

## Mechanism inventory and Studio map

Effort means relative integration work, not elapsed time.
Small: one existing surface. Medium: a surface plus state and measurement.
Large: several surfaces or a shell boundary. Slice numbers refer to the plan below.

| Mechanism                           | t3code source and observed rule                                                                                                                                                                                                                | Studio equivalent                                                                                                                                                           | Gap and proposed port                                                                                                                                                                                    | Effort / slice   |
| ----------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------- |
| Viewport breakpoint names           | `T/hooks/useMediaQuery.ts`: sm 640, md 768, lg 1024, xl 1280, 2xl 1536, 3xl 1600, 4xl 2000, in CSS pixels. Maximum queries subtract one pixel. `useIsMobile` uses max-md.                                                                      | `S/App.tsx`, `S/components/Sidebar.tsx`, `S/servers/MultiServerApp.tsx`, CSS media rules.                                                                                   | One named contract for Studio boundaries. Use complementary ranges, including fractional widths. Do not copy t3code's separate TypeScript and Tailwind scales.                                           | Medium / 1       |
| Several definitions of mobile       | `T/components/ChatView.tsx` and `T/components/chat/ChatComposer.tsx` use max-sm. `T/components/ui/sidebar.tsx` uses max-md. `T/rightPanelLayout.ts` uses max-width 980px.                                                                      | Navigation uses 760px. Team presentation uses 1199px.                                                                                                                       | Name the purpose of each boundary. A composer layout, navigation mode, and panel fit are different rules.                                                                                                | Small / 1        |
| Relative sizes and semantic spacing | `T/index.css`: control radius 0.5rem, sidebar insets 0.5rem and 0.625rem, floating inset 0.75rem. Tailwind supplies utility spacing. `T/appearanceFonts.ts` changes the root font size.                                                        | `S/studio-theme.css`, `S/theme.ts`, `S/studioPreferences.ts`, `S/studio-preferences.css`. Most geometry uses px.                                                            | Add semantic size tokens. Use rem for interface geometry and px for measured coordinates. Preserve separate main and sidebar font preferences. Do not copy the root font mutation.                       | Medium / 1       |
| Reading width and shared gutters    | `T/index.css`: chat maximum 46rem, wide mode 72rem, full mode 100%. Safe-area gutters serve both the header and chat.                                                                                                                          | `S/workspace-layout.css`: chat width 860px, text width 680px, gutter 24px or 14px. `S/style.css`: another chat maximum of 760px. Preferences also set a width ratio.        | Keep distinct composer and text widths. Give each value one purpose. Make the header, transcript, queue, and composer consume the same inset result.                                                     | Medium / 1       |
| Container queries                   | `T/components/chat/ComposerSurface.tsx`, `ComposerControlsBar.tsx`, `ComposerBanner.tsx`: named composer containers with 400px, 560px, and 640px cases. `T/index.css`: chat-lane table rule at 36rem.                                          | `S/workspace-layout.css`: workspace container query at 660px. `S/components/Conversation.tsx`: compact tools at workspace width less than 900px or main font at least 20px. | Expand existing container behavior. Do not claim Studio lacks it. Use the width of each component for label removal, wrap, and table presentation.                                                       | Medium / 1, 3    |
| Shrink and scroll boundaries        | `T/components/preview/PreviewPanelShell.tsx`, `T/components/RightPanelTabs.tsx`, `T/components/chat/MessagesTimeline.tsx`: min-w-0, min-h-0, bounded children, and separate scroll regions.                                                    | `S/style.css`, `S/workspace-layout.css`, `S/components/shell/Workspace.css` already use these rules in places.                                                              | Make every new row and column shrinkable. Keep horizontal scroll local to code, tables, diffs, and tab strips. Never scroll the whole shell.                                                             | Medium / 1, 3    |
| Sidebar width policy                | `T/components/threadSidebarWidth.ts`: default 256px, minimum 208px, main-content floor 640px. Brand measurement can increase the minimum.                                                                                                      | `S/workspace-layout.css`: 240px sidebar. `S/style.css`: base width 256px. `S/servers/servers.css`: separate 270px server sidebar.                                           | Use one policy per sidebar role. Clamp against the actual host and measured header content. Keep Studio's current width as the initial preference.                                                       | Medium / 2       |
| Sidebar width state and reset       | `T/components/AppSidebarLayout.tsx`: finite width from local storage, live viewport clamp, measured brand probe, double-click rail reset.                                                                                                      | `S/components/Sidebar.tsx` stores tree state. `S/App.tsx` stores collapse state. No width control appears in these callers.                                                 | Add local width preference with a reset control. Keep effective width separate. A narrow window must not persist its temporary clamp.                                                                    | Medium / 2       |
| Pointer resize lifecycle            | `T/hooks/useResizeDrag.ts`, `useResizableWidth.ts`: pointer capture, animation-frame writes, final storage write, lost-capture and blur cleanup, scope reset.                                                                                  | `S/components/TerminalDock.tsx`: pointer capture and React state changes during drag.                                                                                       | Port the lifecycle, not the dependency stack. Add keyboard resize and a visible reset action. Avoid transcript updates on each pointer event.                                                            | Medium / 2, 3, 4 |
| Mobile sidebar drawer               | `T/components/ui/sidebar.tsx`: mobile open state differs from desktop open state. Sheet width is viewport minus spacing. Safe-area padding applies inside the sheet. Collapsed desktop content becomes inert.                                  | `S/components/Sidebar.tsx`: Mantine Drawer at min(360px, 100vw), separate mobile flag and desktop collapse flag from App.                                                   | Retain the existing state split and Mantine focus behavior. Fit the drawer to usable space. Restore focus on close and remove hidden content from keyboard navigation.                                   | Small / 2        |
| Inline panel versus sheet           | `T/rightPanelLayout.ts`, `T/components/ChatView.tsx`, `T/components/chat/ChatRightPanel.tsx`: sheet at viewport widths up to 980px, inline otherwise.                                                                                          | `S/components/shell/Workspace.tsx`: right modal Drawer only. App mounts `DeferredWorkspace` outside the main workspace.                                                     | Add one Workspace host that selects inline or drawer mode from actual available width. A viewport-only 980px rule does not account for Studio sidebars.                                                  | Large / 3        |
| Panel width budget                  | `T/hooks/usePreviewPanelInlineSize.ts`: default 540px, panel minimum 360px, sibling floor 360px, viewport fraction cap 0.7. Host measurement occurs before paint and on resize.                                                                | Workspace Drawer uses caps of 840px or 1040px. Team uses 248px inline or a 300px drawer.                                                                                    | Measure the common row. Subtract the sidebar, team, borders, and gaps once. Use a conversation floor of its own. If both minima cannot fit, select the drawer before the width clamp.                    | Large / 3        |
| Panel state and focus across modes  | `T/components/RightPanelTabs.tsx`, `T/components/preview/PreviewPanelShell.tsx`, `T/components/RightPanelSheet.tsx`: tabs scroll into view, sheet stays mounted, collapsed inline content becomes inert, focus returns to the toggle.          | `S/components/shell/DeferredWorkspace.tsx` retains the module. `Workspace.tsx` owns section and selection state. Mantine owns modal focus.                                  | Keep one logical Workspace state owner. A change of host must not create a second active content tree or lose a form. Preserve the first-open deferred load.                                             | Large / 3        |
| Sheet width and titlebar inset      | `T/components/RightPanelSheet.tsx`: min(42vw, 28rem), minimum 20rem. At 760px: min(88vw, 24rem), no minimum. Window Controls Overlay reserves the titlebar.                                                                                    | `S/components/shell/Workspace.tsx`, `S/style.css` mobile Drawer rules, desktop inset-titlebar rules.                                                                        | Keep Studio section caps for drawers. Use safe usable width and the existing native titlebar contract. Do not copy Electron bridge calls.                                                                | Medium / 3       |
| Terminal height budget              | `T/components/ThreadTerminalDrawer.tsx`: 180px minimum, 75% viewport cap, further cap by chat-column height minus header and composer. A tiny host can produce less than the nominal minimum.                                                  | `S/components/TerminalDock.tsx`: saved 360px height, 220px drag floor, viewport minus 100px cap, 38px closed bar. `S/style.css` reserves root height.                       | Derive one effective height from the measured host, visible viewport, header, composer, and safe areas. A hard minimum cannot exceed available height. Preserve the preferred height.                    | Large / 4        |
| Terminal fit and persistence        | `T/components/ThreadTerminalDrawer.tsx`, `T/components/chat/ChatTerminalDrawers.tsx`: local layout revision, terminal fit, bounded retained drawers. Grids use minmax(0, 1fr).                                                                 | `S/components/TerminalDock.tsx`: ResizeObserver, xterm fit, resize request. `S/servers/MultiServerApp.tsx` receives terminal geometry for the combined shell.               | Retain existing sessions and the fit-to-request path. Share the effective height with the shell. Do not create sessions or send duplicate resize requests when only the host changes.                    | Medium / 4       |
| Safe areas and root height          | `T/index.css`, `T/components/AppSidebarLayout.tsx`, `T/components/ui/sidebar.tsx`: svh/dvh, env(safe-area-inset-*), edge-specific utilities, overscroll rules.                                                                                 | `S/hooks/mobileViewport.ts`, `S/style.css`: visible height and top offset, four safe edges, Drawer and Modal adjustments.                                                   | Keep the Studio hook as the geometry owner. Give each edge one padding owner per document and per portal. Avoid inset sums at both root and child.                                                       | Medium / 5       |
| Browser keyboard resize request     | t3code `apps/web/index.html`: viewport-fit=cover and interactive-widget=resizes-content.                                                                                                                                                       | Studio `workspaces/client/apps/web/index.html`: viewport-fit=cover, no interactive-widget setting.                                                                          | Add the resize request only with browser evidence. Keep the existing visualViewport path for browsers that ignore the setting. Do not enable keyboard overlay mode.                                      | Small / 5        |
| Software keyboard detection         | `T/lib/softwareKeyboard.ts`, `T/hooks/useSoftwareKeyboardOpen.ts`: VirtualKeyboard geometry when available, otherwise visualViewport resize with width, scale, and 150px delta checks. Desktop bypass. Composer uses this for blur after send. | `S/hooks/mobileViewport.ts`: scale guard, 100px height difference, top offset, animation-frame events, bottom-safe-area suppression. App enables it only in phone mode.     | Extend the existing geometry path to wider touch windows. Keep a separate keyboard-visible result only if a caller needs it. Do not replace geometry with a keyboard boolean or copy send-blur behavior. | Medium / 5       |
| Touch and pointer capability        | `T/hooks/useMediaQuery.ts`: coarse/fine pointer inputs. `T/index.css`: any-hover variant. `T/components/chat/ComposerControl.tsx`: larger controls in compact layouts.                                                                         | `S/style.css`, `S/components/sidebar-projects.css`, `S/appearance.css`: hover and coarse-pointer rules, 44px phone targets.                                                 | Use pointer capability for target size and action visibility. Width decides arrangement. Consume the separate iOS input fix.                                                                             | Small / 1, 2, 5  |
| Layer order                         | `T/index.css`: sheet 46. Terminal uses 45. `T/components/preview/previewMiniPlayerLayout.ts`: floating preview layers 47, 48, 49. Dialogs and popovers use 50 and greater.                                                                     | `S/theme.ts`: layerZIndex app 100, modal 200, popover 300, modalAbovePopover 350, overlay 400, max 9999. CSS also uses 90, 190, 200, 201, 300, 400, and local 1/2/3.        | Move Studio layer values to the shared layout contract. Preserve Mantine and nested-dialog order. Keep local stacking contexts distinct. Do not copy t3code numbers.                                     | Medium / 1       |
| Desktop window chrome               | `T/index.css`: titlebar-area environment values and Windows resize inset. `T/components/AppSidebarLayout.tsx`: macOS controls inset and fullscreen subscription.                                                                               | `S/style.css`: desktop-inset-titlebar rules. Studio has separate native host code.                                                                                          | Preserve the current bridge and host ownership. Include the usable titlebar region in the web shell budget. Native window control changes are outside this port.                                         | Medium / 1       |
| Floating surfaces and anchors       | `T/components/preview/previewMiniPlayerLayout.ts`: fit within host, keep aspect ratio, avoid composer and details obstacles. `T/contextMenuFallback.ts`: viewport clamp.                                                                       | `S/components/Conversation.tsx`: compact menu placement and resize observations. `S/components/ImageViewer.tsx` measures size.                                              | Reuse host-bound placement for existing menus and previews. No new mini-player. Clamp popovers to the visible viewport and retain focus.                                                                 | Medium / 1, 5    |
| Motion and retained content         | `T/panelAnimations.ts`: reduced-motion preference, route-change suppression, presence scoped to a thread. `PreviewPanelShell.tsx` suppresses width animation for resize and maximize.                                                          | `S/style.css`: reduced-motion rules. Mantine drawers supply transitions. `DeferredWorkspace.tsx` retains load state.                                                        | Keep motion out of live resize. A scope change must cancel its active drag. Respect reduced motion and retain only the correct conversation state.                                                       | Medium / 2, 3, 4 |
| Mobile compact and split modes      | `N/lib/layout.ts` deriveLayout: ordinary split requires width at least 720 and height at least 600, in native logical units. Sidebar width is 32% with bounds 280 and 380.                                                                     | Studio has browser width rules, not a native split navigator.                                                                                                               | Port the pure decision function and its preference/effective-state distinction. Use browser measurements and Studio bounds. Native width and height numbers are reference data, not browser defaults.    | Medium / 1, 3    |
| Mobile auxiliary-pane roles         | `N/lib/layout.ts`: supplementary pane needs 960 content width. Inspector uses an 820 viewport floor and a 560 main floor. Auxiliary width has bounds 260 and 480. Inspector can suppress the sidebar without changing its preference.          | Studio team and Workspace panels have separate presentation rules in App.                                                                                                   | Resolve all visible columns together. Opening Workspace can temporarily suppress the team inline panel. Keep the team preference for later restoration.                                                  | Large / 3        |
| Mobile keyboard and pane preference | `N/features/layout/AdaptiveWorkspaceLayout.tsx`: Android soft keyboard plus composer edit can suppress the sidebar. Hardware-keyboard focus alone does not.                                                                                    | Studio viewport hook does not manage sidebar preference.                                                                                                                    | Preserve the preference/effective-state rule. Do not close a sidebar merely because a textarea has focus. Browser keyboard evidence controls any temporary suppression.                                  | Medium / 5       |
| Native reserved regions             | `N/lib/reserved-regions.ts`, `N/native/native-layout-metrics.tsx`: safe areas, size class, vertical bar edge, division/occlusion rectangles, bottom bar inset.                                                                                 | Studio uses browser safe-area environment values. No equivalent reserved-region provider appears in the inspected web layout.                                               | Define a future optional geometry input. Keep the single-rectangle path complete when no segment evidence exists. Do not synthesize a bar or hinge from screen width.                                    | Dependent / 6    |
| Hinge alignment                     | `N/lib/layout.ts`: special native metrics permit split from usable width 600 and height 400 with regular size class. A division can set the pane edge. alignListPaneToHinge reserves the hinge gap and accounts for a leading rail.            | No web segment policy in the inspected Studio layout.                                                                                                                       | Keep only the geometry requirements here. The Duo research owns browser feasibility and the status-bar evidence.                                                                                         | Dependent / 6    |
| Native text scale and lists         | `N/lib/layout.ts`: thread row estimate follows font and accessibility scale. Fixed row height applies only when text fits. Chat maximum 960, Android reading maximum 640. Mobile Metro config uses a 14px rem polyfill.                        | `S/studioPreferences.ts` stores main/sidebar text sizes. Studio has several fixed-height or virtualized lists.                                                              | Measure text-sensitive rows. Test large text before a fixed row promise. Keep native density values out of the web token contract.                                                                       | Medium / 1, 2, 4 |
| Mobile terminal                     | `N/features/terminal/ThreadTerminalRouteScreen.tsx`: safe-area and keyboard insets, keyboard accessory. `NativeTerminalSurface.tsx`: grid from the actual layout.                                                                              | `S/App.tsx` does not mount TerminalDock in phone mode.                                                                                                                      | Borrow fit and usable-height rules for the existing desktop/tablet terminal. A phone terminal is a separate feature and is not part of these slices.                                                     | Small / 4        |

## Current Studio boundaries

The inspected CSS contains 12 distinct max-width values, in CSS pixels:
360, 420, 480, 520, 560, 600, 640, 650, 760, 850, 1050, and 1199.
The desktop complement is min-width 761px.
This count excludes pointer queries and the 660px container query.

The smaller thresholds belong to model pickers, forms, questions, message rows,
terminal controls, analytics, and diffs. They do not all represent device classes.
The migration registers each purpose before it removes any value.
For example, a terminal control threshold of 650px can become a terminal container rule.
It must not silently become a 640px phone rule.

`Conversation.tsx` already measures workspace width and observes font preferences.
The 900px tools threshold differs from the 660px CSS team-toggle threshold.
These values have different purposes until a layout test proves otherwise.

The combined-server path also matters. `MultiServerApp.tsx` measures its sidebar,
publishes `--shell-sidebar-width`, and receives terminal geometry.
`servers.css` positions the terminal bar and the active server view.
A successful direct App layout does not prove the combined-server layout.

The t3code policies also need adaptation:

- Its sidebar constants multiply 16. They do not follow the root font at runtime.
- Its TypeScript maximum queries subtract one pixel. The port needs explicit fractional-width coverage.
- Its panel presentation follows viewport width. The panel width clamp alone cannot guarantee space for both columns.
- Its native layout consumes size classes and reserved regions. The web port needs browser evidence for those inputs.
- Its layer values belong to Base UI and its preview surfaces. Studio keeps the Mantine layer order.

## Shared contract

### Owner and consumers

Proposed owner: `workspaces/client/apps/web/src/layout/layout-contract.json`.
This file owns responsive boundaries, shared dimensions, ratios, layer values,
and the names that state their purpose. Each entry has `value`, `unit`, and `purpose`.
The contract belongs to the web renderer. It does not belong in the server HTTP schema.

Proposed generator: `workspaces/client/apps/web/scripts/generate-layout-contract.mjs`.
Its write mode emits `src/layout/layout-contract.generated.ts` and
`src/layout/layout-contract.generated.css`. Its check mode detects stale output.
TypeScript exports retain literal types. CSS output contains custom properties.
The generator uses the JSON data and contains no independent defaults.

Consumers:

- `layout/deriveWorkspaceLayout.ts`: pure layout policy and finite-number validation.
- `layout/useWorkspaceLayout.ts`: host measurement and a small layout snapshot.
- `theme.ts`: generated layer values, modal sizes, and applicable Mantine sizes.
- Component styles: generated custom properties for dimensions, spacing, and layers.
- Media-query callers: generated named query strings, not handwritten numbers.
- Tests: the owner data and boundary relations, not another copy of the values.

CSS variables cannot supply media-query or container-query conditions.
For those conditions, the plan keeps static CSS literals plus a mandatory parity check.
Each query carries a contract-key comment. No new CSS preprocessor is necessary.
The check parses query conditions and compares the purpose, unit, operator, and value.
The generator can replace this exception later if the build gains a supported query transform.

No second `layerZIndex` definition remains in `theme.ts`.
That file can re-export the generated object to preserve imports during migration.
Equal values with different purposes remain separate entries.
A 44px touch target does not share ownership with a 44px native titlebar.

### Initial values and units

These are proposed initial values, not verified product requirements.
One rem means the computed browser root font size.
A runtime policy resolves rem values from the current root size before it compares measured pixels.
It repeats the measurement after root-size changes.
The policy does not assume that one rem always equals 16px.

| Contract name                                     | Initial value and unit                          | Purpose                                                                                |
| ------------------------------------------------- | ----------------------------------------------- | -------------------------------------------------------------------------------------- |
| navigation.inlineMin                              | 761 CSS px                                      | Navigation inline at or above this width. Compact uses the strict complement.          |
| team.inlineMin                                    | 1200 CSS px                                     | Initial compatibility boundary. Actual column fit can still suppress the team.         |
| tools.compactContainerMax                         | 900 CSS px, exclusive                           | Current tools threshold for the workspace width.                                       |
| tools.compactFontMin                              | 20 CSS px                                       | Current main-font threshold for compact tools.                                         |
| tools.teamToggleContainerMax                      | 660 CSS px, inclusive                           | Current local team-toggle padding case. Separate from tools presentation.              |
| sidebar.preferred                                 | 15rem                                           | Current 240px appearance at a 16px root.                                               |
| sidebar.min                                       | 13rem                                           | Proposed lower bound from t3code, plus the measured header minimum.                    |
| conversation.min                                  | 40rem                                           | Proposed main-column floor. Fit evidence can revise this owner value.                  |
| conversation.textMax                              | 42.5rem                                         | Current 680px transcript width at a 16px root.                                         |
| conversation.composerMax                          | 53.75rem                                        | Current 860px composer width at a 16px root.                                           |
| workspacePanel.min / preferred                    | 22.5rem / 33.75rem                              | Proposed 360px minimum and 540px preference at a 16px root.                            |
| workspacePanel.viewportMaxRatio                   | 0.7, unitless                                   | Fraction cap before the local host cap.                                                |
| workspaceDrawer.defaultMax / wideMax              | 52.5rem / 65rem                                 | Current 840px and 1040px section caps at a 16px root.                                  |
| team.preferred / drawerMax                        | 15.5rem / 18.75rem                              | Current 248px inline and 300px drawer values at a 16px root.                           |
| terminal.preferred / min                          | 22.5rem / 13.75rem                              | Preserve Studio's 360px preference and 220px nominal floor at a 16px root.             |
| terminal.viewportMaxRatio                         | 0.75, unitless                                  | Proposed viewport cap from t3code. Available content can reduce it further.            |
| terminal.closedBar                                | 2.375rem                                        | Current 38px total bar height at a 16px root, including its border.                    |
| controls.touchMin                                 | 44 CSS px                                       | Existing phone target floor, also applicable to coarse pointers.                       |
| spacing.sidebarInset / controlGap / floatingInset | 0.5rem / 0.5rem / 0.75rem                       | Separate named purposes from t3code.                                                   |
| layers.*                                          | Existing Studio layer values, unitless integers | Preserve the order during migration. Add names for terminal and navigation exceptions. |

The contract also registers the existing component thresholds from the inventory.
It also owns the existing 720px settings-modal cap, under its own purpose name.
It preserves those values until their component slice supplies a replacement witness.
The existing server-selector width has its own role and name.
It does not reuse the conversation sidebar value merely because both are sidebars.

### Runtime rules

The policy accepts one measured host rectangle, safe insets, visible viewport,
preferred widths, and preferred visibility. It returns effective widths and presentation modes.
All rectangles use coordinates from the same document.
The parent shell and a child server view do not subtract the same sidebar twice.

1. Validate all measurements and stored values against the owner contract.
2. Subtract safe areas only if the host rectangle does not already exclude them.
3. Reserve the header and composer from their actual bounds.
4. Apply the navigation mode and its effective width.
5. Reserve the conversation floor before any optional inline column.
6. Give an open Workspace panel priority over the optional inline team panel.
7. If both panel minima cannot fit, use the Workspace drawer.
8. Clamp the effective panel width to the remaining row and viewport fraction.
9. Clamp the terminal to the remaining vertical space, even below its nominal minimum.
10. Keep preferred values unchanged after a temporary clamp.

A clamp never receives a maximum less than its minimum.
An insufficient width changes the presentation mode first.
For a short terminal host, the effective floor becomes the available height.
For a narrow navigation host, the sidebar becomes a drawer.

Only a completed user resize or reset changes a width preference.
A scope change cancels the drag before it can write to a different scope.
A resize observer updates local size state, not conversation data.
Layout state does not alter requests, permissions, terminal identities, or outbox state.

### Parity check

Proposed check: `workspaces/client/apps/web/scripts/check-layout-contract.mjs`.
The central integration owner adds it to the existing contract-check path and fast tier.
The check needs no browser and no provider.

Required checks:

- Parse every source CSS media/container width condition and every TypeScript query caller in the web source.
- Require a contract key for each migrated rule and each new rule.
- Register all remaining legacy rules by path, purpose, and planned owner.
- Reject an unregistered rule, duplicate owner, invalid unit, or stale generated file.
- Compare named query literals against the owner, including inclusive and exclusive operators.
- Reject literal shared dimensions and global layer values outside the owner or generated output.
- Register local stacking contexts separately from global portal layers.
- Keep the native emergency error banner at 2147483647 as a named, explicit exception.
- Exercise the check with one changed boundary, one changed layer, and one added unregistered consumer.
- Require each invalid fixture to fail with its owner key and consumer path.

The migration has a finite legacy list, not a broad directory exemption.
Each slice removes its migrated entries. A new legacy entry requires review.
Runtime validators import the generated contract, so the parity check does not compare two handwritten schemas.

## Ordered implementation slices

One integrator owns common files. The other workers own complete surfaces.
The minimum team has three implementers. The preferred team has five.

### Slice 1: stable conversation controls and shared geometry

User result: the header, composer, transcript, and queue fit the same conversation column.
Controls remain reachable with large text and at fractional viewport boundaries.
This slice includes the contract in its real callers. It is not a separate infrastructure-only change.

Owner A, central integration:

- `S/layout/` contract, generated outputs, policy, and observer modules.
- `workspaces/client/apps/web/scripts/` contract generator and parity check.
- `S/theme.ts`, `S/studio-theme.css`, `S/workspace-layout.css`.
- `S/style.css`, `S/appearance.css`, `S/studio-preferences.css`: common geometry and removal of superseded rules only.
- `S/App.tsx`, `S/components/Conversation.tsx`.
- `S/servers/MultiServerApp.tsx`, `S/servers/servers.css`.
- Root contract-check and fast-tier selection files, only for the new checks.

Work:

1. Establish the owner contract with the current values and explicit purposes.
2. Integrate container-based tools and shared conversation insets through their existing callers.
3. Replace the viewport min/max gap with complementary ranges.
4. Pass the measured row and presentation inputs through the App boundary.
5. Preserve the combined-server sidebar and terminal geometry protocol.
6. Register the remaining component thresholds as legacy consumers.

Witnesses: boundary equality and fractional width, narrow workspace inside a wide
window, main font at 20px, long header text, and no document horizontal overflow.
Use the existing text-clipping checks for the final rendered surfaces.

Dependency: none. This slice establishes the interfaces for slices 2 to 5.
Owner A remains the only editor of the common files after this slice.

### Slice 2: bounded sidebar with a mobile drawer

User result: the user can resize or reset the desktop sidebar without hiding the
conversation. The mobile drawer preserves search and selection behavior.

Owner B:

- `S/components/Sidebar.tsx`, `S/components/sidebar-projects.css`.
- New `S/components/sidebar/SidebarFrame.tsx` and `sidebar-frame.css`.
- New sidebar width policy, resize hook, and focused tests under that directory.
- Existing sidebar browser cases, including combined navigation coverage.

Work:

1. Use the contract bounds and measured header minimum in SidebarFrame.
2. Keep a preferred width and a separate effective width.
3. Add pointer and keyboard resize with cancellation and reset behavior.
4. Keep the compact drawer and desktop visibility preferences separate.
5. Restore focus before hidden content becomes inert or leaves the tree.
6. Apply the shared width to the closed terminal bar through the existing shell measurement.

Witnesses: invalid stored width, large text, drag beyond both limits, lost capture,
window shrink and expand, keyboard reset, and drawer close after chat selection.
The combined shell must report the effective sidebar width, not its preferred width.

Dependency: slice 1 interfaces. Owner A applies any App or common CSS integration patch.
The slice is complete only after that caller patch and its checks pass.

### Slice 3: one Workspace panel, inline or drawer

User result: the user can read Workspace content beside the conversation when it
fits. The same section opens in a drawer when the host becomes too small.

Owner C:

- `S/components/shell/Workspace.tsx`, `Workspace.css`, `DeferredWorkspace.tsx`.
- New `WorkspaceSurface.tsx`, `WorkspacePanelHost.tsx`, and panel-local styles beside Workspace.
- Panel width state and focused tests beside the panel host.
- Existing `tests/workspace/workspace-ui.spec.mjs` cases.

Work:

1. Keep Workspace section, selection, and form state above the host choice.
2. Extract the existing content into one surface without new business logic.
3. Mount the inline host in the measured row supplied by Owner A.
4. Use a Mantine Drawer for the compact presentation.
5. Add bounded inline resize and preserve the section-specific drawer caps.
6. Suppress the optional inline team panel when the Workspace panel needs its space.
7. Restore the team preference when the Workspace panel closes or the host expands.
8. Preserve focus, drafts, scroll, and deferred first-open behavior across modes.

Form drafts and scroll snapshots must live above the host boundary before it changes.
Each child restores its state from that owner after a host change.
A single content instance does not by itself preserve state across a portal remount.

The App already mounts Workspace outside the main conversation.
Owner A supplies the inline slot and panel/team policy at that actual caller.
Owner C does not create a second hidden Workspace instance to avoid this integration.
Presentation changes must not rerun a submit handler or repeat a server request.

Witnesses: direct App and combined shell, sidebar open and closed, team open,
panel minimum at equality, and just below the fit boundary.
Additional cases cover large text and section changes.
A form retains its draft after inline-to-drawer-to-inline changes.
Only the visible host owns a focus trap.

Dependency: slice 1 interfaces. Slice 2 can run in parallel because the panel uses
the measured row, not a second copy of sidebar width state.
The inline presentation changes user-visible behavior and needs its own acceptance cases.

### Slice 4: terminal within the available height

User result: the open terminal does not cover the composer or hide its own controls
when the window becomes short. The combined shell uses the same effective height.

Owner D:

- `S/components/TerminalDock.tsx`, `S/components/terminal-dock.css`.
- New terminal height policy and resize lifecycle modules beside TerminalDock.
- Existing terminal tests and `tests/sidebar/combined-terminal-under-sidebar.spec.mjs`.

Work:

1. Replace the repeated viewport-minus-100 calculations with one local height result.
2. Apply the result to the dock, root reservation, and shell geometry message.
3. Preserve the 38px closed-bar appearance at the default root size.
4. Keep pointer resize local and persist the preference only after completion.
5. Keep the existing xterm fit and terminal resize request path.
6. Refit after the effective size changes without replacing the terminal session.

Owner A removes the old root-height rule and updates shell CSS from the agreed result.
Owner D does not edit those common files.
The effective height must agree across the dock, root reservation, and parent shell.

Witnesses: short window, stored height greater than the host, open/close, font change,
sidebar resize, and server switch.
The terminal grid changes with its host.
The test must observe the existing terminal identity and request path.
Phone terminal access remains out of scope.

Dependency: slice 1 interfaces. Slices 2 and 3 can run in parallel.

### Slice 5: usable space with keyboard and safe areas

User result: the composer, drawer footer, and controls remain inside the visible
area after a software keyboard or browser toolbar changes the viewport.

Owner E:

- `S/hooks/mobileViewport.ts` and focused tests beside the hook.
- `workspaces/client/apps/web/index.html` for the viewport request.
- New `S/hooks/viewport-surface.css` for viewport and portal bounds.
- Existing mobile keyboard cases. Owner A imports the new style and applies App enablement changes.

Work:

1. Keep one geometry observer for the active document.
2. Extend the enablement rule beyond the 760px phone condition where browser evidence requires it.
3. Preserve pinch-zoom ownership and the current composer focus exception.
4. Assign safe-area padding once for each visible edge and portal surface.
5. Compare interactive-widget behavior with the existing visualViewport behavior.
6. Keep the no-visualViewport fallback and remove all event listeners on cleanup.

The separate input zoom task owns the font floor and its original CSS edits.
Owner A reconciles its common-file changes before this slice integrates.
This slice does not add an independent copy of those touch input selectors.

Witnesses: keyboard open at mount, open/close, toolbar resize, rotation, nonzero
top offset, absent visualViewport, wide touch window, and pinch zoom.
Hardware-keyboard focus alone must not suppress a pane.
Actual iOS behavior requires a device or supported simulator check.
Synthetic viewport events prove the policy, not the browser keyboard behavior.

Dependency: slice 1 interfaces and the separate iOS input fix.
The independent hook and test work can proceed while that fix awaits review.

### Slice 6: Duo segments layout

Placeholder only. Dependency: task `work-1792783781369213952-187` and
`docs/research/2026-10-23-iphone-duo-web.md`.

Expected user result: conversation and navigation avoid an observed hinge or
reserved status-bar region when the browser exposes sufficient geometry.

The research result must identify the available browser measurements first.
The later slice can supply optional segment rectangles to the existing layout policy.
The policy keeps its single-rectangle fallback when measurements are absent.
It does not infer a hinge, status bar, or posture from a device name or viewport width.
No native t3code size-class or iOS-version rule enters the browser contract by assumption.

Owned files, acceptance cases, and authorization remain unset until that evidence exists.
This placeholder does not delay slices 1 to 5.

## Parallel work and integration

| Implementers | Assignment                                                                  |
| ------------ | --------------------------------------------------------------------------- |
| Three        | A: slice 1 and integration. B: slices 2 then 5. C: slices 3 then 4.         |
| Four         | A: slice 1, slice 5, integration. B: slice 2. C: slice 3. D: slice 4.       |
| Five         | A: slice 1 and integration. B: slice 2. C: slice 3. D: slice 4. E: slice 5. |

The order is slice 1 first, then slices 2 to 5 in parallel.
Each surface worker supplies a narrow integration patch to Owner A.
Owner A integrates each complete surface with its caller before that slice closes.
Workers do not edit common styles, App, or MultiServerApp on parallel branches.
Generated files change only through the contract generator.
Contract changes return to Owner A before a surface worker depends on them.

An observer hook belongs to the surface with the smallest state boundary that
can own its result. The plan does not route every resize through App.
The shared snapshot contains modes and settled bounds, not every drag coordinate.

## Verification and delivery gates

This design requires no application tests because it changes no runtime source.
Document checks cover source paths, formatting, scope, and the final Git diff.
The implementation uses these gates:

1. Reproduce each reported defect before its fix.
2. Add the smallest deterministic failing witness to the short tier.
3. Run the focused policy and contract checks for the affected surface.
4. Run selected existing browser scenarios against the actual integrated caller.
5. Inspect rendered controls, focus, scroll, and overflow at the boundary values.
6. Run one integrated `just test-fast` before the pull request or merge.

The browser cases cover phone widths 320, 360, 390, and 430px, plus tablet and desktop hosts.
They cover 760/761px and 1199/1200px boundaries, fractional widths, and container fit boundaries.
They also cover a wide viewport with a narrow content row, short landscape height,
large text, coarse pointer, safe-area fixtures, and reduced motion.
Expected fit boundaries come from the owner contract and measured borders, not duplicated literals.

Relevant existing suites:

- `tests/layout/text-clipping.spec.mjs`.
- `tests/mobile/mobile-client-ui.spec.mjs` and `mobile-keyboard-ui.spec.mjs`.
- `tests/workspace/workspace-ui.spec.mjs`.
- `tests/terminal/terminal-dock-ui.spec.mjs` and terminal input cases.
- `tests/sidebar/combined-terminal-under-sidebar.spec.mjs`.
- `tests/servers/multi-server-ui.spec.mjs`.

These paths are relative to `workspaces/client/apps/web`.
Existing manual browser suites remain separate from the short deterministic tier.
New policy witnesses must fit the short tier.
No load test is necessary for this port.

Automated desktop checks use hidden windows and isolated state.
A live backend, user terminal, or active agent is not a layout fixture.
Device evidence is necessary for keyboard and Duo claims that a browser fixture cannot establish.

Every future push to `main` also requires `just test-comprehensive` on the exact commit.
This document authorizes no push, deployment, or package installation.
