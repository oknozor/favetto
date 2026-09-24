//! Client-side sound engine for the TUI.
//!
//! Sounds are produced on the machine running `favetto tui` (never the daemon):
//! [`App`](super::app::App) turns pushes into [`SoundCue`]s, the session loop
//! drains them with [`App::take_sound_cues`](super::app::App::take_sound_cues) and
//! hands them to a [`SoundPlayer`], whose worker thread does all the spawning and
//! waiting. The render/key loop only sends on a channel, so playback never blocks
//! it.
//!
//! Default cues are synthesised as small PCM WAV files on first use (ascending
//! chime for success, descending for failure), so the binary ships no audio
//! assets. File-based players (`paplay`, `aplay`, `afplay`, …) are detected on
//! `PATH`; when there is none the engine falls back to the terminal BEL.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use favetto_core::config::SoundSettings;

/// Terminal bell, used as the always-available fallback cue.
pub const BELL: &[u8] = b"\x07";

/// A sound-worthy event, derived by [`App`](super::app::App) from pushes.
///
/// Cues are never played inline; they are queued and coalesced by the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundCue {
    /// A `task_finished` event with `success: true`.
    TaskFinished,
    /// A `task_finished` event with `success: false`.
    TaskFailed,
    /// A `task_started` event (cue defaults to `none`).
    TaskStarted,
    /// A `task_awaiting_input` event (agent blocked on the user).
    AwaitingInput,
    /// An `agent.exit` push (cue defaults to `none`).
    Attention,
}

impl SoundCue {
    /// Every cue, in the canonical precedence order used to seed the event map.
    pub const ALL: [SoundCue; 5] = [
        SoundCue::AwaitingInput,
        SoundCue::TaskFinished,
        SoundCue::TaskFailed,
        SoundCue::TaskStarted,
        SoundCue::Attention,
    ];

    /// Configuration key under `[tui.sound.events]`.
    pub fn key(self) -> &'static str {
        match self {
            SoundCue::TaskFinished => "task_finished",
            SoundCue::TaskFailed => "task_failed",
            SoundCue::TaskStarted => "task_started",
            SoundCue::AwaitingInput => "awaiting_input",
            SoundCue::Attention => "attention",
        }
    }

    /// Higher wins when cues are coalesced within the debounce window.
    pub fn priority(self) -> u8 {
        match self {
            SoundCue::AwaitingInput => 5,
            SoundCue::TaskFailed => 4,
            SoundCue::TaskFinished => 3,
            SoundCue::Attention => 2,
            SoundCue::TaskStarted => 1,
        }
    }

    /// Merge two cues, keeping the higher-priority one.
    pub fn merge(a: SoundCue, b: SoundCue) -> SoundCue {
        if b.priority() > a.priority() {
            b
        } else {
            a
        }
    }
}

/// Coalesce a batch of cues into the single one worth playing.
pub fn coalesce_batch(cues: &[SoundCue]) -> Option<SoundCue> {
    cues.iter().copied().reduce(SoundCue::merge)
}

/// A synthesised default sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinSound {
    Success,
    Failure,
    Attention,
    Started,
}

impl BuiltinSound {
    fn name(self) -> &'static str {
        match self {
            BuiltinSound::Success => "success",
            BuiltinSound::Failure => "failure",
            BuiltinSound::Attention => "attention",
            BuiltinSound::Started => "started",
        }
    }
}

/// What to play for a cue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoundSpec {
    /// One of the synthesised defaults.
    Builtin(BuiltinSound),
    /// A `.wav` file (resolved from `sound_dir` when relative).
    File(PathBuf),
    /// The terminal bell.
    Bell,
    /// Silenced.
    None,
}

/// How a resolved WAV is turned into audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerMode {
    /// Detect a player on `PATH` at playback time, else BEL.
    Auto,
    /// Write `\x07` to stdout.
    Bell,
    /// Run a command template; `{file}` is substituted with the sound path.
    Command(String),
}

/// Fully resolved sound settings (file < env < CLI).
#[derive(Debug, Clone)]
pub struct ResolvedSound {
    pub enabled: bool,
    pub player: PlayerMode,
    pub sound_dir: Option<PathBuf>,
    pub min_interval: Duration,
    pub only_when_unfocused: bool,
    pub events: BTreeMap<&'static str, SoundSpec>,
}

