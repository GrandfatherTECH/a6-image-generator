# User Guide

This guide documents the Phase 5 Slint desktop interface and the permanent command-line interface.

## Requirements

- A Linux system with a current stable Rust toolchain for source builds.
- An A6API API key with access to `gpt-image-2`.
- Network access to the configured gateway.
- A Wayland or X11 desktop session for the graphical interface.

## Configuration

A6 Image Studio can read connection configuration from environment variables and ordinary desktop preferences from its Settings page:

| Variable | Required | Default | Purpose |
| --- | --- | --- | --- |
| `A6API_KEY` | No* | None | Bearer token used for API requests. It takes precedence over a key stored in the system keyring. |
| `A6API_BASE_URL` | No | saved setting or `https://api.a6api.com` | OpenAI-compatible API root. Both roots with and without `/v1` are accepted. |
| `A6API_IMAGE_MODEL` | No | saved setting or `gpt-image-2` | Image model identifier. |

`*` A key must be available either through `A6API_KEY` or through the optional system-keyring entry before connection or generation can run.

The keyring and saved ordinary preferences are desktop features. The permanent
`check` and `smoke-generate` CLI commands intentionally remain deterministic
environment-driven tools and therefore require `A6API_KEY`; their base URL and
model also come from the environment variables in this table.

Trailing slashes in the base URL are removed. The URL must use HTTP or HTTPS, include a host, and must not contain embedded credentials, a query, or a fragment. The application never prints the complete API key; diagnostics show only a masked form.

Example for the default gateway:

```bash
export A6API_KEY='your-key-here'
export A6API_BASE_URL='https://api.a6api.com'
export A6API_IMAGE_MODEL='gpt-image-2'
```

Shell exports last only for the current shell unless added to a secure environment setup. Do not put API keys in the repository or ordinary configuration files.

Environment variables are read when the process starts. Restart the application after changing one. `A6API_BASE_URL` and `A6API_IMAGE_MODEL` visibly override their saved counterparts for that process, while `A6API_KEY` always overrides a stored key.

The Settings page stores only non-secret preferences in:

```text
$HOME/.config/a6-studio/settings.json
```

The page controls the base URL, model ID, default dimensions/quality/format, output directory, request timeout from 10 to 1800 seconds, and optional generation-history retention. The output directory must be an absolute path.

### Secure API-key storage

Entering a key and choosing `Store in system keyring` stores it through the Linux Secret Service API, normally backed by KWallet on KDE or GNOME Keyring on GNOME. The Settings page always identifies the active source as `environment`, `system keyring`, or `none` and displays only a masked key.

- Secret Service managers such as KeepSecret display the intentional per-user label `org.a6-studio.key.<login-username>`.
- The Secret Service lookup attributes are service `org.a6-studio.key` and username `<login-username>`.
- The application never writes an API key to `settings.json`, `a6-studio.sqlite3`, TOML, logs, or UI history.
- Existing environment credentials are never silently copied into the keyring.
- Storing a key while `A6API_KEY` is set does not replace the active environment key; it becomes available after the environment variable is removed and the application is restarted.
- `Forget stored key` removes only the system-keyring entry. It cannot remove a key supplied by the process environment.
- When no new entry exists, a stored Phase 5 entry named `keyring:A6API_KEY@io.github.grandfathertech.a6-image-studio` is copied to the new identity and removed only after the new write succeeds.

## Desktop interface

Launch the desktop interface with no subcommand:

```bash
cargo run
```

The explicit equivalent is:

```bash
cargo run -- gui
```

The compact header displays the normalized endpoint, masked API key, configured model, and current operation status. It never displays the full key. Use the app-name dropdown at the upper left to switch between:

- `Create`
- `History`
- `Error log`
- `Settings`

The interface follows the system light/dark palette and font. Wide Create windows place the prompt/settings panel beside a large preview canvas; narrower windows stack the same panels vertically. History and Error log likewise change from columns to vertically scrollable cards, and Settings stacks fields that would otherwise become cramped. The complete window remains in a vertical scroll view, so every control is reachable at the minimum window size.

