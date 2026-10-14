# Web & Android client

favetto ships a [Dioxus](https://dioxuslabs.com/) client — a single Rust codebase
that compiles to two targets:

| Target | Build | Served / installed |
|--------|-------|--------------------|
| **Web** (WASM SPA) | `dx build --platform web` | Embedded in the daemon binary, served at `/` |
| **Android** (native) | `dx build --platform android --features mobile` | APK installed on a device or emulator |

Both targets speak the same MessagePack wire protocol as the TUI, over HTTP +
SSE. The web client is same-origin with the daemon (no CORS); the Android app
connects to a remote daemon whose URL you enter on first launch.

## Using the web client

The daemon serves the web client out of the box. Start the daemon:

```bash
favetto daemon
```

Then open <http://127.0.0.1:7878> in a browser. On first visit you will see the
pairing screen — enter the code from `favetto pair` (or open
`http://127.0.0.1:7878/?pair=CODE`) to exchange it for a token. The token is
stored in `localStorage` and reused on subsequent visits.

The web client is enabled by default (`[web].enabled = true`). To disable it:

```toml
[web]
enabled = false
```

### Remote access

The web client works over the same remote setup as the TUI — see
[Remote access](./remote-access). Put the daemon behind a TLS-terminating
reverse proxy and open `https://PROXY_HOST` in a browser. The proxy must
forward the `Authorization` header and the `/rpc` / `/events` paths.

## Developing the web client

The client lives in `web/`, a standalone cargo workspace (excluded from the
root workspace so the native `clippy`/`nextest` gates don't need the Dioxus
toolchain).

### Prerequisites

- **Rust** 1.94+ with the `wasm32-unknown-unknown` target:
  ```bash
  rustup target add wasm32-unknown-unknown
  ```
- **Dioxus CLI** (`dx`), pinned to the version in `web/Cargo.toml`:
  ```bash
  cargo install dioxus-cli --version 0.7.10 --locked
  ```

### Dev server

```bash
cd web
dx serve --platform web
```

This starts a hot-reloading dev server (default port 8080). The daemon must be
running separately; point the dev server at it by setting the daemon URL in
the pairing screen, or proxy `/rpc` and `/events` to the daemon.

### Build the bundle

```bash
cd web
dx bundle --locked --release --platform web
```

Dioxus 0.7 emits the bundle under `dist/public`. The daemon embeds `web/dist`
via `rust-embed`, so flatten it:

```bash
rm -rf dist/assets
cp -R dist/public/. dist/
rm -rf dist/public
```

The resulting `web/dist/` is picked up the next time you build the daemon
(`cargo build -p favetto`). The committed `web/dist/` is only a placeholder —
CI builds the real bundle and uploads it as a workflow artifact.

### Serve from disk during development

To iterate on the client without rebuilding the daemon, point the daemon at the
on-disk bundle:

```bash
favetto daemon --web-dir web/dist
```

Or set it in config:

```toml
[web]
dir = "web/dist"
```

### Checks

```bash
cd web
cargo fmt --all -- --check
cargo test --locked
```

## Android app

The Android build cross-compiles the same Dioxus codebase to a native APK.
At runtime the app prompts for the daemon's base URL (e.g.
`https://favetto.example.com`), tests the connection, then shows the pairing
screen — same flow as the web client, plus the URL step.

### Prerequisites

- **JDK 17** (Temurin or equivalent).
- **Android SDK** with platform 34 and build-tools 34.0.0.
- **Android NDK** 27.0.12077973 (set `ANDROID_NDK_HOME` / `NDK_HOME`).
- **Rust** Android targets:
  ```bash
  rustup target add aarch64-linux-android armv7-linux-androideabi \
      i686-linux-android x86_64-linux-android
  ```
- **Dioxus CLI** (`dx`) 0.7.10 (same as web).

### Build and run on an emulator

```bash
cd web
dx serve --platform android --features mobile
```

`dx serve` builds the app, starts an emulator (or attaches to a running one),
and installs the APK. Hot reload is not available for the mobile target; stop
and re-run to pick up changes.

### Build and run on a device

Enable USB debugging on the device, connect it, then:

```bash
cd web
dx serve --platform android --features mobile --device
```

### Package an APK

```bash
cd web
# Compile the native library
dx build --locked --release --platform android --features mobile \
    --target aarch64-linux-android
# Bundle the APK
dx bundle --locked --platform android --package-types apk \
    --features mobile --target aarch64-linux-android
```

The APK lands in `web/dist/*.apk`. CI builds an `aarch64-linux-android` APK on
every push and uploads it as the `favetto-android-apk` workflow artifact.

To produce APKs for all ABIs, run the two commands once per target
(`aarch64-linux-android`, `armv7-linux-androideabi`, `i686-linux-android`,
`x86_64-linux-android`) and merge them with `bundletool`.

### Permissions

The app declares `POST_NOTIFICATIONS` (Android 13+) so the in-app
`Notification` API can ask at runtime. This is configured in `web/Dioxus.toml`
under `[permissions]`; `dx` maps it into the generated `AndroidManifest.xml`.

## How the daemon embeds the web bundle

The `favetto` crate embeds `web/dist/` at compile time with `rust-embed`:

```rust
#[derive(RustEmbed)]
#[folder = "../../web/dist"]
#[allow_missing = true]
struct Assets;
```

`allow_missing` keeps the crate buildable when `web/dist` is absent (for
example a `cargo package` that omits `web/`); the daemon then serves a short
diagnostic page at `/` instead of the SPA.

Routes:

| Route | Handler |
|-------|---------|
| `GET /` | embedded `index.html` |
| `GET /assets/*` | embedded hashed asset, immutable cache |
| any other `GET` with `Accept: text/html` | SPA fallback → `index.html` |
| `/rpc`, `/events`, `/pair/*`, `/webhooks/*`, … | API routes, never shadowed |

`index.html` is served `no-cache`; hashed assets are long-lived. The WASM MIME
type is explicit (`application/wasm`), which browsers require.

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `/` shows a diagnostic page | Build the web bundle (`dx bundle --platform web`) and flatten `dist/public` into `dist/`, then rebuild the daemon |
| Web client can't connect | Check the daemon is running and `[web].enabled = true`; open the browser dev console for the exact error |
| Android app can't reach the daemon | Use `https://` (not `http://`) for remote daemons; the app validates the scheme. For a LAN daemon, allow cleartext in your reverse proxy or use a TLS tunnel |
| `dx` version mismatch | `dx` is pinned to 0.7.10 in `web/Cargo.toml` and CI; install the same version locally |
| WASM budget exceeded | CI fails when the gzipped WASM exceeds 1 MB (warns at 500 KB); check for new heavy dependencies |
