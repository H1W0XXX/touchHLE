/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `UILocalNotification`.
//!
//! On real iOS, a scheduled `UILocalNotification` is handed to the OS, which
//! fires it (banner + sound) at its `fireDate` whether or not the app is still
//! running. touchHLE bridges this to the host platform's notification system so
//! the same thing happens on the host. See [platform_schedule] /
//! [platform_cancel_all] at the bottom of this file for the host side, and
//! `ui_application.rs` (`scheduleLocalNotification:` /
//! `cancelAllLocalNotifications`)
//! for where these get called.

use crate::frameworks::foundation::NSTimeInterval;
use crate::objc::{id, objc_classes, release, retain, ClassExports, HostObject, NSZonePtr};

#[derive(Default)]
pub(super) struct UILocalNotificationHostObject {
    /// `NSDate*` (retained)
    pub(super) fire_date: id,
    /// `NSString*` (retained)
    pub(super) alert_body: id,
    /// `NSString*` (retained)
    pub(super) alert_action: id,
    /// `NSString*` (retained)
    pub(super) sound_name: id,
    /// `NSTimeZone*` (retained)
    pub(super) time_zone: id,
}
impl HostObject for UILocalNotificationHostObject {}

pub const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

@implementation UILocalNotification: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::<UILocalNotificationHostObject>::default();
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

- (())dealloc {
    let &UILocalNotificationHostObject {
        fire_date,
        alert_body,
        alert_action,
        sound_name,
        time_zone,
    } = env.objc.borrow(this);
    release(env, fire_date);
    release(env, alert_body);
    release(env, alert_action);
    release(env, sound_name);
    release(env, time_zone);
    env.objc.dealloc_object(this, &mut env.mem);
}

- (())setFireDate:(id)date { // NSDate *
    let date = retain(env, date);
    let host = env.objc.borrow_mut::<UILocalNotificationHostObject>(this);
    let old = std::mem::replace(&mut host.fire_date, date);
    release(env, old);
}
- (id)fireDate {
    env.objc.borrow::<UILocalNotificationHostObject>(this).fire_date
}

- (())setTimeZone:(id)time_zone { // NSTimeZone *
    let time_zone = retain(env, time_zone);
    let host = env.objc.borrow_mut::<UILocalNotificationHostObject>(this);
    let old = std::mem::replace(&mut host.time_zone, time_zone);
    release(env, old);
}
- (id)timeZone {
    env.objc.borrow::<UILocalNotificationHostObject>(this).time_zone
}

- (())setAlertBody:(id)body { // NSString *
    let body = retain(env, body);
    let host = env.objc.borrow_mut::<UILocalNotificationHostObject>(this);
    let old = std::mem::replace(&mut host.alert_body, body);
    release(env, old);
}
- (id)alertBody {
    env.objc.borrow::<UILocalNotificationHostObject>(this).alert_body
}

- (())setAlertAction:(id)action { // NSString *
    let action = retain(env, action);
    let host = env.objc.borrow_mut::<UILocalNotificationHostObject>(this);
    let old = std::mem::replace(&mut host.alert_action, action);
    release(env, old);
}
- (id)alertAction {
    env.objc.borrow::<UILocalNotificationHostObject>(this).alert_action
}

- (())setSoundName:(id)name { // NSString *
    let name = retain(env, name);
    let host = env.objc.borrow_mut::<UILocalNotificationHostObject>(this);
    let old = std::mem::replace(&mut host.sound_name, name);
    release(env, old);
}
- (id)soundName {
    env.objc.borrow::<UILocalNotificationHostObject>(this).sound_name
}

@end

};

/// Schedule a host-platform notification.
///
/// `fire_unix_ms` is the absolute wall-clock time (Unix epoch, milliseconds) at
/// which the notification should appear. `notification_id` is a stable id
/// assigned by the caller so the notification can be cancelled/replaced.
pub fn platform_schedule(fire_unix_ms: i64, body: &str, notification_id: i32) {
    #[cfg(target_os = "android")]
    {
        android::schedule(fire_unix_ms, body, notification_id);
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = notification_id;
        log!(
            "Local notification requested (fires at Unix {}ms): {:?}. Host notifications are only implemented on Android; ignoring.",
            fire_unix_ms,
            body
        );
    }
}

/// Cancel every not-yet-fired notification this process has scheduled.
pub fn platform_cancel_all() {
    #[cfg(target_os = "android")]
    {
        android::cancel_all();
    }
    #[cfg(not(target_os = "android"))]
    {
        log!("cancelAllLocalNotifications: no host notifications to cancel (non-Android).");
    }
}

/// Convert seconds-since-the-Unix-epoch (as from
/// `-[NSDate timeIntervalSince1970]`)
/// to integer milliseconds, saturating on non-finite input.
pub fn unix_seconds_to_millis(secs: NSTimeInterval) -> i64 {
    let ms = secs * 1000.0;
    if ms.is_finite() {
        ms as i64
    } else {
        0
    }
}

#[cfg(target_os = "android")]
mod android {
    use std::ffi::c_void;

    extern "C" {
        /// From SDL2. Returns a `JNIEnv*` for the current (SDL main) thread.
        fn SDL_AndroidGetJNIEnv() -> *mut c_void;
    }

    /// Slash-separated name of the Java helper that owns the
    /// AlarmManager/NotificationManager plumbing.
    const HELPER_CLASS: &str = "org/touchhle/android/Notifications";

    fn with_jni_env<F: FnOnce(&mut jni::JNIEnv) -> jni::errors::Result<()>>(f: F) {
        // SAFETY: SDL guarantees this is a valid JNIEnv for the current thread.
        let raw = unsafe { SDL_AndroidGetJNIEnv() } as *mut jni::sys::JNIEnv;
        if raw.is_null() {
            log!("Couldn't get Android JNIEnv; skipping notification call.");
            return;
        }
        let mut env = match unsafe { jni::JNIEnv::from_raw(raw) } {
            Ok(env) => env,
            Err(e) => {
                log!("Couldn't wrap Android JNIEnv ({e}); skipping notification call.");
                return;
            }
        };
        if let Err(e) = f(&mut env) {
            let _ = env.exception_clear();
            log!("Android notification JNI call failed: {e}");
        }
    }

    pub fn schedule(fire_unix_ms: i64, body: &str, notification_id: i32) {
        with_jni_env(|env| {
            let body = env.new_string(body)?;
            env.call_static_method(
                HELPER_CLASS,
                "scheduleLocalNotification",
                "(JLjava/lang/String;I)V",
                &[
                    jni::objects::JValue::Long(fire_unix_ms),
                    jni::objects::JValue::Object(&body),
                    jni::objects::JValue::Int(notification_id),
                ],
            )?;
            Ok(())
        });
    }

    pub fn cancel_all() {
        with_jni_env(|env| {
            env.call_static_method(HELPER_CLASS, "cancelAllLocalNotifications", "()V", &[])?;
            Ok(())
        });
    }
}
