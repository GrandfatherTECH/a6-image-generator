# A6 Image Studio

A6 Image Studio is a Linux desktop and command-line client for generating images through the A6API OpenAI-compatible gateway. It provides a Slint desktop interface, deterministic connectivity and smoke-test commands, local history, secure keyring support, and Arch/CachyOS packaging inputs.

> [!WARNING]
> Image generation can incur provider charges. The desktop labels generation actions, and the CLI smoke test refuses to run without `--yes`. Review your A6API account limits before generating images.

## Requirements

- Linux with a Wayland or X11 desktop session for the GUI.
- A current stable Rust toolchain with Cargo for source builds.
- An A6API key with access to the configured image model.
- Network access to the configured gateway.
- Runtime desktop services for the features you use: Secret Service/KWallet for stored credentials, XDG Desktop Portal for file dialogs, and `wl-copy` or `xclip` for clipboard actions.

The project currently targets Linux. The CLI connectivity commands can run without a graphical session, but they still require the Rust-built binary and environment-based credentials.

## Install From Source

Clone the repository and build the locked dependency set:

```bash
git clone https://github.com/grandfathertech/a6-image-generator.git
cd a6-image-generator
cargo build --release --locked
```

Run the binary in place:

```bash
./target/release/a6-image-studio
```

Arch and CachyOS users can also build the provided package recipe after replacing its release checksum as described in the [packaging guide](docs/PACKAGING.md). No prebuilt package or binary is currently published by this repository.

## Current capabilities

- Validate environment-based A6API configuration without exposing the API key.
- Check `/v1/models` and report authentication, rate-limit, server, timeout, DNS, and TLS failures.
- Make one explicitly confirmed, billable `gpt-image-2` smoke generation.
- Accept either Base64 or URL image responses, validate them, convert desktop output to the selected PNG/WebP/JPEG format, and store it under the configured output directory.
- Capture request ID, retry, and rate-limit response headers when present.
- Retry temporary transport failures and HTTP 429/5xx responses up to three total attempts, honoring `Retry-After` and using cancellable exponential backoff with jitter.
- Launch a responsive, vertically scrollable, translucent Slint desktop window that follows the system light/dark palette and prefers Winit on Wayland or X11, with Qt as a startup fallback.
- Use the documented `gpt-image-2` dimension presets or any custom valid dimensions, plus quality, background, and PNG/WebP/JPEG controls.
- Omit individual optional request fields through compatibility settings when a gateway rejects them.
- Cancel an active GUI request and preview a generated image without decoding it on the UI thread.
- Track idle, connecting, generating, success, cancelled, and error states explicitly.
- Prevent overlapping requests, reject stale async completions, and atomically commit generated output files without filename collisions.
- Save a generated result under another name, copy the image or prompt, regenerate the exact request, and open its containing folder.
- Display request duration, requested and actual dimensions, encoded file size, format, path, and request ID.
- Report proportional provider-adjusted output sizes without treating them as failures, while still warning when the returned aspect ratio changes.
- Switch among the six most recent results from the current session.
- Move between Create, History, Error log, and Settings with a compact animated app switcher; section cards enter gently from below and image changes use a bounded cross-slide.
- Consolidate application-managed settings, cache, output defaults, and the SQLite database below `~/.config/a6-studio/`.
- Optionally store the API key in Secret Service/KWallet under the visible per-user label `io.github.grandfathertech.A6ImageStudio.key.<username>`; environment credentials always take precedence.
- Keep transactional, searchable SQLite generation history grouped by application session, restore old prompts/settings even when an image file has been moved or deleted, and clear metadata without deleting generated images.
- Keep a separate searchable SQLite error log with timestamps, session/request context, and the sanitized provider response body (up to a 1 MiB safety limit).
- Export a reduced diagnostic JSON report that excludes prompts, provider bodies, file paths, preferences, and credentials.
- Import the former JSON settings/history/error files and legacy keyring entry once without deleting the JSON recovery copies.
- Integrate with Linux desktops through a stable XDG app ID, desktop entry, AppStream metadata, freedesktop icon set, consolidated application paths, and portal-backed Save As.
- Provide keyboard navigation, application shortcuts, translatable Slint strings, and an Arch Linux PKGBUILD with optional split debug symbols.

## Quick Start

```bash
export A6API_KEY='your-key-here'
cargo run
```

No command launches the desktop interface; `cargo run -- gui` is equivalent. The permanent command-line connectivity probe remains available:

```bash
cargo run -- check
```

The CLI smoke test requires explicit billable confirmation:

```bash
cargo run -- smoke-generate --yes
```

For an optimized standalone CLI:

```bash
cargo build --release
./target/release/a6-image-studio --help
```

See the [user guide](docs/USER_GUIDE.md) for configuration, desktop controls, and commands, the [developer guide](docs/DEVELOPMENT.md) for architecture and contribution details, and the [packaging guide](docs/PACKAGING.md) for Arch/CachyOS builds.

## Documentation

- [User guide](docs/USER_GUIDE.md): configuration, keyring behavior, desktop workflows, CLI commands, storage, and troubleshooting.
- [Developer guide](docs/DEVELOPMENT.md): architecture, execution model, local checks, persistence contracts, and manual QA.
- [Packaging guide](docs/PACKAGING.md): reproducible release builds and Arch/CachyOS packaging.
- [Contributing](CONTRIBUTING.md): change workflow, testing expectations, and pull-request guidance.
- [Security policy](SECURITY.md): private vulnerability reporting and credential-handling expectations.
- [Changelog](CHANGELOG.md): release-facing changes.

## Image dimensions

`gpt-image-2` supports thousands of resolutions, so the desktop interface combines common presets with a `Custom…` editor rather than presenting an impractically long list. Custom width and height are checked locally against the official constraints: each edge is at most 3840 and divisible by 16, the aspect ratio is no wider than 3:1, and total pixels are between 655,360 and 8,294,400. Outputs above 2560×1440 total pixels are labeled experimental.

See the official [OpenAI image-generation guide](https://developers.openai.com/api/docs/guides/image-generation) and [`gpt-image-2` model page](https://developers.openai.com/api/docs/models/gpt-image-2).

An OpenAI-compatible gateway can return a smaller raster while preserving the
requested aspect ratio. A6 Image Studio reports that as a provider-adjusted size,
shows both requested and decoded dimensions, and preserves the paid response
unchanged. A changed aspect ratio remains a prominent warning. The client never
silently stretches or artificially upscales output.

## Project status

Version `0.1.0` is the first planned public release. The repository includes packaging inputs, but nothing is published or uploaded by the build process. See the [changelog](CHANGELOG.md) for release scope and known limitations.

## Privacy and Local Data

The application sends prompts and selected generation settings to the configured API endpoint. Generated files, preferences, history, and error metadata are stored locally as described in the user guide. API keys are read from the environment or optional system keyring and are not written to application settings or SQLite. Sanitized diagnostic exports intentionally omit prompts, provider response bodies, paths, preferences, and credentials.

## License

Copyright (c) 2026 grandfathertech. All rights reserved. This repository is source-available but not open source; see [LICENSE](LICENSE) for the applicable terms. Third-party dependencies remain subject to their own licenses.
