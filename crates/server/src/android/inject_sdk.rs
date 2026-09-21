//! Touch and keys via `InputManager.injectInputEvent`.
//!
//! A `/dev/uinput` node is not the display. Android drops those events unless
//! the device is the built-in touchscreen. Shell `app_process` can inject into
//! the real display the same way `adb shell input` does.

use jni::objects::{GlobalRef, JObject, JValue};
use jni::{JNIEnv, JavaVM};

use crate::control::Inject;

use super::log_line;

const ACTION_DOWN: u8 = 0;
const ACTION_UP: u8 = 1;
const ACTION_CANCEL: u8 = 3;
const SOURCE_TOUCHSCREEN: i32 = 0x1002;
const SOURCE_KEYBOARD: i32 = 0x101;
const INJECT_ASYNC: i32 = 0;

pub struct SdkInject {
    vm: JavaVM,
    manager: GlobalRef,
    capture_w: i32,
    capture_h: i32,
    display_w: i32,
    display_h: i32,
    down_time: i64,
    logged_fail: bool,
}

impl SdkInject {
    pub fn open(
        env: &mut JNIEnv,
        capture_w: i32,
        capture_h: i32,
        display_w: i32,
        display_h: i32,
    ) -> Result<Self, String> {
        let manager = input_manager(env)?;
        let manager = env
            .new_global_ref(&manager)
            .map_err(|e| format!("input manager ref: {e}"))?;
        let vm = env.get_java_vm().map_err(|e| e.to_string())?;
        Ok(Self {
            vm,
            manager,
            capture_w: capture_w.max(1),
            capture_h: capture_h.max(1),
            display_w: display_w.max(1),
            display_h: display_h.max(1),
            down_time: 0,
            logged_fail: false,
        })
    }

    fn note(&mut self, err: String) {
        if !self.logged_fail {
            self.logged_fail = true;
            log_line(&format!("inject: {err}"));
        }
    }

    fn to_display(&self, x: i32, y: i32) -> (f32, f32) {
        let sx = self.display_w as f32 / self.capture_w as f32;
        let sy = self.display_h as f32 / self.capture_h as f32;
        ((x as f32) * sx, (y as f32) * sy)
    }
}

impl Inject for SdkInject {
    fn touch(&mut self, action: u8, _id: u8, x: i32, y: i32, pressure: u16) {
        let (x, y) = self.to_display(x, y);
        let pressure = if action == ACTION_UP || action == ACTION_CANCEL {
            0.0
        } else {
            (pressure as f32 / 65535.0).clamp(0.05, 1.0)
        };
        let vm = &self.vm;
        let now = match with_vm(vm, uptime_millis) {
            Ok(t) => t,
            Err(e) => {
                self.note(e);
                return;
            }
        };
        if action == ACTION_DOWN || self.down_time == 0 {
            self.down_time = now;
        }
        let down = self.down_time;
        if action == ACTION_UP || action == ACTION_CANCEL {
            self.down_time = 0;
        }
        let vm = &self.vm;
        let manager = &self.manager;
        let err = with_vm(vm, |env| {
            inject_motion(env, manager, down, now, action as i32, x, y, pressure)
        });
        if let Err(e) = err {
            self.note(e);
        }
    }

    fn key(&mut self, action: u8, keycode: u32, _meta: u32) {
        let vm = &self.vm;
        let manager = &self.manager;
        let err = with_vm(vm, |env| inject_key(env, manager, action as i32, keycode as i32));
        if let Err(e) = err {
            self.note(e);
        }
    }

    fn text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        let _ = std::process::Command::new("input").args(["text", s]).status();
    }

    fn scroll(&mut self, x: i32, y: i32, _h: i32, v: i32) {
        if v == 0 {
            return;
        }
        let (x, y) = self.to_display(x, y);
        let y2 = y - (v.signum() as f32) * 120.0;
        let _ = std::process::Command::new("input")
            .args([
                "swipe",
                &format!("{x:.0}"),
                &format!("{y:.0}"),
                &format!("{x:.0}"),
                &format!("{y2:.0}"),
                "40",
            ])
            .status();
    }
}

fn with_vm<T>(vm: &JavaVM, f: impl FnOnce(&mut JNIEnv) -> Result<T, String>) -> Result<T, String> {
    if let Ok(mut env) = vm.get_env() {
        let result = f(&mut env);
        clear_exception(&mut env);
        return result;
    }
    let mut guard = vm
        .attach_current_thread()
        .map_err(|e| format!("attach: {e}"))?;
    let result = f(&mut guard);
    clear_exception(&mut guard);
    result
}

fn input_manager<'a>(env: &mut JNIEnv<'a>) -> Result<JObject<'a>, String> {
    clear_exception(env);
    match service_manager(env) {
        Ok(obj) if !obj.is_null() => return Ok(obj),
        Ok(_) => {}
        Err(e) => log_line(&format!("ServiceManager input: {e}")),
    }
    clear_exception(env);
    match static_instance(env, "android/hardware/input/InputManager") {
        Ok(obj) if !obj.is_null() => return Ok(obj),
        Ok(_) => {}
        Err(e) => log_line(&format!("InputManager.getInstance: {e}")),
    }
    clear_exception(env);
    static_instance(env, "android/hardware/input/InputManagerGlobal")
}

