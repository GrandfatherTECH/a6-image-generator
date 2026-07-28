# A6 Image Studio

A6 Image Studio is a Linux desktop and command-line image-generation client for the A6API OpenAI-compatible gateway. The project is being built in reviewable phases. Phase 4 adds the polished, KDE-oriented desktop experience and Linux desktop identity to the architecture established in earlier phases.

## Current capabilities

- Validate environment-based A6API configuration without exposing the API key.
- Check `/v1/models` and report authentication, rate-limit, server, timeout, DNS, and TLS failures.
- Make one explicitly confirmed, billable `gpt-image-2` smoke generation.
- Accept either Base64 or URL image responses, validate them, convert desktop output to the selected PNG/WebP/JPEG format, and store it under the XDG data directory.
- Capture request ID, retry, and rate-limit response headers when present.
- Launch a responsive, vertically scrollable Slint desktop window that follows the system light/dark palette and uses Qt styling when Qt is available.
- Use the documented `gpt-image-2` dimension presets or any custom valid dimensions, plus quality, background, and PNG/WebP/JPEG controls.
- Omit individual optional request fields through compatibility settings when a gateway rejects them.
- Cancel an active GUI request and preview a generated image without decoding it on the UI thread.
- Track idle, connecting, generating, success, cancelled, and error states explicitly.
- Reject stale async completions and atomically commit generated output files.
- Save a generated result under another name, copy the image or prompt, regenerate the exact request, and open its containing folder.
- Display request duration, dimensions, encoded file size, format, path, and request ID.
- Switch among the six most recent results from the current session without persisting history.
- Integrate with Linux desktops through a stable XDG app ID, desktop entry, AppStream metadata, freedesktop icon set, XDG paths, and portal-backed Save As.

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

## Project status

Phase 4 is implemented. Secure in-app credential storage and persistent generation history remain intentionally deferred to Phase 5; Linux package assembly remains a later phase.
