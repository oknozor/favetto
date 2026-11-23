# GPUI desktop client — end-to-end verification

Date: 2026-11-20
Base commit: `9d95794` (step 10 polish)
Workspace: `/code/favetto/gpui`
Toolchain: `rustc 1.96.0` / `cargo 1.96.0`

This is the step-11 verification pass. No product features were added. The only
code change was a test-lint fix required for a clean `clippy --all-targets`
run.

## Automated checks

Every command was run from `gpui/`, in this order, against the final tree.

| Command | Result |
|---|---|
| `cargo fmt --check` | Pass |
| `cargo clippy --all-targets -- -D warnings` | Pass |
| `cargo test` | Pass — **136 passed**, 0 failed, 0 ignored |
| `cargo build` | Pass (debug) |

The first clippy run found one `clippy::cloned_ref_to_slice_refs` warning in a
test at `src/detail.rs:1212`. It was fixed by passing
`std::slice::from_ref(&session)` instead of cloning into a one-element slice,
then the complete list above was re-run from the start.

The test suite covers framing/transport, SSE parsing and reconnection rules,
URL/token config, Inbox/Tasks/Catalog/detail/workflow/schedule models, status
derivation, action scoping, theme/density, and the no-window detail feed.

## Live smoke run

The client was built and run from `gpui/target/debug/favetto-gpui` against the
live daemon at `http://127.0.0.1:7878`, using the shared token at
`~/.local/share/favetto/token`. To avoid changing the user's desktop settings,
the app used temporary `XDG_CONFIG_HOME` directories; the real
`~/.config/favetto/gpui.toml` was not written. The display was X11
(`DISPLAY=:0`); `xdotool` was used only to send navigation keys and clicks, and
screenshots were captured for the checks below.

At the time of the run `repos.list` reported active repo `favetto`, while
`system.ping` reported the daemon process cwd as `/home/okno`.

### Read-only flows

| Check | Observed result |
|---|---|
| Connect | Status pill **● Connected**; status bar showed endpoint, `ping ok` and a 0 ms round trip. |
| Inbox load | Inbox showed **86 Needs you** rows (recent failures), with the sidebar badge matching. |
| List tasks | `g t` opened Tasks and loaded **500 shown of 500**; filter counts were All 500 / Running 1 / Awaiting input 0 / Failed 99 / Succeeded 397 at capture time. |
| Open a task | `Enter` (and a row click) opened the highlighted `gpui/11-verify` run. The pinned header showed status `running`, `repo favetto`, `agent opencode`, `attempt 1`, elapsed time and the live session id. |
| Switch tabs | Output → Runs both rendered. Output showed the merged live/durable feed (including tool-use rows, or its explicit "No structured output yet" state); Runs showed attempt 1 as running. |
| Catalog preview | `g c` (and the command palette) opened Catalog with **73 definitions** grouped by repo. The selected `repo:bookman/fx_pr_conflict` preview rendered its raw Markdown source. No launch sheet was submitted. |
| Settings | `Ctrl+,` opened Settings. **Test connection** (`Ctrl+T`) reported `✓ http://127.0.0.1:7878 answered in 1 ms · cwd /home/okno`. Agents listed claude/opencode/pi/vibe as available. |

This was the one safe action used: a draft `system.ping` from Settings. No
`tasks.start`, `tasks.start_oneshot`, `tasks.cancel`, `tasks.retry`,
`workflow.cancel`, `workflow.retry`, schedule or agent-reply RPC was sent. The
live task rows (including the currently running `gpui/11-verify` run) were only
selected and opened.

### Degradation checks

To avoid stopping the user's long-lived daemon for the outage test, the
stop/restart check used a disposable daemon on `127.0.0.1:7979` with its own
`--data-dir` and token. The wrong-token check used the live daemon.

