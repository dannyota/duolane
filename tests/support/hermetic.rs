unsafe extern "C" {
    fn atexit(callback: extern "C" fn()) -> i32;
}

static ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

extern "C" fn remove_root() {
    if let Some(root) = ROOT.get() {
        let _ = std::fs::remove_dir_all(root);
    }
}

fn fail(message: &str) -> ! {
    eprintln!("hermetic test isolation failed: {message}");
    std::process::abort()
}

extern "C" fn isolate_process_environment() {
    let dir = tempfile::Builder::new()
        .prefix("duolane-hermetic-")
        .tempdir()
        .unwrap_or_else(|err| fail(&format!("cannot create private root: {err}")));
    let root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let vars = [
        ("HOME", root.join("home")),
        ("USERPROFILE", root.join("home")),
        ("XDG_CONFIG_HOME", root.join("xdg-config")),
        ("XDG_STATE_HOME", root.join("xdg-state")),
        ("XDG_DATA_HOME", root.join("xdg-data")),
        ("APPDATA", root.join("appdata")),
        ("LOCALAPPDATA", root.join("localappdata")),
        ("DUOLANE_CONFIG_DIR", root.join("config")),
        ("DUOLANE_STATE_DIR", root.join("state")),
        ("CODEX_HOME", root.join("codex")),
    ];
    for (name, path) in vars {
        if let Err(err) = std::fs::create_dir_all(&path) {
            fail(&format!("cannot create {}: {err}", path.display()));
        }
        unsafe { std::env::set_var(name, &path) };
    }
    unsafe {
        std::env::remove_var("DUOLANE_CODEX_AUTH_FILE");
        if atexit(remove_root) != 0 {
            fail("cannot register cleanup");
        }
    }
    let _ = ROOT.set(root);
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "macos",
    target_os = "ios",
    target_os = "windows"
)))]
compile_error!(
    "hermetic test isolation supports linux, android, freebsd, macos, ios and windows only"
);

#[used]
#[cfg_attr(
    any(target_os = "linux", target_os = "android", target_os = "freebsd"),
    unsafe(link_section = ".init_array")
)]
#[cfg_attr(
    any(target_os = "macos", target_os = "ios"),
    unsafe(link_section = "__DATA,__mod_init_func")
)]
#[cfg_attr(target_os = "windows", unsafe(link_section = ".CRT$XCU"))]
static ISOLATE: extern "C" fn() = isolate_process_environment;
