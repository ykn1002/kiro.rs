fn main() {
    // 为 app-local 自定义命令生成 ACL 权限（allow-$command / deny-$command）。
    // Tauri 2.11+ 起远程 origin（窗口加载 http://127.0.0.1）的自定义命令也受 ACL 约束，
    // 必须在此声明命令，capability 才能引用 allow-get-desktop-settings 等权限。
    let app_manifest = tauri_build::AppManifest::new().commands(&[
        "get_desktop_settings",
        "set_desktop_settings",
        "get_port_status",
        "check_port_available",
        "set_configured_port",
        "get_logs",
        "set_log_capture",
        "clear_logs",
        "import_config",
    ]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(app_manifest))
        .expect("tauri_build 失败");
}
