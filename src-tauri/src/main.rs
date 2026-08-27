// 桌面版发布时隐藏 Windows 控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! kiro-rs 桌面版（Tauri 2 壳）。
//!
//! 流程：
//! 1. 单实例守卫，避免重复启动多个后端。
//! 2. 后台 spawn axum 服务（桌面模式：系统数据目录 + `127.0.0.1` + 端口占用自动回退）。
//! 3. 服务绑定成功后，创建窗口加载 `http://127.0.0.1:<port>/admin/`，
//!    并在页面脚本执行前注入 `localStorage['adminApiKey']` 实现 Admin UI 免登录。
//! 4. 托盘常驻：显示窗口 / 开机启动 / 静默启动 / 退出。
//!
//! 静默启动（默认开）：仅当本次由开机自启拉起（argv 含 `--autostart`）且开关开启时，
//! 窗口初始隐藏、只驻留托盘；手动双击打开始终显示窗口。

mod log_buffer;
mod settings;

use std::sync::atomic::{AtomicU16, Ordering};

use serde::Serialize;
use tauri::{
    Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder,
    menu::{CheckMenuItem, Menu, MenuItem},
    tray::TrayIconBuilder,
};
use tauri_plugin_autostart::{ManagerExt, MacosLauncher};

use kiro_rs::app::{self, RunOptions, RuntimeMode};
use settings::DesktopSettings;

/// 运行时状态，作为 Tauri managed state。
/// `actual` 在 axum 绑定成功后由 setup 写入（0 表示尚未绑定）。
/// `admin_key` 缓存注入用的 Admin API Key，供关窗销毁后重建窗口时复用。
#[derive(Default)]
struct PortState {
    actual: AtomicU16,
    admin_key: parking_lot::Mutex<Option<String>>,
}

/// 暴露给前端的桌面设置快照。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DesktopSettingsDto {
    /// 静默启动开关
    silent_start: bool,
    /// 开机启动是否已在系统注册
    autostart: bool,
}

/// IPC：读取桌面设置（静默启动 + 开机启动状态）。
#[tauri::command]
fn get_desktop_settings(app: tauri::AppHandle) -> DesktopSettingsDto {
    let s = DesktopSettings::load();
    let autostart = app.autolaunch().is_enabled().unwrap_or(false);
    DesktopSettingsDto {
        silent_start: s.silent_start,
        autostart,
    }
}

/// IPC：写入桌面设置。静默启动落本地文件；开机启动调用系统注册/注销。
#[tauri::command]
fn set_desktop_settings(
    app: tauri::AppHandle,
    silent_start: bool,
    autostart: bool,
) -> Result<(), String> {
    let mut s = DesktopSettings::load();
    s.silent_start = silent_start;
    s.save();

    let launcher = app.autolaunch();
    let cur = launcher.is_enabled().unwrap_or(false);
    if autostart && !cur {
        launcher.enable().map_err(|e| e.to_string())?;
    } else if !autostart && cur {
        launcher.disable().map_err(|e| e.to_string())?;
    }
    // 同步托盘勾选状态由各自菜单项在下次打开时读取，无需在此处理
    Ok(())
}

/// 暴露给前端的端口状态。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PortStatusDto {
    /// 配置中期望的端口（config.json 的 port）
    configured: u16,
    /// 实际监听端口（可能因冲突回退为随机端口）
    actual: u16,
    /// 是否发生端口冲突（期望端口被占用而回退）
    conflicted: bool,
}

/// IPC：读取端口状态（期望端口 / 实际端口 / 是否冲突）。
#[tauri::command]
fn get_port_status(state: State<'_, PortState>) -> PortStatusDto {
    let configured = app::desktop_configured_port();
    let actual = state.actual.load(Ordering::Relaxed);
    PortStatusDto {
        configured,
        actual,
        // 实际端口已知且与期望不一致才算冲突（actual=0 表示尚未绑定，不判定）
        conflicted: actual != 0 && actual != configured,
    }
}

/// IPC：探测端口当前是否可用（空闲）。
#[tauri::command]
fn check_port_available(port: u16) -> bool {
    app::is_port_available(port)
}