/// Overrides captured from `TuiArgs`, so the resolver stays pure and testable.
#[derive(Debug, Clone, Default)]
pub struct CliSound {
    pub enabled: Option<bool>,
    pub command: Option<String>,
}

/// Merge file < env < CLI (CLI wins). `env` is injected so tests need no process
/// environment.
pub fn resolve(
    file: SoundSettings,
    env: &dyn Fn(&str) -> Option<String>,
    cli: &CliSound,
) -> ResolvedSound {
    let enabled = cli
        .enabled
        .or_else(|| env("FAVETTO_SOUND").as_deref().and_then(parse_bool))
        .unwrap_or(file.enabled);

    let player = if let Some(cmd) = cli.command.as_ref().filter(|c| !c.trim().is_empty()) {
        PlayerMode::Command(cmd.clone())
    } else if let Some(cmd) = env("FAVETTO_SOUND_COMMAND").filter(|c| !c.trim().is_empty()) {
        PlayerMode::Command(cmd)
    } else {
        match file.player.trim().to_ascii_lowercase().as_str() {
            "bell" => PlayerMode::Bell,
            "command" => match file.command.as_ref().filter(|c| !c.trim().is_empty()) {
                Some(cmd) => PlayerMode::Command(cmd.clone()),
                None => {
                    tracing::warn!(
                        "tui.sound.player is \"command\" but no command is set; using auto"
                    );
                    PlayerMode::Auto
                }
            },
            _ => PlayerMode::Auto,
        }
    };

    let sound_dir = env("FAVETTO_SOUND_DIR")
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| file.sound_dir.clone())
        .map(favetto_core::paths::expand_tilde);

    ResolvedSound {
        enabled,
        player,
        sound_dir: sound_dir.clone(),
        min_interval: Duration::from_millis(file.min_interval_ms),
        only_when_unfocused: file.only_when_unfocused,
        events: resolve_events(&file.events, sound_dir.as_deref()),
    }
}

/// [`resolve`] against the real process environment.
pub fn resolve_from_env(file: SoundSettings, cli: &CliSound) -> ResolvedSound {
    resolve(file, &|key: &str| std::env::var(key).ok(), cli)
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" | "none" => Some(false),
        _ => None,
    }
}

/// Seed the built-in per-cue defaults, then apply the user's `events` map.
fn resolve_events(
    file_events: &BTreeMap<String, String>,
    sound_dir: Option<&Path>,
) -> BTreeMap<&'static str, SoundSpec> {
    let mut events: BTreeMap<&'static str, SoundSpec> = SoundCue::ALL
        .iter()
        .map(|cue| (cue.key(), default_spec(cue.key())))
        .collect();
    for (key, value) in file_events {
        let Some(slot) = events.get_mut(key.as_str()) else {
            tracing::warn!(key, "unknown tui.sound.events key; ignoring");
            continue;
        };
        match parse_spec(value, sound_dir) {
            Some(spec) => *slot = spec,
            None => {
                tracing::warn!(key, value, "unknown sound spec; using the built-in default");
            }
        }
    }
    events
}

fn default_spec(key: &str) -> SoundSpec {
    match key {
        "task_finished" => SoundSpec::Builtin(BuiltinSound::Success),
        "task_failed" => SoundSpec::Builtin(BuiltinSound::Failure),
        "awaiting_input" => SoundSpec::Builtin(BuiltinSound::Attention),
        _ => SoundSpec::None,
    }
}

/// Parse one `[tui.sound.events]` value. `None` means "unknown" (the caller keeps
/// the built-in default).
fn parse_spec(value: &str, sound_dir: Option<&Path>) -> Option<SoundSpec> {
    let value = value.trim();
    match value {
        "" | "none" => Some(SoundSpec::None),
        "success" => Some(SoundSpec::Builtin(BuiltinSound::Success)),
        "failure" => Some(SoundSpec::Builtin(BuiltinSound::Failure)),
        "attention" => Some(SoundSpec::Builtin(BuiltinSound::Attention)),
        "started" => Some(SoundSpec::Builtin(BuiltinSound::Started)),
        "bell" => Some(SoundSpec::Bell),
        other if other.to_ascii_lowercase().ends_with(".wav") => {
            let path = PathBuf::from(other);
            let path = match (path.is_absolute(), sound_dir) {
                (false, Some(dir)) => dir.join(path),
                _ => path,
            };
            Some(SoundSpec::File(path))
        }
        _ => None,
    }
}

