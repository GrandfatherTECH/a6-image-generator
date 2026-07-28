# Developer Guide

## Scope and phase discipline

The repository currently implements Phase 1. The Phase 0 CLI and transport behavior remain supported after successful real-gateway verification. Phase 1 adds only the minimal external Slint connectivity interface; typed application state and stronger stale-result guarantees remain Phase 2 work.

The command-line interface is a permanent application surface, not throwaway probe code. Future GUI code should call the library modules rather than duplicate configuration or HTTP behavior.

## Architecture

```text
src/main.rs          CLI parsing, human-readable output, exit status
build.rs             compiles the external Slint source at build time
ui/app.slint         minimal desktop layout, properties, and callbacks
src/config.rs        environment loading, validation, secret masking
src/api/types.rs     typed gateway requests, responses, and metadata
src/api/error.rs     structured HTTP and transport error categories
src/api/client.rs    endpoint construction, HTTP calls, response decoding
src/app/controller.rs configuration display, callbacks, workers, cancellation
src/storage/mod.rs   off-thread image conversion, preview pixels, XDG output
tests/               mock-server transport tests; never real API calls
```

`Config` owns the API key and uses custom `Debug` formatting. The raw key is exposed only inside the crate for building the per-request authorization header. `ApiClient` has no Slint dependency, uses separate 15-second connect and 300-second overall timeouts, and does not attach authorization to fallback image-download URLs.

`endpoint_url` treats a configured root ending in `/v1` as versioned. Otherwise it appends `/v1` before the requested resource. This supports both documented A6API base URL forms and proxy roots such as `https://host/openai/v1`.

Generation responses are decoded in this order:

1. Find the first `b64_json` value and decode it.
2. When no Base64 value exists, use the first URL as a compatibility fallback.
3. Fail explicitly if neither form exists.

Image validation and format conversion run through `tokio::task::spawn_blocking`. The desktop save path also obtains RGBA preview pixels during that same decode pass. A `SharedPixelBuffer` is built on a Tokio worker, then the inexpensive Slint `Image` handle is created on the UI event loop. Atomic output writes are scheduled for Phase 2 as required by the product plan.

## Desktop execution model

`app::run` creates the Slint component on the main thread and a separate multi-thread Tokio runtime for background work. Slint callbacks only validate input, update immediate UI properties, and spawn work. They never await network, decoding, or filesystem operations.

`OperationControl` stores the active Tokio `AbortHandle` and a monotonically increasing generation number. `Cancel` invalidates the current generation before aborting its task, so a result already queued from the worker cannot replace the cancelled UI state. Phase 2 will promote this minimal coordination into typed application state with dedicated transition and stale-result tests.

Worker completion uses `Weak<AppWindow>::upgrade_in_event_loop`. The closure checks that its operation generation is still current, clears the busy state, and applies either the sanitized error or successful result. Closing the window drops the strong Slint handle and any later worker result is discarded.

The desktop request currently uses `ImageGenerationRequest::test_image`, which fixes size, quality, and format while accepting the prompt and configured model. The CLI smoke command delegates to the same constructor with its deterministic prompt.

The binary has three invocation surfaces:

- No subcommand: launch the desktop interface.
- `gui`: launch the same desktop interface explicitly.
- `check` and `smoke-generate`: permanent non-GUI operations.

## Quality checks

Run the complete Phase 1 verification set:

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release --locked
```

Tests use a bounded local HTTP mock server and must not use the real gateway or require `A6API_KEY`. Unit tests cover URL normalization, key masking, endpoint construction, Base64/URL parsing, malformed responses, structured/non-JSON errors, and image validation. Integration tests verify request paths, bearer authentication, response metadata, error categorization, and URL fallback transport.

## Adding behavior

- Keep gateway logic free of UI types so the CLI, tests, and Slint controller share it.
- Add typed Serde fields rather than assembling or indexing JSON manually.
- Never add the bearer token to tracing fields, errors, URLs, or default client headers.
- Preserve a bounded sanitized response body for unexpected HTTP errors.
- Put network, decoding, and filesystem work outside Slint callbacks.
- Return worker results through `upgrade_in_event_loop`; do not access components from worker threads.
- Preserve cancellation whenever adding an await point to a desktop operation.
- Add mock transport tests for every new endpoint or compatibility behavior.

## Planned module growth

Phase 2 introduces typed application state, formal cancellation and stale-result tests, a generated-image domain type, and atomic writes. Later phases add validated generation controls, the polished KDE-oriented interface, secure settings/history, and Linux packaging in the order defined by the product specification.

## Phase 1 manual test checklist

1. Start the app under KDE Wayland with `cargo run`, verify all text fits, and resize down to the minimum window size.
2. Confirm only a masked key and normalized endpoint are visible, then run `Test connection`.
3. Generate one image and verify the window stays responsive, buttons disable while busy, the PNG appears in the XDG output directory, and the preview/result metadata update.
4. Start another generation, press `Cancel`, and verify the status returns to cancelled without a late success replacing it.
5. Repeat the launch and connection/generation checks in an X11 Plasma session.
6. Launch without `A6API_KEY` and verify the configuration error is shown without a crash or network request.
7. Re-run `check` and `smoke-generate --yes` to confirm the CLI remains functional.
