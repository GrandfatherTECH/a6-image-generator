# User Guide

This guide documents the Phase 2 Slint desktop interface and the permanent command-line interface.

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

Configuration is read when the process starts. Restart the application after changing an environment variable. Phase 2 does not store credentials or offer an in-app key editor.

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

`Generate test image` is a potentially billable action. It submits the current prompt with the fixed Phase 1/2 settings: `1024x1024`, low quality, and PNG output. While the request is active, the connection and generation buttons are disabled, a busy indicator is visible, and `Cancel` aborts the active asynchronous task. A cancelled or superseded operation cannot later replace the visible state. Successful output is validated, atomically saved, and shown in the preview. The result line includes dimensions, file size, duration, path, and request ID when supplied.

Generated desktop files use this location and filename form:

```text
${XDG_DATA_HOME:-$HOME/.local/share}/a6-image-studio/outputs/generated-<timestamp>-<process>-<sequence>.png
```

The Phase 2 interface intentionally has no size, quality, format, model, or output-directory controls yet.

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
