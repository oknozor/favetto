//! Native key/value persistence (non-wasm targets).
//!
//! The browser build keeps the bearer token, daemon URL and SSE cursor in
//! `localStorage`. Native builds have no `localStorage`, so they persist the same
//! keys as small files:
//!
//! - **Android**: the app's private files directory, resolved through the JNI
//!   context the runtime installs (`getFilesDir()`), so state survives restarts.
//! - **Desktop/host**: `$FAVETTO_WEB_STATE_DIR`, else the user's local data dir.
//!
//! Every operation is best-effort: an unresolvable or read-only directory simply
//! disables persistence rather than failing the app.

use std::path::PathBuf;

/// The directory state files live in, or `None` when none is available.
fn state_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("FAVETTO_WEB_STATE_DIR") {
        return Some(PathBuf::from(dir));
    }

    // Unit tests must never touch the developer's real state directory.
    if cfg!(test) {
        return None;
    }

    #[cfg(target_os = "android")]
    if let Some(dir) = android_files_dir() {
        return Some(dir.join("favetto-web"));
    }

    dirs::data_local_dir()
        .or_else(dirs::home_dir)
        .map(|dir| dir.join("favetto-web"))
}

/// The app's private files directory via the JNI context (`getFilesDir()`).
#[cfg(target_os = "android")]
fn android_files_dir() -> Option<PathBuf> {
    use jni::objects::{JObject, JString};

    // SAFETY: the runtime (`android-activity`/`ndk-glue`) initializes the Android
    // context before the app's `main` runs, and the pointers are valid for the
    // process lifetime.
    let ctx = ndk_context::android_context();
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };

    let files = env
        .call_method(&activity, "getFilesDir", "()Ljava/io/File;", &[])
        .ok()?
        .l()
        .ok()?;
    let path = env
        .call_method(&files, "getAbsolutePath", "()Ljava/lang/String;", &[])
        .ok()?
        .l()
        .ok()?;
    let path = JString::from(path);
    let path = env.get_string(&path).ok()?;
    Some(PathBuf::from(String::from(path)))
}

/// The file backing `key`, with the key sanitized to a safe file name.
fn path_for(key: &str) -> Option<PathBuf> {
    let dir = state_dir()?;
    let safe: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(dir.join(safe))
}

/// Read a persisted string; `None` when unset or unreadable.
pub fn load_string(key: &str) -> Option<String> {
    let path = path_for(key)?;
    std::fs::read_to_string(path)
        .ok()
        .filter(|value| !value.is_empty())
}

/// Persist a string, ignoring any I/O failure.
pub fn save_string(key: &str, value: &str) {
    let Some(path) = path_for(key) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, value);
}

/// Remove a persisted key, ignoring any I/O failure.
pub fn remove_key(key: &str) {
    if let Some(path) = path_for(key) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_the_state_dir_override() {
        let dir = std::env::temp_dir().join(format!("favetto-web-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("FAVETTO_WEB_STATE_DIR", &dir);

        assert_eq!(load_string("favetto.token"), None);
        save_string("favetto.token", "secret");
        assert_eq!(load_string("favetto.token").as_deref(), Some("secret"));
        // The key is sanitized into a single flat file.
        assert!(dir.join("favetto.token").exists());
        remove_key("favetto.token");
        assert_eq!(load_string("favetto.token"), None);

        std::env::remove_var("FAVETTO_WEB_STATE_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
