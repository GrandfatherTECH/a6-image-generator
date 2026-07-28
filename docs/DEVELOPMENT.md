# Developer Guide

## Scope and phase discipline

The repository currently implements Phase 3. The Phase 0 CLI, manually verified Phase 1 desktop workflow, and Phase 2 state architecture remain supported. Phase 3 adds typed generation options, optional compatibility fields, format-aware persistence, a scrollable interface, result metadata, and result actions.

The command-line interface is a permanent application surface, not throwaway probe code. Future GUI code should call the library modules rather than duplicate configuration or HTTP behavior.

## Architecture

```text
src/main.rs          CLI parsing, human-readable output, exit status
build.rs             compiles the external Slint source at build time
ui/app.slint         scrollable Phase 3 desktop layout, properties, callbacks
src/config.rs        environment loading, validation, secret masking
src/generation.rs    typed controls, validation, API values, compatibility flags
src/api/types.rs     typed gateway requests, responses, and metadata
src/api/error.rs     structured HTTP and transport error categories
src/api/client.rs    endpoint construction, HTTP calls, response decoding
src/domain.rs        durable generated-image application model
src/app/state.rs     typed state machine and operation identity
src/app/controller.rs configuration display, callbacks, workers, presentation
src/app/actions.rs   Save As, clipboard, and containing-folder integrations
src/storage/mod.rs   off-thread format conversion and atomic output commits
tests/               mock-server transport tests; never real API calls
```

`Config` owns the API key and uses custom `Debug` formatting. The raw key is exposed only inside the crate for building the per-request authorization header. `ApiClient` has no Slint dependency, uses separate 15-second connect and 300-second overall timeouts, and does not attach authorization to fallback image-download URLs.

`endpoint_url` treats a configured root ending in `/v1` as versioned. Otherwise it appends `/v1` before the requested resource. This supports both documented A6API base URL forms and proxy roots such as `https://host/openai/v1`.

Generation responses are decoded in this order:

1. Find the first `b64_json` value and decode it.
2. When no Base64 value exists, use the first URL as a compatibility fallback.
3. Fail explicitly if neither form exists.

`GenerationInput` owns the trimmed prompt and `GenerationOptions`. The four option enums accept only the Phase 3 values and expose canonical API strings. `CompatibilitySettings` determines whether each optional Serde field is present; disabled fields use `skip_serializing_if` and are absent from the JSON rather than serialized as null. Transparent JPEG is rejected before network access.

Image validation and format conversion run through `tokio::task::spawn_blocking`. PNG, WebP, and JPEG output are supported. JPEG conversion flattens alpha onto white. The desktop save path also obtains RGBA preview pixels during that same decode pass. A `SharedPixelBuffer` is built on a Tokio worker, then the inexpensive Slint `Image` handle is created on the UI event loop.

Output is written to a uniquely named hidden temporary file in the destination directory. The file is fully written, flushed, and synchronized before a same-directory rename exposes the final image. Failures attempt to remove the temporary file, so readers do not observe partial final output. Save As reuses the same atomic write path.

## Desktop execution model

`app::run` creates a separate multi-thread Tokio runtime for background work, enters its reactor context on the main thread, and then creates and runs the Slint component there. Keeping the reactor entered for the lifetime of the Slint event loop is important because Linux desktop integrations may acquire Tokio-backed `zbus` behavior through Cargo feature unification even when Slint polls their futures itself. Slint callbacks only validate input, transition application state, update immediate UI properties, and spawn work. They never await network, decoding, or filesystem operations. A single configured `ApiClient` is created at startup and cheaply cloned for both connectivity and generation operations.

`StateMachine` is the source of truth for the explicit `Idle`, `Connecting`, `Generating`, `Success`, `Cancelled`, and `Error` states. Each async operation receives a typed `OperationId`. `OperationControl` couples that state machine to the active Tokio `AbortHandle`; cancellation enters `Cancelled` before aborting the worker. A completion is accepted only if its ID still belongs to the active connecting or generating state, so a cancelled or superseded request cannot overwrite newer UI state.

