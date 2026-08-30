//! DSH (DeepSeek Harness) lifecycle: readiness probe, spawn, teardown.
//!
//! Launch strategy — local-first, download only on explicit consent:
//!   1. `DSH_CMD` env override (optional `DSH_CWD`) replaces the whole chain;
//!      for source-checkout development (`pnpm dsh web` in the repo).
//!   2. `dsh web` — a globally installed `dsh` found on PATH (npm/pnpm -g).
//!   3. Project-local install — `node_modules\.bin\dsh.cmd` searched in the
//!      exe's directory, the working directory, then the user profile.
//!   4. `npx @deepseek-ai/dsh web` — downloads the package, so it runs
//!      automatically only after the user picked "download" once (persisted
//!      in settings.json); otherwise a "notfound" event asks the user to
//!      choose download or exit.
//! Each candidate has its own readiness window; early exit or timeout falls
//! through to the next, and every attempt is logged to dsh.log and reported in
//! the final error. The shell either *attaches* to a DSH already listening on
//! 127.0.0.1:3080 or *spawns* its own tree; only a DSH we spawned is torn down
//! on exit, an attached instance is left untouched.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};
use uuid::Uuid;

const DSH_ORIGIN: &str = "http://127.0.0.1:3080";
const DSH_BASE: &str = "http://127.0.0.1:3080";

/// Substring anchor of the `dsh web` launch line that carries the
/// process-scoped browser-auth token, e.g.
/// `dsh web: http://127.0.0.1:3080/?token=<base64url>`.
/// DSH v0.1.2-alpha.1+ requires this token (or the signed cookie it mints) to
/// serve `/`; the token lives only in the launching process's stdout and in
/// memory, so the shell must capture it from the child's console tail on
/// self-launched runs. Attach runs have no child of ours — they rely on the
/// signed cookie minted on the first tokenized visit (30-day default).
const DSH_LAUNCH_URL_ANCHOR: &str = "http://127.0.0.1:3080/?token=";

/// Readiness window for the single-command `DSH_CMD` chain.
const DSH_CMD_WINDOW: Duration = Duration::from_secs(120);
/// Readiness window for the npx candidate: its first run downloads the full
/// package (500+ dependencies) before booting, which took over two minutes
/// in practice — five minutes leaves headroom for slow links.
const NPX_FIRST_RUN_WINDOW: Duration = Duration::from_secs(300);
/// Window for the global-`dsh` candidate: boot is fast, and a missing command
/// exits immediately instead of consuming the window.
const GLOBAL_WINDOW: Duration = Duration::from_secs(30);
/// Interval between readiness probes.
const PROBE_INTERVAL: Duration = Duration::from_secs(1);

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Local port the DSH web server listens on; also the anchor for finding an
/// attached instance's PID at restart time.
const DSH_PORT: u16 = 3080;

/// A DSH subprocess we spawned (and therefore own the lifecycle of).
struct DshInner {
    pid: u32,
}

/// True while teardown/restart intentionally kill the owned child — the
/// supervisor watcher must not treat those exits as crashes.
static INTENTIONAL_STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Consecutive short-lived respawns; the crash-loop guard's trip counter.
static QUICK_DEATHS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Managed state holding the owned subprocess, if any.
/// `None` ⇒ attached mode (do not kill on exit).
pub struct DshState {
    inner: Mutex<Option<DshInner>>,
}

impl DshState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }
}

/// One launch attempt in the candidate chain.
struct Candidate {
    /// Human label surfaced in `dsh-status` events and error reports.
    label: String,
    /// Shell command executed through `cmd /C`.
    cmd: String,
    /// Working directory for the child.
    cwd: String,
    /// How long this candidate may take to become ready.
    window: Duration,
}

/// Build the launch chain, local-first. `DSH_CMD` (with optional `DSH_CWD`)
/// leads but no longer replaces the chain — a stale override falls through to
/// the saved custom path, the PATH-global `dsh` (found via `where dsh`,
/// covering the npm global `dsh`/`dsh.cmd` pair), a project-local
/// `node_modules\.bin\dsh.cmd`, and the npx download the user consented to.
/// An empty chain means "no local DSH" — startup reports `notfound` and the
/// boot page asks the user.
/// The default working directory is the user profile — a neutral, writable
/// dir that never depends on a repo location. A stale `DSH_CWD` pointing at a
/// deleted directory falls back to the profile dir instead of failing every
/// spawn with "directory name invalid" (os error 267).
fn candidates() -> Vec<Candidate> {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".to_string());
    let cwd = std::env::var("DSH_CWD")
        .ok()
        .filter(|dir| Path::new(dir).is_dir())
        .unwrap_or_else(|| home.clone());
    let mut list = Vec::new();
    if let Ok(cmd) = std::env::var("DSH_CMD") {
        if !cmd.trim().is_empty() {
            // User-owned command string: never rewritten (also means the
            // rc.8+ auto browser-open is theirs to manage via --no-open).
            list.push(Candidate {
                label: "自定义启动命令".to_string(),
                cmd,
                cwd: cwd.clone(),
                window: DSH_CMD_WINDOW,
            });
        }
    }
    if let Some(path) = custom_dsh_path() {
        list.push(Candidate {
            label: format!("自定义路径({path})"),
            cmd: format!("\"{path}\" web{}", no_open_suffix(&path)),
            cwd: cwd.clone(),
            window: GLOBAL_WINDOW,
        });
    }
    if dsh_on_path() {
        // Absolute path: a GUI-started process can carry a PATH that cmd's
        // own re-resolution doesn't honor for bare `dsh` (2026-08-18 home
        // report: "'dsh' 不是内部或外部命令" from the GUI chain).
        let via = where_first("dsh").unwrap_or_else(|| "dsh".to_string());
        list.push(Candidate {
            label: "dsh web".to_string(),
            cmd: format!("\"{via}\" web{}", no_open_suffix(&via)),
            cwd: cwd.clone(),
            window: GLOBAL_WINDOW,
        });
    }
    if let Some((shim, root)) = find_local_install() {
        list.push(Candidate {
            label: format!("本地安装({})", root.display()),
            cmd: format!("\"{}\" web{}", shim.display(), no_open_suffix(&shim.display().to_string())),
            cwd: root.display().to_string(),
            window: GLOBAL_WINDOW,
        });
    }
    if prefer_npx() {
        list.push(npx_candidate(cwd));
    }
    list
}

/// Shell command string for invoking the dsh CLI with a subcommand (e.g.
/// `plugin --profile web add pkg@ver`), resolved through the same local-first
/// discovery as the launch chain. DSH_CMD is deliberately skipped: it is a
/// raw command string that may carry its own `web` argument and cannot be
/// reliably re-targeted. `None` when no usable dsh exists.
pub(crate) fn dsh_cli_command(sub: &str) -> Option<String> {
    if let Some(path) = custom_dsh_path() {
        return Some(format!("\"{path}\" {sub}"));
    }
    if dsh_on_path() {
        // Absolute shim path — immune to GUI-env PATH quirks (see candidates).
        let via = where_first("dsh").unwrap_or_else(|| "dsh".to_string());
        return Some(format!("\"{via}\" {sub}"));
    }
    if let Some((shim, _root)) = find_local_install() {
        return Some(format!("\"{}\" {sub}", shim.display()));
    }
    None
}

/// The official zero-install command; its first run downloads 500+
/// dependencies before booting.
/// ` --no-open` when the resolved dsh understands it, else empty. rc.8+
/// auto-opens the default browser on local `dsh web` — wrong for a desktop
/// shell that shows the webchat itself — but older builds reject unknown
/// options outright, so the flag is only appended after probing
/// `<dsh> web --help` for it. Cached per resolved path per session.
fn no_open_suffix(dsh_path: &str) -> &'static str {
    static CACHE: Mutex<Option<(String, bool)>> = Mutex::new(None);
    if let Ok(cache) = CACHE.lock() {
        if let Some((path, supported)) = cache.as_ref() {
            if path == dsh_path {
                return if *supported { " --no-open" } else { "" };
            }
        }
    }
    let supported = web_help_mentions_no_open(dsh_path);
    if let Ok(mut cache) = CACHE.lock() {
        *cache = Some((dsh_path.to_string(), supported));
    }
    if supported {
        supervision_log(&format!(
            "dsh at {dsh_path} supports --no-open (rc.8+); suppressing auto browser"
        ));
    }
    if supported { " --no-open" } else { "" }
}

