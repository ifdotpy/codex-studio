# Backend benchmark scenarios

## Change Contract

For contributors measuring backend behavior: keep each scenario in its own named
directory, with a README, executable entry point, and focused tests. Exercise the
production code; only the workload and external providers should be synthetic.
Use temporary state outside the checkout, never an existing user's database or
provider session. Each scenario owns its workload, limits, and result format.
Its local tests must cover successful delivery and failure reporting.

## Choose a measurement

- [Message delivery](message_delivery/README.md): how long synthetic agent
  messages take to reach a local HTTP client through sync invalidation and pulls
  or the direct transcript stream, with and without concurrent history import.
- The runtime-load benchmark is retired because its check fixture could not run
  and the harness depended on the legacy `/api/state` snapshot. Its historical
  measurements remain in `docs/verification/2026-10-01-runtime-latency.md`.
- [Analytics parser](../analytics/README.md#benchmark): how long it takes to
  translate a batch of journal records. This component-local benchmark stays
  next to the parser it measures.

A scenario spans multiple components; a component benchmark measures a smaller
operation. They answer different questions. A fast parser does not by itself
prove that a busy chat receives updates quickly.

## Add another scenario

Create a sibling of `message_delivery/`, rather than adding unrelated scripts to
that directory. Keep supporting fixtures and tests inside the new scenario. Share
helpers only after two real scenarios need the same behavior; do not add a generic
benchmark framework in advance.

Document where the clock starts and stops, which production paths run, and which
parts are simulated or excluded. Keep fixture setup outside the measured window.
Use predictable inputs and check outputs: losing work must fail the run, not make
the numbers look faster. Bound setup, execution, and cleanup, and report failures
with enough context to reproduce them.

Results and temporary databases belong outside this checkout. Compare runs on
the same machine with the same workload and similar background load. A single
run describes that experiment; it is not a universal performance guarantee.
Keep the total offered rate and rate per subscriber visible: adding subscribers
at a fixed total rate gives each subscriber fewer messages per second. Differences
between those cases are not by themselves evidence of improved scalability.
