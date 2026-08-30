// 桌面版发布时隐藏 Windows 控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! kiro-rs 桌面版（Tauri 2 壳）。**双进程架构**：
//!
//! - **常驻主进程**（argv 不含 `--ui`）：托盘 + axum 代理常驻后台，**永不创建 WebView**，
//!   内存低水位（实测 ~37MB）。唤出 UI 时按需拉起子进程。
//! - **UI 子进程**（argv 含 `--ui`）：仅承载 WebView 窗口，**关窗即整个退出**，
//!   彻底释放 WKWebView/WebView2 的框架与渲染进程内存。通过环境变量从主进程接收
//!   后端端口与 admin key。
//!
//! 拆进程的动机：WebView 框架 + 渲染进程占内存大头且进程内无法反初始化；
//! 让独立子进程承载并在关窗时退出，即可彻底归还，而代理后端在主进程持续不中断。
//!
//! 主↔子协调：主进程持有子进程句柄；「显示窗口」时若子进程存活，经本地控制通道
//! （子进程监听的临时端口，端口号写在数据目录的 `ui-control.port`）请求其前置聚焦；
//! 否则拉起新的子进程。

mod settings;

use std::process::Child;
use std::sync::atomic::{AtomicU16, Ordering};

use kiro_rs::app::{self, RunOptions, RuntimeMode};
use kiro_rs::log_buffer;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use settings::DesktopSettings;
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

pub(crate) const MAIN_WINDOW_LABEL: &str = "main";

/// 传给开机自启项的标志。argv 含此值即表示本次由开机自启拉起（据此决定是否静默驻留托盘）。
const AUTOSTART_FLAG: &str = "--autostart";
/// UI 子进程角色标志。argv 含此值 = UI 子进程；否则 = 常驻主进程。
const UI_FLAG: &str = "--ui";
/// UI 子进程从环境变量读取后端端口与 admin key（不走 argv，避免 key 出现在进程列表）。
const ENV_UI_PORT: &str = "KIRO_UI_PORT";
const ENV_UI_ADMIN_KEY: &str = "KIRO_UI_ADMIN_KEY";
/// 装配失败时传给 UI 子进程的错误信息（非空则子进程展示错误页而非加载 /admin）。
const ENV_UI_ERROR: &str = "KIRO_UI_ERROR";

fn main() {
    // generate_context! 内含 Info.plist 嵌入，全二进制只能展开一次；在此生成后按角色传入。
    let context = tauri::generate_context!();
    // 按 argv 分角色：含 --ui = UI 子进程；否则 = 常驻主进程。
    if std::env::args().any(|a| a == UI_FLAG) {
        run_ui(context);
    } else {
        run_resident(context);
    }
}

/// 初始化 tracing。`with_buffer` 为真时额外挂内存日志 Layer（仅常驻主进程需要，
/// 它跑 axum 产生代理日志，经 /api/admin/logs 供 UI 子进程读取）。
fn init_tracing(with_buffer: bool) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer());
    if with_buffer {
        registry.with(log_buffer::BufferLayer).init();
    } else {
        registry.init();
    }
}

/// 常驻主进程共享状态：实际监听端口、admin key、UI 子进程句柄、装配错误。
#[derive(Default)]
struct ResidentState {
    /// axum 绑定成功后写入的实际端口（0 = 尚未绑定）
    actual_port: AtomicU16,
    /// admin key，拉起 UI 子进程时经环境变量传入
    admin_key: RwLock<Option<String>>,
    /// 当前 UI 子进程句柄（None = 未拉起或已退出）
    ui_child: Mutex<Option<Child>>,
    /// 装配失败信息（非空时唤出的 UI 子进程展示错误页）
    startup_error: RwLock<Option<String>>,
}