/// Run `"<dsh>" web --help` hidden and check whether the flag family lists
/// --no-open. --help is safe on every version (unknown options only fail at
/// parse time).
fn web_help_mentions_no_open(dsh_path: &str) -> bool {
    let mut command = Command::new("cmd");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.raw_arg(format!("/S /C \"\"{dsh_path}\" web --help\""));
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        command.arg("-c").arg(format!("'{dsh_path}' web --help"));
    }
    let Ok(output) = command.output() else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout).to_string()
        + &String::from_utf8_lossy(&output.stderr);
    text.contains("--no-open")
}

fn npx_candidate(cwd: String) -> Candidate {
    Candidate {
        label: "npx 下载并启动".to_string(),
        cmd: "npx --yes @deepseek-ai/dsh web".to_string(),
        cwd,
        window: NPX_FIRST_RUN_WINDOW,
    }
}

/// True if a `dsh` command (npm/pnpm global install) resolves on PATH.
fn dsh_on_path() -> bool {
    let mut command = Command::new("where");
    command.arg("dsh");
    apply_no_window(&mut command);
    command.status().map(|s| s.success()).unwrap_or(false)
}

/// Directories searched for a project-local DSH install
/// (`node_modules\.bin\dsh.cmd`), in order: next to the exe (the "download
/// the exe, `pnpm add` beside it" setup), the working directory, then the
/// user profile. Returns the shim and its owning root.
fn find_local_install() -> Option<(PathBuf, PathBuf)> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            roots.push(dir.to_path_buf());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        let home = PathBuf::from(home);
        if !roots.contains(&home) {
            roots.push(home);
        }
    }
    roots.into_iter().find_map(|root| {
        let shim = root.join("node_modules").join(".bin").join("dsh.cmd");
        shim.is_file().then_some((shim, root))
    })
}

/// User preferences persisted beside dsh.log: `preferNpx` set once the user
/// picks "download" (later cold starts run npx directly), and `customDshPath`
/// a user-entered dsh location that outlives the notfound dialog. Best-effort
/// — an unreadable file just reads empty.
fn settings_path() -> PathBuf {
    log_path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("settings.json")
}

fn read_settings() -> Value {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_else(|| json!({}))
}

fn write_settings(map: Value) {
    let _ = std::fs::write(settings_path(), serde_json::to_string(&map).unwrap_or_default());
}

fn prefer_npx() -> bool {
    read_settings()
        .get("preferNpx")
        .and_then(|x| x.as_bool())
        .unwrap_or(false)
}

fn save_prefer_npx(value: bool) {
    let mut settings = read_settings();
    settings["preferNpx"] = json!(value);
    write_settings(settings);
}

/// User-entered dsh executable (dsh.cmd/dsh.exe) saved from the notfound
/// dialog; `None` when unset or the file no longer exists (self-healing).
fn custom_dsh_path() -> Option<String> {
    read_settings()
        .get("customDshPath")
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .filter(|path| Path::new(path).is_file())
}

/// Navigate the main window to a top-level URL. The webchat is loaded as a
/// TOP-LEVEL page — not an iframe — so cookies (such as the dsh-remote
/// session cookie with `SameSite=Lax`) behave first-party. An iframe would
/// be a third-party context where Chromium refuses to store/send Lax cookies
/// and login keeps bouncing back to the login page.
fn navigate_main(app: &AppHandle, url: &str) {
    if let Some(w) = app.get_webview_window("main") {
        if let Ok(u) = tauri::Url::parse(url) {
            let _ = w.navigate(u);
        }
    }
}

/// Extract the authenticated webchat URL (`http://127.0.0.1:3080/?token=…`)
/// from the child's console tail, if DSH (v0.1.2-alpha.1+) printed it.
/// Older DSH versions without the browser-auth layer print no such line and
/// the caller falls back to the bare `DSH_BASE`. Only the SELF-LAUNCHED
/// child's tail is consulted — attach runs must NOT read the tail (it may
/// hold a stale token from a previous run of ours; the signed cookie minted
/// on the first tokenized visit covers attach runs instead).
fn resolve_launch_url() -> Option<String> {
    for line in child_tail_last(CHILD_TAIL_CAP) {
        if let Some(pos) = line.find(DSH_LAUNCH_URL_ANCHOR) {
            let rest = &line[pos + DSH_LAUNCH_URL_ANCHOR.len()..];
            // token = base64url: [A-Za-z0-9_-]+
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
                .unwrap_or(rest.len());
            if end > 0 {
                return Some(format!("http://127.0.0.1:3080/?token={}", &rest[..end]));
            }
        }
    }
    None
}

/// Top-level navigation to the webchat.
/// - `spawned=true` (we launched the child): prefer the tokenized URL parsed
///   from the child's console tail (new DSH browser auth), else the bare
///   origin (legacy DSH).
/// - `spawned=false` (attach): bare origin only — never the tail, whose
///   content belongs to a previous run and whose token is stale. The signed
///   cookie minted on the first tokenized visit covers this run.
fn navigate_webchat(app: &AppHandle, spawned: bool) {
    if spawned {
        // The launch-URL line is printed by `dsh web` early, but the pipe
        // reader may still be draining it when the readiness probe wins —
        // a short bounded retry covers that without the multi-second spin.
        for _ in 0..5 {
            if let Some(url) = resolve_launch_url() {
                navigate_main(app, &url);
                return;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    navigate_main(app, DSH_BASE);
}

/// Back to the shell boot page (tauri://localhost on release builds).
fn navigate_shell(app: &AppHandle) {
    navigate_main(app, "tauri://localhost/index.html");
}

/// Probe 3080 once; true when ANY HTTP server answers on the port.
/// Deliberately NOT the `host.describe` RPC envelope: an auth layer such as
/// dsh-remote answers 403 without a session cookie while the DSH web is
/// perfectly alive — 401/403/404 all prove a server is listening and served
/// (the shell then attaches and the embedded webchat shows the login page).
/// A plain TCP connect stays insufficient (an open port is not necessarily
/// DSH).
fn probe_ready_once() -> bool {
    match ureq::get(&format!("{DSH_BASE}/"))
        .timeout(Duration::from_secs(3))
        .call()
    {
        Ok(_) => true,                          // 2xx/3xx: served
        Err(ureq::Error::Status(_, _)) => true, // 401/403/…: an auth wall still proves the server runs
        Err(_) => false,                        // connection refused / timeout
    }
}

/// True once the auth-wall takeover flow has fired this run — guards against
/// duplicate prompts when `on_page_load` fires across the multi-stage
/// navigation (login plugin 200 page → login → DSH 401).
static AUTH_WALL_HANDLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Event-driven auth-wall check, invoked from lib.rs `on_page_load` whenever
/// the webview lands on a DSH origin. DSH v0.1.2-alpha.1+ renders a plain-text
/// 401 page ("dsh web authentication required") in the shell's own WebView2
/// when no signed cookie is present — exactly what the user sees. An HTTP
/// probe is blind to this through a front-auth plugin, so read the rendered
/// page back: inject JS that mirrors the marker into BOTH the document.title
/// and the location.hash (the hash is always readable back through `w.url()`),
/// then Rust checks the window title and the URL. When detected, offer to
/// restart the host (shell takes over and self-launches a DSH whose token it
/// can capture) or quit.
///
/// Event-driven (no poll loop) because the navigation itself carries the
/// signal: `on_page_load` fires per page load, and the dsh-remote multi-stage
/// flow (200 login page first, then a top-level 401 after sign-in) is each a
/// separate navigation, so this fires again exactly when the 401 surfaces.
pub(crate) fn check_auth_wall_now(app: &AppHandle) {
    // Attach mode only: a self-launched instance holds the token and never
    // shows the wall.
    if app.state::<DshState>().inner.lock().unwrap().is_some() {
        return;
    }
    if AUTH_WALL_HANDLED.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        // Let the just-finished page load render its body before the read.
        std::thread::sleep(Duration::from_millis(200));
        let Some(w) = app.get_webview_window("main") else {
            return;
        };
        let js = "(function(){ \
                   try { \
                     var b = document.body; \
                     var t = b && (b.innerText || b.textContent) || ''; \
                     if (t.indexOf('dsh web authentication required') >= 0) { \
                       document.title = '__DSH_AUTH_WALL__'; \
                       try { location.hash = 'dsh-shell-auth-wall'; } catch (e2) {} \
                     } \
                   } catch (e) {} \
                 })();";
        let _ = w.eval(js);
        std::thread::sleep(Duration::from_millis(150));
        let title = w.title().unwrap_or_default();
        let url = w.url().map(|u| u.to_string()).unwrap_or_default();
        if title.contains("__DSH_AUTH_WALL__") || url.contains("dsh-shell-auth-wall") {
            // Claim the guard before showing the modal; a concurrent page-load
            // invocation must not stack a second prompt.
            if AUTH_WALL_HANDLED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let take_over = crate::prompt_yes_no(
                "DSH 需要浏览器认证",
                concat!(
                    "检测到 DSH 需要浏览器令牌(新版本认证),但当前实例不是本应用启动的,无法获取令牌。\n\n",
                    "「是」重启 dsh web 宿主(由本应用接管,自动完成认证)\n",
                    "「否」退出本应用(保留当前 DSH 实例)",
                ),
            );
            if take_over {
                // 先清掉 3080 上的旧实例再回 boot 页,否则 boot 页加载后
                // emit_current_status 探测到旧实例仍在会 emit `ready`,
                // 用户看不到《正在启动 DSH…》的重启反馈。
                teardown(&app); // attach 模式无自有子进程, no-op
                kill_port_listeners();
                navigate_shell(&app);
                // 等旧实例完全退出(端口释放 + 进程树收尾)再自启,避免新
                // 实例撞上旧残留(usage-billing writer / TIME_WAIT / 锁
                // 文件)而崩溃。端口一停即继续,不额外固定 sleep。
                let deadline = Instant::now() + Duration::from_secs(10);
                while probe_ready_once() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(500));
                }
                startup(app.clone());
            } else {
                app.exit(0);
            }
        }
    });
}