fn service_manager<'a>(env: &mut JNIEnv<'a>) -> Result<JObject<'a>, String> {
    let sm = find(env, "android/os/ServiceManager")?;
    let name = env
        .new_string("input")
        .map_err(|e| jerr(env, "input", e))?;
    let binder = env
        .call_static_method(
            sm,
            "getService",
            "(Ljava/lang/String;)Landroid/os/IBinder;",
            &[JValue::Object(&name)],
        )
        .and_then(|v| v.l())
        .map_err(|e| jerr(env, "getService", e))?;
    if binder.is_null() {
        return Err("input service missing".into());
    }
    let stub = find(env, "android/hardware/input/IInputManager$Stub")?;
    let manager = env
        .call_static_method(
            stub,
            "asInterface",
            "(Landroid/os/IBinder;)Landroid/hardware/input/IInputManager;",
            &[JValue::Object(&binder)],
        )
        .and_then(|v| v.l())
        .map_err(|e| jerr(env, "asInterface", e))?;
    if manager.is_null() {
        return Err("IInputManager null".into());
    }
    Ok(manager)
}

fn static_instance<'a>(env: &mut JNIEnv<'a>, class_name: &str) -> Result<JObject<'a>, String> {
    let cls = find(env, class_name)?;
    let sig = format!("()L{class_name};");
    env.call_static_method(&cls, "getInstance", &sig, &[])
        .and_then(|v| v.l())
        .map_err(|e| jerr(env, "getInstance", e))
}

fn uptime_millis(env: &mut JNIEnv) -> Result<i64, String> {
    let cls = find(env, "android/os/SystemClock")?;
    env.call_static_method(cls, "uptimeMillis", "()J", &[])
        .and_then(|v| v.j())
        .map_err(|e| jerr(env, "uptimeMillis", e))
}

fn inject_motion(
    env: &mut JNIEnv,
    manager: &GlobalRef,
    down: i64,
    now: i64,
    action: i32,
    x: f32,
    y: f32,
    pressure: f32,
) -> Result<(), String> {
    let cls = find(env, "android/view/MotionEvent")?;
    // obtain(downTime, eventTime, action, x, y, pressure, size, meta, xPrec, yPrec, deviceId, edgeFlags)
    let event = env
        .call_static_method(
            cls,
            "obtain",
            "(JJIFFFFIFFII)Landroid/view/MotionEvent;",
            &[
                JValue::Long(down),
                JValue::Long(now),
                JValue::Int(action),
                JValue::Float(x),
                JValue::Float(y),
                JValue::Float(pressure),
                JValue::Float(1.0),
                JValue::Int(0),
                JValue::Float(1.0),
                JValue::Float(1.0),
                JValue::Int(-1),
                JValue::Int(0),
            ],
        )
        .and_then(|v| v.l())
        .map_err(|e| jerr(env, "MotionEvent.obtain", e))?;
    let _ = env.call_method(
        &event,
        "setSource",
        "(I)V",
        &[JValue::Int(SOURCE_TOUCHSCREEN)],
    );
    clear_exception(env);
    let _ = env.call_method(&event, "setDisplayId", "(I)V", &[JValue::Int(0)]);
    clear_exception(env);
    inject_event(env, manager, &event)?;
    let _ = env.call_method(&event, "recycle", "()V", &[]);
    clear_exception(env);
    Ok(())
}

fn inject_key(
    env: &mut JNIEnv,
    manager: &GlobalRef,
    action: i32,
    keycode: i32,
) -> Result<(), String> {
    let event = env
        .new_object(
            "android/view/KeyEvent",
            "(II)V",
            &[JValue::Int(action), JValue::Int(keycode)],
        )
        .map_err(|e| jerr(env, "KeyEvent", e))?;
    let _ = env.call_method(&event, "setSource", "(I)V", &[JValue::Int(SOURCE_KEYBOARD)]);
    clear_exception(env);
    inject_event(env, manager, &event)
}

fn inject_event(env: &mut JNIEnv, manager: &GlobalRef, event: &JObject) -> Result<(), String> {
    match env.call_method(
        manager,
        "injectInputEvent",
        "(Landroid/view/InputEvent;I)Z",
        &[JValue::Object(event), JValue::Int(INJECT_ASYNC)],
    ) {
        Ok(v) => {
            if v.z().unwrap_or(false) {
                Ok(())
            } else {
                Err("injectInputEvent returned false".into())
            }
        }
        Err(e) => {
            clear_exception(env);
            env.call_method(
                manager,
                "injectInputEvent",
                "(Landroid/view/InputEvent;II)Z",
                &[
                    JValue::Object(event),
                    JValue::Int(INJECT_ASYNC),
                    JValue::Int(-1),
                ],
            )
            .map_err(|e2| jerr(env, "injectInputEvent", e2))
            .and_then(|v| {
                if v.z().unwrap_or(false) {
                    Ok(())
                } else {
                    Err(format!("injectInputEvent returned false ({e})"))
                }
            })
        }
    }
}

fn find<'a>(env: &mut JNIEnv<'a>, name: &str) -> Result<jni::objects::JClass<'a>, String> {
    env.find_class(name).map_err(|e| jerr(env, name, e))
}

fn jerr(env: &mut JNIEnv, ctx: &str, e: jni::errors::Error) -> String {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    format!("{ctx}: {e}")
}

fn clear_exception(env: &mut JNIEnv) {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
}
