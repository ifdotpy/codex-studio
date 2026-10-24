# Edge rail integration

Owner A applies `App.integration.patch` after the Slice 6 commit.
The patch targets the contract base `10643d021`.
It keeps the existing menu items, handlers, and permission checks.
It hides the header settings, team, messages, and actions controls while the rail is active.
The sidebar control and conversation title remain in the header.

`useEdgeRail()` reads the shared layout contract and the resolved `#root` padding.
Slice 5 owns the root height, safe-area padding, and keyboard compensation.
The hook observes root size, root attributes, document attributes, and viewport events.
Pass `active && !!data && !room?.radio` as its second argument.
This prevents inactive server screens from showing extra rails.

The menu opens toward the content side.
The rail scrolls when the viewport cannot show all controls.
A viewport change keeps the current control inside the rail scroll area.
The status reserve and bottom inset remain outside the rail.

The terminal action uses `screen.execute` for a server screen.
It uses the existing `studio-terminal-command` event for a direct App.
The direct path needs Slice 4's listener update.
The terminal label stays neutral when App cannot read the terminal state.
The component also accepts `terminal.expanded` when that state becomes available.

Checks cover the component, geometry policy, and synthetic browser insets.
The browser fixture uses the real component and hook with the shared contract.
It does not prove the complete App integration or Safari device behavior.
Owner A must check the final integrated header and the terminal command paths.