/// Outcome of one candidate attempt.
enum Attempt {
    Ready,
    Failed(String),
}


/// Emit `ready` repeatedly for a short window instead of once. The boot page's
/// listener registers only after the embedded webview finishes loading, which
/// races the fast local readiness probe — a one-shot emit can be missed and
/// leave the window stuck on the boot spinner.
/// Re-announce the backend state to a freshly (re)loaded shell page. F5
/// reloads the webview; the fresh frontend missed the original `ready` emit
/// and would sit on the boot spinner forever while the backend is actually
/// up — the backend itself never restarted. Idempotent with the normal
/// startup flow (the frontend collapses ready re-emits into transitions).
pub(crate) fn emit_current_status(app: &AppHandle) {
    // Let the fresh page's listener register first.
    std::thread::sleep(Duration::from_millis(600));
    for _ in 0..5 {
        if probe_ready_once() {
            let attached = app.state::<DshState>().inner.lock().unwrap().is_none();
            let _ = app.emit(
                "dsh-status",
                json!({ "status": "ready", "attached": attached, "method": "页面重载" }),
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    // Backend not answering: leave the UI to the normal startup/supervision
    // flow — don't invent state here.
}

fn emit_ready(app: &AppHandle, attached: bool, method: Option<String>) {    let app2 = app.clone();
    std::thread::spawn(move || {
        for _ in 0..10 {
            let mut payload = json!({ "status": "ready", "attached": attached });
            if let Some(m) = &method {
                payload["method"] = json!(m);
            }
            let _ = app2.emit("dsh-status", payload);
            std::thread::sleep(Duration::from_millis(400));
        }
    });
}

/// Drive startup from a background thread. Emits `dsh-status` events:
/// `{status:"starting",method}` per attempt, then either
/// `{status:"ready",attached,method}` or `{status:"error",message}` with every
/// attempt's failure reason.
pub fn startup(app: AppHandle) {
    // Attach path: DSH already up — never spawn, never kill on exit.
    if probe_ready_once() {
        emit_ready(&app, true, None);
        // Attach: bare URL only (cookie from an earlier tokenized visit
        // covers this run; the tail belongs to no child of ours). The
        // auth-wall check is event-driven from lib.rs `on_page_load` — it
        // fires once per page load and only acts when DSH's real 401 page
        // appears, covering the login-plugin multi-stage flow too.
        navigate_webchat(&app, false);
        return;
    }

    let mut failures: Vec<String> = Vec::new();
    let chain = candidates();
    // No local DSH and no consented download: hand the choice to the user
    // instead of silently pulling 500+ dependencies.
    if chain.is_empty() {
        let _ = app.emit("dsh-status", json!({ "status": "notfound" }));
        return;
    }
    for candidate in chain {
        let _ = app.emit(
            "dsh-status",
            json!({ "status": "starting", "method": candidate.label }),
        );
        match try_candidate(&app, &candidate) {
            Attempt::Ready => return,
            Attempt::Failed(reason) => {
                failures.push(format!("「{}」{}", candidate.label, reason));
            }
        }
    }
    let _ = app.emit(
        "dsh-status",
        json!({
            "status": "error",
            "message": format!("所有启动方式均失败:\n{}", failures.join("\n")),
        }),
    );
}

/// Spawn one candidate and poll until ready, early exit, or window expiry.
/// One candidate attempt: spawn, wait for readiness, early-exit, or window
/// expiry. The child's console tail is kept in memory — on early exit it
/// names the real cause (e.g. pnpm's fresh-release cooldown silently
/// skipping a profile bundle → "cannot resolve profile bundle"), and a
/// missing-bundle death triggers one cooldown-bypassed repair + respawn of
/// the same candidate before giving up.
fn try_candidate(app: &AppHandle, candidate: &Candidate) -> Attempt {
    log_attempt(candidate);
    for attempt in 0..2 {
        child_tail_clear();
        let mut child = match spawn_command(&candidate.cmd, &candidate.cwd) {
            Ok(c) => c,
            Err(e) => return Attempt::Failed(format!("无法启动({e})\n")),
        };
        let pid = child.id();
        let deadline = Instant::now() + candidate.window;
        loop {
            if probe_ready_once() {
                // The state keeps only the pid (for teardown's tree kill); the
                // Child handle moves to the supervisor thread, which reaps the
                // process and reacts to unexpected exits.
                let state = app.state::<DshState>();
                *state.inner.lock().unwrap() = Some(DshInner { pid });
                INTENTIONAL_STOP.store(false, std::sync::atomic::Ordering::Relaxed);
                emit_ready(app, false, Some(candidate.label.clone()));
                navigate_webchat(app, true);
                let app2 = app.clone();
                std::thread::spawn(move || supervise_child(app2, child, pid, Instant::now()));
                return Attempt::Ready;
            }
            // A missing command exits immediately; surface that instead of
            // waiting out the whole window — with the child's own last words.
            match child.try_wait() {
                Ok(Some(status)) => {
                    let tail = child_tail_last(8);
                    let excerpt = child_tail_excerpt(4);
                    if !excerpt.is_empty() {
                        log_write(
                            LogLevel::Error,
                            &format!("candidate died early ({status}); last output: {excerpt}"),
                        );
                    }
                    // File-level self-heals before signature matching: a
                    // corrupt settings.yaml (YAML writer dropped the colon
                    // space) kills every candidate identically, so check the
                    // file itself rather than the child's wording.
                    if attempt == 0 && settings_yaml_selfheal() {
                        supervision_log("settings.yaml repaired; retrying the same candidate");
                        break; // respawn the candidate once
                    }
                    if attempt == 0 && !tail.is_empty() {
                        if let Some(pkg) = missing_bundle(&tail) {
                            if repair_profile_bundle(&pkg) {
                                supervision_log(&format!(
                                    "bundle repair installed {pkg}; retrying the same candidate"
                                ));
                                break; // respawn the candidate once
                            }
                            return Attempt::Failed(format!(
                                "进程提前退出({status})——缺少 profile 包 {pkg}(疑似 pnpm 新发布冷却期拦截)\n手动修复:在 %USERPROFILE%\\.dsh\\profiles\\web 执行 pnpm add --save-exact {pkg} --config.minimumReleaseAge=0\n完整日志:面板→日志,或 %LOCALAPPDATA%\\dsh-desktop\\dsh.log\n{}",
                                tail.join("\n")
                            ));
                        }
                    }
                    let excerpt = if tail.is_empty() {
                        String::new()
                    } else {
                        format!("\n{}", tail.join("\n"))
                    };
                    return Attempt::Failed(format!("进程提前退出({status}){excerpt}\n"));
                }
                Ok(None) => {}
                Err(e) => return Attempt::Failed(format!("无法查询子进程({e})\n")),
            }
            if Instant::now() >= deadline {
                kill_tree(pid);
                let _ = child.wait();
                let excerpt = child_tail_excerpt(4);
                return Attempt::Failed(format!(
                    "就绪超时{}\n",
                    if excerpt.is_empty() {
                        String::new()
                    } else {
                        format!(",最后输出: {excerpt}")
                    }
                ));
            }
            std::thread::sleep(PROBE_INTERVAL);
        }
    }
    unreachable!("repair path always returns or respawns exactly once")
}

/// Rolling tail of the current DSH child's console output. The full stream
/// is deliberately NOT written to dsh.log (unbounded, and DSH keeps its own
/// logs) — but crash causes like "cannot resolve profile bundle" only ever
/// appear on the child's stderr, so the last lines stay in memory for error
/// payloads and the auto-repair signature.
static CHILD_TAIL: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
const CHILD_TAIL_CAP: usize = 60;

fn child_tail_push(line: String) {
    if let Ok(mut tail) = CHILD_TAIL.lock() {
        if tail.len() >= CHILD_TAIL_CAP {
            tail.pop_front();
        }
        tail.push_back(line);
    }
}

/// Last `n` lines of the child's console output, oldest first.
pub(crate) fn child_tail_last(n: usize) -> Vec<String> {
    CHILD_TAIL
        .lock()
        .map(|t| t.iter().rev().take(n).rev().cloned().collect())
        .unwrap_or_default()
}

fn child_tail_clear() {
    if let Ok(mut tail) = CHILD_TAIL.lock() {
        tail.clear();
    }
}

/// One-line digest of the tail for log lines.
pub(crate) fn child_tail_excerpt(max_lines: usize) -> String {
    child_tail_last(max_lines).join(" ⏎ ")
}

/// Keep the tail ring fed; runs on its own thread per stream and ends at
/// EOF when the child dies.
fn pump_tail<R: std::io::Read>(stream: R) {
    use std::io::BufRead;
    for line in std::io::BufReader::new(stream).lines() {
        match line {
            Ok(l) => child_tail_push(l),
            Err(_) => break,
        }
    }
}

/// The package named by dsh's fatal `cannot resolve profile bundle "<pkg>"`
/// — the signature of a bundle pnpm's fresh-release cooldown silently
/// skipped during install (seen 2026-08-18: exit-0 sync, missing bundle,
/// crash loop with zero UI feedback).
fn missing_bundle(tail: &[String]) -> Option<String> {
    const MARKER: &str = "cannot resolve profile bundle \"";
    for line in tail.iter().rev() {
        if let Some(pos) = line.find(MARKER) {
            let rest = &line[pos + MARKER.len()..];
            if let Some(end) = rest.find('"') {
                let pkg = &rest[..end];
                if !pkg.is_empty() {
                    return Some(pkg.to_string());
                }
            }
        }
    }
    None
}

/// Bundle repair runs at most once per app session — a failing repair must
/// not turn into a second crash loop.
static BUNDLE_REPAIR_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Install a missing bundle into the web profile with the fresh-release
/// cooldown bypassed — the same override plugin sync uses. Returns whether
/// the caller should retry the candidate.
fn repair_profile_bundle(pkg: &str) -> bool {
    if BUNDLE_REPAIR_DONE.swap(true, std::sync::atomic::Ordering::SeqCst) {
        log_write(
            LogLevel::Warn,
            "[dsh-desktop] bundle repair skipped: already attempted this session",
        );
        return false;
    }
    // Strict name sanity — this string is headed for a shell command.
    let clean = pkg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '/' | '-' | '.' | '_'));
    if !clean {
        log_write(
            LogLevel::Warn,
            &format!("[dsh-desktop] bundle repair skipped: odd package name {pkg:?}"),
        );
        return false;
    }
    let Some(pnpm) = pnpm_path() else {
        log_write(LogLevel::Warn, "[dsh-desktop] bundle repair skipped: pnpm not found");
        return false;
    };
    let profile_dir = dsh_home().join("profiles").join("web");
    if !profile_dir.is_dir() {
        log_write(
            LogLevel::Warn,
            "[dsh-desktop] bundle repair skipped: web profile dir missing",
        );
        return false;
    }
    supervision_log(&format!("bundle repair: pnpm add {pkg} into web profile (cooldown bypassed)"));
    let cmd = format!(
        "cd /d \"{}\" && \"{}\" add --save-exact {pkg} --config.minimumReleaseAge=0",
        profile_dir.display(),
        pnpm.display()
    );
    run_bounded(&cmd, Duration::from_secs(300), "bundle repair")
}

/// Settings self-heal runs at most once per app session.
static SETTINGS_HEAL_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Self-heal `~/.dsh/settings.yaml` when a YAML writer dropped the space
/// after a mapping colon (`key:value` — invalid YAML). 2026-08-18 incident:
/// the web UI's serializer emitted `reasoningEfforts:max`, the hot reload
/// crash-looped dsh web, and the desktop showed an empty window with endless
/// startup retries. Guarded by a full parse both before (is it actually
/// broken?) and after (did the fix make it valid?) — a fix that still fails
/// to parse is refused, and the original is always backed up first.
fn settings_yaml_selfheal() -> bool {
    if SETTINGS_HEAL_DONE.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return false;
    }
    let path = dsh_home().join("settings.yaml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    if yaml_parses(&text) {
        return false; // healthy — the crash is something else
    }
    let backup = path.with_extension("yaml.dshbak");
    let _ = std::fs::copy(&path, &backup);
    let fixed = fix_yaml_colon_spacing(&text);
    if !yaml_parses(&fixed) {
        log_write(
            LogLevel::Error,
            &format!(
                "[dsh-desktop] settings.yaml 解析失败且冒号空格修复无效;手动编辑 {} 保证每个 key: 后有空格(原文件已备份 {})",
                path.display(),
                backup.display()
            ),
        );
        return false;
    }
    match std::fs::write(&path, &fixed) {
        Ok(()) => {
            log_write(
                LogLevel::Warn,
                &format!(
                    "[dsh-desktop] settings.yaml 自动修复:为缺失空格的 key: 补空格(疑似 Web UI 序列化器缺陷,建议反馈);原文件备份 {}",
                    backup.display()
                ),
            );
            true
        }
        Err(e) => {
            log_write(
                LogLevel::Error,
                &format!("[dsh-desktop] settings.yaml 修复写入失败:{e}"),
            );
            false
        }
    }
}