/// Split a player command template into argv, substituting `{file}`.
fn build_command_argv(template: &str, file: &Path) -> Vec<String> {
    let file = file.to_string_lossy();
    template
        .split_ascii_whitespace()
        .map(|token| token.replace("{file}", &file))
        .collect()
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Whether `candidate` resolves to an executable on `path_var`.
fn find_on_path(candidate: &str, path_var: &str) -> bool {
    if candidate.contains(std::path::MAIN_SEPARATOR) {
        return is_executable(Path::new(candidate));
    }
    std::env::split_paths(path_var).any(|dir| is_executable(&dir.join(candidate)))
}

/// Detect a suitable player for this platform (best-effort; BEL is the fallback).
fn detect_player() -> PlayerMode {
    let path = std::env::var("PATH").unwrap_or_default();
    detect_player_in(&path)
}

#[cfg(target_os = "linux")]
fn detect_player_in(path: &str) -> PlayerMode {
    for (program, args) in [
        ("paplay", "{file}"),
        ("pw-play", "{file}"),
        ("aplay", "{file}"),
        ("ffplay", "-nodisp -autoexit {file}"),
        ("canberra-gtk-play", "-f {file}"),
    ] {
        if find_on_path(program, path) {
            return PlayerMode::Command(format!("{program} {args}"));
        }
    }
    PlayerMode::Bell
}

#[cfg(target_os = "macos")]
fn detect_player_in(path: &str) -> PlayerMode {
    if find_on_path("afplay", path) {
        PlayerMode::Command("afplay {file}".to_string())
    } else {
        PlayerMode::Bell
    }
}

#[cfg(windows)]
fn detect_player_in(path: &str) -> PlayerMode {
    if find_on_path("powershell", path) || find_on_path("pwsh", path) {
        PlayerMode::Command(
            "powershell -NoProfile -Command (New-Object Media.SoundPlayer '{file}').PlaySync()"
                .to_string(),
        )
    } else {
        PlayerMode::Bell
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn detect_player_in(_path: &str) -> PlayerMode {
    PlayerMode::Bell
}

const SAMPLE_RATE: u32 = 44_100;
const CHANNELS: u16 = 1;
const BITS_PER_SAMPLE: u16 = 16;

struct Tone {
    freq: f32,
    ms: u32,
}

fn tones(sound: BuiltinSound) -> &'static [Tone] {
    match sound {
        BuiltinSound::Success => &[
            Tone {
                freq: 523.25,
                ms: 160,
            },
            Tone {
                freq: 659.25,
                ms: 260,
            },
        ],
        BuiltinSound::Failure => &[
            Tone {
                freq: 440.0,
                ms: 200,
            },
            Tone {
                freq: 349.23,
                ms: 300,
            },
        ],
        BuiltinSound::Attention => &[Tone {
            freq: 880.0,
            ms: 180,
        }],
        BuiltinSound::Started => &[Tone {
            freq: 660.0,
            ms: 90,
        }],
    }
}

/// Synthesise a deterministic 44.1 kHz mono 16-bit PCM WAV for a built-in cue.
pub fn synth_wav(sound: BuiltinSound) -> Vec<u8> {
    let mut samples: Vec<i16> = Vec::new();
    for tone in tones(sound) {
        let count = (SAMPLE_RATE as f32 * tone.ms as f32 / 1000.0).round() as u32;
        let duration = tone.ms as f32 / 1000.0;
        for i in 0..count {
            let t = i as f32 / SAMPLE_RATE as f32;
            let sample = (t * tone.freq * std::f32::consts::TAU).sin()
                * envelope(t, duration)
                * (1.0 - 0.25 * t / duration)
                * 0.6;
            samples.push((sample * i16::MAX as f32) as i16);
        }
    }
    wav_bytes(&samples)
}

/// Short linear attack and release, keeping tone boundaries click-free.
fn envelope(t: f32, duration: f32) -> f32 {
    let attack = 0.008_f32.min(duration * 0.5);
    let release = 0.03_f32.min(duration * 0.5);
    if t < attack {
        t / attack
    } else if t > duration - release {
        ((duration - t) / release).max(0.0)
    } else {
        1.0
    }
}

fn wav_bytes(samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let block_align = CHANNELS * BITS_PER_SAMPLE / 8;
    let byte_rate = SAMPLE_RATE * block_align as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&CHANNELS.to_le_bytes());
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&BITS_PER_SAMPLE.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// Whether `bytes` starts with a RIFF/WAVE header.
pub fn is_riff_wave(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE"
}

/// Materialises synthesised WAVs once per process under the temp dir.
struct WavCache {
    dir: PathBuf,
}

impl WavCache {
    fn new() -> Self {
        Self {
            dir: std::env::temp_dir().join(format!("favetto-sounds-{}", std::process::id())),
        }
    }

    fn path(&mut self, sound: BuiltinSound) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("{}.wav", sound.name()));
        if !path.exists() {
            let bytes = synth_wav(sound);
            debug_assert!(is_riff_wave(&bytes));
            std::fs::write(&path, bytes)?;
        }
        Ok(path)
    }
}

