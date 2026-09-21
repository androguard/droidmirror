package com.droidmirror;

/**
 * app_process entry. Loads the Rust server and hands it the command line.
 * Hidden-API access works because this process is shell uid via app_process.
 */
public final class Server {
    public static void main(String[] args) {
        String lib = "/data/local/tmp/droidmirror/libdroidmirror_server.so";
        if (args != null) {
            for (String arg : args) {
                if (arg != null && arg.startsWith("--lib=")) {
                    lib = arg.substring("--lib=".length());
                }
            }
        }
        System.load(lib);
        nativeMain(args);
    }

    private static native void nativeMain(String[] args);
}
