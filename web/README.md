# favetto web client

A [Dioxus](https://dioxuslabs.com/) client shared by two targets:

- **Web** (`wasm32-unknown-unknown`) — built with `dx build --platform web` and
  served by the daemon from its embedded `web/dist` assets.
- **Android** (native) — built with `dx build --platform android --features mobile`
  and connects to a remote daemon.

This crate is a **standalone cargo workspace** (excluded from the root
workspace) so the native `cargo clippy`/`nextest` gates don't need the Dioxus
toolchain.

## Prerequisites

```bash
rustup target add wasm32-unknown-unknown
cargo install dioxus-cli --version 0.7.10 --locked
```

The `dx` version is pinned in lockstep with the `dioxus` dependency in
`Cargo.toml`; update both together.

## Develop

```bash
# Web dev server (hot reload)
dx serve --platform web

# Android emulator / device
dx serve --platform android --features mobile
```

## Build

```bash
# Web bundle (for the daemon's rust-embed)
dx bundle --locked --release --platform web
# Dioxus 0.7 emits under dist/public; flatten into dist/:
rm -rf dist/assets && cp -R dist/public/. dist/ && rm -rf dist/public

# Android APK
dx build --locked --release --platform android --features mobile --target aarch64-linux-android
dx bundle --locked --platform android --package-types apk --features mobile --target aarch64-linux-android
```

## Styling and icons

Styling is **Tailwind CSS v4**. `dx` autodetects the root [`tailwind.css`](./tailwind.css)
and runs its bundled Tailwind CLI (v4.1.5) to generate `assets/tailwind.css`
before the Rust compile, for both `dx serve` and `dx build`. There is **no Node
or npm toolchain**.

The generated `assets/tailwind.css` is committed so the host `cargo test` and
`cargo clippy` builds — which expand the `asset!("/assets/tailwind.css")` macro —
succeed without first running `dx`. **After editing `tailwind.css`, regenerate
it** so the committed artifact does not drift (CI enforces this):

```bash
dx build --platform web    # regenerates assets/tailwind.css as part of the build
```

The design tokens (palette, radii, shadows, fonts) live in the `@theme` block;
light/dark are one `prefers-color-scheme` token flip. Icons are
[Phosphor](https://phosphoricons.com/), rendered through one `AppIcon` wrapper in
`src/components/icon.rs`; there are no emoji/unicode icon glyphs in the client.

## Checks

```bash
cargo fmt --all -- --check
cargo test --locked
```

## Docs

See [docs/guide/web-client.md](../docs/guide/web-client.md) for the full
guide (using, developing, packaging, and troubleshooting both targets).
