# User Guide

This guide documents the Phase 4 Slint desktop interface and the permanent command-line interface.

## Requirements

- A Linux system with a current stable Rust toolchain for source builds.
- An A6API API key with access to `gpt-image-2`.
- Network access to the configured gateway.
- A Wayland or X11 desktop session for the graphical interface.

## Configuration

A6 Image Studio reads configuration from environment variables:

| Variable | Required | Default | Purpose |
| --- | --- | --- | --- |
| `A6API_KEY` | Yes | None | Bearer token used for API requests. |
| `A6API_BASE_URL` | No | `https://api.a6api.com` | OpenAI-compatible API root. Both roots with and without `/v1` are accepted. |
| `A6API_IMAGE_MODEL` | No | `gpt-image-2` | Image model identifier. |

Trailing slashes in the base URL are removed. The URL must use HTTP or HTTPS, include a host, and must not contain embedded credentials, a query, or a fragment. The application never prints the complete API key; diagnostics show only a masked form.

Example for the default gateway:

```bash
export A6API_KEY='your-key-here'
export A6API_BASE_URL='https://api.a6api.com'
export A6API_IMAGE_MODEL='gpt-image-2'
```

Shell exports last only for the current shell unless added to a secure environment setup. Do not put API keys in the repository or ordinary configuration files.

Configuration is read when the process starts. Restart the application after changing an environment variable. Phase 4 does not store credentials or offer an in-app key editor.

## Desktop interface

Launch the desktop interface with no subcommand:

```bash
cargo run
```

The explicit equivalent is:

```bash
cargo run -- gui
```

The compact header displays the normalized endpoint, masked API key, configured model, and current operation status. It never displays the full key. The interface follows the system light/dark palette and font. Wide windows place the prompt/settings panel beside a large preview canvas; narrower windows stack the same panels vertically. The complete window remains in a vertical scroll view, so every control is reachable at the minimum window size.

`Test connection` calls the models endpoint and reports its HTTP status and whether the configured model is listed. A model missing from that list does not necessarily mean image generation is unavailable.

`Generate image` is a potentially billable action. While the request is active, the connection and generation buttons are disabled, a busy indicator is visible, and `Cancel` aborts the active asynchronous task. A cancelled or superseded operation cannot later replace the visible state.

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

### Compatibility settings

Expand the compact `Compatibility settings` row to control whether the request sends `size`, `quality`, `background`, and `output_format`. The switches use an aligned two-column layout, and all four are enabled for the target `gpt-image-2` workflow by default.

If A6API or a configured compatible model rejects one parameter:

1. Read the complete sanitized server error shown in the application.
2. Expand compatibility settings.
3. Clear only the rejected parameter.
4. Retry the request.

The corresponding normal control is hidden while its parameter is disabled. The disabled value remains available if the parameter is re-enabled. When `output_format` is omitted, the selected format still determines the local file produced after the response is decoded.

### Generated result and actions

Successful output is validated, atomically saved, and shown in the large preview canvas. Up to six successful images are retained as an in-memory `Recent this session` strip; selecting a thumbnail changes the preview and metadata, and makes file actions and Regenerate target that result. This strip is cleared when the program exits and does not yet create persistent history.

Expand `Request details` to inspect the successful prompt and sent/omitted options together with request duration, image dimensions, encoded file size, output format, exact path, and request ID when supplied.

- `Save As…` opens the desktop portal file dialog and writes an atomic copy. Keep the selected format's extension.
- `Copy image` sends the encoded file to the desktop clipboard. On Linux this uses `wl-copy` on Wayland or `xclip` on X11 when available.
- `Copy prompt` sends the current prompt text to the same clipboard integration.
- `Regenerate` restores and repeats the exact prompt, options, and compatibility switches from the selected recent generation. It can incur another charge.
- `Open containing folder` opens the output directory through `xdg-open`.

Result actions run away from Slint's event loop and show nonintrusive completion or failure text below the buttons. Large generated images retain their full saved dimensions while the in-memory display copy is bounded to a 1600-pixel maximum edge.

Generated desktop files use this location and filename form:

```text
${XDG_DATA_HOME:-$HOME/.local/share}/a6-image-studio/outputs/generated-<timestamp>-<process>-<sequence>.<png|webp|jpg>
```

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
${XDG_DATA_HOME:-$HOME/.local/share}/a6-image-studio/outputs/
```

The command prints the exact path, dimensions, byte size, request ID when available, and elapsed time. It never opens the image automatically.

## Diagnostics and exit status

Set `RUST_LOG` to enable additional sanitized Rust diagnostics, for example:

```bash
RUST_LOG=info cargo run -- check
```

Exit status `0` means success, `1` means configuration/network/API/output failure, and `2` means the billable smoke request was not confirmed. CLI syntax errors also use Clap's standard nonzero status.

Authentication headers are never logged. Error responses are parsed when structured JSON is available; otherwise a bounded, control-character-cleaned portion is shown with the configured key redacted.

## Desktop troubleshooting

A6 Image Studio includes Slint's Qt and Winit backends. On systems where Qt was available at build time, the default UI uses Qt styling; Winit remains available as a Wayland/X11 fallback. Force the Winit software renderer with:

```bash
SLINT_BACKEND=winit-software cargo run
```

To require Qt explicitly:

```bash
SLINT_BACKEND=qt cargo run
```

Run from a terminal with `RUST_LOG=info` to retain sanitized startup and transport diagnostics. A startup error about `A6API_KEY` means the variable was not exported into the environment of the launched process.

On KDE Wayland, install `wl-clipboard` if `Copy image` or `Copy prompt` reports that clipboard support is unavailable. On an X11 session, install `xclip`. Clipboard commands receive only the image or prompt explicitly requested for copying; the API key is never passed to them.

`Save As…` uses the XDG desktop portal on Linux. If no dialog appears, verify that the desktop portal and KDE portal backend are installed and running for the current session.

## Desktop identity and XDG locations

The stable desktop application ID is:

```text
io.github.grandfathertech.a6-image-studio
```

Source assets are provided under `packaging/` and `assets/icons/hicolor/` for a later packaging phase. Package maintainers should install the desktop entry to `share/applications`, AppStream metadata to `share/metainfo`, and each icon to its corresponding `share/icons/hicolor` directory.

The application resolves standard XDG locations:

| Purpose | Default path |
| --- | --- |
| Generated output | `${XDG_DATA_HOME:-$HOME/.local/share}/a6-image-studio/outputs/` |
| Save As starting directory | `${XDG_PICTURES_DIR:-$HOME/Pictures}` |
| Future configuration | `${XDG_CONFIG_HOME:-$HOME/.config}/a6-image-studio/` |
| Future cache data | `${XDG_CACHE_HOME:-$HOME/.cache}/a6-image-studio/` |

The configuration and cache paths are reserved for later phases; Phase 4 does not persist settings or history there.
