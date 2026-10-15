//! Android system notifications through JNI.
//!
//! The Android WebView does not expose the web `Notification` API and the
//! Dioxus desktop backend does not bridge the notification permission, so this
//! module talks to the platform directly through the JNI context the runtime
//! installs (the same context [`crate::storage`] uses):
//!
//! - [`request_notification_permission`] asks the `Activity` for
//!   `POST_NOTIFICATIONS` and polls `checkSelfPermission` until the user answers
//!   (there is no result callback without custom Kotlin).
//! - [`post_notification`] creates a notification channel and posts through
//!   `NotificationManager`, with a `PendingIntent` that reopens the app and
//!   carries the task id as an intent extra.
//! - [`take_pending_deep_link`] reads and clears that extra for the router.
//!
//! Every call is best-effort: a missing context or a JNI failure is swallowed
//! (and any pending Java exception is cleared so the permanently attached thread
//! stays usable), never panicking the UI.

use std::time::Duration;

use jni::objects::{JObject, JObjectArray, JString, JValue};
use jni::JNIEnv;

use crate::notifications::{notification_id, Permission};

/// The Android 13+ runtime notification permission.
const POST_NOTIFICATIONS: &str = "android.permission.POST_NOTIFICATIONS";
/// `Activity.requestPermissions`'s request code; any stable value works.
const REQUEST_CODE: i32 = 0x293;
/// The intent extra carrying a deep-linked task id from a notification tap.
const EXTRA_DEEP_LINK: &str = "favetto.deep_link";
/// Notification channel id and user-visible name.
const CHANNEL_ID: &str = "favetto";
const CHANNEL_NAME: &str = "favetto";
/// How long to wait for the user to answer the permission dialog.
const POLL_INTERVAL_MS: u64 = 250;
const POLL_ATTEMPTS: u32 = 240; // ~60s

/// Attach the current thread (permanently) and run `f` with a `JNIEnv` and the
/// app's `Activity`. Returns `None` on a missing context or any JNI failure; a
/// pending Java exception is cleared before returning so later calls on the same
/// (permanently attached) thread are not poisoned.
///
/// `f` must only return owned data: the local references it allocates are freed
/// when the local frame is popped. Bounding them matters because the polling
/// router calls into here repeatedly on the never-detached UI thread.
fn with_activity<T>(f: impl FnOnce(&mut JNIEnv, &JObject) -> jni::errors::Result<T>) -> Option<T> {
    let ctx = ndk_context::android_context();
    // SAFETY: the runtime (`tao`/`ndk-glue`) initializes the Android context
    // before the app's `main` runs and passes the Activity as a global reference
    // valid for the process lifetime.
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
    let mut env = vm.attach_current_thread_permanently().ok()?;
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };

    let result = env.with_local_frame(16, |env| f(env, &activity));

    match result {
        Ok(value) => Some(value),
        Err(_) => {
            let _ = env.exception_clear();
            None
        }
    }
}

/// `android.os.Build.VERSION.SDK_INT` (0 when unavailable).
fn sdk_int() -> i32 {
    with_activity(|env, _activity| {
        env.get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?
            .i()
    })
    .unwrap_or(0)
}

/// Whether `POST_NOTIFICATIONS` is currently granted to the app, or `None` when
/// the JNI context is unavailable.
fn permission_granted() -> Option<bool> {
    with_activity(|env, activity| {
        let permission = env.new_string(POST_NOTIFICATIONS)?;
        let granted = env
            .call_method(
                activity,
                "checkSelfPermission",
                "(Ljava/lang/String;)I",
                &[JValue::Object(&permission)],
            )?
            .i()?;
        // android.content.pm.PackageManager.PERMISSION_GRANTED == 0.
        Ok(granted == 0)
    })
}

/// The live OS state of `POST_NOTIFICATIONS`, or `None` when the JNI context is
/// unavailable. Below Android 13 the permission does not exist, so the manifest
/// declaration is enough and the state is `Granted`.
pub fn permission_state() -> Option<Permission> {
    let sdk = sdk_int();
    if sdk == 0 {
        return None;
    }
    if sdk < 33 {
        return Some(Permission::Granted);
    }
    permission_granted().map(|granted| {
        if granted {
            Permission::Granted
        } else {
            Permission::Denied
        }
    })
}