/// Full-file YAML parse check (round-trip guard: nothing invalid is ever
/// written back).
fn yaml_parses(text: &str) -> bool {
    serde_yaml::from_str::<serde_yaml::Value>(text).is_ok()
}

/// Insert the missing space in block-mapping `key:value` lines. Lines inside
/// literal/indented block scalars (`|` or `>`) are left untouched so string
/// content is never rewritten.
fn fix_yaml_colon_spacing(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut block_indent: Option<usize> = None;
    for line in text.lines() {
        let indent = line.len() - line.trim_start().len();
        if let Some(depth) = block_indent {
            if line.trim().is_empty() || indent > depth {
                out.push_str(line);
                out.push('\n');
                continue;
            }
            block_indent = None;
        }
        if let Some(fixed) = fix_one_colon(line) {
            out.push_str(&fixed);
        } else {
            let trimmed = line.trim_end();
            if trimmed.ends_with('|') || trimmed.ends_with('>') {
                block_indent = Some(indent);
            }
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// `key:value` → `key: value` for one line, when the first colon is
/// immediately followed by a non-space, non-comment character. Quoted keys
/// are left alone (conservative).
fn fix_one_colon(line: &str) -> Option<String> {
    let indent_len = line.len() - line.trim_start().len();
    let rest = &line[indent_len..];
    let rest = rest.strip_prefix("- ").unwrap_or(rest);
    let colon = rest.find(':')?;
    let (key, value) = rest.split_at(colon);
    let value = &value[1..];
    if value.is_empty() || value.starts_with(' ') || value.starts_with('\t') || value.starts_with('#') {
        return None;
    }
    if key.trim().is_empty() || key.contains('"') || key.contains('\'') {
        return None;
    }
    let prefix_len = line.len() - rest.len();
    Some(format!(
        "{}{}: {}",
        &line[..prefix_len],
        key,
        value
    ))
}

/// pnpm as an absolute path: PATH first, then the npm-global fallback (GUI
/// processes can start with a PATH that doesn't resolve .cmd shims).
fn pnpm_path() -> Option<PathBuf> {
    where_first("pnpm")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("APPDATA")
                .ok()
                .map(|a| Path::new(&a).join("npm").join("pnpm.cmd"))
                .filter(|p| p.is_file())
        })
}

/// Run one hidden shell command with a hard kill at the cap (repairs can be
/// big installs; nothing downstream may stall on them).
fn run_bounded(cmd: &str, cap: Duration, what: &str) -> bool {
    let mut command = Command::new("cmd");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.raw_arg(format!("/S /C \"{cmd}\""));
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        command.arg("/C").arg(cmd);
    }
    command.env("CI", "true");
    let Ok(mut child) = command.spawn() else {
        log_write(
            LogLevel::Warn,
            &format!("[dsh-desktop] {what} spawn failed: {cmd}"),
        );
        return false;
    };
    let deadline = Instant::now() + cap;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let ok = status.success();
                log_write(
                    if ok { LogLevel::Info } else { LogLevel::Warn },
                    &format!(
                        "[dsh-desktop] {what} {} (exit {})",
                        if ok { "ok" } else { "FAILED" },
                        status.code().unwrap_or(-1)
                    ),
                );
                return ok;
            }
            Ok(None) if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                log_write(LogLevel::Warn, &format!("[dsh-desktop] {what} timed out ({cap:?}, killed)"));
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(500)),
            Err(e) => {
                log_write(LogLevel::Warn, &format!("[dsh-desktop] {what} wait failed: {e}"));
                return false;
            }
        }
    }
}

