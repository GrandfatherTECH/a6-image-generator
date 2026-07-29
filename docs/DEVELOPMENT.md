# Developer Guide

## Scope and phase discipline

The repository currently implements Phase 5 plus the reviewed Phase 4 UI and response-integrity fixes. The permanent CLI, manually verified generation workflow, explicit state architecture, typed controls, format-aware output persistence, and result actions from earlier phases remain supported. Phase 5 includes validated persistent preferences, optional Secret Service/KWallet credentials, and transactional SQLite storage for sessions, generation history, and searchable diagnostic records.

The command-line interface is a permanent application surface, not throwaway probe code. Future GUI code should call the library modules rather than duplicate configuration or HTTP behavior.

## Architecture

```text
src/main.rs          CLI parsing, human-readable output, exit status
build.rs             compiles the external Slint source at build time
ui/app.slint         responsive multi-section desktop layout and callbacks
src/config.rs        connection validation, secret masking, request timeout
src/preferences.rs   non-secret settings and atomic JSON persistence/migration
src/secrets.rs       environment/keyring credential resolution and mutation
src/history.rs       SQLite sessions/history/errors and legacy JSON migration
src/generation.rs    typed controls, validation, API values, compatibility flags
src/api/types.rs     typed gateway requests, responses, and metadata
src/api/error.rs     structured HTTP and transport error categories
src/api/client.rs    endpoint construction, HTTP calls, response decoding
src/domain.rs        durable generated-image application model
src/app/state.rs     typed state machine and operation identity
src/app/controller.rs configuration display, callbacks, workers, presentation
src/app/actions.rs   Save As, clipboard, and containing-folder integrations
src/storage/mod.rs   off-thread format conversion and atomic output commits
src/xdg.rs           stable app ID, consolidated app root, legacy XDG discovery
assets/              freedesktop scalable and raster application icons
packaging/           desktop entry and AppStream metadata
tests/               mock-server transport tests; never real API calls
```

`Config` owns the API key and uses custom `Debug` formatting. The raw key is exposed only inside the crate for building the per-request authorization header. `ApiClient` has no Slint dependency, uses a fixed 15-second connect timeout and the validated configurable overall timeout, and does not attach authorization to fallback image-download URLs.

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
The presentation layer separately compares aspect ratios with a small tolerance:
a proportional provider-adjusted raster such as 1672×941 for 3840×2160 is
informational, while a materially changed aspect ratio is a warning. Both remain
successful durable results because discarding a paid response would cause data
loss. The client never stretches, crops, or upscales the response.

Output is written to a uniquely named hidden temporary file in the configured destination directory. The file is fully written, flushed, and synchronized before a same-directory rename exposes the final image. Failures attempt to remove the temporary file, so readers do not observe partial final output. Save As reuses the same atomic write path. Persisted history previews are loaded and decoded through the same bounded off-thread preview preparation path.

## Settings, credentials, and persistence

`AppPreferences` contains only the base URL, model, defaults, output directory, timeout, and history-retention switch. `PreferencesStore` reads and atomically replaces `~/.config/a6-studio/settings.json`; validation happens before a new backend is applied. If that file is absent, the former XDG `a6-image-studio/settings.json` is read once, its old default output path is changed to the new default, and the validated result is atomically written to the new location. `A6API_BASE_URL` and `A6API_IMAGE_MODEL` remain explicit process-level overrides and are identified in the UI.

Credentials are deliberately outside the preferences and SQLite schemas. `secrets` resolves `A6API_KEY` first, then queries keyring 4's native Linux store using service `org.a6-studio.key`, the login username as the account, and the visible label `org.a6-studio.key.<sanitized-login-username>`. `keyring-core` supplies the Secret Service label modifier after keyring 4 initializes the platform-native store. Keyring work runs in `spawn_blocking`; the environment source is never silently migrated. Store and forget are explicit user actions. If the new entry is absent, the former `io.github.grandfathertech.a6-image-studio`/`A6API_KEY` entry is copied and removed only after the replacement succeeds. UI and debug formatting expose only `ApiKey::masked`.

`HistoryRepository` owns one SQLite database at `~/.config/a6-studio/a6-studio.sqlite3`:

