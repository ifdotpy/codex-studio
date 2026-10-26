# Layout owners

`layout-contract.json` owns the layout dimensions, conditions and spacing.
The generated TypeScript and CSS files expose that contract to consumers.
`deriveWorkspaceLayout` owns the Duo profile. `mobileViewport` publishes
`data-duo-ui` for that profile in either orientation.

## Native portrait title band

The iOS shell (PR #528) owns `data-viewport-top-strip` on the root element,
the top edge rail, and `--native-safe-top` in CSS pixels.
It sets `--viewport-overlay-inset-top` in CSS pixels when drawers must start
below the strip. The shared surface defaults that inset to zero.

The web client (PR #532) owns the title band material and text clearance.
When the Duo profile, top strip and top rail are present, the band spans
the viewport. Its height is the greater of the title height contract and
the native top inset. The transcript reserves that same height.
The first rail button bounds the title text. The rail stays above the band.

`duo-title-bar.spec.ts` builds a stand-in for the shell DOM until PR #528
merges. It verifies the consumer contract, not native shell publication.

Touch drawers paint through the bottom safe inset. Their scroll content
reserves that inset. While a touch modal is open, the passive usage band
is hidden so the conversation cannot show below the modal.

## Pane background bounds

`useWorkspaceLayout` measures the app safe insets in CSS pixels.
`deriveWorkspaceLayout` uses the bottom inset to extend Sidebar, Team,
Workspace and Terminals backgrounds through the Home region.
Conversation bounds keep the safe content frame. Navigation content reserves
the same bottom inset as inner padding. The navigation layer covers the
passive conversation usage band; the edge rail stays above navigation.

The conversation uses left-position motion so its native title stays fixed
to the viewport. Other panes keep their transform slides. The title text
starts inside the conversation column, clear of the sidebar and top rail.
The passive bottom-band helper has no background during the pane path;
the extended panes supply that paint.

## Paired-server navigation

The active `App` owns the pane path state. `PanePathShellContext` publishes
its derived presentation to `MultiServerApp`. The combined shell renders
that sidebar rectangle and forwards toggle and close actions to the screen.
It does not derive another layout or copy the active screen state.

`useWorkspaceLayout` reads `--viewport-overlay-inset-top` in CSS pixels.
The layout owner applies that clearance to navigation panes only, so their
headers start below the native top rail. The conversation keeps its title
band at the viewport top. A previous desktop collapse preference does not
hide a sidebar selected by the pane path.