/// Log severity for the shell's own event log.
#[derive(Clone, Copy)]
pub(crate) enum LogLevel {
    Info,
    Warn,
    Error,
}

impl LogLevel {
    fn tag(self) -> &'static str {
        match self {
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }
}

/// Local time via GetLocalTime — this is a Windows-only shell, so raw FFI
/// beats pulling a datetime crate. Returns (date, time, filename stamp):
/// ("2026-08-18", "12:40:03", "2026-08-18T12-40-03-123").
#[cfg(windows)]
pub(crate) fn local_time_parts() -> (String, String, String) {
    #[repr(C)]
    struct SysTime {
        year: u16,
        month: u16,
        _dow: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        millis: u16,
    }
    extern "system" {
        fn GetLocalTime(system_time: *mut SysTime);
    }
    let mut st = SysTime {
        year: 0,
        month: 0,
        _dow: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        millis: 0,
    };
    unsafe { GetLocalTime(&mut st) };
    (
        format!("{:04}-{:02}-{:02}", st.year, st.month, st.day),
        format!("{:02}:{:02}:{:02}", st.hour, st.minute, st.second),
        format!(
            "{:04}-{:02}-{:02}T{:02}-{:02}-{:02}-{:03}",
            st.year, st.month, st.day, st.hour, st.minute, st.second, st.millis
        ),
    )
}

#[cfg(not(windows))]
pub(crate) fn local_time_parts() -> (String, String, String) {
    ("1970-01-01".into(), "00:00:00".into(), "1970-01-01T00-00-00-000".into())
}

/// Append one raw line to the shared log (no timestamp/level) — session
/// banner lines only.
fn log_raw(line: &str) {
    use std::io::Write;
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{line}");
    }
}

/// The one writer for shell events: `[YYYY-MM-DD HH:MM:SS] [LEVEL] message`.
/// Everything the shell wants remembered goes through here so the log tab
/// and diagnostic bundles read uniformly. DSH's own server output is NOT
/// logged (it keeps its own logs under ~/.dsh/logs) — this log records only
/// the shell's startup and runtime events, a few dozen lines per session.
pub(crate) fn log_write(level: LogLevel, message: &str) {
    use std::io::Write;
    let (date, time, _) = local_time_parts();
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "[{date} {time}] [{}] {message}", level.tag());
    }
}

/// Shell event at Info level (shorthand for the common case).
fn supervision_log(line: &str) {
    log_write(LogLevel::Info, line);
}

/// Shell event at Warn level.
fn supervision_warn(line: &str) {
    log_write(LogLevel::Warn, line);
}

/// First `where <name>` hit, windowless.
fn where_first(name: &str) -> Option<String> {
    let mut command = Command::new("where");
    command.arg(name);
    apply_no_window(&mut command);
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
}

/// One captured run of a program (`node --version` style). No-window flags
/// matter here: env_info now runs at startup, and a flashing console per
/// probe (node/powershell) would pop terminals on every launch.
fn run_capture(program: &str, args: &[&str]) -> Option<String> {
    use std::io::Read;
    let mut command = Command::new(program);
    command.args(args);
    apply_no_window(&mut command);
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    let _ = child.wait();
    let trimmed = out.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Pid currently listening on the DSH port.
fn port_listener_pid() -> Option<u32> {
    let mut command = Command::new("netstat");
    command.args(["-ano", "-p", "tcp"]);
    apply_no_window(&mut command);
    let output = command.output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let suffix = format!(":{DSH_PORT}");
    text.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        (fields.len() >= 5 && fields[0] == "TCP" && fields[1].ends_with(&suffix) && fields[4] != "0")
            .then(|| fields[4].parse::<u32>().ok())
            .flatten()
    })
}

/// Who owns the DSH port: pid, command line, and whether the parent chain
/// leads back to this app (ours) or to an external instance. PowerShell does
/// the chain walk; JSON keeps the boundary parse-free.
fn port_owner_info() -> Option<Value> {
    let pid = port_listener_pid()?;
    let script = format!(
        "$p = Get-CimInstance Win32_Process -Filter 'ProcessId={pid}'; \
         if ($null -eq $p) {{ exit 1 }}; \
         $names = @($p.Name); $cur = $p; $owned = $false; \
         for ($i = 0; $i -lt 5 -and $null -ne $cur; $i++) {{ \
           if ($cur.Name -eq 'dsh-desktop-windowos.exe') {{ $owned = $true; break }}; \
           $cur = Get-CimInstance Win32_Process -Filter ('ProcessId=' + $cur.ParentProcessId); \
           if ($null -ne $cur) {{ $names += $cur.Name }} \
         }}; \
         [pscustomobject]@{{ pid = $p.ProcessId; cmd = $p.CommandLine; chain = ($names -join ' <- '); owned = $owned }} | ConvertTo-Json -Compress"
    );
    let output = run_capture("powershell", &["-NoProfile", "-Command", &script])?;
    serde_json::from_str(&output).ok()
}

/// Plugin package versions installed in the user's web profile.
fn profile_plugin_versions() -> Value {
    let read_version = |name: &str| -> Value {
        let manifest = dsh_home()
            .join("profiles")
            .join("web")
            .join("node_modules")
            .join(name)
            .join("package.json");
        std::fs::read_to_string(manifest)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|doc| doc["version"].as_str().map(str::to_string))
            .map(Value::String)
            .unwrap_or(Value::Null)
    };
    json!({
        "dshDesktopPlugin": read_version("dsh-desktop-plugin"),
        "dshmarket": read_version("dshmarket"),
    })
}