- `sessions` stores the stable session identifier plus first/last activity timestamps.
- `generations` stores output path, prompt, model, sent/omitted settings, UTC timestamp, session ID, decoded dimensions, and request ID.
- `errors` stores operation context, prompt, model, sanitized endpoint, category/summary, UTC timestamp, session ID, request ID, and the captured sanitized non-success response body.
- `app_metadata` stores import-completion markers.

The connection enables foreign keys, a five-second busy timeout, WAL mode, and normal synchronous durability. Schema changes use SQLite's `user_version`; a database created by a newer unsupported build is rejected rather than modified. Inserts, retention trimming, clears, and session cleanup are atomic transactions. The in-memory presentation index is updated only after commit, keeping UI reads off the Slint event loop's filesystem path. Generation and error retention remain bounded to 2,000 and 1,000 records.

On first discovery, the repository transactionally imports the former `history.json` and `errors.json` files with `INSERT OR IGNORE`, records an independent completion marker for each source, and leaves both files untouched as recovery backups. Existing generated files are not moved. History never stores encoded image bytes or Base64 response data. A generated file can disappear independently; `HistoryEntry::image_exists` gates preview and folder actions while `StoredGenerationSettings::to_input` still supports prompt/settings restoration. Clearing history removes SQLite metadata only.

HTTP error capture has two bounds: the transient summary is 4096 characters and the persisted sanitized body is at most 1 MiB. The exact active API key is replaced before either form leaves the transport layer. `ApiError::full_response` is implemented only for non-successful HTTP results, so successful Base64 image responses cannot enter diagnostics.

## Desktop execution model

`app::run` creates a separate multi-thread Tokio runtime for background work, enters its reactor context on the main thread, selects Slint's platform, sets `org.a6studio.A6ImageStudio` before creating a window, and then creates and runs the component there. Keeping the reactor entered for the lifetime of the Slint event loop is important because Linux desktop integrations and the Secret Service stack may acquire Tokio-backed `zbus` behavior through Cargo feature unification even when Slint polls their futures itself. Slint callbacks only validate input, transition application state, update immediate UI properties, and spawn work. They never await network, keyring, decoding, or filesystem operations.

`BackendStore` permits validated Settings changes to atomically replace the active client/model/output-directory bundle without restarting the UI. `DesktopServices` shares preferences, credential state, the backend store, history repository, current session ID, and browser selection. Every process launch receives a fresh session identifier.

`StateMachine` is the source of truth for the explicit `Idle`, `Connecting`, `Generating`, `Success`, `Cancelled`, and `Error` states. Each async operation receives a typed `OperationId`. `OperationControl` couples that state machine to the active Tokio `AbortHandle`; cancellation enters `Cancelled` before aborting the worker. A completion is accepted only if its ID still belongs to the active connecting or generating state, so a cancelled or superseded request cannot overwrite newer UI state.

Worker completion uses `Weak<AppWindow>::upgrade_in_event_loop`. The closure asks the state machine to accept the operation ID before presenting its sanitized error or successful result. Closing the window drops the strong Slint handle and any later worker result is discarded.

The transport layer returns raw generation output. After validation and persistence, the controller constructs `domain::GeneratedImage`, which records the final path, actual dimensions, byte size, elapsed time, response metadata, output format, and the exact successful `GenerationInput`. Successful generation state owns this domain object rather than transport bytes or ad-hoc UI strings. The controller pairs each domain object with its bounded `SharedPixelBuffer` in an in-memory `ResultStore`. The store retains at most six newest accepted results and tracks one selected result for preview and actions; rejected stale results never enter it. If retention is enabled, the accepted domain object is then converted into a metadata-only persistent history entry.

The desktop request uses `ImageGenerationRequest::configured`. The permanent CLI smoke path remains deliberately fixed and uses `ImageGenerationRequest::smoke_test`, so GUI compatibility options cannot weaken its deterministic contract.

Result actions are independent of the generation state machine:

- `rfd` provides an asynchronous XDG portal Save As dialog. Its async-std feature deliberately
  keeps the shared Linux `zbus` dependency on the async-io backend used by Slint and AccessKit;
  enabling `rfd`'s Tokio feature would switch that shared dependency globally and make
  Slint-owned threads require a Tokio reactor.
