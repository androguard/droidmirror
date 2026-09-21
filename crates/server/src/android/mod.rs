mod capture;
mod inject_sdk;
mod net;
mod power;
mod run;
mod uinput;

use std::ffi::CString;
use std::os::unix::process::CommandExt;
use std::process::Command;

pub fn log_line(msg: &str) {
    eprintln!("droidmirror: {msg}");
    let Ok(text) = CString::new(msg.replace('\0', " ")) else {
        return;
    };
    let tag = CString::new("droidmirror").unwrap();
    unsafe {
        __android_log_write(4, tag.as_ptr(), text.as_ptr());
    }
}

unsafe extern "C" {
    fn __android_log_write(prio: i32, tag: *const libc::c_char, text: *const libc::c_char) -> i32;
}

/// Replace this process with `app_process` so the JVM (and shell uid) exist,
/// then the Java bootstrap loads `libdroidmirror_server.so`.
pub fn exec_via_app_process() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe
        .parent()
        .ok_or_else(|| "no parent dir for server binary".to_string())?;
    let dex = dir.join("droidmirror.dex");
    let so = dir.join("libdroidmirror_server.so");
    if !dex.exists() {
        return Err(format!("missing {}", dex.display()));
    }
    if !so.exists() {
        return Err(format!("missing {}", so.display()));
    }
    let app = ["/system/bin/app_process64", "/system/bin/app_process"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
        .ok_or_else(|| "app_process not found".to_string())?;
    let lib_arg = format!("--lib={}", so.display());
    let mut cmd = Command::new(app);
    cmd.env("CLASSPATH", &dex)
        .arg("/")
        .arg("com.droidmirror.Server")
        .arg(lib_arg);
    // Forward the rest of our args (bitrate, codec, …).
    for arg in std::env::args().skip(1) {
        cmd.arg(arg);
    }
    let err = cmd.exec();
    Err(format!("exec {app}: {err}"))
}

#[no_mangle]
pub extern "system" fn Java_com_droidmirror_Server_nativeMain<'local>(
    env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    args: jni::objects::JObject<'local>,
) {
    run::serve_from_java(env, args);
}