/// True when the user has permanently denied `POST_NOTIFICATIONS`: the OS would
/// silently deny `requestPermissions` instead of showing the dialog
/// (`shouldShowRequestPermissionRationale` is false after a denial). Only
/// meaningful once the app has asked at least once; `false` on any failure so
/// the caller still attempts the request.
fn permanently_denied() -> bool {
    with_activity(|env, activity| {
        let permission = env.new_string(POST_NOTIFICATIONS)?;
        let rationale = env
            .call_method(
                activity,
                "shouldShowRequestPermissionRationale",
                "(Ljava/lang/String;)Z",
                &[JValue::Object(&permission)],
            )?
            .z()?;
        Ok(!rationale)
    })
    .unwrap_or(false)
}

/// Ask the `Activity` for `POST_NOTIFICATIONS`. Returns `false` on any failure.
fn request_via_activity() -> bool {
    with_activity(|env, activity| {
        let permission = env.new_string(POST_NOTIFICATIONS)?;
        let permissions: JObjectArray = env.new_object_array(1, "java/lang/String", &permission)?;
        env.call_method(
            activity,
            "requestPermissions",
            "([Ljava/lang/String;I)V",
            &[JValue::Object(&permissions), JValue::Int(REQUEST_CODE)],
        )?;
        Ok(())
    })
    .is_some()
}

/// Request the runtime notification permission and return the resolved state.
///
/// Runs on the Dioxus UI thread (Dioxus executes spawned tasks on the
/// VirtualDom's thread); `Activity.requestPermissions` must be called there.
///
/// `asked_before` is whether a prior request was persisted as denied. When the
/// permission is permanently denied, `requestPermissions` never shows a dialog,
/// so this settles immediately instead of polling for the full timeout.
pub async fn request_notification_permission(asked_before: bool) -> Permission {
    // The runtime permission only exists on Android 13+; on older releases the
    // manifest declaration is enough.
    if sdk_int() < 33 {
        return Permission::Granted;
    }
    // Already granted (for example from the system settings): nothing to ask.
    if permission_granted() == Some(true) {
        return Permission::Granted;
    }
    if asked_before && permanently_denied() {
        return Permission::Denied;
    }
    if !request_via_activity() {
        return Permission::Denied;
    }
    // Poll until the dialog is answered.
    for _ in 0..POLL_ATTEMPTS {
        if permission_granted() == Some(true) {
            return Permission::Granted;
        }
        tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
    Permission::Denied
}

/// Create the notification channel (idempotent; API 26+).
fn create_channel(env: &mut JNIEnv, manager: &JObject) -> jni::errors::Result<()> {
    let id = env.new_string(CHANNEL_ID)?;
    let name = env.new_string(CHANNEL_NAME)?;
    let importance = env
        .get_static_field("android/app/NotificationManager", "IMPORTANCE_DEFAULT", "I")?
        .i()?;
    let channel = env.new_object(
        "android/app/NotificationChannel",
        "(Ljava/lang/String;Ljava/lang/CharSequence;I)V",
        &[
            JValue::Object(&id),
            JValue::Object(&name),
            JValue::Int(importance),
        ],
    )?;
    env.call_method(
        manager,
        "createNotificationChannel",
        "(Landroid/app/NotificationChannel;)V",
        &[JValue::Object(&channel)],
    )?;
    Ok(())
}

/// The launch intent for this package, or a null object when none exists.
fn launch_intent<'local>(
    env: &mut JNIEnv<'local>,
    activity: &JObject,
) -> jni::errors::Result<JObject<'local>> {
    let package = env
        .call_method(activity, "getPackageName", "()Ljava/lang/String;", &[])?
        .l()?;
    let manager = env
        .call_method(
            activity,
            "getPackageManager",
            "()Landroid/content/pm/PackageManager;",
            &[],
        )?
        .l()?;
    env.call_method(
        &manager,
        "getLaunchIntentForPackage",
        "(Ljava/lang/String;)Landroid/content/Intent;",
        &[JValue::Object(&package)],
    )?
    .l()
}

/// A `PendingIntent` that reopens the app carrying `task_id`, or `None` when the
/// launch intent is unavailable.
fn pending_intent<'local>(
    env: &mut JNIEnv<'local>,
    activity: &JObject,
    intent: &JObject,
    task_id: &str,
) -> jni::errors::Result<Option<JObject<'local>>> {
    if intent.is_null() {
        return Ok(None);
    }
    let extra = env.new_string(EXTRA_DEEP_LINK)?;
    let task = env.new_string(task_id)?;
    env.call_method(
        intent,
        "putExtra",
        "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
        &[JValue::Object(&extra), JValue::Object(&task)],
    )?;
    let single_top = env
        .get_static_field("android/content/Intent", "FLAG_ACTIVITY_SINGLE_TOP", "I")?
        .i()?;
    let clear_top = env
        .get_static_field("android/content/Intent", "FLAG_ACTIVITY_CLEAR_TOP", "I")?
        .i()?;
    env.call_method(
        intent,
        "addFlags",
        "(I)Landroid/content/Intent;",
        &[JValue::Int(single_top | clear_top)],
    )?;

    let update_current = env
        .get_static_field("android/app/PendingIntent", "FLAG_UPDATE_CURRENT", "I")?
        .i()?;
    let immutable = env
        .get_static_field("android/app/PendingIntent", "FLAG_IMMUTABLE", "I")?
        .i()?;
    let pending = env
        .call_static_method(
            "android/app/PendingIntent",
            "getActivity",
            "(Landroid/content/Context;ILandroid/content/Intent;I)Landroid/app/PendingIntent;",
            &[
                JValue::Object(activity),
                JValue::Int(notification_id(task_id)),
                JValue::Object(intent),
                JValue::Int(update_current | immutable),
            ],
        )?
        .l()?;
    Ok(Some(pending))
}

