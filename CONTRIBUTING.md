# Contributing

## Before starting

Open an issue before large feature, schema, storage-layout, or UI architecture changes. Small bug fixes and documentation corrections can go directly to a focused pull request.

Do not include real API keys, prompts from private workloads, generated customer images, provider response bodies, diagnostic exports, or local database/settings files in commits or issues.

## Development workflow

1. Create a branch from the current default branch.
2. Keep the change focused and follow the module boundaries in [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md).
3. Add or update tests for behavior changes. Network tests must use the local mock server and must never contact A6API or another real provider.
4. Update user, developer, packaging, and Rustdoc documentation when their contracts change.
5. Add user-visible changes under `Unreleased` in [`CHANGELOG.md`](CHANGELOG.md).
6. Run the required checks before opening a pull request.

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo build --release --locked
```

When the relevant tools are installed, also validate the desktop and packaging metadata:

```bash
desktop-file-validate packaging/io.github.grandfathertech.A6ImageStudio.desktop
appstreamcli validate --no-net packaging/io.github.grandfathertech.A6ImageStudio.metainfo.xml
xmllint --noout assets/icons/hicolor/scalable/apps/io.github.grandfathertech.A6ImageStudio.svg
bash -n packaging/arch/PKGBUILD
```

## Pull requests

Describe the user-visible result, implementation approach, tests performed, and remaining manual verification. Explicitly call out changes that affect API billing, credentials, diagnostics, database schemas, migrations, output files, desktop identity, or package metadata.

Do not make a real billable request solely to validate a pull request. If manual provider testing is necessary, state exactly what was tested and keep credentials and generated private content out of the repository.

## Code and documentation expectations

- Prefer existing typed domain and storage APIs over ad hoc JSON or path handling.
- Keep network, decoding, keyring, and filesystem work off the Slint event loop.
- Preserve credential redaction and never attach bearer credentials to provider-returned image URLs.
- Document non-obvious invariants and security boundaries, not trivial implementation steps.
- Keep platform-specific behavior explicit and retain useful error context without exposing secrets.

By contributing, you agree that your contribution may be incorporated under the repository's proprietary license. No ownership transfer or open-source license grant is implied beyond the terms accepted by the repository owner.