/// Last `n` lines of the shared shell log for the console pane.
pub(crate) fn log_tail(n: usize) -> Vec<String> {
    std::fs::read_to_string(log_path())
        .map(|text| {
            let lines: Vec<&str> = text.lines().collect();
            let start = lines.len().saturating_sub(n);
            lines[start..].iter().map(|l| l.to_string()).collect()
        })
        .unwrap_or_default()
}

/// Total bytes under `dir`, bounded: the walk stops counting past 50k files
/// (returns the partial sum) so a huge profile tree can't stall env_info.
fn dir_size_bounded(dir: &Path) -> Option<u64> {
    fn walk(dir: &Path, seen: &mut u32, total: &mut u64) {
        const MAX_FILES: u32 = 50_000;
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            if *seen >= MAX_FILES {
                return;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                walk(&entry.path(), seen, total);
            } else {
                *seen += 1;
                *total += meta.len();
            }
        }
    }
    if !dir.is_dir() {
        return None;
    }
    let (mut seen, mut total) = (0u32, 0u64);
    walk(dir, &mut seen, &mut total);
    Some(total)
}

/// Environment facts for the env panel, modelled on Comfy Desktop's
/// StatusFactPanel data shape: every field is gathered independently and
/// degrades to null — the panel never hangs on a probe.
pub fn env_info(app: &AppHandle) -> Value {
    let settings = read_settings();
    let profile_dir = dsh_home().join("profiles").join("web");
    let install_dir = tauri::utils::platform::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.display().to_string()));
    let info = json!({
        "app": {
            "version": app.package_info().version.to_string(),
            "installDir": install_dir,
        },
        "dsh": {
            "portAnswering": probe_ready_once(),
            "owner": port_owner_info(),
            "dshCmd": std::env::var("DSH_CMD").ok().filter(|v| !v.trim().is_empty()),
            "dshCwd": std::env::var("DSH_CWD").ok().filter(|v| !v.trim().is_empty()),
            "customPath": settings.get("customDshPath").and_then(Value::as_str),
            "whereDsh": where_first("dsh"),
            "localInstall": find_local_install().map(|(shim, root)| json!({ "shim": shim.display().to_string(), "root": root.display().to_string() })),
            "preferNpx": prefer_npx(),
        },
        "node": {
            "path": where_first("node"),
            "version": run_capture("node", &["--version"]),
        },
        "plugins": profile_plugin_versions(),
        "profileDir": profile_dir.display().to_string(),
        "logDir": log_path().parent().map(|p| p.display().to_string()),
        // Where the DSH child actually runs: explicit override, else the
        // effective default (user profile).
        "workspaceDir": std::env::var("DSH_CWD")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| std::env::var("USERPROFILE").ok())
            .filter(|v| !v.is_empty()),
        "cacheDir": Option::<String>::None,
        "profileSizeBytes": dir_size_bounded(&profile_dir),
        "logTail": log_tail(25),
    });
    // The probes run windowless; leave a breadcrumb in the shared log so the
    // panel's log tab shows that a gather just happened (and when).
    supervision_log("env_info gathered (port/where/node/plugins)");
    info
}

/// Watch the owned DSH child and heal the stack when it dies unexpectedly
/// (a DSH crash, or dshmarket's self-restart killing the host for an update).
/// Expected exits (teardown / tray restart) set INTENTIONAL_STOP first.
fn supervise_child(app: AppHandle, mut child: Child, pid: u32, spawned_at: Instant) {
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(_) => return,
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let _ = child.wait(); // reap
    if INTENTIONAL_STOP.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    supervision_log(&format!("supervised dsh web (pid {pid}) exited unexpectedly; healing"));
    // The child's last words name the real cause (e.g. a cooldown-skipped
    // profile bundle) — keep them in the log and in user-facing errors.
    let excerpt = child_tail_excerpt(6);
    if !excerpt.is_empty() {
        log_write(LogLevel::Error, &format!("[child] {excerpt}"));
    }
    // Crash-loop guard: three consecutive children that lived under 30s stop
    // the auto-respawn and surface an error instead of spinning forever.
    if spawned_at.elapsed() < Duration::from_secs(30) {
        let deaths = QUICK_DEATHS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if deaths >= 3 {
            supervision_warn(&format!("supervised dsh web (pid {pid}) crashed 3x quickly; auto-respawn stopped"));
            let _ = app.emit(
                "dsh-status",
                json!({ "status": "error", "message": format!("DSH 反复意外退出,已停止自动重启{}\n完整日志:面板→日志,或 %LOCALAPPDATA%\\dsh-desktop\\dsh.log;可用托盘「重启 dsh web(后端)」重试", if excerpt.is_empty() { String::new() } else { format!("\n最近输出: {excerpt}") }) }),
            );
            return;
        }
    } else {
        QUICK_DEATHS.store(0, std::sync::atomic::Ordering::SeqCst);
    }
    // dshmarket's restart helper races a replacement onto 3080 ~1.5s after
    // the host dies; that replacement is an orphan outside our supervision
    // (a later quit would not stop it), so let it land, clear the port, and
    // spawn our own child instead.
    std::thread::sleep(Duration::from_millis(2500));
    kill_port_listeners();
    let deadline = Instant::now() + Duration::from_secs(10);
    while probe_ready_once() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
    }
    // startup() re-emits `ready`, and the persistent shell reloads its
    // webchat iframe on that event — no window.eval navigation to a page
    // we no longer control.
    startup(app.clone());
}

/// Frontend "使用此路径启动" from the notfound dialog: remember the
/// user-entered dsh executable and retry startup with it leading the chain.
/// Returns Err with a user-facing message when the path does not exist.
pub fn set_custom_path(app: &AppHandle, raw: String) -> Result<(), String> {
    let path = raw.trim().trim_matches('"').to_string();
    if path.is_empty() {
        return Err("路径不能为空".to_string());
    }
    if !Path::new(&path).is_file() {
        return Err(format!("找不到文件:{path}(需要 dsh.cmd 或 dsh.exe 的完整路径)"));
    }
    let mut settings = read_settings();
    settings["customDshPath"] = json!(path);
    write_settings(settings);
    teardown(app);
    let app2 = app.clone();
    std::thread::spawn(move || startup(app2));
    Ok(())
}

/// The two npm registries the one-click installer may use, probed in
/// parallel at boot; the faster becomes the default and the UI offers the
/// other as an explicit choice.
pub const REGISTRY_NPMJS: &str = "https://registry.npmjs.org";
pub const REGISTRY_NPMMIRROR: &str = "https://registry.npmmirror.com";

/// Time one GET of the package's /latest metadata in ms; None when the
/// registry is unreachable within 4s.
fn probe_registry_ms(base: &str) -> Option<u128> {
    let start = std::time::Instant::now();
    let reached = ureq::get(&format!("{base}/@deepseek-ai/dsh/latest"))
        .timeout(Duration::from_secs(4))
        .call()
        .is_ok();
    reached.then(|| start.elapsed().as_millis())
}

/// Probe both registries in parallel (bounded by one timeout window). The
/// JSON feeds the boot page's source chooser; `fastest` is null when both
/// are unreachable (the UI then falls back to the plain npm default).
pub fn npm_probe() -> Value {
    let npmjs = std::thread::spawn(|| probe_registry_ms(REGISTRY_NPMJS));
    let mirror = std::thread::spawn(|| probe_registry_ms(REGISTRY_NPMMIRROR));
    let npmjs_ms = npmjs.join().unwrap_or(None);
    let mirror_ms = mirror.join().unwrap_or(None);
    let fastest = match (npmjs_ms, mirror_ms) {
        (Some(a), Some(b)) => Some(if a <= b { "npmjs" } else { "npmmirror" }),
        (Some(_), None) => Some("npmjs"),
        (None, Some(_)) => Some("npmmirror"),
        (None, None) => None,
    };
    json!({ "npmjsMs": npmjs_ms, "npmmirrorMs": mirror_ms, "fastest": fastest })
}