| Check | Observed result |
|---|---|
| Wrong token | A temporary `gpui.toml` with `token = "definitely-wrong-token"` produced the pill **● Unauthorized**, the Inbox error panel naming the token file, **Copy error** + **Retry**, and the same Unauthorized label in the status bar. |
| Daemon stopped | After `SIGTERM` to the disposable daemon, within ~3 s the pill changed to **◌ Reconnecting (2)** and the status bar showed `Reconnecting (attempt 2, retry in 2s)`. The loaded list stayed visible. |
| Daemon restarted | Restarting the same daemon and data dir recovered automatically: within ~8 s the pill returned to **● Connected**, `ping ok`, and the Inbox loaded again. No manual Reconnect was needed. |

Screenshots were kept only in `/tmp/favetto-gpui-verify/shots/` and were not
added to the repository.

## Manual checklist (repeatable)

1. Start the daemon and put its token in `~/.local/share/favetto/token`.
2. From `gpui/`: `cargo build`, then run `./target/debug/favetto-gpui`.
3. Confirm the connection pill is **● Connected** and the status bar has a ping.
4. In Inbox: `↑`/`↓` moves the highlight; `Enter` or a row click opens the run.
5. On Tasks: `↑`/`↓`, Home/End and PageUp/PageDown move the list; `[`/`]` cycle
   the filters; `/` focuses search; `Enter` opens the run. Do not press `c`/`r`.
6. In the open detail: switch Output/Runs; the header stays pinned and Follow
   stays on unless the viewport is scrolled away from the tail. On a terminal
   run, confirm the durable log renders and the **Raw output** disclosure
   toggles to the persisted transcript.
7. On Catalog: `/` searches, `↑`/`↓` changes the definition, `h` collapses a
   folder, and the preview pane shows the selected Markdown. Do not submit the
   launch sheet.
8. In Settings: test the current connection; edit an endpoint without saving to
   see the normalized preview; `Esc` leaves the fields.
9. Wrong token: replace `token` in `gpui.toml` with a bad value, relaunch, and
   confirm **Unauthorized** plus the token path.
10. Outage: stop the daemon and confirm **Reconnecting**; start it again and
    confirm **Connected** without pressing Reconnect.

## Known limitations

These are framework or current-scope limits, not regressions from this pass:

- **No accessibility tree/AT-SPI** in the pinned `gpui 0.2.2`. The client
  guarantees labels, focus order, keyboard operation and 32/40 px targets, but a
  screen reader cannot consume widget roles from this build
  (`src/components.rs`, `src/motion.rs`; README step 10).
- **No text selection or IME marked-text composition.** Output, source, DOT and
  errors are copyable through explicit buttons; typed text is append/backspace
  only (`src/views/detail.rs`, `src/views/catalog.rs`, `src/views/settings.rs`).
- **No embedded Graphviz.** `workflow.get` is readable DOT source with copy and
  a legend (`src/views/workflows.rs`, `src/workflow.rs`).
- **No daemon version.** `system.ping` does not report one; Settings says so
  (`src/views/settings.rs`).
- **No container queries.** The responsive breakpoint reads the window width
  (`src/layout.rs`, `src/theme.rs`).
- **Workflows roots/buckets are not virtualized.** Tasks and the detail Output
  feed are; this is unchanged from step 10 (`gpui/README.md`).
- **No live permission-banner pass in this smoke run.** The live daemon had no
  `awaiting_input` run at the time (Failed 99, Awaiting input 0). The banner and
  exact `request_id` correlation remain covered by the `detail` model tests and
  the step-10 keyboard pass (`src/detail.rs`, `src/views/detail.rs`).

## Follow-up work

1. **Exercise a real permission reply when one exists.** The model path is
   tested, but a live `awaiting_input` session would confirm the banner, the
   disabled-in-flight state and the `agents.reply` round trip end to end
   (`src/state.rs::reply_to_pending`, `src/views/detail.rs::banner`).
2. **Revisit list virtualization for Workflows/Schedules** if those surfaces
   routinely exceed the 500-row snapshot; the same lazy-window approach used by
   Tasks and the detail Output feed applies (`src/views/tasks.rs`,
   `src/views/detail.rs`).
3. **Keep this check repeatable.** The commands and the disposable-daemon setup
   above assume a live local daemon; a future verification step could wrap the
   stop/restart portion in a script under `gpui/` without changing the client.