/// 常驻主进程：托盘 + axum，不创建 WebView。
fn run_resident(context: tauri::Context) {
    init_tracing(true);

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // 第二个完整实例（如再次双击 App）→ 唤出 UI 子进程而非重复启动后端
            spawn_or_focus_ui(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![AUTOSTART_FLAG]),
        ))
        .manage(ResidentState::default())
        .setup(|app| {
            setup_tray(app.handle())?;

            // 是否本次由开机自启拉起且开启了静默：静默则仅驻留托盘，不自动唤出 UI
            let launched_by_autostart = std::env::args().any(|a| a == AUTOSTART_FLAG);
            let silent = launched_by_autostart && DesktopSettings::load().silent_start;

            // 后台启动 axum，绑定成功后记录端口/admin key；非静默则立即唤出 UI 子进程
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
                        let admin_key = server.admin_api_key().map(|s| s.to_string());
                        let st = handle.state::<ResidentState>();
                        st.actual_port.store(port, Ordering::Relaxed);
                        *st.admin_key.write() = admin_key.clone();

                        if server.port_conflicted() {
                            tracing::warn!(
                                "端口冲突：期望 {} 被占用，已回退到 {}。可在设置中修改端口后重启",
                                server.requested_port(),
                                port
                            );
                        }

                        if !silent {
                            let h = handle.clone();
                            let _ = handle.run_on_main_thread(move || spawn_or_focus_ui(&h));
                        }

                        if let Err(e) = server.serve().await {
                            tracing::error!("HTTP 服务退出: {}", e);
                        }
                    }
                    Err(e) => {
                        // 无 WebView 可直接展示：记录错误并唤出 UI 子进程的错误页
                        tracing::error!("启动失败: {}", e);
                        let st = handle.state::<ResidentState>();
                        *st.startup_error.write() = Some(e.to_string());
                        let h = handle.clone();
                        let _ = handle.run_on_main_thread(move || spawn_or_focus_ui(&h));
                    }
                }
            });

            Ok(())
        })
        .build(context)
        .expect("构建 Tauri 应用失败")
        .run(|app, event| match event {
            // 应用完成启动后把常驻主进程降级为后台应用（不进 Dock、⌘Tab 不出现）。
            // 必须在 Ready（启动完成）设置——在 setup() 里设过早、会被启动流程覆盖。
            // 主进程从不开窗，故始终不占 Dock；UI 子进程是独立的 Regular 进程，
            // 开窗时自己进 Dock、退出即消失。
            RunEvent::Ready => {
                #[cfg(target_os = "macos")]
                let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            }
            // 点击 Dock 图标（macOS Reopen）唤出 UI。常驻进程为 Accessory 通常无 Dock 图标，
            // 但保留此路径以防激活策略变化。
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => spawn_or_focus_ui(app),
            // 无窗口场景一般不会触发 code=None 的退出；防御性阻止，保持托盘常驻
            RunEvent::ExitRequested { api, code, .. } => {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
            _ => {}
        });
}

/// UI 控制通道端口文件路径（子进程写入其监听端口，主进程读取后连接请求聚焦）。
fn control_file_path() -> std::path::PathBuf {
    app::desktop_data_dir().join("ui-control.port")
}