`Test connection` calls the models endpoint and reports its HTTP status and whether the configured model is listed. A model missing from that list does not necessarily mean image generation is unavailable.

`Generate image` is a potentially billable action. While the request is active, the connection and generation buttons are disabled, one busy indicator is visible beside the generation controls, and `Cancel` aborts the active asynchronous task. The preview canvas does not show a duplicate spinner. A cancelled or superseded operation cannot later replace the visible state.

### Generation controls

| Control | Values |
| --- | --- |
| Dimensions | `auto`, common square/landscape/portrait presets from 1024 through 4K, or `Custom…` |
| Quality | `auto`, `low`, `medium`, `high` |
| Background | `auto`, `opaque`, `transparent` |
| Output format | PNG, WebP, JPEG |

The initial settings remain `1024x1024`, low quality, automatic background, and PNG. Prompt input is trimmed and must not be empty. Transparent background with JPEG is rejected locally because JPEG cannot preserve alpha; choose PNG or WebP instead.

The selected output format controls both the API request and the durable file format. If the gateway returns another decodable format, the application converts it off the UI thread before the atomic write.

#### Dimension presets and custom sizes

The preset menu includes:

- `auto`
- `1024x1024`
- `1536x1024` and `1024x1536`
- `2048x2048`
- `2048x1152` and `1152x2048`
- `2560x1440` and `1440x2560`
- `3840x2160` and `2160x3840`
- `Custom…`

`Custom…` exposes numeric width and height controls in 16-pixel steps. A custom size is accepted only when:

- both edges are divisible by 16;
- neither edge exceeds 3840 pixels;
- the long edge is no more than three times the short edge;
- total pixels are between 655,360 and 8,294,400, inclusive.