/// Frontend "一键全局安装并启动": run `npm install -g @deepseek-ai/dsh`
/// (optionally pinned to one of the two probed registries) and retry
/// startup — afterwards `where dsh` leads the chain permanently. The
/// install (500+ packages) can take minutes; it runs on its own thread and
/// reports progress through the usual `dsh-status` events.
pub fn install_global_npm(app: AppHandle, registry: Option<&str>) {
    // Whitelist: only the two probed registries may enter the command line.
    // Owned because the install thread outlives this call.
    let url = registry
        .filter(|u| *u == REGISTRY_NPMJS || *u == REGISTRY_NPMMIRROR)
        .map(str::to_string);
    let app2 = app.clone();
    std::thread::spawn(move || {
        let (label, spec) = match url {
            Some(u) if u == REGISTRY_NPMMIRROR => (
                "npm 全局安装中(国内镜像,约 1-3 分钟)",
                format!("npm install -g @deepseek-ai/dsh --registry={u}"),
            ),
            Some(u) => (
                "npm 全局安装中(官方源,约 1-3 分钟)",
                format!("npm install -g @deepseek-ai/dsh --registry={u}"),
            ),
            None => (
                "npm 全局安装中(约 1-3 分钟)",
                "npm install -g @deepseek-ai/dsh".to_string(),
            ),
        };
        let _ = app2.emit(
            "dsh-status",
            json!({ "status": "starting", "method": label }),
        );
        let mut command = Command::new("cmd");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.raw_arg(format!("/S /C \"{spec}\""));
        }
        #[cfg(not(windows))]
        {
            command.arg("-c").arg(&spec);
        }
        command
            .current_dir(std::env::var("USERPROFILE").unwrap_or_else(|_| ".".to_string()))
            .stdout(log_stdio())
            .stderr(null_stdio());
        apply_no_window(&mut command);
        match command.output() {
            Ok(output) if output.status.success() => {
                retry(app2);
            }
            Ok(output) => {
                let _ = app2.emit(
                    "dsh-status",
                    json!({
                        "status": "error",
                        "message": format!(
                            "npm 全局安装失败(退出码 {})。详见日志 {}\\dsh.log,或改用「下载并启动(npx)」/手动路径。",
                            output.status.code().unwrap_or(-1),
                            std::env::var("LOCALAPPDATA").unwrap_or_default(),
                        ),
                    }),
                );
            }
            Err(error) => {
                let _ = app2.emit(
                    "dsh-status",
                    json!({ "status": "error", "message": format!("无法运行 npm(需要已安装 Node.js):{error}") }),
                );
            }
        }
    });
}

/// Re-arm after a failure: tear down any stale owned subprocess, then startup.
pub fn retry(app: AppHandle) {
    teardown(&app);
    let app2 = app.clone();
    std::thread::spawn(move || startup(app2));
}

// ── 检查更新(DSH CLI 自身): npm registry 最新版 vs 当前 dsh --version ──────
// DSH CLI 没有内置更新命令, 官方更新路径是 `npm install -g @deepseek-ai/dsh@latest`。

/// 执行一行 cmd 命令并捕获输出与成功与否(窗口隐藏)。
fn run_capture_line(cmdline: &str) -> Option<(String, bool)> {
    let mut command = Command::new("cmd");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.raw_arg(format!("/S /C \"{cmdline}\""));
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        command.arg("/C").arg(cmdline);
    }
    let out = command.output().ok()?;
    let ok = out.status.success();
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Some((text, ok))
}

/// npm registry 上 @deepseek-ai/dsh 的最新版本号(官方源优先, 国内镜像兜底)。
fn fetch_latest_version() -> Option<String> {
    for base in ["https://registry.npmjs.org", "https://registry.npmmirror.com"] {
        let url = format!("{base}/@deepseek-ai/dsh/latest");
        let Ok(resp) = ureq::get(&url).timeout(Duration::from_secs(6)).call() else {
            continue;
        };
        let Ok(v) = resp.into_json::<Value>() else { continue };
        if let Some(l) = v.get("version").and_then(|x| x.as_str()) {
            return Some(l.to_string());
        }
    }
    None
}

/// 简单版本比较(主版本数值 + prerelease rc 号), Greater 表示 a 比 b 新。
fn cmp_ver(a: &str, b: &str) -> std::cmp::Ordering {
    let norm = |v: &str| -> (Vec<u32>, u32) {
        let (base, pre) = match v.split_once('-') {
            Some((b, p)) => (b, p),
            None => (v, ""),
        };
        let nums: Vec<u32> = base.split('.').map(|x| x.parse().unwrap_or(0)).collect();
        let pren = pre
            .strip_prefix("rc")
            .and_then(|n| n.trim().parse::<u32>().ok())
            .unwrap_or(0);
        (nums, pren)
    };
    let (an, ap) = norm(a);
    let (bn, bp) = norm(b);
    for i in 0..an.len().max(bn.len()) {
        let x = an.get(i).copied().unwrap_or(0);
        let y = bn.get(i).copied().unwrap_or(0);
        if x != y {
            return x.cmp(&y);
        }
    }
    ap.cmp(&bp)
}

/// 托盘「检查更新(DSH)」: 查 npm 最新版, 有新版弹窗确认后用官方命令更新。
/// 全程在后台线程执行(版本检查/弹窗/更新), 不阻塞托盘与事件线程, 不影响前台。
pub fn check_update(_app: AppHandle) {
    std::thread::spawn(|| {
        let current = run_capture_line("dsh --version")
            .map(|(t, _)| t)
            .unwrap_or_default();
        let Some(latest) = fetch_latest_version() else {
            crate::prompt_yes_no("检查更新", "无法获取最新版本(网络不可达或 registry 失败)。");
            return;
        };
        if current.trim().is_empty() {
            crate::prompt_yes_no("检查更新", "无法获取当前 dsh 版本(请确认 dsh 命令可用)。");
            return;
        }
        if cmp_ver(&latest, &current) != std::cmp::Ordering::Greater {
            crate::prompt_yes_no("检查更新", &format!("已是最新版本: {latest}"));
            return;
        }
        let question = format!(
            "发现新版本 {latest}(当前 {current})\n\n是否立即更新?\n(将执行: npm install -g @deepseek-ai/dsh@{latest})"
        );
        if !crate::prompt_yes_no("检查更新", &question) {
            return;
        }
        // npm 装包可能耗时, 同样在后台线程执行, 完成后再弹窗提示。
        let ok = run_capture_line(&format!("npm install -g @deepseek-ai/dsh@{latest}"))
            .map(|(_, ok)| ok)
            .unwrap_or(false);
        if ok {
            crate::prompt_yes_no("检查更新", "更新成功。请重启 DSH(dsh web)使新版本生效。");
        } else {
            crate::prompt_yes_no(
                "检查更新",
                &format!("更新失败。请手动执行:\nnpm install -g @deepseek-ai/dsh@{latest}"),
            );
        }
    });
}

/// Frontend "下载并启动" after `notfound` (or as an error fallback): persist the
/// consent so future cold starts include the npx candidate automatically, then
/// run exactly that candidate now.
pub fn download_and_start(app: AppHandle) {
    save_prefer_npx(true);
    teardown(&app);
    let app2 = app.clone();
    std::thread::spawn(move || {
        let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".to_string());
        let cwd = std::env::var("DSH_CWD")
            .ok()
            .filter(|dir| Path::new(dir).is_dir())
            .unwrap_or(home);
        let candidate = npx_candidate(cwd);
        let _ = app2.emit(
            "dsh-status",
            json!({ "status": "starting", "method": candidate.label }),
        );
        if let Attempt::Failed(reason) = try_candidate(&app2, &candidate) {
            let _ = app2.emit(
                "dsh-status",
                json!({
                    "status": "error",
                    "message": format!("「{}」{}", candidate.label, reason),
                }),
            );
        }
    });
}