/// 唤出 UI：子进程存活则请求其前置聚焦；否则拉起新的 UI 子进程。
/// 必须在主线程调用（读取 managed state + spawn）。
fn spawn_or_focus_ui(app: &tauri::AppHandle) {
    let st = app.state::<ResidentState>();
    let port = st.actual_port.load(Ordering::Relaxed);
    let startup_error = st.startup_error.read().clone();

    // 后端尚未就绪且无错误 → 稍后再试
    if port == 0 && startup_error.is_none() {
        tracing::warn!("后端端口尚未就绪，暂无法唤出 UI");
        return;
    }

    let mut guard = st.ui_child.lock();
    // 已有存活子进程 → 请求聚焦，不重复拉起
    if let Some(child) = guard.as_mut() {
        match child.try_wait() {
            Ok(None) => {
                drop(guard);
                focus_ui_child();
                return;
            }
            // 已退出或查询失败 → 清理句柄后重新拉起
            _ => *guard = None,
        }
    }

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("获取自身可执行路径失败: {}", e);
            return;
        }
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(UI_FLAG);
    cmd.env(ENV_UI_PORT, port.to_string());
    if let Some(k) = st.admin_key.read().clone() {
        cmd.env(ENV_UI_ADMIN_KEY, k);
    }
    if let Some(err) = startup_error {
        cmd.env(ENV_UI_ERROR, err);
    }
    match cmd.spawn() {
        Ok(child) => {
            *guard = Some(child);
            drop(guard);
            // 有 UI 窗口了 → 常驻主进程提升为 Regular，bundle 在 Dock 显示图标+运行点。
            // （由主进程这个 ASN 拥有者控制 Dock 最可靠：子进程是同 bundle 子进程，拿不到独立 Dock 身份。）
            #[cfg(target_os = "macos")]
            let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
            start_child_exit_watcher(app);
        }
        Err(e) => tracing::error!("拉起 UI 子进程失败: {}", e),
    }
}

/// 监视 UI 子进程退出：一旦退出，把常驻主进程切回 Accessory（Dock 图标消失）。
/// 达成「开窗有 Dock 图标+点、关窗无」。
fn start_child_exit_watcher(app: &tauri::AppHandle) {
    let handle = app.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let st = handle.state::<ResidentState>();
            let mut guard = st.ui_child.lock();
            match guard.as_mut() {
                Some(child) => match child.try_wait() {
                    Ok(Some(_)) => {
                        *guard = None;
                        drop(guard);
                        let h = handle.clone();
                        let _ = handle.run_on_main_thread(move || {
                            #[cfg(target_os = "macos")]
                            let _ = h.set_activation_policy(tauri::ActivationPolicy::Accessory);
                        });
                        break;
                    }
                    Ok(None) => {} // 仍在运行
                    Err(_) => {
                        *guard = None;
                        break;
                    }
                },
                None => break, // 句柄已被（退出菜单等）取走
            }
        }
    });
}

/// 主进程侧：连接 UI 子进程的控制通道，请求其前置聚焦。失败即静默放弃（子进程可能刚退出）。
fn focus_ui_child() {
    use std::io::Write;
    let Ok(content) = std::fs::read_to_string(control_file_path()) else {
        return;
    };
    let Ok(port) = content.trim().parse::<u16>() else {
        return;
    };
    if let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) {
        let _ = stream.write_all(b"focus\n");
    }
}

/// 构建托盘图标与菜单（显示窗口 / 开机启动 / 静默启动 / 退出）。常驻主进程专用。
fn setup_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?;

    let autostart_enabled = app.autolaunch().is_enabled().unwrap_or(false);
    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "开机启动",
        true,
        autostart_enabled,
        None::<&str>,
    )?;

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

    // macOS 菜单栏惯例：单色模板图标（跟随深/浅色自动反色）、左键单击即弹菜单。
    #[cfg(target_os = "macos")]
    let builder = {
        const TRAY_RGBA: &[u8] = include_bytes!("../icons/tray-template.rgba");
        let icon = tauri::image::Image::new(TRAY_RGBA, 44, 44);
        TrayIconBuilder::new()
            .icon(icon)
            .icon_as_template(true)
            .show_menu_on_left_click(true)
    };
    // Windows/Linux 惯例：彩色 App 图标、左键单击唤出窗口、右键弹菜单。
    #[cfg(not(target_os = "macos"))]
    let builder = TrayIconBuilder::new()
        .icon(app.default_window_icon().unwrap().clone())
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let tauri::tray::TrayIconEvent::Click {
                button: tauri::tray::MouseButton::Left,
                button_state: tauri::tray::MouseButtonState::Up,
                ..
            } = event
            {
                spawn_or_focus_ui(tray.app_handle());
            }
        });

    let _tray = builder
        .menu(&menu)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "show" => spawn_or_focus_ui(app),
            "autostart" => {
                let launcher = app.autolaunch();
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
                let mut cfg = DesktopSettings::load();
                cfg.silent_start = !cfg.silent_start;
                cfg.save();
                let _ = silent_start.set_checked(cfg.silent_start);
            }
            "quit" => {
                // 退出前结束 UI 子进程，避免其成为孤儿
                if let Some(mut child) = app.state::<ResidentState>().ui_child.lock().take() {
                    let _ = child.kill();
                }
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;

    Ok(())
}