Worker completion uses `Weak<AppWindow>::upgrade_in_event_loop`. The closure asks the state machine to accept the operation ID before presenting its sanitized error or successful result. Closing the window drops the strong Slint handle and any later worker result is discarded.

The transport layer returns raw generation output. After validation and persistence, the controller constructs `domain::GeneratedImage`, which records the final path, dimensions, byte size, elapsed time, response metadata, shared preview pixels, output format, and the exact successful `GenerationInput`. Successful generation state owns this domain object rather than transport bytes or ad-hoc UI strings. A small result store retains the last accepted image when a later connection check changes the primary state; rejected stale results never enter that store.

The desktop request uses `ImageGenerationRequest::configured`. The permanent CLI smoke path remains deliberately fixed and uses `ImageGenerationRequest::smoke_test`, so GUI compatibility options cannot weaken its deterministic contract.

Result actions are independent of the generation state machine:

- `rfd` provides an asynchronous XDG portal Save As dialog. Its async-std feature deliberately
  keeps the shared Linux `zbus` dependency on the async-io backend used by Slint and AccessKit;
  enabling `rfd`'s Tokio feature would switch that shared dependency globally and make
  Slint-owned threads require a Tokio reactor.
- Save As copies the accepted durable file through `storage::copy_atomic`.
- Clipboard writes run in blocking workers and invoke `wl-copy` or `xclip` directly without a shell.
- Folder opening invokes `xdg-open` directly without a shell.
- All action completion text returns through `upgrade_in_event_loop`.

The Slint root uses a `ScrollView` whose viewport height follows the content's minimum height. The content keeps a fixed natural layout while the viewport shrinks, preventing lower controls from becoming unreachable.

The binary has three invocation surfaces:

- No subcommand: launch the desktop interface.
- `gui`: launch the same desktop interface explicitly.
- `check` and `smoke-generate`: permanent non-GUI operations.

## Quality checks

Run the complete Phase 3 verification set:

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release --locked
```

Tests use a bounded local HTTP mock server and must not use the real gateway or require `A6API_KEY`. Unit tests cover URL normalization, key masking, endpoint construction, Base64/URL parsing, malformed responses, structured/non-JSON errors, generation-option parsing and validation, optional-field omission, PNG/WebP/JPEG conversion, Save As extension checks, all state transitions, worker abortion, stale-result rejection, and atomic output commits. Integration tests verify request paths, all Phase 3 JSON fields, bearer authentication, response metadata, error categorization, and URL fallback transport.

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

Phase 4 adds the polished KDE-oriented visual design. Later phases add secure settings/history and Linux packaging in the order defined by the product specification.

## Phase 3 manual test checklist

1. Start the app under KDE Wayland with `cargo run`, shrink the window to its minimum height, and verify the mouse wheel and scrollbar reach every control and result action.
2. Confirm only a masked key and normalized endpoint are visible, then run `Test connection`.
3. Generate one low-quality `1024x1024` PNG. Verify the window stays responsive, the preview appears, the result fields are readable, and no `.tmp` file remains.
4. If charges are acceptable, test one portrait or landscape request and one WebP or JPEG request. Verify the file extension, detected encoding, dimensions, and displayed format agree.
5. Select transparent background with JPEG and verify local validation rejects it without starting a request. Select PNG or WebP and verify the combination can be submitted.
6. Expand compatibility settings, disable one field, generate if acceptable, and verify the server response is clear. Re-enable the field afterward.
7. Test Save As, Copy image into a KDE application, Copy prompt into a text editor, Open containing folder, and Regenerate. Remember that Regenerate is billable.
8. Start another generation, press `Cancel`, then start a connection test. Verify no late generation result replaces the newer state.
9. Repeat scrolling and clipboard checks under X11 with `xclip` installed where available.
10. Launch without `A6API_KEY` and verify the explicit error state is shown without a crash or network request.
11. Re-run `check` and, only if another billable request is acceptable, `smoke-generate --yes` to confirm the CLI remains functional.
