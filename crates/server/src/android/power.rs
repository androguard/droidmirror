//! Keep the main display composing for the mirror session.
//!
//! Screen timeout calls PowerManager.goToSleep(), and SurfaceFlinger then stops
//! the layer stack this capture is mirroring. A screen wake lock blocks that
//! timeout whether or not the phone is charging. `stay_on_while_plugged_in`
//! covers the same case for a USB session if the wake lock is rejected.

use std::process::Command;

use jni::objects::{GlobalRef, JObject, JString, JValue};
use jni::JNIEnv;

use super::log_line;

const SCREEN_BRIGHT_WAKE_LOCK: i32 = 0x0000_000a;
const ACQUIRE_CAUSES_WAKEUP: i32 = 0x1000_0000;
const ON_AFTER_RELEASE: i32 = 0x2000_0000;
/// AC | USB | wireless. While any of these is connected, the screen timeout does not sleep.
const STAY_ON_PLUGGED: i32 = 7;

pub struct StayAwake {
    lock: Option<GlobalRef>,
    previous_stay_on: Option<i32>,
}

impl StayAwake {
    pub fn acquire(env: &mut JNIEnv) -> Self {
        let lock = match acquire_wake_lock(env) {
            Ok(lock) => {
                log_line("screen timeout suspended for this session");
                Some(lock)
            }
            Err(e) => {
                log_line(&format!("screen wake lock unavailable ({e})"));
                None
            }
        };
        let previous_stay_on = match enable_stay_on_while_plugged() {
            Ok(prev) => prev,
            Err(e) => {
                log_line(&format!("stay_on_while_plugged_in unchanged ({e})"));
                None
            }
        };
        if lock.is_none() && previous_stay_on.is_some() {
            log_line("screen timeout suspended while the phone is plugged in");
        }
        Self {
            lock,
            previous_stay_on,
        }
    }

    pub fn release(self, env: &mut JNIEnv) {
        if let Some(lock) = &self.lock {
            let held = env
                .call_method(lock.as_obj(), "isHeld", "()Z", &[])
                .and_then(|v| v.z())
                .unwrap_or(false);
            if held {
                if let Err(e) = env.call_method(lock.as_obj(), "release", "()V", &[]) {
                    clear_exception(env);
                    log_line(&format!("wake lock release: {e}"));
                }
            }
        }
        if let Some(prev) = self.previous_stay_on {
            if let Err(e) = settings_put(prev) {
                log_line(&format!("restore stay_on_while_plugged_in {prev}: {e}"));
            } else {
                log_line("restored screen timeout");
            }
        }
    }
}

fn acquire_wake_lock(env: &mut JNIEnv) -> Result<GlobalRef, String> {
    let context = system_context(env)?;
    let power = new_string(env, "power")?;
    let pm = call(
        env,
        &context,
        "getSystemService",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        &[JValue::Object(&power)],
    )?;
    let tag = new_string(env, "droidmirror:screen")?;
    let flags = SCREEN_BRIGHT_WAKE_LOCK | ACQUIRE_CAUSES_WAKEUP | ON_AFTER_RELEASE;
    let lock = call(
        env,
        &pm,
        "newWakeLock",
        "(ILjava/lang/String;)Landroid/os/PowerManager$WakeLock;",
        &[JValue::Int(flags), JValue::Object(&tag)],
    )?;
    env.call_method(&lock, "acquire", "()V", &[])
        .map_err(|e| jerr(env, "acquire", e))?;
    env.new_global_ref(lock).map_err(|e| e.to_string())
}

fn system_context<'a>(env: &mut JNIEnv<'a>) -> Result<JObject<'a>, String> {
    let cls = env
        .find_class("android/app/ActivityThread")
        .map_err(|e| jerr(env, "ActivityThread", e))?;
    let thread = match env.call_static_method(
        &cls,
        "currentActivityThread",
        "()Landroid/app/ActivityThread;",
        &[],
    ) {
        Ok(v) => {
            let obj = v.l().map_err(|e| jerr(env, "currentActivityThread", e))?;
            if obj.is_null() {
                None
            } else {
                Some(obj)
            }
        }
        Err(e) => {
            let _ = jerr(env, "currentActivityThread", e);
            None
        }
    };
    let thread = match thread {
        Some(thread) => thread,
        None => env
            .call_static_method(&cls, "systemMain", "()Landroid/app/ActivityThread;", &[])
            .and_then(|v| v.l())
            .map_err(|e| jerr(env, "systemMain", e))?,
    };
    env.call_method(
        thread,
        "getSystemContext",
        "()Landroid/app/ContextImpl;",
        &[],
    )
    .and_then(|v| v.l())
    .map_err(|e| jerr(env, "getSystemContext", e))
}

/// Returns the previous value when this call changed it.
fn enable_stay_on_while_plugged() -> Result<Option<i32>, String> {
    let raw = settings_get()?;
    let current = if raw == "null" || raw.is_empty() {
        0
    } else {
        raw.parse::<i32>()
            .map_err(|_| format!("unexpected stay_on value {raw}"))?
    };
    let next = current | STAY_ON_PLUGGED;
    if next == current {
        return Ok(None);
    }
    settings_put(next)?;
    Ok(Some(current))
}

fn settings_get() -> Result<String, String> {
    let out = Command::new("/system/bin/settings")
        .args(["get", "global", "stay_on_while_plugged_in"])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn settings_put(value: i32) -> Result<(), String> {
    let value = value.to_string();
    let out = Command::new("/system/bin/settings")
        .args(["put", "global", "stay_on_while_plugged_in", &value])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn new_string<'a>(env: &mut JNIEnv<'a>, s: &str) -> Result<JString<'a>, String> {
    env.new_string(s).map_err(|e| jerr(env, "string", e))
}

fn call<'a>(
    env: &mut JNIEnv<'a>,
    obj: &JObject,
    name: &str,
    sig: &str,
    args: &[JValue],
) -> Result<JObject<'a>, String> {
    env.call_method(obj, name, sig, args)
        .and_then(|v| v.l())
        .map_err(|e| jerr(env, name, e))
}

fn jerr(env: &mut JNIEnv, ctx: &str, e: jni::errors::Error) -> String {
    clear_exception(env);
    format!("{ctx}: {e}")
}

fn clear_exception(env: &mut JNIEnv) {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
}
