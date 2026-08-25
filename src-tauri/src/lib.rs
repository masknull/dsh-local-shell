//! App wiring: tray icon + menu, window close→hide, DSH lifecycle, and the
//! task-completion event monitor.

mod dsh;
mod menu;
mod monitor;
mod update;

use tauri::{
    menu::{Menu, MenuItem},
    tray::{TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, WindowEvent,
};

/// AppUserModelID stamped on toasts; must match the registry registration in
/// `ensure_toast_aumid` and the tauri.conf identifier.
pub(crate) const TOAST_AUMID: &str = "com.dsh.desktop";

/// Frontend-invoked retry after a failed start.
#[tauri::command]
fn dsh_retry(app: AppHandle) {
    dsh::retry(app);
}

/// Frontend-invoked npx download consent after `notfound` (or as an error
/// fallback): persists the choice and runs the npx candidate.
#[tauri::command]
fn dsh_download(app: AppHandle) {
    dsh::download_and_start(app);
}

/// Frontend-invoked one-click global install: npm i -g @deepseek-ai/dsh,
/// then startup leads with the freshly installed global dsh. The registry
/// is optional and whitelisted Rust-side (probe winner or user's pick).
#[tauri::command]
fn dsh_install_npm(app: AppHandle, registry: Option<String>) {
    dsh::install_global_npm(app, registry.as_deref());
}

/// Frontend-invoked registry speed probe for the install-source chooser.
#[tauri::command]
fn dsh_npm_probe() -> serde_json::Value {
    dsh::npm_probe()
}

/// Frontend-invoked environment facts for the env panel.
#[tauri::command]
fn env_info(app: AppHandle) -> serde_json::Value {
    dsh::env_info(&app)
}

/// Frontend-invoked "open this directory in Explorer" for env-panel paths.
#[tauri::command]
fn open_path(app: AppHandle, path: String) {
    use tauri_plugin_opener::OpenerExt;
    let _ = app.opener().open_path(path, None::<&str>);
}

/// Tail of the shared dsh.log for the log tab of the secondary panel.
#[tauri::command]
fn log_tail(lines: usize) -> Vec<String> {
    dsh::log_tail(lines.clamp(50, 1000))
}

/// Panel「重启」: restart the dsh web backend (same flow as the tray entry —
/// teardown, clear the port, re-run the startup chain; the shell's boot view
/// and webchat iframe re-attach through the usual events).
#[tauri::command]
fn dsh_restart_backend(app: AppHandle) {
    dsh::restart(app);
}

/// Panel/tray「前后端重启」: fresh app process onto the same exe,
/// owned DSH torn down — the new instance re-runs the whole chain.
#[tauri::command]
fn app_full_restart(app: AppHandle) {
    update::restart_app(&app);
}

/// One-paste AI context: env facts + this session's log as a markdown
/// bundle, saved beside the log and returned so the panel can also put it
/// on the clipboard. Solves "AI has to hunt through the whole DSH install".
#[tauri::command]
fn diagnostic_export(app: AppHandle) -> Result<serde_json::Value, String> {
    let (date, time, stamp) = dsh::local_time_parts();
    let info = dsh::env_info(&app);
    let version = app.package_info().version.to_string();
    let exe = tauri::utils::platform::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let log_dir = dsh::shell_data_dir();
    let session_log = std::fs::read_to_string(log_dir.join("dsh.log"))
        .unwrap_or_else(|_| "(读取失败)".to_string());

    let mut content = String::new();
    content.push_str("# DSH Desktop 诊断包\n\n");
    content.push_str(&format!("- 生成时间: {date} {time}\n"));
    content.push_str(&format!("- 应用: v{version} ({exe})\n"));
    content.push_str(&format!("- 日志目录: {}\n\n", log_dir.display()));
    content.push_str("## 环境配置 (env_info)\n\n");
    content.push_str("```json\n");
    content.push_str(
        &serde_json::to_string_pretty(&info).unwrap_or_else(|_| "{}".to_string()),
    );
    content.push_str("\n```\n\n");
    content.push_str("## 本次会话日志 (dsh.log,仅壳事件)\n\n");
    content.push_str("~~~text\n");
    content.push_str(&session_log);
    content.push_str("\n~~~\n");

    let path = log_dir.join(format!("diagnostics-{stamp}.md"));
    std::fs::create_dir_all(&log_dir)
        .and_then(|_| std::fs::write(&path, &content))
        .map_err(|e| format!("诊断包写入失败:{e}"))?;
    dsh::log_write(
        dsh::LogLevel::Info,
        &format!("[dsh-desktop] diagnostic bundle exported: {}", path.display()),
    );
    Ok(serde_json::json!({
        "path": path.display().to_string(),
        "dir": log_dir.display().to_string(),
        "content": content,
    }))
}

/// Tray「环境信息」: show the window and open the env overlay. The shell stays
/// loaded next to the webchat iframe, so this is a plain event — no navigation.
fn open_env_page(app: &AppHandle) {
    show_main_window(app);
    let _ = app.emit("show-env", ());
}

/// Frontend-invoked custom dsh path from the notfound dialog: validates it
/// exists, persists it, and retries startup with it leading the chain.
#[tauri::command]
fn dsh_custom_path(app: AppHandle, path: String) -> Result<(), String> {
    dsh::set_custom_path(&app, path)
}

/// Frontend-invoked exit from the notfound choice.
#[tauri::command]
fn dsh_exit(app: AppHandle) {
    dsh::teardown(&app);
    app.exit(0);
}

// --- Titlebar window controls, as app commands. The frontend window-plugin
// calls (plugin:window|*) silently no-op'd in this setup while custom
// commands (the same channel env_info uses) worked fine; driving the window
// from Rust needs no capability entries and sidesteps that entirely. ---

#[tauri::command]
fn window_minimize(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.minimize();
    }
}

