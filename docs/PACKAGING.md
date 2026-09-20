# Arch Linux and CachyOS packaging

The canonical application ID is `io.github.grandfathertech.A6ImageStudio`. The
desktop entry, AppStream component, icon filenames, and Slint window identity
must keep that exact spelling. The Secret Service lookup uses the separate
service name `io.github.grandfathertech.A6ImageStudio.key`.

## Reproducible release build

Build from a clean, reviewed tag using the stable Rust toolchain represented by
`Cargo.lock`. The lockfile is part of the source release and must be committed.

```bash
export SOURCE_DATE_EPOCH="$(git log -1 --pretty=%ct)"
export CARGO_INCREMENTAL=0
export CARGO_TARGET_DIR=target
export RUSTFLAGS="--remap-path-prefix=$(pwd)=/usr/src/debug/a6-image-studio"
cargo fetch --locked --target x86_64-unknown-linux-gnu
cargo build --frozen --offline --release
cargo test --frozen --offline --all-targets
```

`--frozen --offline` makes the build fail instead of changing `Cargo.lock` or
accessing the network after dependency acquisition. The release profile enables
thin LTO, one code-generation unit, panic aborts, and line-table debug data.

## Build the Arch package

1. Create and review tag `v0.1.0` without publishing it from this workflow.
2. Create the matching source archive and calculate its BLAKE2 checksum.
3. Replace `SKIP` in `packaging/arch/PKGBUILD` with that checksum. Never
   distribute a PKGBUILD whose release archive checksum is `SKIP`.
4. From a copy of `packaging/arch`, run:

```bash
makepkg --cleanbuild --syncdeps --check
namcap PKGBUILD a6-image-studio-*.pkg.tar.*
```

The source URL assumes the repository will live at
`https://github.com/grandfathertech/a6-image-generator`; update `url` and `source`
together if the actual repository differs. No package is uploaded by these
instructions.

## Debug symbols

The PKGBUILD enables Arch's `debug` option while retaining the normal `strip`
behavior. Current `makepkg` versions therefore emit an optional
`a6-image-studio-debug` package containing separated symbols. Keep `debug = 1`
in Cargo's release profile so Rust line information reaches that package. Users
normally install only `a6-image-studio`; install the debug package when
collecting a native crash trace.

## What belongs in Git

Commit source, UI, tests, `Cargo.toml`, `Cargo.lock`, `build.rs`, documentation,
the PKGBUILD, desktop/AppStream metadata, and source icons. Do not commit
`target/`, generated Arch `src/` or `pkg/` trees, package archives, `.env` files,
API keys, diagnostic exports, screenshots made during testing, or generated
translation binaries. A generated `.SRCINFO` belongs in an AUR publishing
repository, not necessarily in this application source repository.