impl Drop for WavCache {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Write the terminal bell to stdout.
fn write_bell() -> std::io::Result<()> {
    let mut out = std::io::stdout();
    out.write_all(BELL)?;
    out.flush()
}

/// Spawn a player process for `file`, detaching a reaper thread. Output streams
/// are null so a player can never scribble over the alt-screen.
fn spawn_player(player: &PlayerMode, file: &Path) -> anyhow::Result<()> {
    match player {
        PlayerMode::Bell | PlayerMode::Auto => {
            write_bell()?;
        }
        PlayerMode::Command(template) => {
            let argv = build_command_argv(template, file);
            let Some((program, args)) = argv.split_first() else {
                anyhow::bail!("empty sound command");
            };
            let mut child = Command::new(program)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
    Ok(())
}

fn play_spec(spec: &SoundSpec, player: &PlayerMode, cache: &mut WavCache) -> anyhow::Result<()> {
    match spec {
        SoundSpec::None => Ok(()),
        SoundSpec::Bell => {
            write_bell()?;
            Ok(())
        }
        SoundSpec::File(path) => {
            if !path.exists() {
                tracing::warn!(path = %path.display(), "sound file missing; using bell");
                write_bell()?;
                return Ok(());
            }
            spawn_player(player, path)
        }
        SoundSpec::Builtin(builtin) => {
            let path = cache.path(*builtin)?;
            spawn_player(player, &path)
        }
    }
}

fn player_label(mode: &PlayerMode) -> String {
    match mode {
        PlayerMode::Auto => "auto".to_string(),
        PlayerMode::Bell => "bell".to_string(),
        PlayerMode::Command(cmd) => format!("command: {cmd}"),
    }
}

fn should_play(muted: &AtomicBool, focused: &AtomicBool, only_when_unfocused: bool) -> bool {
    if muted.load(Ordering::Relaxed) {
        return false;
    }
    !(only_when_unfocused && focused.load(Ordering::Relaxed))
}

/// The background cue player. Cheap to clone-free: hand out `&SoundPlayer`.
pub struct SoundPlayer {
    tx: Option<Sender<SoundCue>>,
    muted: Arc<AtomicBool>,
    focused: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl SoundPlayer {
    /// Spawn the worker. Returns a no-op player when sound is disabled or every
    /// cue resolves to `none`.
    pub fn start(cfg: ResolvedSound) -> Self {
        let muted = Arc::new(AtomicBool::new(false));
        let focused = Arc::new(AtomicBool::new(false));
        let playable = cfg.enabled
            && cfg
                .events
                .values()
                .any(|spec| !matches!(spec, SoundSpec::None));
        if !playable {
            return Self {
                tx: None,
                muted,
                focused,
                worker: None,
            };
        }

        let player = match &cfg.player {
            PlayerMode::Auto => detect_player(),
            other => other.clone(),
        };
        let min_interval = cfg.min_interval;
        let only_when_unfocused = cfg.only_when_unfocused;
        let events = cfg.events;

        let (tx, rx) = mpsc::channel::<SoundCue>();
        let worker_muted = muted.clone();
        let worker_focused = focused.clone();
        let worker = std::thread::spawn(move || {
            let mut cache = WavCache::new();
            let mut last_play: Option<Instant> = None;
            while let Ok(first) = rx.recv() {
                if !should_play(&worker_muted, &worker_focused, only_when_unfocused) {
                    continue;
                }
                let mut batch = vec![first];
                while let Ok(cue) = rx.try_recv() {
                    if should_play(&worker_muted, &worker_focused, only_when_unfocused) {
                        batch.push(cue);
                    }
                }
                // Trailing debounce: wait out the rest of the window, coalescing
                // anything that arrives meanwhile. The highest-priority cue wins
                // (awaiting-input outranks failure).
                if let Some(last) = last_play {
                    let mut remaining = min_interval.saturating_sub(last.elapsed());
                    while !remaining.is_zero() {
                        match rx.recv_timeout(remaining) {
                            Ok(cue) => {
                                if should_play(&worker_muted, &worker_focused, only_when_unfocused)
                                {
                                    batch.push(cue);
                                }
                                remaining = min_interval.saturating_sub(last.elapsed());
                            }
                            Err(RecvTimeoutError::Timeout) => break,
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                    }
                }
                if let Some(cue) = coalesce_batch(&batch) {
                    let spec = events.get(cue.key()).cloned().unwrap_or(SoundSpec::None);
                    if let Err(e) = play_spec(&spec, &player, &mut cache) {
                        tracing::warn!(error = %e, "failed to play sound; using bell");
                        let _ = write_bell();
                    }
                    last_play = Some(Instant::now());
                }
            }
        });

        Self {
            tx: Some(tx),
            muted,
            focused,
            worker: Some(worker),
        }
    }

    /// Fire-and-forget: enqueue a cue. Sends on an unbounded channel, never blocks.
    pub fn play(&self, cue: SoundCue) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cue);
        }
    }

    /// Mute or unmute playback.
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    /// Report terminal focus (for `only_when_unfocused`).
    pub fn set_focused(&self, focused: bool) {
        self.focused.store(focused, Ordering::Relaxed);
    }
}

impl Drop for SoundPlayer {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Render the `--test-sound` report: the master switch, the resolved player, and
/// the per-cue mapping.
///
/// This report is the command's stdout contract (documented in
/// `docs/guide/tui.md`), so [`test`] writes it to stdout unchanged and it stays
/// scriptable. Diagnostics about the resolved `sound_dir` and about a
/// sound-enabled configuration with no playable cues go through `tracing`
/// instead.
fn write_report<W: Write>(
    cfg: &ResolvedSound,
    player: &PlayerMode,
    out: &mut W,
) -> std::io::Result<()> {
    writeln!(
        out,
        "sound: {}",
        if cfg.enabled { "enabled" } else { "disabled" }
    )?;
    writeln!(out, "player: {}", player_label(player))?;
    if cfg.enabled {
        for (key, spec) in &cfg.events {
            let label = match spec {
                SoundSpec::None => "none".to_string(),
                other => format!("{other:?}"),
            };
            writeln!(out, "  {key} -> {label}")?;
        }
    }
    Ok(())
}

/// Play every configured cue once and report the resolved player. Used by
/// `favetto tui --test-sound`, before the terminal is initialised.
///
/// The report goes to stdout; only diagnostics use `tracing`, so the terminal is
/// never written to from inside the TUI loop.
pub fn test(cfg: &ResolvedSound) -> anyhow::Result<()> {
    let player = match &cfg.player {
        PlayerMode::Auto => detect_player(),
        other => other.clone(),
    };
    if let Some(dir) = &cfg.sound_dir {
        tracing::debug!(sound_dir = %dir.display(), "resolved sound directory");
    }
    write_report(cfg, &player, &mut std::io::stdout())?;
    if !cfg.enabled {
        return Ok(());
    }

    let mut cache = WavCache::new();
    let mut played = 0;
    for spec in cfg.events.values() {
        if !matches!(spec, SoundSpec::None) {
            play_spec(spec, &player, &mut cache)?;
            played += 1;
        }
    }
    if played == 0 {
        tracing::warn!("sound is enabled but no cues are configured to play");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn synth_wav_is_valid_and_deterministic() {
        let mut total = 0;
        for sound in [
            BuiltinSound::Success,
            BuiltinSound::Failure,
            BuiltinSound::Attention,
            BuiltinSound::Started,
        ] {
            let first = synth_wav(sound);
            let second = synth_wav(sound);
            assert_eq!(first, second, "{sound:?} is not deterministic");
            assert!(is_riff_wave(&first));
            assert_eq!(&first[12..16], b"fmt ");
            assert_eq!(u16::from_le_bytes([first[20], first[21]]), 1); // PCM
            assert_eq!(
                u32::from_le_bytes([first[24], first[25], first[26], first[27]]),
                44_100
            );
            assert_eq!(&first[36..40], b"data");
            let data_len =
                u32::from_le_bytes([first[40], first[41], first[42], first[43]]) as usize;
            assert!(data_len > 0);
            assert_eq!(first.len(), 44 + data_len);
            total += first.len();
        }
        assert!(total < 200 * 1024, "synthesised sounds too large: {total}");
    }

    #[test]
    fn resolve_enabled_precedence() {
        let file = SoundSettings::default();
        assert!(resolve(file.clone(), &no_env, &CliSound::default()).enabled);

        let env_off = |key: &str| (key == "FAVETTO_SOUND").then(|| "off".to_string());
        assert!(!resolve(file.clone(), &env_off, &CliSound::default()).enabled);

        // `--sound` beats an env override.
        let cli_on = CliSound {
            enabled: Some(true),
            command: None,
        };
        assert!(resolve(file.clone(), &env_off, &cli_on).enabled);

        // `--no-sound` beats the file.
        let cli_off = CliSound {
            enabled: Some(false),
            command: None,
        };
        assert!(!resolve(file, &no_env, &cli_off).enabled);
    }

    #[test]
    fn resolve_player_precedence() {
        let bell = SoundSettings {
            player: "bell".to_string(),
            ..SoundSettings::default()
        };
        assert_eq!(
            resolve(bell.clone(), &no_env, &CliSound::default()).player,
            PlayerMode::Bell
        );

        let cli = CliSound {
            enabled: None,
            command: Some("paplay {file}".to_string()),
        };
        assert_eq!(
            resolve(bell.clone(), &no_env, &cli).player,
            PlayerMode::Command("paplay {file}".to_string())
        );

        let env = |key: &str| (key == "FAVETTO_SOUND_COMMAND").then(|| "aplay {file}".to_string());
        assert_eq!(
            resolve(bell, &env, &CliSound::default()).player,
            PlayerMode::Command("aplay {file}".to_string())
        );

        let command = SoundSettings {
            player: "command".to_string(),
            command: Some("my-player {file}".to_string()),
            ..SoundSettings::default()
        };
        assert_eq!(
            resolve(command, &no_env, &CliSound::default()).player,
            PlayerMode::Command("my-player {file}".to_string())
        );
    }

    #[test]
    fn resolve_event_specs() {
        let mut events = BTreeMap::new();
        events.insert("task_finished".to_string(), "bell".to_string());
        events.insert("task_started".to_string(), "attention".to_string());
        events.insert("task_failed".to_string(), "bloop.wav".to_string());
        events.insert("attention".to_string(), "none".to_string());
        let file = SoundSettings {
            sound_dir: Some(PathBuf::from("/sounds")),
            events,
            ..SoundSettings::default()
        };

        let resolved = resolve(file, &no_env, &CliSound::default());
        assert_eq!(resolved.events["task_finished"], SoundSpec::Bell);
        assert_eq!(
            resolved.events["task_started"],
            SoundSpec::Builtin(BuiltinSound::Attention)
        );
        assert_eq!(
            resolved.events["task_failed"],
            SoundSpec::File(PathBuf::from("/sounds/bloop.wav"))
        );
        assert_eq!(resolved.events["attention"], SoundSpec::None);
    }

    #[test]
    fn env_sound_dir_beats_file() {
        let file = SoundSettings {
            sound_dir: Some(PathBuf::from("/file")),
            ..SoundSettings::default()
        };
        let env = |key: &str| (key == "FAVETTO_SOUND_DIR").then(|| "/env".to_string());
        assert_eq!(
            resolve(file, &env, &CliSound::default()).sound_dir,
            Some(PathBuf::from("/env"))
        );
    }

    #[test]
    fn unknown_spec_keeps_builtin_default() {
        let mut events = BTreeMap::new();
        events.insert("task_finished".to_string(), "bloop".to_string());
        let file = SoundSettings {
            events,
            ..SoundSettings::default()
        };
        let resolved = resolve(file, &no_env, &CliSound::default());
        assert_eq!(
            resolved.events["task_finished"],
            SoundSpec::Builtin(BuiltinSound::Success)
        );
    }

    #[test]
    fn test_sound_report_lists_state_player_and_cues() {
        let cfg = resolve(SoundSettings::default(), &no_env, &CliSound::default());
        let mut out = Vec::new();
        write_report(&cfg, &PlayerMode::Bell, &mut out).unwrap();
        let report = String::from_utf8(out).unwrap();

        assert!(report.starts_with("sound: enabled\n"), "{report}");
        assert!(report.contains("player: bell\n"), "{report}");
        assert!(
            report.contains("  task_finished -> Builtin(Success)\n"),
            "{report}"
        );
        assert!(report.contains("  task_started -> none\n"), "{report}");
    }

    #[test]
    fn test_sound_report_omits_cues_when_disabled() {
        let mut cfg = resolve(SoundSettings::default(), &no_env, &CliSound::default());
        cfg.enabled = false;
        let mut out = Vec::new();
        write_report(&cfg, &PlayerMode::Bell, &mut out).unwrap();

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "sound: disabled\nplayer: bell\n"
        );
    }

    #[test]
    fn missing_wav_falls_back_to_bell() {
        let mut cache = WavCache::new();
        let spec = SoundSpec::File(PathBuf::from("/does/not/exist.wav"));
        assert!(play_spec(&spec, &PlayerMode::Bell, &mut cache).is_ok());
    }

    #[test]
    fn build_command_argv_substitutes_file() {
        assert_eq!(
            build_command_argv("paplay --volume=1 {file}", Path::new("/tmp/a.wav")),
            vec!["paplay", "--volume=1", "/tmp/a.wav"]
        );
        assert_eq!(
            build_command_argv("aplay", Path::new("/tmp/a.wav")),
            vec!["aplay"]
        );
        assert!(build_command_argv("   ", Path::new("/tmp/a.wav")).is_empty());
    }

    #[test]
    fn find_on_path_requires_executable() {
        let dir = std::env::temp_dir().join(format!("favetto-sound-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.to_string_lossy().to_string();

        let exe = dir.join("fake-player");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(find_on_path("fake-player", &path));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let noexec = dir.join("noexec-player");
            std::fs::write(&noexec, b"").unwrap();
            std::fs::set_permissions(&noexec, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(!find_on_path("noexec-player", &path));
        }

        assert!(!find_on_path("missing-player", &path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bell_constant_and_disabled_player_is_noop() {
        assert_eq!(BELL, b"\x07");

        let mut cfg = resolve(SoundSettings::default(), &no_env, &CliSound::default());
        cfg.enabled = false;
        let player = SoundPlayer::start(cfg);
        player.play(SoundCue::TaskFinished);
        assert!(!player.muted());
        player.set_muted(true);
        assert!(player.muted());
    }

    #[test]
    fn coalesce_prefers_failure() {
        assert_eq!(
            SoundCue::merge(SoundCue::TaskFailed, SoundCue::TaskFinished),
            SoundCue::TaskFailed
        );
        assert_eq!(
            SoundCue::merge(SoundCue::TaskFinished, SoundCue::TaskFailed),
            SoundCue::TaskFailed
        );
        assert_eq!(
            coalesce_batch(&[
                SoundCue::TaskStarted,
                SoundCue::TaskFinished,
                SoundCue::TaskFailed,
            ]),
            Some(SoundCue::TaskFailed)
        );
        assert_eq!(coalesce_batch(&[]), None);
    }

    #[test]
    fn wav_cache_materialises_once() {
        let mut cache = WavCache::new();
        let first = cache.path(BuiltinSound::Success).unwrap();
        let second = cache.path(BuiltinSound::Success).unwrap();
        assert_eq!(first, second);
        assert!(first.exists());
    }

    #[test]
    fn awaiting_input_cue_defaults_on() {
        let resolved = resolve(SoundSettings::default(), &no_env, &CliSound::default());
        assert_eq!(
            resolved.events["awaiting_input"],
            SoundSpec::Builtin(BuiltinSound::Attention)
        );
        assert_eq!(SoundCue::AwaitingInput.key(), "awaiting_input");
        assert!(SoundCue::AwaitingInput.priority() > SoundCue::TaskFailed.priority());
        assert_eq!(SoundCue::ALL.len(), 5);
    }

    #[test]
    fn coalesce_prefers_awaiting_input_over_failure() {
        assert_eq!(
            SoundCue::merge(SoundCue::TaskFailed, SoundCue::AwaitingInput),
            SoundCue::AwaitingInput
        );
        assert_eq!(
            coalesce_batch(&[
                SoundCue::TaskStarted,
                SoundCue::TaskFailed,
                SoundCue::AwaitingInput,
            ]),
            Some(SoundCue::AwaitingInput)
        );
    }
}