/// UI 子进程共享状态：后端端口（由主进程经环境变量传入）。
struct UiState {
    port: u16,
}

/// UI 子进程：仅承载 WebView 窗口，关窗即整个退出以释放 WebKit 内存。
fn run_ui(context: tauri::Context) {
    init_tracing(false);

    let port: u16 = std::env::var(ENV_UI_PORT)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let admin_key = std::env::var(ENV_UI_ADMIN_KEY).ok();
    let startup_error = std::env::var(ENV_UI_ERROR).ok().filter(|s| !s.is_empty());

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![AUTOSTART_FLAG]),
        ))
        .manage(UiState { port })
        .invoke_handler(tauri::generate_handler![
            get_desktop_settings,
            set_desktop_settings,
            get_port_status,
            check_port_available,
            set_configured_port,
            import_config,
            scan_sso_credentials,
            check_update,
            open_url
        ])
        .setup(move |app| {
            // UI 子进程设为 Accessory：它自己不出 Dock 图标——否则父进程(Regular)与子进程
            // 两个同 bundle 前台进程会在 Dock 里显示两个图标。Dock 那一个图标由常驻主进程提供
            // (主进程在子进程存活期间 Regular、退出后 Accessory)。子进程 Accessory 仍能显示窗口，
            // 靠下面的 set_focus 前置。
            #[cfg(target_os = "macos")]
            let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            if let Some(err) = startup_error {
                create_error_window(app.handle(), &err)?;
            } else {
                create_ui_window(app.handle(), port, admin_key.as_deref())?;
            }
            if let Some(win) = app.get_webview_window(MAIN_WINDOW_LABEL) {
                let _ = win.set_focus();
            }
            start_focus_listener(app.handle());
            Ok(())
        })
        .on_window_event(|window, event| {
            // 关闭主窗口 = 子进程整体退出，彻底释放 WebView 内存（不再隐藏/销毁复用）
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                if window.label() == MAIN_WINDOW_LABEL {
                    window.app_handle().exit(0);
                }
            }
        })
        .build(context)
        .expect("构建 UI 子进程失败")
        .run(|_app, _event| {});
}

/// UI 子进程侧：监听本地控制通道；收到 "focus" 即前置聚焦窗口。
/// 端口写入数据目录的 `ui-control.port` 供主进程读取。
fn start_focus_listener(app: &tauri::AppHandle) {
    use std::io::{BufRead, BufReader};

    let listener = match std::net::TcpListener::bind(("127.0.0.1", 0)) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("UI 控制通道监听失败: {}", e);
            return;
        }
    };
    let Ok(addr) = listener.local_addr() else {
        return;
    };
    let path = control_file_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, addr.port().to_string());

    let handle = app.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let mut line = String::new();
            if BufReader::new(stream).read_line(&mut line).is_ok() && line.trim() == "focus" {
                let h = handle.clone();
                let _ = handle.run_on_main_thread(move || {
                    if let Some(win) = h.get_webview_window(MAIN_WINDOW_LABEL) {
                        let _ = win.unminimize();
                        let _ = win.show();
                        let _ = win.set_focus();
                    }
                });
            }
        }
    });
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
/// 注：开机自启注册的是 `<exe> --autostart`（不含 --ui），因此自启拉起的是常驻主进程角色。
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
    Ok(())
}

