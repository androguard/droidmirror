fn main() {
    #[cfg(target_os = "android")]
    {
        if let Err(e) = droidmirror_server::exec_via_app_process() {
            eprintln!("droidmirror-server: {e}");
            std::process::exit(1);
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        eprintln!(
            "droidmirror-server is the on-device binary. Cross-compile with scripts/build-server-android.sh"
        );
        std::process::exit(1);
    }
}