/// IPC：修改期望端口并写回配置。端口变更需重启应用后生效。
///
/// 写回前校验端口可用（排除当前实际端口自身：改回当前正在用的端口视为合法）。
#[tauri::command]
fn set_configured_port(port: u16, state: State<'_, PortState>) -> Result<(), String> {
    if port == 0 {
        return Err("端口不能为 0".to_string());
    }
    let actual = state.actual.load(Ordering::Relaxed);
    // 若目标端口既不是当前实际端口、又不可用，则拒绝（避免写入一个已被占用的端口）
    if port != actual && !app::is_port_available(port) {
        return Err(format!("端口 {port} 已被占用"));
    }
    app::desktop_set_configured_port(port)
}

/// 导入配置的返回体。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportConfigDto {
    /// 用户是否取消了文件选择
    cancelled: bool,
    /// 导入成功后配置里声明的端口（供提示；cancelled 时为 0）
    port: u16,
}

/// IPC：弹出系统文件选择器导入完整 config.json（整体覆盖，重启生效）。
///
/// 用户取消返回 `cancelled: true`；校验失败返回 Err（前端 toast 展示）。
#[tauri::command]
async fn import_config(app: tauri::AppHandle) -> Result<ImportConfigDto, String> {
    use tauri_plugin_dialog::DialogExt;

    // 阻塞式文件选择（在独立线程弹出原生对话框）
    let file = app
        .dialog()
        .file()
        .add_filter("配置文件", &["json"])
        .set_title("选择要导入的 config.json")
        .blocking_pick_file();

    let Some(path) = file else {
        return Ok(ImportConfigDto {
            cancelled: true,
            port: 0,
        });
    };

    // FilePath → 本地路径字符串
    let path_str = path
        .into_path()
        .map_err(|e| format!("无法解析所选文件路径: {e}"))?
        .to_string_lossy()
        .to_string();

    let port = app::desktop_import_config(&path_str)?;
    Ok(ImportConfigDto {
        cancelled: false,
        port,
    })
}

/// IPC：扫描本机 AWS SSO 缓存，返回可导入的 Kiro IdC 凭证候选（不验活）。
/// 前端拿到后逐条调 add_credential（走 HTTP）完成导入与验活。
#[tauri::command]
fn scan_sso_credentials() -> Vec<app::SsoCredentialCandidate> {
    app::scan_sso_credentials()
}

/// 前端拉取日志的返回体。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LogPullDto {
    /// 捕获是否开启
    enabled: bool,
    /// 序号 > after 的新日志行
    lines: Vec<log_buffer::LogLine>,
}

/// IPC：拉取序号 > `after` 的日志行（前端轮询增量用）。
#[tauri::command]
fn get_logs(after: u64) -> LogPullDto {
    LogPullDto {
        enabled: log_buffer::is_enabled(),
        lines: log_buffer::since(after),
    }
}

/// IPC：开启/关闭日志捕获。
#[tauri::command]
fn set_log_capture(enabled: bool) {
    log_buffer::set_enabled(enabled);
}

/// IPC：清空日志缓冲。
#[tauri::command]
fn clear_logs() {
    log_buffer::clear();
}

/// 显示并聚焦主窗口；若窗口已被销毁（关窗释放 WebView），则用缓存的端口/密钥重建。
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = win.show();
        let _ = win.set_focus();
        return;
    }
    // 窗口不存在（已销毁或 axum 尚未就绪）→ 尝试用共享状态重建
    let st = app.state::<PortState>();
    let port = st.actual.load(Ordering::Relaxed);
    if port == 0 {
        // axum 还没绑定端口，窗口会在 setup 里首次创建，这里无需处理
        return;
    }
    let admin_key = st.admin_key.lock().clone();
    if let Err(e) = create_main_window(app, port, admin_key.as_deref()) {
        tracing::error!("重建窗口失败: {}", e);
    }
}

const MAIN_WINDOW_LABEL: &str = "main";

/// 传给开机自启项的标志。进程 argv 含此值即表示本次由开机自启拉起，
/// 手动双击打开时不带，据此区分是否应静默驻留托盘。
const AUTOSTART_FLAG: &str = "--autostart";

