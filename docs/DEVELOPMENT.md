# Developer Guide

## Scope and phase discipline

The repository currently implements Phase 4 plus its reviewed UI and response-integrity fixes. The permanent CLI, manually verified generation workflow, explicit state architecture, typed controls, format-aware persistence, and result actions from earlier phases remain supported. Phase 4 adds arbitrary validated dimensions, a KDE-oriented responsive interface, bounded in-memory recent previews, standard XDG identity and paths, Winit-first Wayland/X11 execution with Qt fallback, and Linux desktop metadata.

The command-line interface is a permanent application surface, not throwaway probe code. Future GUI code should call the library modules rather than duplicate configuration or HTTP behavior.

## Architecture

```text
src/main.rs          CLI parsing, human-readable output, exit status
build.rs             compiles the external Slint source at build time
ui/app.slint         responsive Phase 4 desktop layout, properties, callbacks
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
src/xdg.rs           stable app ID and XDG config/data/cache/pictures discovery
assets/              freedesktop scalable and raster application icons
packaging/           desktop entry and AppStream metadata
tests/               mock-server transport tests; never real API calls
```

`Config` owns the API key and uses custom `Debug` formatting. The raw key is exposed only inside the crate for building the per-request authorization header. `ApiClient` has no Slint dependency, uses separate 15-second connect and 300-second overall timeouts, and does not attach authorization to fallback image-download URLs.

`endpoint_url` treats a configured root ending in `/v1` as versioned. Otherwise it appends `/v1` before the requested resource. This supports both documented A6API base URL forms and proxy roots such as `https://host/openai/v1`.

Generation responses are decoded in this order:

1. Find the first `b64_json` value and decode it.
2. When no Base64 value exists, use the first URL as a compatibility fallback.
3. Fail explicitly if neither form exists.

`GenerationInput` owns the trimmed prompt and `GenerationOptions`. Quality, background, and format enums expose canonical API strings. `ImageSize` is either `Auto` or validated `ImageDimensions`; arbitrary explicit dimensions must meet the documented edge, 16-pixel alignment, 3:1 aspect-ratio, and total-pixel boundaries before network access. `CompatibilitySettings` determines whether each optional Serde field is present; disabled fields use `skip_serializing_if` and are absent from the JSON rather than serialized as null. Transparent JPEG is rejected before network access.

Image validation and format conversion run through `tokio::task::spawn_blocking`. PNG, WebP, and JPEG output are supported. JPEG conversion flattens alpha onto white. The desktop save path also obtains RGBA preview pixels during that same decode pass. Preview dimensions are bounded to a 1600-pixel maximum edge, while the encoded durable output retains its actual dimensions. A `SharedPixelBuffer` is built on a Tokio worker, then the inexpensive Slint `Image` handle is created on the UI event loop.

After decoding, `GeneratedImage::dimensions_match_request` compares the actual
raster with an explicitly sent non-`auto` size. The comparison deliberately
occurs against decoded pixels rather than response claims or preview dimensions.
A mismatch remains a successful, durable result because discarding a paid
response would cause data loss, but presentation marks it prominently and logs
only the requested/received dimensions and optional request ID. The client does
not stretch, crop, or upscale the response because that would hide gateway
nonconformance and cannot recreate missing native detail.

Output is written to a uniquely named hidden temporary file in the destination directory. The file is fully written, flushed, and synchronized before a same-directory rename exposes the final image. Failures attempt to remove the temporary file, so readers do not observe partial final output. Save As reuses the same atomic write path.

## Desktop execution model

`app::run` creates a separate multi-thread Tokio runtime for background work, enters its reactor context on the main thread, selects Slint's platform, sets `io.github.grandfathertech.a6-image-studio` before creating a window, and then creates and runs the component there. Keeping the reactor entered for the lifetime of the Slint event loop is important because Linux desktop integrations may acquire Tokio-backed `zbus` behavior through Cargo feature unification even when Slint polls their futures itself. Slint callbacks only validate input, transition application state, update immediate UI properties, and spawn work. They never await network, decoding, or filesystem operations. A single configured `ApiClient` is created at startup and cheaply cloned for both connectivity and generation operations.

`StateMachine` is the source of truth for the explicit `Idle`, `Connecting`, `Generating`, `Success`, `Cancelled`, and `Error` states. Each async operation receives a typed `OperationId`. `OperationControl` couples that state machine to the active Tokio `AbortHandle`; cancellation enters `Cancelled` before aborting the worker. A completion is accepted only if its ID still belongs to the active connecting or generating state, so a cancelled or superseded request cannot overwrite newer UI state.

Worker completion uses `Weak<AppWindow>::upgrade_in_event_loop`. The closure asks the state machine to accept the operation ID before presenting its sanitized error or successful result. Closing the window drops the strong Slint handle and any later worker result is discarded.

The transport layer returns raw generation output. After validation and persistence, the controller constructs `domain::GeneratedImage`, which records the final path, actual dimensions, byte size, elapsed time, response metadata, output format, and the exact successful `GenerationInput`. Successful generation state owns this domain object rather than transport bytes or ad-hoc UI strings. The controller pairs each domain object with its bounded `SharedPixelBuffer` in an in-memory `ResultStore`. The store retains at most six newest accepted results and tracks one selected result for preview and actions; rejected stale results never enter it. Persistence of this history belongs to Phase 5.

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

