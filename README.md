# A6 Image Studio

A6 Image Studio is a Linux desktop and command-line image-generation client for the A6API OpenAI-compatible gateway. The project is being built in reviewable phases. Phase 1 adds a deliberately minimal Slint interface on top of the production-oriented transport, storage, and CLI foundation proven in Phase 0.

## Current capabilities

- Validate environment-based A6API configuration without exposing the API key.
- Check `/v1/models` and report authentication, rate-limit, server, timeout, DNS, and TLS failures.
- Make one explicitly confirmed, billable `gpt-image-2` smoke generation.
- Accept either Base64 or URL image responses, validate the image, convert it to PNG when needed, and store it under the XDG data directory.
- Capture request ID, retry, and rate-limit response headers when present.
- Launch a responsive Slint desktop window for connection testing and fixed-setting image generation.
- Cancel an active GUI request and preview a generated image without decoding it on the UI thread.

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

## Project status

Phase 1 is implemented and awaits manual desktop verification on KDE Plasma under Wayland and X11. Later controls and final visual design are intentionally deferred to their specified phases.