fn main() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    // 同时输出到 stdout 和内存缓冲（供「日志」Tab 读取）
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(log_buffer::BufferLayer)
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // 第二个实例启动时，聚焦已有窗口而非新开
            show_main_window(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![AUTOSTART_FLAG]),
        ))
        .plugin(tauri_plugin_dialog::init())
        .manage(PortState::default())
        .invoke_handler(tauri::generate_handler![
            get_desktop_settings,
            set_desktop_settings,
            get_port_status,
            check_port_available,
            set_configured_port,
            get_logs,
            set_log_capture,
            clear_logs,
            import_config,
            scan_sso_credentials
        ])
        .setup(|app| {
            setup_tray(app.handle())?;

            // 是否应静默启动：仅当「本次由开机自启拉起」且「静默开关开启」时不弹窗
            let launched_by_autostart =
                std::env::args().any(|a| a == AUTOSTART_FLAG);
            let silent = launched_by_autostart && DesktopSettings::load().silent_start;
            if silent {
                tracing::info!("开机自启且静默启动已开启，窗口驻留托盘");
            }

            // 后台启动 axum，绑定成功后在主线程创建窗口
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                match app::build(RunOptions {
                    mode: RuntimeMode::Desktop,
                    config_path: None,
                    credentials_path: None,
                })
                .await
                {
                    Ok(server) => {
                        let port = server.local_port();
                        let requested = server.requested_port();
                        let conflicted = server.port_conflicted();
                        let admin_key = server.admin_api_key().map(|s| s.to_string());

                        // 记录实际端口与 admin_key 到共享状态（供 IPC 读取 + 关窗后重建窗口）
                        {
                            let st = handle.state::<PortState>();
                            st.actual.store(port, Ordering::Relaxed);
                            *st.admin_key.lock() = admin_key.clone();
                        }

                        if conflicted {
                            tracing::warn!(
                                "端口冲突：期望 {} 被占用，已回退到 {}。可在设置中修改端口后重启",
                                requested,
                                port
                            );
                        }

                        // 静默启动：不创建窗口，仅驻留托盘（省去 WebView 内存），
                        // 用户从托盘/Dock 唤出时再由 show_main_window 按需创建。
                        // 非静默：立即创建并显示窗口。
                        if silent {
                            tracing::info!("静默启动：跳过窗口创建，仅驻留托盘");
                        } else if let Err(e) =
                            create_main_window(&handle, port, admin_key.as_deref())
                        {
                            tracing::error!("创建窗口失败: {}", e);
                        }
                        if let Err(e) = server.serve().await {
                            tracing::error!("HTTP 服务退出: {}", e);
                        }
                    }
                    Err(e) => {
                        tracing::error!("启动失败: {}", e);
                        // 装配失败时仍创建一个窗口展示错误
                        let _ = create_error_window(&handle, &e.to_string());
                    }
                }
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            // 关闭主窗口时销毁窗口（释放 WebView 内存，后台仅保留 Rust 服务），
            // 不退出进程；下次从托盘/Dock 唤出时 show_main_window 会重建窗口。
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == MAIN_WINDOW_LABEL {
                    api.prevent_close();
                    let _ = window.destroy();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("构建 Tauri 应用失败")
        .run(|app, event| match event {
            // 点击 Dock 图标（macOS Reopen）时唤出窗口
            RunEvent::Reopen { .. } => show_main_window(app),
            // 关闭最后一个窗口不退出进程：托盘常驻，靠托盘/Dock 再唤出窗口
            RunEvent::ExitRequested { api, code, .. } => {
                // code 为 None 表示是「窗口全关」触发的退出请求 → 阻止；
                // 有 code（如托盘「退出」调用 app.exit(0)）则放行
                if code.is_none() {
                    api.prevent_exit();
                }
            }
            _ => {}
        });
}

/// 构建托盘图标与菜单（显示窗口 / 开机启动 / 退出）。
fn setup_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?;

    // 「开机启动」勾选项，初始勾选状态取自当前系统注册情况
    let autostart_enabled = app.autolaunch().is_enabled().unwrap_or(false);
    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "开机启动",
        true,
        autostart_enabled,
        None::<&str>,
    )?;

    // 「静默启动」勾选项，初始状态取自本地设置（默认开启）
    let silent_enabled = DesktopSettings::load().silent_start;
    let silent_start = CheckMenuItem::with_id(
        app,
        "silent_start",
        "静默启动（开机时不弹窗）",
        true,
        silent_enabled,
        None::<&str>,
    )?;

    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &autostart, &silent_start, &quit])?;

    let _tray = TrayIconBuilder::new()
        .icon(app.default_window_icon().unwrap().clone())
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
            "autostart" => {
                let launcher = app.autolaunch();
                // 依当前状态切换，并把勾选同步回菜单项
                let now_enabled = match launcher.is_enabled() {
                    Ok(true) => {
                        if let Err(e) = launcher.disable() {
                            tracing::error!("关闭开机启动失败: {}", e);
                            true
                        } else {
                            false
                        }
                    }
                    Ok(false) => {
                        if let Err(e) = launcher.enable() {
                            tracing::error!("开启开机启动失败: {}", e);
                            false
                        } else {
                            true
                        }
                    }
                    Err(e) => {
                        tracing::error!("读取开机启动状态失败: {}", e);
                        return;
                    }
                };
                let _ = autostart.set_checked(now_enabled);
            }
            "silent_start" => {
                // 翻转并持久化，同时同步勾选状态
                let mut cfg = DesktopSettings::load();
                cfg.silent_start = !cfg.silent_start;
                cfg.save();
                let _ = silent_start.set_checked(cfg.silent_start);
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;

    Ok(())
}

