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
/// async + spawn_blocking: 两个 4 秒超时的网络探测绝不能占用主线程
/// (Tauri v2 同步命令直接跑在 UI 线程上, 会把窗口冻住)。
#[tauri::command]
async fn dsh_npm_probe() -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(dsh::npm_probe)
        .await
        .map_err(|e| e.to_string())
}

/// Frontend-invoked environment facts for the env panel.
/// async + spawn_blocking: env_info 里有 PowerShell 进程链查询(秒级)、
/// 目录大小遍历(上万文件)、node --version 探测 — 曾经是同步命令, 每次
/// ready 事件波都把主线程冻住数秒(窗口"未响应"的直接原因)。
#[tauri::command]
async fn env_info(app: AppHandle) -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(move || dsh::env_info(&app))
        .await
        .map_err(|e| e.to_string())
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
/// async + spawn_blocking: 同 env_info, 重组件不能冻结主线程。
#[tauri::command]
async fn diagnostic_export(app: AppHandle) -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
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
    })
    .await
    .map_err(|e| e.to_string())?
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
/// 传入 AppHandle 以获取主窗口 HWND, 使弹窗居中于父窗口。
/// 居中说明: MessageBoxW 从后台线程调用时, 即便传了 owner HWND, Windows 也
/// 不保证把弹窗摆到父窗口中央(401 认证墙弹窗曾偏到屏幕角落)。这里用经典
/// CBT hook 方案: 在调用线程装 WH_CBT 钩子, 弹窗 HCBT_ACTIVATE 时按父窗口
/// 矩形手动居中, 再卸载钩子 —— 与线程/前台状态无关, 稳定居中。
pub(crate) fn prompt_yes_no(app: &AppHandle, title: &str, text: &str) -> bool {
    #[cfg(windows)]
    unsafe {
        // 钩子回调需要跨 FFI 携带的父窗口矩形; 弹窗创建前记录, 回调中取用。
        // 用 Tauri 的窗口 bounds(逻辑像素 × DPI 缩放 = 物理像素), 不在回调里
        // 查 HWND —— WebView2 宿主线程外查 owner HWND 的 GetWindowRect 可能失败。
        static OWNER_RECT: std::sync::Mutex<(i32, i32, i32, i32)> =
            std::sync::Mutex::new((0, 0, 0, 0));

        // 显式链接 user32: 依赖树里 user32.lib 不总是进入链接输入
        // (windows 系 crate 多用 raw-dylib), 不声明会导致 GetWindowRect
        // 等符号 LNK2019。
        #[link(name = "user32")]
        extern "system" {
            fn MessageBoxW(
                hwnd: *const core::ffi::c_void,
                lp_text: *const u16,
                lp_caption: *const u16,
                u_type: u32,
            ) -> i32;
            fn SetWindowsHookExW(
                idHook: i32,
                lpfn: unsafe extern "system" fn(i32, usize, isize) -> isize,
                hmod: *const core::ffi::c_void,
                dwThreadId: u32,
            ) -> *mut core::ffi::c_void;
            fn UnhookWindowsHookEx(hhk: *mut core::ffi::c_void) -> i32;
            fn CallNextHookEx(
                hhk: *const core::ffi::c_void,
                nCode: i32,
                wParam: usize,
                lParam: isize,
            ) -> isize;
            fn GetWindowRect(hwnd: *const core::ffi::c_void, lprect: *mut Rect) -> i32;
            fn MoveWindow(
                hwnd: *const core::ffi::c_void,
                x: i32,
                y: i32,
                nWidth: i32,
                nHeight: i32,
                bRepaint: i32,
            ) -> i32;
            fn GetCurrentThreadId() -> u32;
        }

        #[repr(C)]
        struct Rect {
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
        }

        const HCBT_ACTIVATE: i32 = 5;

        unsafe extern "system" fn cbt_proc(code: i32, wparam: usize, _lparam: isize) -> isize {
            if code == HCBT_ACTIVATE {
                let dialog = wparam as *const core::ffi::c_void;
                let (l, t, r, b) = *OWNER_RECT.lock().unwrap_or_else(|e| e.into_inner());
                let mut dr = Rect { left: 0, top: 0, right: 0, bottom: 0 };
                if r > l && b > t && GetWindowRect(dialog, &mut dr) != 0 {
                    let dw = dr.right - dr.left;
                    let dh = dr.bottom - dr.top;
                    let x = l + ((r - l) - dw) / 2;
                    let y = t + ((b - t) - dh) / 2;
                    MoveWindow(dialog, x, y, dw, dh, 0);
                }
            }
            CallNextHookEx(std::ptr::null(), code, wparam, _lparam)
        }

        let text: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let caption: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        use raw_window_handle::HasWindowHandle;
        let hwnd = app
            .get_webview_window("main")
            .and_then(|w| {
                // WindowHandle 借用 w, 必须在闭包内立刻解出裸指针再返回。
                w.window_handle().ok().and_then(|h| match h.as_raw() {
                    raw_window_handle::RawWindowHandle::Win32(wh) =>
                        Some(wh.hwnd.get() as *const core::ffi::c_void),
                    _ => None,
                })
            })
            .unwrap_or(core::ptr::null());
        // 主窗口物理像素矩形: Tauri 的逻辑坐标 × DPI 缩放。
        let owner_rect = app
            .get_webview_window("main")
            .map(|w| {
                let scale = w.scale_factor().unwrap_or(1.0);
                let pos = w.outer_position().unwrap_or_default();
                let size = w.outer_size().unwrap_or_default();
                let (x, y) = (pos.x as f64 * scale, pos.y as f64 * scale);
                (
                    x as i32,
                    y as i32,
                    x as i32 + (size.width as f64 * scale) as i32,
                    y as i32 + (size.height as f64 * scale) as i32,
                )
            })
            .unwrap_or((0, 0, 0, 0));
        *OWNER_RECT.lock().unwrap_or_else(|e| e.into_inner()) = owner_rect;
        // WH_CBT(5), 线程局部钩子: dwThreadId 必须传「当前线程ID」。
        // 传 0 = 全局钩子, 而全局钩子要求 hMod 是 DLL 模块句柄, NULL 时
        // SetWindowsHookExW 直接失败(弹窗不居中的根因)。
        let hook = SetWindowsHookExW(5, cbt_proc, std::ptr::null(), GetCurrentThreadId());
        // MB_YESNO(0x4) | MB_ICONQUESTION(0x20) | MB_APPLMODAL(0) | MB_TOPMOST(0x40000)
        let answer = MessageBoxW(hwnd, text.as_ptr(), caption.as_ptr(), 0x4 | 0x20 | 0x40000);
        if !hook.is_null() {
            UnhookWindowsHookEx(hook);
        }
        answer == 6 // IDYES
    }
    #[cfg(not(windows))]
    {
        let _ = app;
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
                if payload.event() == PageLoadEvent::Finished {
                    let url = payload.url().to_string();
                    let app = webview.app_handle().clone();
                    // Auth wall check (attach mode): fires once per page load
                    // on the DSH origin — covers the bare attach URL AND the
                    // login-plugin flow (200 login page first, top-level 401
                    // after sign-in). Event-driven, no poll loop.
                    if url.starts_with("http://127.0.0.1:3080") {
                        std::thread::spawn(move || dsh::check_auth_wall_now(&app));
                    }
                    // F5 (or any webview reload) remounts the shell page, which
                    // missed the original `ready` emit — re-announce the current
                    // backend state so the fresh page doesn't sit on the boot
                    // spinner while the backend is actually up.
                    if !url.starts_with("about:") {
                        // Record the shell boot page's real URL (Windows serves
                        // at http://tauri.localhost) so navigate_shell can go
                        // back to it from the DSH webchat origin later.
                        if !url.starts_with("http://127.0.0.1:3080") {
                            dsh::record_shell_url(&url);
                        }
                        let app = webview.app_handle().clone();
                        std::thread::spawn(move || dsh::emit_current_status(&app));
                    }
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
            // 冷启动与托盘「重启 dsh web(后端)」同一套流程(清残留+spawn 自己),
            // 不再走 attach 裸 URL(那会撞 dsh-remote 登录页 + 401 死循环)。
            std::thread::spawn(move || dsh::cold_start(lifecycle));
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
