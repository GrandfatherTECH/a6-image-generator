# A6 Image Studio

A6 Image Studio is a Linux desktop and command-line image-generation client for the A6API OpenAI-compatible gateway. The project is being built in reviewable phases. The Phase 5 desktop now uses secure system-keyring credentials and a transactional SQLite database for persistent sessions, generation history, and searchable diagnostics.

## Current capabilities

- Validate environment-based A6API configuration without exposing the API key.
- Check `/v1/models` and report authentication, rate-limit, server, timeout, DNS, and TLS failures.
- Make one explicitly confirmed, billable `gpt-image-2` smoke generation.
- Accept either Base64 or URL image responses, validate them, convert desktop output to the selected PNG/WebP/JPEG format, and store it under the configured output directory.
- Capture request ID, retry, and rate-limit response headers when present.
- Launch a responsive, vertically scrollable, translucent Slint desktop window that follows the system light/dark palette and prefers Winit on Wayland or X11, with Qt as a startup fallback.
- Use the documented `gpt-image-2` dimension presets or any custom valid dimensions, plus quality, background, and PNG/WebP/JPEG controls.
- Omit individual optional request fields through compatibility settings when a gateway rejects them.
- Cancel an active GUI request and preview a generated image without decoding it on the UI thread.
- Track idle, connecting, generating, success, cancelled, and error states explicitly.
- Reject stale async completions and atomically commit generated output files.
- Save a generated result under another name, copy the image or prompt, regenerate the exact request, and open its containing folder.
- Display request duration, requested and actual dimensions, encoded file size, format, path, and request ID.
- Report proportional provider-adjusted output sizes without treating them as failures, while still warning when the returned aspect ratio changes.
- Switch among the six most recent results from the current session.
- Consolidate application-managed settings, cache, output defaults, and the SQLite database below `~/.config/a6-studio/`.
- Optionally store the API key in Secret Service/KWallet under the visible per-user label `org.a6-studio.key.<username>`; environment credentials always take precedence.
- Keep transactional, searchable SQLite generation history grouped by application session, restore old prompts/settings even when an image file has been moved or deleted, and clear metadata without deleting generated images.
- Keep a separate searchable SQLite error log with timestamps, session/request context, and the sanitized provider response body (up to a 1 MiB safety limit).
- Import the former JSON settings/history/error files and legacy keyring entry once without deleting the JSON recovery copies.
- Integrate with Linux desktops through a stable XDG app ID, desktop entry, AppStream metadata, freedesktop icon set, consolidated application paths, and portal-backed Save As.

## Quick start

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

See the [user guide](docs/USER_GUIDE.md) for configuration, desktop controls, and commands, and the [developer guide](docs/DEVELOPMENT.md) for architecture and contribution details.

## Image dimensions

`gpt-image-2` supports thousands of resolutions, so the desktop interface combines common presets with a `Custom…` editor rather than presenting an impractically long list. Custom width and height are checked locally against the official constraints: each edge is at most 3840 and divisible by 16, the aspect ratio is no wider than 3:1, and total pixels are between 655,360 and 8,294,400. Outputs above 2560×1440 total pixels are labeled experimental.

See the official [OpenAI image-generation guide](https://developers.openai.com/api/docs/guides/image-generation) and [`gpt-image-2` model page](https://developers.openai.com/api/docs/models/gpt-image-2).

An OpenAI-compatible gateway can return a smaller raster while preserving the
requested aspect ratio. A6 Image Studio reports that as a provider-adjusted size,
shows both requested and decoded dimensions, and preserves the paid response
unchanged. A changed aspect ratio remains a prominent warning. The client never
silently stretches or artificially upscales output.

## Project status

Phase 5 is implemented, including its SQLite persistence and storage-layout migration. Linux package assembly remains intentionally deferred to the next phase.
