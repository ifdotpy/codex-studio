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