/// Tray "重启 dsh web(后端)": tear down + re-run the startup chain. The shell
/// stays loaded the whole time: a `starting` event swaps it back to the boot
/// view, and the chain's `ready` events reload the webchat iframe — the same
/// flow as a cold start, no page navigation involved.
/// Stop the DSH backend unconditionally: tear down the owned tree (if we
/// spawned it) and clear whatever still listens on 3080 — an attached
/// external instance included. The full app restart uses this: leaving a
/// stale attached backend behind meant plugins announcing 「重启后生效」
/// never actually loaded (2026-08-19 report).
pub fn stop_backend(app: &AppHandle) {
    // Return the shell to its boot page first; startup() will navigate back
    // to the webchat once the backend is ready again.
    navigate_shell(app);
    teardown(app);
    kill_port_listeners();
}

pub fn restart(app: AppHandle) {
    // Pop the window first so a restart triggered while hidden in the tray is
    // visibly underway instead of looking like a no-op.
    crate::show_main_window(&app);
    std::thread::spawn(move || {
        let _ = app.emit(
            "dsh-status",
            json!({ "status": "starting", "method": "正在重启 dsh web" }),
        );
        stop_backend(&app);
        // Wait for the old instance to stop answering so startup's attach
        // probe cannot latch onto the dying server and report ready.
        let deadline = Instant::now() + Duration::from_secs(10);
        while probe_ready_once() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(500));
        }
        startup(app.clone());
    });
}

/// Kill any process still listening on the DSH port: an attached instance we
/// never spawned, or a straggler the owned-tree taskkill missed. Locale-safe:
/// matches the numeric local-address column, not the state text.
fn kill_port_listeners() {
    let mut command = Command::new("netstat");
    command.args(["-ano", "-p", "tcp"]);
    apply_no_window(&mut command);
    let Ok(output) = command.output() else {
        return;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let suffix = format!(":{DSH_PORT}");
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 5
            && fields[0] == "TCP"
            && fields[1].ends_with(&suffix)
            && fields[4] != "0"
        {
            if let Ok(pid) = fields[4].parse::<u32>() {
                kill_tree(pid);
            }
        }
    }
}

/// Tear down the owned subprocess tree (if we spawned one). Safe to call from
/// attached mode — it is a no-op then.
pub fn teardown(app: &AppHandle) {
    INTENTIONAL_STOP.store(true, std::sync::atomic::Ordering::Relaxed);
    let state = app.state::<DshState>();
    let mut guard = state.inner.lock().unwrap();
    if let Some(inner) = guard.take() {
        // Child::kill only reaps the cmd shim on Windows; taskkill /T kills the
        // whole node tree so no orphan keeps holding 3080. The supervisor
        // thread reaps the Child and stays quiet (intentional stop).
        kill_tree(inner.pid);
    }
}

/// Append an attempt header so failures are attributable.
fn log_attempt(candidate: &Candidate) {
    log_write(
        LogLevel::Info,
        &format!("===== 启动尝试: {} =====", candidate.label),
    );
}

/// Spawn a shell command detached, no console window, with stdout+stderr
/// piped into the bounded in-memory tail ring (crash causes stay visible;
/// the unbounded stream is NOT logged — DSH keeps its own logs).
fn spawn_command(cmd: &str, cwd: &str) -> std::io::Result<Child> {
    let mut command = Command::new("cmd");
    // pnpm/npx/dsh are .cmd shims on Windows, so route through cmd /C; the
    // candidate command is a shell command string either way. Pass it via
    // `raw_arg` as `/S /C "…"`: std's automatic quoting would re-wrap the
    // whole string and backslash-escape the inner quotes around
    // space-containing paths, which cmd then misparses — `/S` strips only
    // the outermost quote pair, preserving our inner quotes verbatim.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.raw_arg(format!("/S /C \"{cmd}\""));
    }
    #[cfg(not(windows))]
    {
        command.arg("/C").arg(cmd);
    }
    command.current_dir(cwd);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    apply_no_window(&mut command);
    let mut child = command.spawn()?;
    if let Some(out) = child.stdout.take() {
        std::thread::spawn(move || pump_tail(out));
    }
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || pump_tail(err));
    }
    Ok(child)
}

/// Rotate the log ComfyUI-style at session start: archive the previous
/// session under a timestamped name, keep only the 20 newest archives, and
/// banner the fresh file. Runs before any child spawns, so every later
/// append handle lands on the new file.
pub(crate) fn rotate_log(app: &AppHandle) {
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
        if path.exists() {
            let (_, _, stamp) = local_time_parts();
            let _ = std::fs::rename(&path, path.with_file_name(format!("dsh.log_{stamp}.log")));
            let mut archives: Vec<_> = std::fs::read_dir(parent)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .map(|e| e.path())
                        .filter(|p| {
                            p.file_name()
                                .map(|n| n.to_string_lossy().starts_with("dsh.log_"))
                                .unwrap_or(false)
                        })
                        .collect()
                })
                .unwrap_or_default();
            // 改版: 自动清理超过 7 天的历史日志(按修改时间), 避免长期堆积;
            // 同时保留数量上限 20 个(双保险)。
            let now = std::time::SystemTime::now();
            let seven_days = std::time::Duration::from_secs(7 * 24 * 3600);
            archives.retain(|p| {
                let keep = std::fs::metadata(p)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| now.duration_since(t).ok())
                    .map(|age| age <= seven_days)
                    .unwrap_or(true); // 拿不到时间就不删
                if !keep {
                    let _ = std::fs::remove_file(p);
                }
                keep
            });
            archives.sort();
            while archives.len() > 20 {
                let _ = std::fs::remove_file(&archives[0]);
                archives.remove(0);
            }
        }
    }
    let (date, time, _) = local_time_parts();
    let version = app.package_info().version.to_string();
    let exe = tauri::utils::platform::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let dir = path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    log_raw(&format!("** dsh-desktop session: {date} {time}"));
    log_raw(&format!("** app: v{version} ({exe})"));
    log_raw(&format!("** log dir: {dir}"));
}

/// Two append handles to the same log file, one each for stdout/stderr.
fn log_streams() -> std::io::Result<(Stdio, Stdio)> {
    let path = log_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stdout = OpenOptions::new().create(true).append(true).open(&path)?;
    let stderr = stdout.try_clone()?;
    Ok((Stdio::from(stdout), Stdio::from(stderr)))
}

/// One append handle to the log (stdout only).
fn log_stdio() -> Stdio {
    match log_streams() {
        Ok((stdout, _)) => stdout,
        Err(_) => Stdio::null(),
    }
}

fn null_stdio() -> Stdio {
    Stdio::null()
}

/// Shell data root: everything this app persists (settings.json, dsh.log,
/// WebView2 cookies/cache) lives in ONE directory so the shell is fully
/// portable and never touches the user's home or the DSH config root.
/// Default: a `dsh-shell-data` folder next to the exe (the "install dir").
/// Override with the DSH_SHELL_DATA environment variable when the exe sits
/// in a read-only location (e.g. Program Files).
pub(crate) fn shell_data_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("DSH_SHELL_DATA") {
        if !dir.trim().is_empty() {
            return std::path::PathBuf::from(dir);
        }
    }
    tauri::utils::platform::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| {
            std::env::var("LOCALAPPDATA")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from("."))
        })
        .join("dsh-shell-data")
}

/// The real DSH config root: `$DSH_HOME` when set (the official CLI's
/// semantic), else `%USERPROFILE%\.dsh`. All paths the shell inspects for
/// profile bundles, settings.yaml self-heal, etc. go through this so a
/// relocated DSH_HOME (e.g. `D:\.dsh`) is honoured instead of the stale
/// `~/.dsh` default.
pub(crate) fn dsh_home() -> std::path::PathBuf {
    std::env::var("DSH_HOME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var("USERPROFILE")
                .map(|home| Path::new(&home).join(".dsh"))
                .unwrap_or_else(|_| PathBuf::from(".dsh"))
        })
}

fn log_path() -> std::path::PathBuf {
    shell_data_dir().join("dsh.log")
}

/// Kill a process *tree* by root PID. `cmd /C …` → node is a grandchild; `/T`
/// walks the tree so nothing survives on 3080.
fn kill_tree(pid: u32) {
    let mut command = Command::new("taskkill");
    command.args(["/PID", &pid.to_string(), "/T", "/F"]);
    apply_no_window(&mut command);
    let _ = command.status();
}

#[cfg(windows)]
fn apply_no_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn apply_no_window(_command: &mut Command) {}