/// 创建主窗口并注入自动认证脚本。
///
/// `initialization_script` 在每个页面/子框架的脚本执行前运行，
/// 因此 App.tsx 读取 `getApiKey()` 时 localStorage 已就绪，直达主界面。
fn create_main_window(
    app: &tauri::AppHandle,
    port: u16,
    admin_key: Option<&str>,
) -> tauri::Result<()> {
    // 注意：axum nest 下 `/admin`（无尾斜杠）命中首页；`/admin/` 会 404。
    // vite base 为绝对路径 `/admin/`，故文档 URL 无尾斜杠不影响资源解析。
    let url = format!("http://127.0.0.1:{port}/admin");
    let parsed = url
        .parse()
        .map_err(|e| tauri::Error::WebviewNotFound.tap_err(e))?;

    let mut builder = WebviewWindowBuilder::new(app, MAIN_WINDOW_LABEL, WebviewUrl::External(parsed))
        .title("kiro-rs")
        .inner_size(1200.0, 800.0)
        .min_inner_size(900.0, 600.0);

    if let Some(key) = admin_key {
        // 仅注入 JSON 转义后的 key，避免脚本注入
        let escaped = serde_json_escape(key);
        let script = format!(
            "try {{ localStorage.setItem('adminApiKey', {escaped}); }} catch (e) {{}}"
        );
        builder = builder.initialization_script(&script);
    }

    builder.build()?;
    Ok(())
}

/// 装配失败时的兜底错误窗口。
fn create_error_window(app: &tauri::AppHandle, message: &str) -> tauri::Result<()> {
    let html = format!(
        "data:text/html,<html><body style='font-family:sans-serif;padding:2rem'>\
         <h2>kiro-rs 启动失败</h2><pre style='white-space:pre-wrap'>{}</pre></body></html>",
        html_escape(message)
    );
    let parsed = html
        .parse()
        .map_err(|e| tauri::Error::WebviewNotFound.tap_err(e))?;
    WebviewWindowBuilder::new(app, MAIN_WINDOW_LABEL, WebviewUrl::External(parsed))
        .title("kiro-rs - 启动失败")
        .inner_size(700.0, 400.0)
        .build()?;
    Ok(())
}

/// 把字符串转成安全的 JSON 字符串字面量（含引号）。
fn serde_json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 最小 HTML 转义（用于错误信息展示）。
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// 辅助：把解析错误映射到 tauri::Error（`Url::parse` 错误类型不直接兼容）。
trait TapErr {
    fn tap_err<E: std::fmt::Display>(self, e: E) -> Self;
}
impl TapErr for tauri::Error {
    fn tap_err<E: std::fmt::Display>(self, e: E) -> Self {
        tracing::error!("URL 解析失败: {}", e);
        self
    }
}