The interface displays the pixel count and aspect ratio for valid custom sizes. It labels sizes containing more than 3,686,400 pixels—the total pixel count of 2560×1440—as experimental. These rules and the common presets come from the official [OpenAI image-generation guide](https://developers.openai.com/api/docs/guides/image-generation) for [`gpt-image-2`](https://developers.openai.com/api/docs/models/gpt-image-2).

The direct OpenAI `gpt-image-2` endpoint currently documents transparent backgrounds as unsupported. The control remains available for A6API or other compatible gateways; disable the background field under compatibility settings if the configured gateway rejects it.

#### Requested and received dimensions

When an explicit size is enabled, the client sends that exact `WIDTHxHEIGHT`
string in the JSON `size` field. After the response is decoded, the client
compares the real raster dimensions with the request:

- A matching result is accepted normally.
- A smaller or otherwise adjusted raster with effectively the same aspect ratio
  is reported as a normal provider adjustment.
- A returned raster with a materially different aspect ratio is saved but shown
  with a prominent warning.
- The request details distinguish the requested dimensions from the actual
  dimensions displayed above the preview.
- The client never silently stretches, crops, or artificially upscales a paid
  response.

For example, `1672×941` returned for a `3840x2160` request is treated as a
provider-adjusted 16:9 result, while `1402×1122` is called out because its aspect
ratio differs. This is not preview downscaling: display previews are separately
bounded in memory, but the durable file retains the decoded response dimensions.

### Compatibility settings

Expand the compact, fixed-height `Compatibility settings` control to choose whether the request sends `size`, `quality`, `background`, and `output_format`. The control no longer stretches when expanded; the switches use an aligned two-column layout, and all four are enabled for the target `gpt-image-2` workflow by default.

If A6API or a configured compatible model rejects one parameter:

1. Read the complete sanitized server error shown in the application.
2. Expand compatibility settings.
3. Clear only the rejected parameter.
4. Retry the request.

The corresponding normal control is hidden while its parameter is disabled. The disabled value remains available if the parameter is re-enabled. When `output_format` is omitted, the selected format still determines the local file produced after the response is decoded.

### Generated result and actions

Successful output is validated, atomically saved, and shown in the large preview canvas. Up to six successful images are retained in the in-memory `Recent this session` strip; selecting a thumbnail changes the preview and metadata, and makes file actions and Regenerate target that result.

`Request details` is grouped with the other actions in a padded control well below the preview, so it remains visible without colliding with the lower card edge. Expand it to inspect the successful prompt and sent/omitted options together with request duration, actual image dimensions, encoded file size, output format, exact path, and request ID when supplied. The details region has bounded rows and clipping so long prompts and paths cannot draw beyond its border.

- `Save As…` opens the desktop portal file dialog and writes an atomic copy. Keep the selected format's extension.
- `Copy image` sends the encoded file to the desktop clipboard. On Linux this uses `wl-copy` on Wayland or `xclip` on X11 when available.
- `Copy prompt` sends the current prompt text to the same clipboard integration.
- `Regenerate` restores and repeats the exact prompt, options, and compatibility switches from the selected recent generation. It can incur another charge.
- `Open containing folder` opens the output directory through `xdg-open`.

Result actions run away from Slint's event loop and show nonintrusive completion or failure text below the buttons. Large generated images retain their full saved dimensions while the in-memory display copy is bounded to a 1600-pixel maximum edge.

Generated desktop files use this location and filename form:

```text
$HOME/.config/a6-studio/outputs/generated-<timestamp>-<process>-<sequence>.<png|webp|jpg>
```

The default directory can be changed in Settings. Changing it affects subsequent desktop generations; existing history entries continue to reference the path at which each image was originally saved.

## Persistent generation history

When `Retain local generation history` is enabled, every successful desktop generation is recorded transactionally in `a6-studio.sqlite3` and grouped by application session. Each entry contains only:

- timestamp and session identifier;
- output path;
- prompt and model;
- generation/compatibility settings;
- decoded dimensions;
- request ID when supplied.

Image bytes and Base64 provider payloads are never duplicated into SQLite. Search matches prompts, models, paths, request IDs, timestamps, and session IDs.

Selecting an entry loads a bounded preview from the original output path. Its prompt, model, resolution, settings, session ID, request ID, and file path remain in their normal detail rows but can be selected and copied in full or in part. `Load in Create` restores its prompt and settings. If the image was moved or deleted, the interface explains that the file is unavailable, disables file-dependent actions, and still allows the cached prompt/settings to be restored. `Clear history` removes metadata only; it deliberately does not delete generated image files.

Disabling retention stops new successful generations from being appended. Existing metadata remains available until explicitly cleared.

## Error log

The separate Error log records connection, generation, validation, and other operational failures in the same SQLite database with:

- UTC timestamp and application session;
- operation and error category;
- prompt when one was involved;
- model, sanitized endpoint, and request ID;
- a concise summary;
- the complete captured provider response body, sanitized and bounded to a 1 MiB safety limit.

Search covers all of those fields, including provider response text. The detail pane keeps each response bound to its originating session and request context. Its metadata, prompt, and error text remain separate visual rows while supporting full or partial text selection; the existing provider-response pane is also read-only and selectable. The pane explicitly marks a response that reached the safety limit. Transport failures without an HTTP response show that no body was available.

Successful image responses and Base64 image payloads are never copied into the error log. The active API key is redacted before persistence. `Clear error log` removes the local diagnostic metadata.

## CLI commands

Show command help:

```bash
cargo run -- --help
```

Launch the graphical interface explicitly:

```bash
cargo run -- gui
```

### Check connectivity

```bash
cargo run -- check
```

This performs a non-generation request to the models endpoint. It prints the sanitized gateway root, HTTP status, request ID and rate-limit information when supplied, and whether the configured model appears in the response. A missing models endpoint is reported separately because generation can still work even when model listing is unavailable.

### Run the smoke generation

```bash
cargo run -- smoke-generate --yes
```

This command makes one potentially billable request using a fixed prompt, `1024x1024` size, low quality, and PNG output. Omitting `--yes` exits without loading credentials or contacting the gateway.

The resulting image is validated before it is saved. CLI smoke files use the following location:

```text
$HOME/.config/a6-studio/outputs/
```

The command prints the exact path, dimensions, byte size, request ID when available, and elapsed time. It never opens the image automatically.

## Diagnostics and exit status

Set `RUST_LOG` to enable additional sanitized Rust diagnostics, for example:

```bash
RUST_LOG=info cargo run -- check
```

Exit status `0` means success, `1` means configuration/network/API/output failure, and `2` means the billable smoke request was not confirmed. CLI syntax errors also use Clap's standard nonzero status.

Authentication headers are never logged. Error responses are parsed when structured JSON is available. A short sanitized summary is used in transient status messages, while the Error log retains the sanitized captured response body up to its 1 MiB safety limit.

## Desktop troubleshooting

A6 Image Studio compiles Slint's Winit Wayland and X11 integrations plus the Qt backend. With no override, startup tries Winit first; Winit uses the active Wayland or X11 display. Qt is attempted only if Winit cannot initialize. The Winit window requests transparency and compositor blur. KDE Wayland can honor the blur request through KWin; unsupported compositors may show translucency without blur, and Winit does not currently implement this blur hint on X11. These differences do not affect generation.

Force the Winit software renderer with:

```bash
SLINT_BACKEND=winit-software cargo run
```

To require Qt explicitly:

```bash
SLINT_BACKEND=qt cargo run
```

Any non-empty `SLINT_BACKEND` value is treated as an explicit override rather
than being replaced by the automatic Winit-first policy.

Run from a terminal with `RUST_LOG=info` to retain sanitized startup and transport diagnostics. A startup error about `A6API_KEY` means the variable was not exported into the environment of the launched process.

If a stored key cannot be loaded, first confirm that the desktop Secret Service is running and the login keyring is unlocked. On KDE this normally means KWallet plus its Secret Service integration. The application continues in an explicit unconfigured state instead of copying or exposing credentials.

On KDE Wayland, install `wl-clipboard` if `Copy image` or `Copy prompt` reports that clipboard support is unavailable. On an X11 session, install `xclip`. Clipboard commands receive only the image or prompt explicitly requested for copying; the API key is never passed to them.

`Save As…` uses the XDG desktop portal on Linux. If no dialog appears, verify that the desktop portal and KDE portal backend are installed and running for the current session.

## Desktop identity and application storage

The stable desktop application ID is:

```text
org.a6studio.A6ImageStudio
```

Source assets are provided under `packaging/` and `assets/icons/hicolor/` for a later packaging phase. Package maintainers should install the desktop entry to `share/applications`, AppStream metadata to `share/metainfo`, and each icon to its corresponding `share/icons/hicolor` directory.

Application-managed state is consolidated below `~/.config/a6-studio` as requested. The user pictures directory remains the starting location for Save As:

| Purpose | Default path |
| --- | --- |
| Generated output | `$HOME/.config/a6-studio/outputs/` |
| Save As starting directory | `${XDG_PICTURES_DIR:-$HOME/Pictures}` |
| Ordinary settings | `$HOME/.config/a6-studio/settings.json` |
| Sessions, generation history, and errors | `$HOME/.config/a6-studio/a6-studio.sqlite3` |
| Application cache | `$HOME/.config/a6-studio/cache/` |

SQLite uses foreign keys, transactional mutations, a five-second busy timeout, and write-ahead logging. The adjacent `a6-studio.sqlite3-wal` and `a6-studio.sqlite3-shm` files can exist while the application is running and are part of normal SQLite operation.

### Migration from the former layout

On the first run with the new layout:

- `settings.json` is imported from the former XDG configuration directory when the new file does not exist;
- `history.json` and `errors.json` are imported transactionally from the former XDG data directory into SQLite;
- the old JSON files are left untouched as recovery backups;
- import-completion markers in SQLite prevent duplicate imports;
- an old default output-directory setting changes to the new `~/.config/a6-studio/outputs/` default;
- existing generated images are not moved, and imported history continues to reference their original paths.

The API key is not stored in any application-managed file or SQLite table. It remains in the environment or the desktop Secret Service.
