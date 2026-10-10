# Studio HTTP API

The Studio API owns request routing and runtime-facing HTTP behavior. Its
component tests live beside the API modules. The [runtime load scenario](benchmarks/runtime_load/README.md)
measures the production API and browser sync path with synthetic active workers;
it does not start model sessions.