The Slint root uses a `ScrollView` whose viewport height follows the content's minimum height. The content keeps a natural layout while the viewport shrinks, preventing lower controls from becoming unreachable. At 900 logical pixels the main workspace changes between side-by-side and stacked panels. Reusable Slint components keep the behavior and two-way bindings identical in both layouts. The compatibility expander has an explicit native-control height so Winit cannot stretch it into the available layout space. Request-detail rows use explicit bounded heights inside a clipped card. Only the generation-controls row owns an indeterminate spinner; the canvas uses static progress copy. Colors come from `Palette`, native controls retain system focus treatment, and the UI adds no custom font family, gradients, glow, or decorative motion.

`backend-winit-wayland`, `backend-winit-x11`, and `backend-qt` are compiled.
Unless `SLINT_BACKEND` is explicitly set, `select_desktop_backend` requests
Winit first and tries Qt only if Winit initialization fails. Winit chooses the
active Wayland or X11 display and can use the femtovg or software renderer.
`SLINT_BACKEND` can explicitly select `qt`, `winit-femtovg`, or
`winit-software`; explicit selection errors are returned rather than silently
changing the requested backend. The Qt dependency remains optional at Slint's
build-detection level.

`AppPaths` follows XDG environment variables with absolute-path validation and home-directory fallbacks. Durable generated files live below the data directory, Save As starts in the user pictures directory, and the configuration/cache paths are exposed for later phases without writing persistent state in Phase 4.

The binary has three invocation surfaces:

- No subcommand: launch the desktop interface.
- `gui`: launch the same desktop interface explicitly.
- `check` and `smoke-generate`: permanent non-GUI operations.

## Quality checks

Run the complete Phase 4 verification set:

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release --locked
```

Tests use a bounded local HTTP mock server and must not use the real gateway or require `A6API_KEY`. Unit tests cover URL normalization, key masking, endpoint construction, Base64/URL parsing, malformed responses, structured/non-JSON errors, documented dimension boundaries, requested/received dimension mismatch reporting, optional-field omission, bounded previews, XDG discovery, recent-result selection, PNG/WebP/JPEG conversion, Save As extension checks, all state transitions, worker abortion, stale-result rejection, and atomic output commits. Integration tests verify request paths, exact `3840x2160` serialization, all generation JSON fields, bearer authentication, response metadata, error categorization, and URL fallback transport.

Validate the freedesktop assets as well:

```bash
desktop-file-validate packaging/io.github.grandfathertech.a6-image-studio.desktop
appstreamcli validate --no-net packaging/io.github.grandfathertech.a6-image-studio.metainfo.xml
xmllint --noout assets/icons/hicolor/scalable/apps/io.github.grandfathertech.a6-image-studio.svg
```

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

Phase 5 adds secure settings and persistent optional history. Later phases complete installation/package assembly in the order defined by the product specification.

## Phase 4 manual test checklist

1. Start under KDE Wayland with `RUST_LOG=info cargo run`. Verify the log reports the preferred Winit backend and the interface uses the system palette. Repeat in an X11 session where available. Run `SLINT_BACKEND=qt cargo run` only to verify the fallback/override.
2. Resize across the 900-pixel responsive breakpoint and down to minimum width/height. Verify side-by-side panels become stacked and the scrollbar reaches every control and action.
3. Confirm the compact header shows only a masked key and normalized endpoint, then run `Test connection`.
4. Expand compatibility settings. Verify the compact expander remains exactly one control row tall, does not stretch vertically, and all four switches align in two columns.
5. Open Dimensions and inspect every preset. Choose `Custom…`; test a valid custom size, a non-16-pixel edge, a ratio over 3:1, and pixel counts below/above the limits. Invalid input must be rejected before network access.
6. Generate one low-quality `1024x1024` PNG. Verify there is exactly one spinner beside the generation controls, no spinner on the canvas, responsiveness, preview, collapsed request details, exact metadata, and absence of leftover `.tmp` files.
7. If charges are acceptable, generate several different sizes/formats. Verify up to six recent thumbnails appear, selecting an older one updates preview/metadata/action target, and a seventh removes the oldest.
8. Test a 4K request only if the provider supports it and the charge is acceptable. If the gateway returns 3840×2160, verify the saved file remains exact while preview memory is bounded. If it returns another raster, verify the prominent warning states both requested and received dimensions and the actual file is preserved unchanged.
9. Select transparent background with JPEG and verify local rejection. For direct OpenAI `gpt-image-2`, expect transparent background itself to be unsupported; A6API compatibility may differ.
10. Test Save As (defaulting to Pictures), Copy image, Copy prompt, Open folder, and Regenerate. Remember that Regenerate is billable.
11. Cancel an active generation, then start a connection test. Verify no late result replaces the newer state.
12. Repeat the launch, scroll, and clipboard checks under X11 where available.
13. Launch without `A6API_KEY` and verify the explicit error state appears without a crash or network request.
14. Re-run `check` and, only if another billable request is acceptable, `smoke-generate --yes` to confirm the permanent CLI remains functional.
