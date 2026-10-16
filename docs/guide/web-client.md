# Web & Android client

favetto ships a [Dioxus](https://dioxuslabs.com/) client — a single Rust codebase
that compiles to two targets:

| Target | Build | Served / installed |
|--------|-------|--------------------|
| **Web** (WASM SPA) | `dx build --platform web` | Embedded in the daemon binary, served at `/` |
| **Android** (native) | `dx bundle --release --platform android --features mobile` | APK installed on a device or emulator (see [Package a release APK](#package-a-release-apk)) |

Both targets speak the same MessagePack wire protocol as the TUI, over HTTP +
SSE. The web client is same-origin with the daemon (no CORS); the Android app
connects to a remote daemon whose URL you enter on first launch.

## Using the web client

The daemon serves the web client out of the box. Start the daemon:

```bash
favetto daemon
```

Then open <http://127.0.0.1:7878> in a browser. On first visit you will see the
sign-in screen. Enter the username and password of an account created with
`favetto user add <name>` on the daemon host; the client calls
`POST /auth/login` and receives the bearer token. A pairing code is still
available behind the "Use a pairing code" toggle (or by opening
`http://127.0.0.1:7878/?pair=CODE`). The token is stored in `localStorage` and
reused on subsequent visits; a 401 clears it and returns to the sign-in screen.

The web client is enabled by default (`[web].enabled = true`). To disable it:

```toml
[web]
enabled = false
```

The **Tasks** list and the **Inbox** cards label each row with a catalog-derived
**kind badge**: `agent` (the default), `action` (a built-in deterministic step),
or `scheduled` (a cron trigger, which loses to `action` when both apply). The
badge is definition-based rather than per-run, and a row whose catalog entry is
unknown shows none. See [the catalog](./catalog) and
[schedules and dependencies](./schedules-and-dependencies).

Opening a task row shows its detail page, which renders the persisted run output
through `tasks.get`. For an **action** task this is the durable
`output.envelope.summary` the built-in step produced; the web client never calls
`agents.start`, so opening an action run can't launch an agent session. The
daemon also rejects `agents.start` for an action task.

### Remote access

The web client works over the same remote setup as the TUI — see
[Remote access](./remote-access). Put the daemon behind a TLS-terminating
reverse proxy and open `https://PROXY_HOST` in a browser. The proxy must
forward the `Authorization` header and the `/rpc`, `/events`, `/auth`, and
`/pair` paths.

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
the pairing screen, or proxy `/rpc`, `/events`, `/auth`, and `/pair` to the
daemon (see the `[[web.proxy]]` entries in `web/Dioxus.toml`). Skipping `/auth`
makes username/password sign-in fail with `HTTP 405` because the login POST
never reaches the daemon.

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

### Bundle size

CI enforces a gzipped WASM budget (warn over **500 KB**, fail over **1 MB**;
see the `web (wasm)` job in `.github/workflows/ci.yml`). The release build is
sized through `[profile.wasm-release]` in `web/Cargo.toml` — `dx` derives that
profile name from the platform and build mode (`wasm` + `release`), so
`dx bundle --release --platform web` picks it up automatically. It sets
`opt-level = "z"`, fat LTO, `codegen-units = 1` and `panic = "abort"`; `strip`
is intentionally left to `dx`, which runs `wasm-opt -Oz --strip-debug` itself.
`dioxus` is also built with `default-features = false` plus only `router`,
`launch` and `lib`, which drops the unused `devtools` and `logger` features.

As measured with the CI command (`gzip -c <wasm> | wc -c`):

| Variant | Uncompressed | Gzipped | vs budget |
|---|---|---|---|
| `favetto-web_bg-*.wasm` | 2,017,800 B | **710,349 B (~693 KiB)** | warn (> 500 KB) |

Keep new dependencies and per-call `format!`/`Debug` usage in check: without
the profile the same bundle measured ~983 KiB gzipped, and any regression that
pushes the artifact back over 1 MB fails CI.

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

### Styling and icons

The client is styled with **Tailwind CSS v4** and a **Phosphor** icon set.

`dx` autodetects `web/tailwind.css` and runs its bundled Tailwind CLI to build
`web/assets/tailwind.css` before the Rust compile, for both `dx serve` and
`dx build` — no Node/npm is involved. The generated `assets/tailwind.css` is
committed (host `cargo test`/`cargo clippy` need it, since they expand the
`asset!` macro), so regenerate it after editing `tailwind.css`:

```bash
cd web
dx build --platform web
```

CI runs `git diff --exit-code -- assets/tailwind.css` after the bundle to catch
drift. Icons are rendered through the shared `AppIcon` wrapper
(`web/src/components/icon.rs`) rather than emoji/unicode glyphs.

## Android app

The Android build cross-compiles the same Dioxus codebase to a native APK.
At runtime the app prompts for the daemon's base URL (e.g.
`https://favetto.example.com`), tests the connection, then shows the pairing
screen — same flow as the web client, plus the URL step.

Native builds have no browser `fetch`, so the transport uses `reqwest` (rustls
with bundled webpki roots, no system OpenSSL). Point it at an `https://` URL with
a publicly-trusted certificate (for example a Let's Encrypt cert behind a
TLS-terminating reverse proxy); a self-signed certificate is rejected. The bearer
token and daemon URL persist in the app's private files directory, so sign-in
survives a restart.

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

### Package a release APK

A release APK is roughly 15 MB (the current app builds to ~13 MiB); the default
debug APK is ~200 MB because its Gradle `debug` build type keeps the Rust debug
symbols (`jniLibs.keepDebugSymbols`) and disables minification.

Getting a release APK takes **both** `--release` **and** an Android signing
config, because `dx` 0.7.10 only runs Gradle `assembleRelease` when the build is
a release build *and* `[bundle.android]` is set (`build/android.rs`); otherwise
it silently runs `assembleDebug`.

#### 1. Create a signing keystore (once)

```bash
cd web
keytool -genkeypair -v -keystore favetto-release.jks \
    -alias favetto -keyalg RSA -keysize 2048 -validity 10000 \
    -storepass 'CHANGE_ME' -keypass 'CHANGE_ME' \
    -dname 'CN=favetto, O=favetto'
```

Keep the keystore out of version control — `*.jks` is git-ignored.

#### 2. Point `web/Dioxus.toml` at it

Uncomment and fill in the `[bundle.android]` table (there is a template in the
file):

```toml
[bundle.android]
jks_file = "favetto-release.jks"
jks_password = "CHANGE_ME"
key_alias = "favetto"
key_password = "CHANGE_ME"
```

`jks_file` is relative to `web/`. Use `[bundle.android]` (not
`[android.signing]`, which the 0.7 schema advertises but `dx` 0.7.10 does not
read).

#### 3. Bundle

```bash
cd web
dx bundle --release --platform android --features mobile \
    --target aarch64-linux-android --package-types apk
```

`dx bundle` compiles the app itself, so no separate `dx build` step is needed.
The APK lands in `web/dist/*.apk`.

#### 4. Sign CI builds with your own key (optional)

CI builds an `aarch64-linux-android` release APK on every push and uploads it as
the `favetto-android-apk` workflow artifact. It generates an ephemeral keystore
by default (fine for dev artifacts, not for Play). To sign with your release
key, add these repository secrets (Settings → Secrets and variables → Actions):

| Secret | Value |
|--------|-------|
| `ANDROID_KEYSTORE_BASE64` | `base64 -w0 favetto-release.jks` |
| `ANDROID_KEYSTORE_PASSWORD` | the store password |
| `ANDROID_KEY_ALIAS` | the key alias (`favetto`) |
| `ANDROID_KEY_PASSWORD` | the key password |

When `ANDROID_KEYSTORE_BASE64` is set, the workflow writes it to
`web/android-release.jks` and signs the APK with it; otherwise it falls back to
an ephemeral key so the artifact is still a release build.

To produce APKs for all ABIs, run the bundle once per target
(`aarch64-linux-android`, `armv7-linux-androideabi`, `i686-linux-android`,
`x86_64-linux-android`) and merge them with `bundletool`.

### App icon

`web/assets/icon.png` is the 512×512 favetto badge derived from
`docs/public/logo.png`. The Android launcher resources live in
`web/android/res`: the adaptive icon (`mipmap-anydpi-v26/ic_launcher.xml` plus
`values/ic_launcher_background.xml`) and the legacy
`mipmap-{mdpi,hdpi,xhdpi,xxhdpi,xxxhdpi}/ic_launcher.png` and
`ic_launcher_foreground.png` densities. They are generated by
`web/android/generate_icons.py`:

```bash
python3 web/android/generate_icons.py    # requires Pillow; output is committed
```

`dx` 0.7.10 ignores `[bundle] icon` on Android and hardcodes the stock Android
Studio launcher icon (DioxusLabs/dioxus#3685), so Android builds must go through
`web/android/dx.sh`. The wrapper installs a committed Gradle init script
(`web/android/gradle/favetto-icons.gradle.kts`) that overlays `web/android/res`
onto the project `dx` generates, before Gradle merges resources:

```bash
cd web
./android/dx.sh bundle --release --platform android --features mobile \
    --target aarch64-linux-android --package-types apk
```

CI bundles through the same wrapper and runs `aapt2 dump resources` on the APK to
confirm it carries `mipmap/ic_launcher_foreground`. If a future `dx` honours
`[android] icon`, the overlay can be dropped and the icon wired through config.

### Permissions

The app declares `POST_NOTIFICATIONS` (Android 13+) in `web/Dioxus.toml` under
`[permissions]`; `dx` maps it into the generated `AndroidManifest.xml`.

On web the browser `Notification` API asks for permission and shows the
notification. On Android the WebView does not implement that API, so the client
talks to the platform directly from Rust through JNI
(`web/src/android_notify.rs`): it requests `POST_NOTIFICATIONS` from the
`Activity`, posts the notification with `NotificationManager`, and reads the
tap deep link. Android reconciles the badge with the live OS state on mount, so
a permission granted or revoked in the system settings is reflected (and a
"don't ask again" denial settles the request promptly instead of polling).
Background paging to a closed app still goes through the daemon's `ntfy`
channel.

**Settings → App notifications** lets you pick which task events raise a
notification. Only `task_awaiting_input` (the default) and `task_finished`
(success/failure derived from the payload, with the run summary as the body) are
offered: `task_finished` is the single completion signal, so subscribing to it
never double-notifies alongside `task_completed` / `task_failed`. The selection
is persisted locally and restored on restart.

### Soft keyboard

The soft keyboard overlays the WebView instead of resizing it. Focusing a field
used to shrink the viewport, which pushed the fixed bottom navigation, dialogs
and toasts up (the WebView treats `position: fixed` as viewport-relative). Two
pieces of wiring keep it in place:

- the client's viewport meta sets
  `interactive-widget=overlays-content` (`web/index.html`);
- the native shell forces `SOFT_INPUT_ADJUST_NOTHING` in the custom
  `web/android/MainActivity.kt`, which `dx` picks up through the
  `android_main_activity` key in `web/Dioxus.toml`.

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
| Android keyboard shifts the bottom nav or dialogs | Update to a build that carries the `interactive-widget=overlays-content` viewport hint and the `web/android/MainActivity.kt` override; an old APK still resizes the WebView |
| Android APK is ~200 MB | It's a debug build. Pass `--release` **and** configure `[bundle.android]` signing; `dx` silently falls back to `assembleDebug` without both (see [Package a release APK](#package-a-release-apk)) |
| WASM budget exceeded | CI fails when the gzipped WASM exceeds 1 MB (warns at 500 KB); check for new heavy dependencies |