/// 暴露给前端的端口状态。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PortStatusDto {
    /// 配置中期望的端口（config.json 的 port）
    configured: u16,
    /// 实际监听端口（可能因冲突回退为随机端口；由主进程经环境变量传入）
    actual: u16,
    /// 是否发生端口冲突（期望端口被占用而回退）
    conflicted: bool,
}

/// IPC：读取端口状态。实际端口取自主进程传入的 UiState（后端在主进程绑定）。
#[tauri::command]
fn get_port_status(state: State<'_, UiState>) -> PortStatusDto {
    let configured = app::desktop_configured_port();
    let actual = state.port;
    PortStatusDto {
        configured,
        actual,
        conflicted: actual != 0 && actual != configured,
    }
}

/// IPC：探测端口当前是否可用（空闲）。
#[tauri::command]
fn check_port_available(port: u16) -> bool {
    app::is_port_available(port)
}

/// IPC：修改期望端口并写回配置。端口变更需重启应用后生效。
#[tauri::command]
fn set_configured_port(port: u16, state: State<'_, UiState>) -> Result<(), String> {
    if port == 0 {
        return Err("端口不能为 0".to_string());
    }
    // 若目标端口既不是当前实际端口、又不可用，则拒绝（避免写入一个已被占用的端口）
    if port != state.port && !app::is_port_available(port) {
        return Err(format!("端口 {port} 已被占用"));
    }
    app::desktop_set_configured_port(port)
}

/// 导入配置的返回体。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportConfigDto {
    cancelled: bool,
    port: u16,
}

/// IPC：弹出系统文件选择器导入完整 config.json（整体覆盖，重启生效）。
#[tauri::command]
async fn import_config(app: tauri::AppHandle) -> Result<ImportConfigDto, String> {
    use tauri_plugin_dialog::DialogExt;

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
#[tauri::command]
fn scan_sso_credentials() -> Vec<app::SsoCredentialCandidate> {
    app::scan_sso_credentials()
}

/// IPC：检查 GitHub 最新 release 是否有新版本。
#[tauri::command]
async fn check_update() -> Result<app::UpdateInfo, String> {
    app::check_update(env!("CARGO_PKG_VERSION")).await
}

/// IPC：用系统默认浏览器打开外部 URL（仅 http(s)）。
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("仅支持 http(s) 链接".to_string());
    }
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("cmd")
        .args(["/C", "start", "", &url])
        .spawn();
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let result = std::process::Command::new("xdg-open").arg(&url).spawn();

    result.map(|_| ()).map_err(|e| format!("打开链接失败: {e}"))
}

/// 创建 UI 窗口并注入自动认证脚本。
///
/// `initialization_script` 在每个页面脚本执行前运行，因此 App.tsx 读取 `getApiKey()`
/// 时 localStorage 已就绪，直达主界面。
fn create_ui_window(
    app: &tauri::AppHandle,
    port: u16,
    admin_key: Option<&str>,
) -> tauri::Result<()> {
    // 注意：axum nest 下 `/admin`（无尾斜杠）命中首页；`/admin/` 会 404。
    let url = format!("http://127.0.0.1:{port}/admin");
    let parsed = url
        .parse()
        .map_err(|e| tauri::Error::WebviewNotFound.tap_err(e))?;

    let mut builder =
        WebviewWindowBuilder::new(app, MAIN_WINDOW_LABEL, WebviewUrl::External(parsed))
            .title("kiro-rs")
            .inner_size(1200.0, 800.0)
            .min_inner_size(900.0, 600.0);

    if let Some(key) = admin_key {
        let escaped = serde_json_escape(key);
        let script =
            format!("try {{ localStorage.setItem('adminApiKey', {escaped}); }} catch (e) {{}}");
        builder = builder.initialization_script(&script);
    }

    builder.build()?;
    Ok(())
}

/// 装配失败时的错误窗口（由 UI 子进程在收到 ENV_UI_ERROR 时展示）。
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