/// Post a system notification; a tap opens `/inbox/{task_id}`.
pub fn post_notification(title: &str, body: &str, task_id: &str) {
    let _ = with_activity(|env, activity| {
        let service = env.new_string("notification")?;
        let manager = env
            .call_method(
                activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[JValue::Object(&service)],
            )?
            .l()?;
        if manager.is_null() {
            return Ok(());
        }

        // `NotificationChannel` and the channel-id Builder constructor exist on
        // API 26+; older releases post without a channel.
        let has_channel = sdk_int() >= 26;
        if has_channel {
            create_channel(env, &manager)?;
        }

        let intent = launch_intent(env, activity)?;
        let pending = pending_intent(env, activity, &intent, task_id)?;

        let builder = if has_channel {
            let channel = env.new_string(CHANNEL_ID)?;
            env.new_object(
                "android/app/Notification$Builder",
                "(Landroid/content/Context;Ljava/lang/String;)V",
                &[JValue::Object(activity), JValue::Object(&channel)],
            )?
        } else {
            env.new_object(
                "android/app/Notification$Builder",
                "(Landroid/content/Context;)V",
                &[JValue::Object(activity)],
            )?
        };

        let title = env.new_string(title)?;
        let body = env.new_string(body)?;
        env.call_method(
            &builder,
            "setContentTitle",
            "(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;",
            &[JValue::Object(&title)],
        )?;
        env.call_method(
            &builder,
            "setContentText",
            "(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;",
            &[JValue::Object(&body)],
        )?;
        let icon = env
            .get_static_field("android/R$drawable", "ic_dialog_info", "I")?
            .i()?;
        env.call_method(
            &builder,
            "setSmallIcon",
            "(I)Landroid/app/Notification$Builder;",
            &[JValue::Int(icon)],
        )?;
        if let Some(pending) = pending {
            env.call_method(
                &builder,
                "setContentIntent",
                "(Landroid/app/PendingIntent;)Landroid/app/Notification$Builder;",
                &[JValue::Object(&pending)],
            )?;
        }
        env.call_method(
            &builder,
            "setAutoCancel",
            "(Z)Landroid/app/Notification$Builder;",
            &[JValue::Bool(jni::sys::JNI_TRUE)],
        )?;
        let notification = env
            .call_method(&builder, "build", "()Landroid/app/Notification;", &[])?
            .l()?;

        let tag = env.new_string(task_id)?;
        env.call_method(
            &manager,
            "notify",
            "(Ljava/lang/String;ILandroid/app/Notification;)V",
            &[
                JValue::Object(&tag),
                JValue::Int(notification_id(task_id)),
                JValue::Object(&notification),
            ],
        )?;
        Ok(())
    });
}

/// Read and clear the deep-linked task id from the launch/`onNewIntent` intent.
pub fn take_pending_deep_link() -> Option<String> {
    let task_id = with_activity(|env, activity| {
        let intent = env
            .call_method(activity, "getIntent", "()Landroid/content/Intent;", &[])?
            .l()?;
        if intent.is_null() {
            return Ok(String::new());
        }
        let extra = env.new_string(EXTRA_DEEP_LINK)?;
        let value = env
            .call_method(
                &intent,
                "getStringExtra",
                "(Ljava/lang/String;)Ljava/lang/String;",
                &[JValue::Object(&extra)],
            )?
            .l()?;
        if value.is_null() {
            return Ok(String::new());
        }
        let value = JString::from(value);
        let task_id = String::from(env.get_string(&value)?);
        if task_id.is_empty() {
            return Ok(String::new());
        }
        env.call_method(
            &intent,
            "removeExtra",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&extra)],
        )?;
        Ok(task_id)
    });
    task_id.filter(|task_id| !task_id.is_empty())
}
