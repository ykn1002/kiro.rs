//! 轻量模式：关窗（或静默启动）后延迟销毁 WebView 以释放内存。
//!
//! 背景：Tauri 桌面壳常驻内存约 ~160MB，主要是 WKWebView/WebView2 的渲染进程。
//! 单纯 `hide()` 不释放这部分内存。参考 Clash Verge Rev 的做法，关窗后延迟一段时间
//! 真正 `destroy()` 窗口，把内存降到低水位（仅剩 Rust 后端 + axum，约几十 MB）；
//! 托盘/Dock 再唤出时重建窗口、重新加载 `/admin` 页面。
//!
//! 与 CVR 相比这里更简单：我们只有单一主窗口，且 destroy 后窗口对象不存在，
//! 系统没有「隐藏的 WebContent」可在内存紧张时被杀，因此不需要 CVR 那套
//! `on_web_content_process_terminated` 重载补偿——唤出即全新重建，天然无白屏。
//!
//! macOS 额外细节：进入轻量模式时把激活策略切到 `Accessory`（从 Dock 应用降级为
//! 纯托盘后台应用，Dock 图标消失、⌘Tab 不再出现），唤出时切回 `Regular`。
//! 其它平台这两步是 no-op。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tauri::{AppHandle, Manager};

use crate::{create_main_window_for_reopen, MAIN_WINDOW_LABEL};

/// 是否处于轻量模式（窗口已销毁）。唤出/重建后置回 false。
static IN_LIGHTWEIGHT: AtomicBool = AtomicBool::new(false);

/// 定时器代数。每次「安排销毁」自增；到点的任务比对代数，
/// 不一致说明期间已被取消或重新安排，直接放弃，无需持有可取消句柄。
static TIMER_GENERATION: AtomicU64 = AtomicU64::new(0);

/// 取消尚未触发的销毁定时器（自增代数令旧任务失效）。
pub fn cancel_timer() {
    TIMER_GENERATION.fetch_add(1, Ordering::AcqRel);
}

/// 关窗后调用：先隐藏窗口（保持可秒开），再按配置安排延迟销毁。
///
/// - `minutes == 0`：立即进入轻量模式（直接销毁）。
/// - `minutes > 0`：延迟该时长后销毁；期间若窗口被唤出会取消。
pub fn schedule_after_close(app: &AppHandle, minutes: u64) {
    if minutes == 0 {
        enter(app);
        return;
    }

    // 新一代定时器；到点时若代数已变则说明被取消/重排
    let generation = TIMER_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(minutes.saturating_mul(60))).await;
        if TIMER_GENERATION.load(Ordering::Acquire) != generation {
            return; // 期间被取消或重新安排
        }
        // 回主线程销毁窗口（窗口操作必须在主线程）
        let for_main = app.clone();
        let _ = app.run_on_main_thread(move || {
            enter(&for_main);
        });
    });
}

/// 进入轻量模式：销毁主窗口 + （macOS）切到后台激活策略。
/// 必须在主线程调用。
pub fn enter(app: &AppHandle) {
    // 已在轻量模式则跳过
    if IN_LIGHTWEIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    // 使后续到点的旧定时器失效
    TIMER_GENERATION.fetch_add(1, Ordering::AcqRel);

    if let Some(win) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        if let Err(e) = win.destroy() {
            tracing::warn!("轻量模式销毁窗口失败: {}", e);
            IN_LIGHTWEIGHT.store(false, Ordering::Release);
            return;
        }
        tracing::info!("已进入轻量模式（窗口已销毁，释放 WebView 内存）");
    }

    #[cfg(target_os = "macos")]
    if let Err(e) = app.set_activation_policy(tauri::ActivationPolicy::Accessory) {
        tracing::warn!("切换 Accessory 激活策略失败: {}", e);
    }
}

/// 退出轻量模式并显示窗口：重建主窗口 + （macOS）切回前台激活策略。
/// 必须在主线程调用。
pub fn exit_and_show(app: &AppHandle) {
    cancel_timer();

    // 无论是否处于轻量模式，都先确保激活策略为 Regular（macOS）
    #[cfg(target_os = "macos")]
    if let Err(e) = app.set_activation_policy(tauri::ActivationPolicy::Regular) {
        tracing::warn!("切换 Regular 激活策略失败: {}", e);
    }

    IN_LIGHTWEIGHT.store(false, Ordering::Release);

    // 窗口已被销毁则重建；否则直接显示并聚焦
    if let Some(win) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = win.show();
        let _ = win.set_focus();
    } else if let Err(e) = create_main_window_for_reopen(app) {
        tracing::error!("轻量模式唤出重建窗口失败: {}", e);
    }
}