- Save As copies the accepted durable file through `storage::copy_atomic`.
- Clipboard writes run in blocking workers and invoke `wl-copy` or `xclip` directly without a shell.
- Folder opening invokes `xdg-open` directly without a shell.
- The default-output-directory chooser uses the same asynchronous portal integration.
- All action completion text returns through `upgrade_in_event_loop`.

The Slint root uses a `ScrollView` whose viewport height follows the selected section's natural height. The app-name `ComboBox` routes among Create, History, Error log, and Settings without creating additional windows. Direct page components use layout stretch rather than binding themselves to the unpadded viewport width, so their right borders remain inside the root's asymmetric content padding. At 900 logical pixels Create changes from side-by-side to stacked panels; History and Error log replace their desktop columns with vertically scrollable master/detail cards, and Settings stacks its field groups. History and error metadata use read-only `TextEdit` controls so users can select and copy complete values or arbitrary substrings. The compatibility expander has an explicit native-control height so Winit cannot stretch it into available layout space. Request-detail rows use bounded heights inside a clipped card, and all result actions—including Request details—share a padded action well. Only the generation-controls row owns an indeterminate spinner; the canvas uses static progress copy. Colors come from translucent `Palette` brushes, native controls retain system focus treatment, and the UI adds no custom font family, gradients, glow, or decorative motion.

`backend-winit-wayland`, `backend-winit-x11`, and `backend-qt` are compiled.
Unless `SLINT_BACKEND` is explicitly set, `select_desktop_backend` requests
Winit first and tries Qt only if Winit initialization fails. Winit chooses the
active Wayland or X11 display and can use the femtovg or software renderer.
`SLINT_BACKEND` can explicitly select `qt`, `winit-femtovg`, or
`winit-software`; explicit selection errors are returned rather than silently
changing the requested backend. The `unstable-winit-030` integration installs a
window-attributes hook that requests a transparent surface and compositor blur
before the Winit window is created. KWin can honor that blur hint on Wayland;
Winit currently leaves it unsupported on X11, where the palette translucency
remains. The Qt dependency remains optional at Slint's build-detection level.

`AppPaths` deliberately consolidates application-managed state under `~/.config/a6-studio`: settings and SQLite live at the root, generated files default to `outputs/`, and reserved cache data belongs in `cache/`. It still resolves the XDG pictures directory for Save As and the former XDG config/data/cache roots solely for one-time migration.

The binary has three invocation surfaces:

- No subcommand: launch the desktop interface.
- `gui`: launch the same desktop interface explicitly.
- `check` and `smoke-generate`: permanent non-GUI operations.

The non-GUI commands deliberately retain their Phase 0 environment-only
configuration contract. They do not open desktop keyring or settings services,
which keeps headless behavior explicit and makes the billable confirmation path
independent of desktop state.

## Quality checks

