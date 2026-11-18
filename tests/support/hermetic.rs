unsafe extern "C" {
    fn atexit(callback: extern "C" fn()) -> i32;
}

static ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

extern "C" fn remove_root() {
    if let Some(root) = ROOT.get() {
        let _ = std::fs::remove_dir_all(root);
    }
}

extern "C" fn isolate_process_environment() {
    let root = std::env::temp_dir().join(format!("duolane-hermetic-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    if std::fs::create_dir_all(&root).is_err() {
        return;
    }
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
        let _ = std::fs::create_dir_all(&path);
        unsafe { std::env::set_var(name, &path) };
    }
    unsafe {
        std::env::remove_var("DUOLANE_CODEX_AUTH_FILE");
        atexit(remove_root);
    }
    let _ = ROOT.set(root);
}

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
