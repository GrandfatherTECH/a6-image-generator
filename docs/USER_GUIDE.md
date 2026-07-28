# User Guide

This guide documents the Phase 3 Slint desktop interface and the permanent command-line interface.

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

Configuration is read when the process starts. Restart the application after changing an environment variable. Phase 3 does not store credentials or offer an in-app key editor.

## Desktop interface

Launch the desktop interface with no subcommand:

```bash
cargo run
```

The explicit equivalent is:

```bash
cargo run -- gui
```

The connection panel displays the normalized endpoint, masked API key, configured model, and current operation status. It never displays the full key.

`Test connection` calls the models endpoint and reports its HTTP status and whether the configured model is listed. A model missing from that list does not necessarily mean image generation is unavailable.

`Generate image` is a potentially billable action. While the request is active, the connection and generation buttons are disabled, a busy indicator is visible, and `Cancel` aborts the active asynchronous task. A cancelled or superseded operation cannot later replace the visible state.

The complete window is inside a vertical scroll view. Mouse-wheel, touchpad, and scrollbar navigation keep every control and result action reachable when the window is shorter than its content.

### Generation controls

| Control | Values |
| --- | --- |
| Size | `auto`, `1024x1024`, `1024x1536`, `1536x1024` |
| Quality | `auto`, `low`, `medium`, `high` |
| Background | `auto`, `opaque`, `transparent` |
| Output format | PNG, WebP, JPEG |

The initial settings remain `1024x1024`, low quality, automatic background, and PNG. Prompt input is trimmed and must not be empty. Transparent background with JPEG is rejected locally because JPEG cannot preserve alpha; choose PNG or WebP instead.

The selected output format controls both the API request and the durable file format. If the gateway returns another decodable format, the application converts it off the UI thread before the atomic write.

### Compatibility settings

Expand `Compatibility settings` to control whether the request sends `size`, `quality`, `background`, and `output_format`. All four are enabled for the target `gpt-image-2` workflow by default.

If A6API or a configured compatible model rejects one parameter:

1. Read the complete sanitized server error shown in the application.
2. Expand compatibility settings.
3. Clear only the rejected parameter.
4. Retry the request.

The corresponding normal control is hidden while its parameter is disabled. The disabled value remains available if the parameter is re-enabled. When `output_format` is omitted, the selected format still determines the local file produced after the response is decoded.

### Generated result and actions

Successful output is validated, atomically saved, and shown in the preview. The result panel reports request duration, image dimensions, encoded file size, output format, exact path, and request ID when supplied.

- `Save As…` opens the desktop portal file dialog and writes an atomic copy. Keep the selected format's extension.
- `Copy image` sends the encoded file to the desktop clipboard. On Linux this uses `wl-copy` on Wayland or `xclip` on X11 when available.
- `Copy prompt` sends the current prompt text to the same clipboard integration.
- `Regenerate` restores and repeats the exact prompt, options, and compatibility switches from the last successful generation. It can incur another charge.
- `Open containing folder` opens the output directory through `xdg-open`.

Result actions run away from Slint's event loop and show completion or failure text below the buttons.

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

If the default GPU renderer cannot start, try Slint's software renderer:

```bash
SLINT_BACKEND=winit-software cargo run
```

Run from a terminal with `RUST_LOG=info` to retain sanitized startup and transport diagnostics. A startup error about `A6API_KEY` means the variable was not exported into the environment of the launched process.

On KDE Wayland, install `wl-clipboard` if `Copy image` or `Copy prompt` reports that clipboard support is unavailable. On an X11 session, install `xclip`. Clipboard commands receive only the image or prompt explicitly requested for copying; the API key is never passed to them.

`Save As…` uses the XDG desktop portal on Linux. If no dialog appears, verify that the desktop portal and KDE portal backend are installed and running for the current session.