#[tauri::command]
fn window_toggle_maximize(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let maximized = w.is_maximized().unwrap_or(false);
        if maximized {
            let _ = w.unmaximize();
        } else {
            let _ = w.maximize();
        }
    }
}

/// Same path as the native X would take: CloseRequested → hide to tray.
#[tauri::command]
fn window_close(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.close();
    }
}

/// Titlebar drag: invoked on mousedown in the drag strip. The OS caption
/// semantics (move, and double-click → maximize) come along for free.
#[tauri::command]
fn window_start_drag(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.start_dragging();
    }
}

#[tauri::command]
fn window_is_maximized(app: AppHandle) -> bool {
    app.get_webview_window("main")
        .and_then(|w| w.is_maximized().ok())
        .unwrap_or(false)
}

/// Show and focus the main window (tray double-click / Open DSH menu item /
/// toast "打开窗口" button / second-instance relaunch).
pub(crate) fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Quit path: tear down our DSH subprocess tree, then exit. Attached mode tears
/// down to nothing and leaves the pre-existing instance running.
fn quit_dsh(app: &AppHandle) {
    // 改版: 托盘「退出」只退出壳, DSH 服务保持运行(下次打开自动附加)。
    app.exit(0);
}

/// 通用 MessageBox 确认弹窗(是/否), 供 dsh.rs 的更新检查等功能使用。
pub(crate) fn prompt_yes_no(title: &str, text: &str) -> bool {
    #[cfg(windows)]
    unsafe {
        extern "system" {
            fn MessageBoxW(
                hwnd: *const core::ffi::c_void,
                lp_text: *const u16,
                lp_caption: *const u16,
                u_type: u32,
            ) -> i32;
        }
        let text: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let caption: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        // MB_YESNO(0x4) | MB_ICONQUESTION(0x20) | MB_APPLMODAL(0)
        MessageBoxW(core::ptr::null(), text.as_ptr(), caption.as_ptr(), 0x4 | 0x20) == 6 // IDYES
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Windows toast identity for a portable bare exe. tauri-plugin-notification
/// stamps toasts with the app identifier as AppUserModelID, but Windows only
/// displays toasts for an AUMID registered via an installer's Start Menu
/// shortcut — and we deliberately ship without an installer. Register the AUMID
/// through the documented registry alternative instead (the same method other
/// portable apps use); without it Windows silently drops every toast.
/// Idempotent; a failure only degrades toast attribution, never the app.
#[cfg(windows)]
fn ensure_toast_aumid() {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    // Must equal the tauri.conf.json identifier the notification stamps.
    const AUMID: &str = TOAST_AUMID;
    let register = |exe: &std::path::Path| -> std::io::Result<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu.create_subkey(format!(
            r"Software\Classes\AppUserModelId\{AUMID}"
        ))?;
        key.set_value("DisplayName", &"DeepSeek Harness")?;
        // Strip the `\\?\` verbatim prefix current_exe() can carry — Windows
        // expects a plain path here for the toast attribution icon.
        let exe_path = exe.display().to_string();
        let exe_path = exe_path.strip_prefix(r"\\?\").unwrap_or(&exe_path);
        key.set_value("IconUri", &exe_path)?;
        Ok(())
    };
    match tauri::utils::platform::current_exe() {
        Ok(exe) => {
            if let Err(e) = register(&exe) {
                eprintln!("[dsh-desktop] toast AUMID registration failed: {e}");
            }
        }
        Err(e) => eprintln!("[dsh-desktop] toast AUMID registration skipped: {e}"),
    }
}

/// (改版) 已移除 ensure_tray_promoted: 强制写 IsPromoted=1 会覆盖用户对
/// 托盘图标的手动排列(移动位置/收进折叠区)。现在首次默认进折叠区(Windows
/// 对新图标的行为), 之后位置完全跟随用户调整, 由系统持久化。
#[cfg_attr(windows, allow(dead_code))]
fn ensure_tray_promoted_unused() {}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {    tauri::Builder::default()
        // Registered first: a second launch (e.g. toast foreground activation,
        // or the user double-clicking the exe again) focuses the existing window
        // instead of starting a second instance.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        // opener 插件: 显式关闭它的 JS 侧链接点击拦截(open_js_links_on_click: false)。
        // 该拦截会在 window 冒泡阶段抢走左键/Ctrl+点击的 `target="_blank"` 外链并改道到
        // JS 侧 `plugin:opener|open_url` IPC, 而 3080 远程页面里没有可靠的 Tauri IPC 桥,
        // 导致左键/Ctrl+点击两头落空(原生 new-window 被 preventDefault 压掉, IPC 又不通)。
        // 关闭后左键/Ctrl+点击回到原生 NewWindowRequested → 本窗口 .on_new_window → 系统浏览器,
        // 与右键同一条已验证可靠的通道(menu.rs 还会显式把这类点击接管为 window.open 兜底)。
        .plugin(
            tauri_plugin_opener::Builder::new()
                .open_js_links_on_click(false)
                .build(),
        )
        .manage(dsh::DshState::new())
        .invoke_handler(tauri::generate_handler![dsh_retry, dsh_download, dsh_custom_path, dsh_install_npm, dsh_npm_probe, env_info, open_path, log_tail, diagnostic_export, dsh_restart_backend, app_full_restart, dsh_exit, window_minimize, window_toggle_maximize, window_close, window_start_drag, window_is_maximized])
        .setup(|app| {
            // Session-start log rotation (ComfyUI-style) before anything logs
            // or spawns: previous session archived under a timestamped name.
            dsh::rotate_log(app.handle());

            #[cfg(windows)]
            ensure_toast_aumid();

            // The window is built here (not in tauri.conf.json) so it can carry
            // a new-window handler: every new-window request (target=_blank
            // links, window.open from the link menu) is handed to the system
            // default browser instead of being silently denied by wry.
            let opener_app = app.handle().clone();
            tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .title("DeepSeek Harness")
            .inner_size(1280.0, 800.0)
            .min_inner_size(720.0, 520.0)
            // 改版: 保留系统标题栏(自带 最小化/最大化/关闭 按钮)。
            // 顶层导航到 webchat 后壳页自绘标题栏不可见, 必须依赖系统按钮。
            .decorations(true)
            // Runs in every frame on document creation; self-guards on
            // `location.origin === 'http://127.0.0.1:3080'` so it installs the
            // link context menu exactly inside the webchat iframe.
            .initialization_script(menu::MENU_SCRIPT)
            // GitHub 加速镜像(gh-proxy)注入: 插件对 github 的 fetch/XHR 请求
            // 自动重写为 https://gh-proxy.com/<原URL> (本地 3080 请求不受影响)
            .initialization_script(menu::GH_MIRROR_SCRIPT)
            // WebView2's default drag-drop handler swallows file drops before
            // the page sees them, so HTML5 drag-and-drop (image attachments)
            // only works with the handler disabled — the tauri-documented
            // requirement for browser-parity dnd on Windows. Clipboard access
            // rides along for image paste.
            .disable_drag_drop_handler()
            .enable_clipboard_access()
            // 说明:WebView2 的用户数据目录(cookie/登录态)由 Tauri 固定为
            // %LOCALAPPDATA%\com.dsh.desktop\EBWebView,WebviewWindowBuilder
            // 没有自定义 API;登录态照常持久(登录一次 dsh-remote 即记住)。
            // 壳自身的设置/日志仍全部写在 exe 旁 dsh-shell-data。
            // F5 (or any webview reload) remounts the shell page, which
            // missed the original `ready` emit — re-announce the current
            // backend state so the fresh page doesn't sit on the boot
            // spinner while the backend is actually up.
            .on_page_load(|webview, payload| {
                use tauri::webview::PageLoadEvent;
                if payload.event() == PageLoadEvent::Finished
                    && !payload.url().to_string().starts_with("about:")
                {
                    let app = webview.app_handle().clone();
                    std::thread::spawn(move || dsh::emit_current_status(&app));
                }
            })
            .on_new_window(move |url, _features| {
                let app = opener_app.clone();
                let url = url.to_string();
                tauri::async_runtime::spawn(async move {
                    use tauri_plugin_opener::OpenerExt;
                    let _ = app.opener().open_url(url, None::<&str>);
                });
                tauri::webview::NewWindowResponse::Deny
            })
            .build()?;

            let open = MenuItem::with_id(app, "open", "打开主界面", true, None::<&str>)?;
            // Backend-only restart: relaunches the dsh web process, not the
            // app — the name says so explicitly now (it used to read "重启
            // DSH", which users reasonably read as "this also updates").
            let restart = MenuItem::with_id(app, "restart", "重启 dsh web(后端)", true, None::<&str>)?;
            let restart_app_item =
                MenuItem::with_id(app, "restart-app", "前后端重启", true, None::<&str>)?;
            let env = MenuItem::with_id(app, "env", "环境信息", true, None::<&str>)?;
            let check_update =
                MenuItem::with_id(app, "check-update", "检查更新(DSH)", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出壳(DSH 保持运行)", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &restart, &restart_app_item, &env, &check_update, &quit])?;

            TrayIconBuilder::with_id("main-tray")
                .icon(
                    app.default_window_icon()
                        .expect("default window icon missing")
                        .clone(),
                )
                .tooltip("DeepSeek Harness")
                .menu(&menu)
                // Left-click should not pop the menu; double-click opens the window.
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => show_main_window(app),
                    "restart" => dsh::restart(app.clone()),
                    "restart-app" => update::restart_app(app),
                    "env" => open_env_page(app),
                    "check-update" => dsh::check_update(app.clone()),
                    "quit" => quit_dsh(app),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::DoubleClick { .. } = event {
                        show_main_window(tray.app_handle());
                    }
                })
                .build(app)?;

            // 改版: 不再强制 IsPromoted=1。托盘图标首次默认在折叠区,
            // 用户拖动/调整后由 Windows 记住自定义位置, 重启不再覆盖。

            // DSH lifecycle (probe/spawn/wait) and the event monitor run on their
            // own blocking threads; both share the AppHandle. The self-update
            // check runs in parallel — the boot page's version pill narrates it.
            let lifecycle = app.handle().clone();
            std::thread::spawn(move || dsh::startup(lifecycle));
            let monitor_app = app.handle().clone();
            std::thread::spawn(move || monitor::run(monitor_app));

            // (改版) 自动自检更新与插件同步已禁用:本壳没有独立 Release
            // 渠道,也不允许任何自动覆盖/同步动作碰用户的 DSH 配置。
            Ok(())
        })
        .on_window_event(|window, event| {
            // 改版: X 关闭 = 最小化到托盘(不再每次弹窗); 通过托盘「退出壳」真正退出。
            // DSH 服务无论哪种都保留。
            if window.label() == "main" {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
