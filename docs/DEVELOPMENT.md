# Developer Guide

## Scope and phase discipline

The repository currently implements Phase 2. The Phase 0 CLI and transport behavior and the manually verified Phase 1 desktop workflow remain supported. Phase 2 formalizes application state, cancellation, stale-result protection, the generated-image domain model, and atomic output writes without adding generation controls.

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
src/domain.rs        durable generated-image application model
src/app/state.rs     typed state machine and operation identity
src/app/controller.rs configuration display, callbacks, workers, presentation
src/storage/mod.rs   off-thread conversion and atomic XDG output commits
tests/               mock-server transport tests; never real API calls
```

`Config` owns the API key and uses custom `Debug` formatting. The raw key is exposed only inside the crate for building the per-request authorization header. `ApiClient` has no Slint dependency, uses separate 15-second connect and 300-second overall timeouts, and does not attach authorization to fallback image-download URLs.

`endpoint_url` treats a configured root ending in `/v1` as versioned. Otherwise it appends `/v1` before the requested resource. This supports both documented A6API base URL forms and proxy roots such as `https://host/openai/v1`.

Generation responses are decoded in this order:

1. Find the first `b64_json` value and decode it.
2. When no Base64 value exists, use the first URL as a compatibility fallback.
3. Fail explicitly if neither form exists.

Image validation and format conversion run through `tokio::task::spawn_blocking`. The desktop save path also obtains RGBA preview pixels during that same decode pass. A `SharedPixelBuffer` is built on a Tokio worker, then the inexpensive Slint `Image` handle is created on the UI event loop.

Output is written to a uniquely named hidden temporary file in the destination directory. The file is fully written, flushed, and synchronized before a same-directory rename exposes the final PNG. Failures attempt to remove the temporary file, so readers do not observe partial final output.

## Desktop execution model

`app::run` creates the Slint component on the main thread and a separate multi-thread Tokio runtime for background work. Slint callbacks only validate input, transition application state, update immediate UI properties, and spawn work. They never await network, decoding, or filesystem operations. A single configured `ApiClient` is created at startup and cheaply cloned for both connectivity and generation operations.

`StateMachine` is the source of truth for the explicit `Idle`, `Connecting`, `Generating`, `Success`, `Cancelled`, and `Error` states. Each async operation receives a typed `OperationId`. `OperationControl` couples that state machine to the active Tokio `AbortHandle`; cancellation enters `Cancelled` before aborting the worker. A completion is accepted only if its ID still belongs to the active connecting or generating state, so a cancelled or superseded request cannot overwrite newer UI state.

Worker completion uses `Weak<AppWindow>::upgrade_in_event_loop`. The closure asks the state machine to accept the operation ID before presenting its sanitized error or successful result. Closing the window drops the strong Slint handle and any later worker result is discarded.

The transport layer returns raw generation output. After validation and persistence, the controller constructs `domain::GeneratedImage`, which records the final path, dimensions, byte size, elapsed time, response metadata, and shared preview pixels. Successful generation state owns this domain object rather than transport bytes or ad-hoc UI strings.

The desktop request currently uses `ImageGenerationRequest::test_image`, which fixes size, quality, and format while accepting the prompt and configured model. The CLI smoke command delegates to the same constructor with its deterministic prompt.

The binary has three invocation surfaces:

- No subcommand: launch the desktop interface.
- `gui`: launch the same desktop interface explicitly.
- `check` and `smoke-generate`: permanent non-GUI operations.

## Quality checks

Run the complete Phase 2 verification set:

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release --locked
```

Tests use a bounded local HTTP mock server and must not use the real gateway or require `A6API_KEY`. Unit tests cover URL normalization, key masking, endpoint construction, Base64/URL parsing, malformed responses, structured/non-JSON errors, image validation, all state transitions, worker abortion, stale-result rejection, and atomic output commits. Integration tests verify request paths, bearer authentication, response metadata, error categorization, and URL fallback transport.

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

Phase 3 adds validated image-generation controls and output actions. Later phases add the polished KDE-oriented interface, secure settings/history, and Linux packaging in the order defined by the product specification.

## Phase 2 manual test checklist

1. Start the app under KDE Wayland with `cargo run`; verify the endpoint, key, and model rows all fit inside the connection card, then resize down to the minimum window size.
2. Confirm only a masked key and normalized endpoint are visible, then run `Test connection`.
3. Generate one image and verify the window stays responsive, buttons disable while busy, the PNG appears in the XDG output directory, and no `.tmp` file remains beside it.
4. Verify the preview and result metadata update only after the final PNG exists and is readable.
5. Start another generation, press `Cancel`, and verify the status returns to cancelled without a late success or error replacing it.
6. Rapidly exercise cancellation followed by a new operation and verify the older completion cannot overwrite the newer state.
7. Repeat the launch and connection/generation checks in an X11 Plasma session.
8. Launch without `A6API_KEY` and verify the explicit error state is shown without a crash or network request.
9. Re-run `check` and, only if another billable request is acceptable, `smoke-generate --yes` to confirm the CLI remains functional.
