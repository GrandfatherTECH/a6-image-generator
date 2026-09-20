# Changelog

All notable user-visible changes to A6 Image Studio are documented here. The project does not yet promise semantic-versioning compatibility before `1.0.0`.

## Unreleased

- No user-visible changes recorded yet.

## 0.1.0 - 2026-08-02

### Added

- Slint desktop interface for image generation, preview, recent results, history, error logs, settings, and desktop file actions.
- Permanent `check` and explicitly confirmed `smoke-generate` CLI commands.
- Typed image dimensions, quality, background, output format, and gateway compatibility controls.
- PNG, WebP, and JPEG validation/conversion with bounded previews and atomic output persistence.
- Cancellable retries for temporary transport failures and HTTP 429/5xx responses, including `Retry-After` support.
- Optional Secret Service/KWallet credential storage with environment-variable precedence.
- Transactional SQLite sessions, generation history, error records, and one-time migration from the former JSON layout.
- Sanitized diagnostic JSON export, keyboard navigation, accessibility labels, localization hooks, and responsive desktop layouts.
- Freedesktop icons, desktop entry, AppStream metadata, and an Arch/CachyOS PKGBUILD.

### Security

- Credentials are masked in user-visible configuration and redacted from persisted/exported diagnostics.
- Bearer authorization is not attached to provider-returned image-download URLs.
- Provider error capture and successful image response bodies have explicit size limits.

### Known limitations

- Linux is the only supported desktop platform.
- No prebuilt binaries or packages are published from this repository.
- Actual gateway parameter support and returned dimensions can differ across OpenAI-compatible providers.
- Clipboard integration requires `wl-copy` on Wayland or `xclip` on X11.