Run the complete Phase 5 verification set:

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release --locked
```

Tests use a bounded local HTTP mock server and must not use the real gateway, keyring, or require `A6API_KEY`. Unit tests cover URL normalization, key masking/source labels, the visible per-user keyring label, endpoint construction, configurable timeout, Base64/URL parsing, malformed responses, full sanitized error capture, documented dimension boundaries, proportional and aspect-changing provider adjustments, optional-field omission, bounded previews, consolidated/legacy path discovery, settings migration, SQLite schema/version handling, one-time JSON import, database round trips, missing output files, recent-result selection, PNG/WebP/JPEG conversion, Save As extension checks, all state transitions, worker abortion, stale-result rejection, and atomic output commits. Integration tests verify request paths, exact `3840x2160` serialization, all generation JSON fields, bearer authentication, response metadata, error categorization, and URL fallback transport.

Validate the freedesktop assets as well:

```bash
desktop-file-validate packaging/org.a6studio.A6ImageStudio.desktop
appstreamcli validate --no-net packaging/org.a6studio.A6ImageStudio.metainfo.xml
xmllint --noout assets/icons/hicolor/scalable/apps/org.a6studio.A6ImageStudio.svg
```

## Adding behavior

- Keep gateway logic free of UI types so the CLI, tests, and Slint controller share it.
- Add typed Serde fields rather than assembling or indexing JSON manually.
- Never add the bearer token to tracing fields, errors, URLs, or default client headers.
- Preserve the summary/body separation and bounded sanitized response for unexpected HTTP errors.
- Keep credentials out of settings and SQLite schemas; keyring mutation must remain explicit.
- Put network, decoding, and filesystem work outside Slint callbacks.
- Return worker results through `upgrade_in_event_loop`; do not access components from worker threads.
- Preserve cancellation whenever adding an await point to a desktop operation.
- Add mock transport tests for every new endpoint or compatibility behavior.

## Planned module growth

Phase 5 is complete. Later phases complete installation/package assembly in the order defined by the product specification.

## Phase 5 manual test checklist

1. Start under KDE Wayland with `RUST_LOG=info cargo run`. Verify the log reports the preferred Winit backend, the window remains translucent, and KWin blurs content behind it. Repeat in an X11 session where available; translucency should remain but Winit blur may be unsupported. Run `SLINT_BACKEND=qt cargo run` only to verify the fallback/override.
2. Resize across the 900-pixel responsive breakpoint and down to minimum width/height. Verify side-by-side panels become stacked and the scrollbar reaches every control and action.
3. Confirm the compact header shows only a masked key and normalized endpoint, then run `Test connection`.
4. Expand compatibility settings. Verify the compact expander remains exactly one control row tall, does not stretch vertically, and all four switches align in two columns.
5. Open Dimensions and inspect every preset. Choose `Custom…`; test a valid custom size, a non-16-pixel edge, a ratio over 3:1, and pixel counts below/above the limits. Invalid input must be rejected before network access.
6. Generate one low-quality `1024x1024` PNG. Verify there is exactly one spinner beside the generation controls, no spinner on the canvas, responsiveness, preview, visible/padded result-action rows, collapsible request details, exact metadata, and absence of leftover `.tmp` files.
7. If charges are acceptable, generate several different sizes/formats. Verify up to six recent thumbnails appear, selecting an older one updates preview/metadata/action target, and a seventh removes the oldest.
8. Test a 4K request only if the charge is acceptable. If the provider returns 1672×941 for 3840×2160, verify it is described as a proportional provider adjustment rather than an error. A result with a changed aspect ratio must remain a warning. In either case, verify the actual file is preserved unchanged.
9. Select transparent background with JPEG and verify local rejection. For direct OpenAI `gpt-image-2`, expect transparent background itself to be unsupported; A6API compatibility may differ.
10. Test Save As (defaulting to Pictures), Copy image, Copy prompt, Open folder, and Regenerate. Remember that Regenerate is billable.
11. Cancel an active generation, then start a connection test. Verify no late result replaces the newer state.
12. Repeat the launch, scroll, and clipboard checks under X11 where available.
13. Open Settings, change each ordinary default and the output directory, save, restart, and verify persistence. Test an invalid/relative output path and out-of-range timeout. If base/model environment overrides are present, verify the UI names them.
14. With `A6API_KEY` unset, store a test key in the system keyring, restart, and verify the source is `system keyring`. In KeepSecret, verify the visible label is `org.a6-studio.key.<login-username>`. Then set `A6API_KEY`, restart, and verify `environment` takes precedence without copying it. `Forget stored key` must not affect the environment key.
15. Generate entries in two application launches. Confirm `~/.config/a6-studio/a6-studio.sqlite3` exists, search History by prompt, model, path, request ID, timestamp, and session, and load an old request into Create. Move one output file and verify the missing-file state preserves prompt/settings but disables file actions. Clear history and confirm image files remain.
16. Trigger a safe local validation failure and a provider HTTP failure. Search Error log by session, prompt, category, request ID, and response text. Verify full captured response text is bound to the correct request, the API key is absent, and clearing the log does not delete outputs.
17. Launch without either `A6API_KEY` or a stored key and verify the explicit unconfigured state appears without a crash or network request.
18. Re-run `check` and, only if another billable request is acceptable, `smoke-generate --yes` to confirm the permanent CLI remains functional.
19. For migration QA, start from copies of the former `settings.json`, `history.json`, and `errors.json` plus the legacy keyring entry. Verify the settings and key are migrated, SQLite imports each JSON source once, the old JSON files remain unchanged, and existing image paths still work.
