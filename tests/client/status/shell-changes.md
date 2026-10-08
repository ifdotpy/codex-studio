# Shell browser spec changes

- `models/model-catalog-pending-browser.spec.mjs`: replaced one-second polling expectations with explicit model resource-change signals; class 2, based on `ef95d230` replacing polling with resource watches. Kept checks for the pending state, retained models, successful refresh, permanent errors, unavailable-account handling, disabled cleanup, and aborting stale requests.
- `models/model-catalog-pending-browser.spec.mjs`: updated the fetch double to read URL and abort signal from the `Request` argument used by the API client; class 1, based on the failed trace showing `/api/models` fell through to Vite and returned HTML.
