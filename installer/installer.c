/* dsh-local-shell self-contained installer (Win32 GUI).
 * All comments are ASCII. Build (MSVC):
 *   rc resource.rc
 *   cl installer.c resource.res user32.lib gdi32.lib shell32.lib comctl32.lib /SUBSYSTEM:WINDOWS /UNICODE
 */

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <commctrl.h>
#include <shellapi.h>
#include <shlobj.h>
#include <string.h>
#include <wchar.h>
#include <stdarg.h>

#define IDC_INSTALL       1
#define IDC_UNINSTALL     2
#define IDC_UNINSTALLDATA 3
#define IDC_EXIT          4
#define IDC_PROGRESS      5

#define CLI_ROOT      L"apps\\cli\\lib\\bin.js"
#define PROG_DIR      L"dsh-desktop-local"
#define PROG_NAME     L"dsh-local-shell.exe"
#define DATA_DIR      L"dsh-shell-data"
#define APPDATA_DIR   L"com.dsh.desktop"
#define SHORTCUT_NAME L"DeepSeek Harness"
#define WEBURL_NAME   L"DeepSeek Harness Web.url"

static HINSTANCE g_hinst = NULL;
static HWND g_hwnd = NULL;
static HWND g_hwndProg = NULL;
static HWND g_log = NULL;               /* log EDIT (selectable/copyable) */
static WNDPROC g_editProc = NULL;       /* subclassed EDIT for Ctrl+A */
static BOOL g_zh = FALSE;
static BOOL g_busy = FALSE;
static WCHAR g_root[1024] = L"";
static WCHAR g_binJs[1024] = L"";

static WCHAR g_logBuf[8192];
static int g_logLen = 0;

/* ---------------- logging ---------------- */

/* The log is a native EDIT control so the user can select & copy text.
 * We always write the WHOLE buffer via SetWindowTextW (never partial
 * REPLACESEL), so the control repaints entirely and no layered text can
 * ever remain. */
static void logfx(const WCHAR *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    if (g_logLen > 7000) { /* bounded: drop the oldest half */
        int keep = 4096;
        memmove(g_logBuf, g_logBuf + g_logLen - keep, keep * sizeof(WCHAR));
        g_logLen = keep;
    }
    {
        int n = wvsprintfW(g_logBuf + g_logLen, fmt, ap);
        if (n > 0) g_logLen += n;
    }
    va_end(ap);
    if (g_logLen > 8188) g_logLen = 8188;
    g_logBuf[g_logLen++] = L'\r';
    g_logBuf[g_logLen++] = L'\n';
    g_logBuf[g_logLen] = 0;
    if (g_log) {
        SetWindowTextW(g_log, g_logBuf);
        /* keep the log scrolled to the newest line */
        int len = GetWindowTextLengthW(g_log);
        SendMessageW(g_log, EM_SETSEL, len, len);
        SendMessageW(g_log, EM_SCROLLCARET, 0, 0);
    }
}

static void clearLog(void)
{
    g_logLen = 0;
    g_logBuf[0] = 0;
    if (g_log) SetWindowTextW(g_log, L"");
}

#define LOGT(zh, en, ...) logfx((g_zh) ? (zh) : (en), ##__VA_ARGS__)

/* ---------------- small path helpers ---------------- */

static void NormalizeSeps(WCHAR *p)
{
    for (; *p; p++) if (*p == L'/') *p = L'\\';
}

/* remove the last path component, keeping any trailing separator */
static void StripFileName(WCHAR *p)
{
    WCHAR *q = p + lstrlenW(p);
    while (q > p && q[-1] == L'\\') q--;
    while (q > p && q[-1] != L'\\') q--;
    *q = 0;
}

/* cut one trailing path component; returns 0 when at the drive root */
static int GoUpOne(WCHAR *p)
{
    WCHAR *q = p + lstrlenW(p);
    while (q > p && q[-1] == L'\\') q--;
    while (q > p && q[-1] != L'\\') q--;
    if (q <= p) return 0;
    q--;                     /* backslash before the removed component */
    if (q <= p) return 0;    /* drive root reached (e.g. "C:") */
    *q = 0;
    return 1;
}

/* ---------------- process helpers ---------------- */

static int RunHidden(WCHAR *cmdline, DWORD waitMs)
{
    STARTUPINFOW si;
    PROCESS_INFORMATION pi;
    memset(&si, 0, sizeof(si));
    si.cb = sizeof(si);
    si.dwFlags = STARTF_USESHOWWINDOW;
    si.wShowWindow = SW_HIDE;
    if (!CreateProcessW(NULL, cmdline, NULL, NULL, FALSE, CREATE_NO_WINDOW,
                        NULL, NULL, &si, &pi))
        return -1;
    CloseHandle(pi.hThread);
    if (waitMs) WaitForSingleObject(pi.hProcess, waitMs);
    DWORD code = 0;
    if (!GetExitCodeProcess(pi.hProcess, &code)) code = 0xFFFFFFFF;
    CloseHandle(pi.hProcess);
    return (int)code;
}

/* run "cmd.exe /c <cmdline>" hidden and capture stdout into out */
static int RunCapture(const WCHAR *cmdline, char *out, int outsz)
{
    SECURITY_ATTRIBUTES sa;
    HANDLE hRead = NULL, hWrite = NULL;
    sa.nLength = sizeof(sa);
    sa.bInheritHandle = TRUE;
    sa.lpSecurityDescriptor = NULL;
    if (!CreatePipe(&hRead, &hWrite, &sa, 0)) return -1;
    SetHandleInformation(hRead, HANDLE_FLAG_INHERIT, 0);

    STARTUPINFOW si;
    PROCESS_INFORMATION pi;
    memset(&si, 0, sizeof(si));
    si.cb = sizeof(si);
    si.dwFlags = STARTF_USESTDHANDLES | STARTF_USESHOWWINDOW;
    si.wShowWindow = SW_HIDE;
    si.hStdInput = NULL;
    si.hStdOutput = hWrite;
    si.hStdError = hWrite;

    WCHAR cmd[600];
    lstrcpyW(cmd, L"cmd.exe /c ");
    lstrcatW(cmd, cmdline);
    BOOL ok = CreateProcessW(NULL, cmd, NULL, NULL, TRUE,
                             CREATE_NO_WINDOW, NULL, NULL, &si, &pi);
    CloseHandle(hWrite);
    int n = 0;
    if (ok) {
        CloseHandle(pi.hThread);
        WaitForSingleObject(pi.hProcess, 15000);
        DWORD rd = 0;
        char tmp[512];
        while (n < outsz - 1 &&
               ReadFile(hRead, tmp, sizeof(tmp), &rd, NULL) && rd > 0) {
            int c = (int)rd;
            if (n + c >= outsz - 1) c = outsz - 1 - n;
            memcpy(out + n, tmp, c);
            n += c;
        }
        CloseHandle(pi.hProcess);
    }
    CloseHandle(hRead);
    out[n] = 0;
    return ok ? n : -1;
}

/* ---------------- DSH root detection ---------------- */

static const char *FindStrCI(const char *hay, const char *needle)
{
    size_t nl = strlen(needle);
    if (!nl) return hay;
    for (; *hay; hay++)
        if (_strnicmp(hay, needle, nl) == 0) return hay;
    return NULL;
}

static int LineEndsWithCmd(const char *s)
{
    size_t n = strlen(s);
    while (n && (s[n - 1] == '\r' || s[n - 1] == '\n' || s[n - 1] == ' ')) n--;
    if (n < 4) return 0;
    return _strnicmp(s + n - 4, ".cmd", 4) == 0;
}

/* find the first quoted argument after "node" that contains "bin.js"
 * inside the .cmd file text; copy it (wide) into out */
static int ExtractBinJs(const char *text, WCHAR *out, int outn)
{
    const char *p = text;
    out[0] = 0;
    while ((p = FindStrCI(p, "node")) != NULL) {
        const char *q = p + 4;
        while (*q == ' ' || *q == '\t') q++;
        if (*q != '"') { p = q; continue; }
        q++;
        {
            const char *r = q;
            while (*r && *r != '"') r++;
            if (*r != '"') { p = q; continue; }
            if ((size_t)(r - q) < (size_t)outn && FindStrCI(q, "bin.js") != NULL) {
                char tmp[1024];
                size_t len = (size_t)(r - q);
                if (len >= sizeof(tmp)) len = sizeof(tmp) - 1;
                memcpy(tmp, q, len);
                tmp[len] = 0;
                MultiByteToWideChar(CP_ACP, 0, tmp, -1, out, outn);
                return 1;
            }
            p = r;
        }
    }
    return 0;
}

static void DetectDshRoot(void)
{
    static char cmdText[65536];
    char out[4096];
    WCHAR wpath[2048];

    LOGT(L"正在定位 DSH 安装...", L"Locating DSH installation...");
    g_root[0] = 0;
    if (RunCapture(L"where dsh", out, sizeof(out)) < 0 || !out[0]) {
        LOGT(L"'where dsh' 未找到 dsh 命令。", L"'where dsh' did not find the dsh command.");
        return;
    }

    /* first output line that ends with ".cmd" is the launcher script */
    {
        char *ln = out;
        while (ln && *ln) {
            char *e = strchr(ln, '\n');
            size_t len = e ? (size_t)(e - ln) : strlen(ln);
            char line[2048];
            if (len >= sizeof(line)) len = sizeof(line) - 1;
            memcpy(line, ln, len);
            line[len] = 0;
            if (LineEndsWithCmd(line)) {
                MultiByteToWideChar(CP_ACP, 0, line, -1, wpath, 2048);
                /* strip any trailing CR/LF/space from the parsed path —
                 * otherwise the path silently contains "\r" and CreateFile
                 * fails with a nonsensical "cannot read script" */
                {
                    int wl = lstrlenW(wpath);
                    while (wl > 0 && (wpath[wl - 1] == L'\r' || wpath[wl - 1] == L'\n' ||
                                      wpath[wl - 1] == L' '))
                        wpath[--wl] = 0;
                }
                break;
            }
            ln = e ? e + 1 : NULL;
        }
    }
    if (!wpath[0]) {
        LOGT(L"未找到 .cmd 形式的 dsh 入口。", L"No .cmd dsh entry found.");
        return;
    }

    /* read the launcher script (open with full sharing so another process
     * holding the file can't block us) */
    {
        HANDLE hf = CreateFileW(wpath, GENERIC_READ,
                                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                                NULL, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL);
        if (hf == INVALID_HANDLE_VALUE) {
            LOGT(L"无法读取脚本：%s", L"Cannot read the script: %s", wpath);
            return;
        }
        DWORD rd = 0;
        BOOL ok = ReadFile(hf, cmdText, sizeof(cmdText) - 1, &rd, NULL);
        CloseHandle(hf);
        if (!ok || rd == 0) {
            LOGT(L"无法读取脚本：%s", L"Cannot read the script: %s", wpath);
            return;
        }
        cmdText[rd] = 0;
    }

    if (!ExtractBinJs(cmdText, g_binJs, 1024) || !g_binJs[0]) {
        LOGT(L"脚本中未找到 bin.js 路径。", L"bin.js path not found in the script.");
        return;
    }

    /* npm style launchers reference "%~dp0\..\apps\cli\lib\bin.js";
     * resolve %dp0 against the directory of the script itself */
    if (_wcsnicmp(g_binJs, L"%dp0%", 5) == 0 ||
        _wcsnicmp(g_binJs, L"%~dp0", 5) == 0) {
        WCHAR dir[2048];
        WCHAR rest[1024];
        lstrcpyW(dir, wpath);
        StripFileName(dir);
        lstrcpyW(rest, g_binJs + 5);
        wsprintfW(g_binJs, L"%s%s", dir, rest);
    }
    NormalizeSeps(g_binJs);

    /* walk upward from bin.js's own directory until
     * <cur>\apps\cli\lib\bin.js equals the located bin.js */
    {
        WCHAR cur[1024];
        lstrcpyW(cur, g_binJs);
        StripFileName(cur);
        for (;;) {
            WCHAR cand[1200];
            wsprintfW(cand, L"%s\\%s", cur, CLI_ROOT);
            if (_wcsicmp(cand, g_binJs) == 0) {
                lstrcpyW(g_root, cur);
                break;
            }
            if (!GoUpOne(cur)) break;
        }
    }

    if (g_root[0]) {
        LOGT(L"DSH 根目录：%s", L"DSH root: %s", g_root);
    } else {
        LOGT(L"未能确定 DSH 根目录。", L"Could not determine the DSH root directory.");
    }
}

/* ---------------- install ---------------- */

static void SetProgress(int p)
{
    SendMessageW(g_hwndProg, PBM_SETPOS, (WPARAM)p, 0);
}

static void EnableButtons(BOOL en)
{
    EnableWindow(GetDlgItem(g_hwnd, IDC_INSTALL), en);
    EnableWindow(GetDlgItem(g_hwnd, IDC_UNINSTALL), en);
    EnableWindow(GetDlgItem(g_hwnd, IDC_UNINSTALLDATA), en);
    EnableWindow(GetDlgItem(g_hwnd, IDC_EXIT), en);
}

/* write RCDATA resource 102 (the bundled dsh-local-shell.exe) to dst */
static int WritePayload(const WCHAR *dst)
{
    HRSRC hr = FindResourceW(NULL, MAKEINTRESOURCEW(102), (LPCWSTR)RT_RCDATA);
    if (!hr) return -1;
    HGLOBAL hg = LoadResource(NULL, hr);
    if (!hg) return -2;
    void *data = LockResource(hg);
    DWORD sz = SizeofResource(NULL, hr);
    if (!data || sz == 0) return -3;
    HANDLE hf = CreateFileW(dst, GENERIC_WRITE, 0, NULL, CREATE_ALWAYS,
                            FILE_ATTRIBUTE_NORMAL, NULL);
    if (hf == INVALID_HANDLE_VALUE) return -4;
    DWORD wr = 0;
    BOOL ok = WriteFile(hf, data, sz, &wr, NULL);
    CloseHandle(hf);
    return (ok && wr == sz) ? 0 : -5;
}

/* create the desktop shortcut with an inline PowerShell one-liner;
 * every path is passed through process environment variables */
static int CreateDesktopShortcut(const WCHAR *target, const WCHAR *work)
{
    WCHAR desk[MAX_PATH];
    if (SHGetFolderPathW(NULL, CSIDL_DESKTOPDIRECTORY, NULL, SHGFP_TYPE_CURRENT, desk) != S_OK)
        return -1;
    SetEnvironmentVariableW(L"SC_DESK", desk);
    SetEnvironmentVariableW(L"SC_NAME", SHORTCUT_NAME);
    SetEnvironmentVariableW(L"SC_TARGET", target);
    SetEnvironmentVariableW(L"SC_WORK", work);
    WCHAR ps[1600];
    lstrcpyW(ps, L"powershell -NoProfile -Command "
                 L"\"$s=(New-Object -ComObject WScript.Shell).CreateShortcut(("
                 L"Join-Path $env:SC_DESK ($env:SC_NAME+'.lnk')));"
                 L"$s.TargetPath=$env:SC_TARGET;"
                 L"$s.WorkingDirectory=$env:SC_WORK;"
                 L"$s.IconLocation=($env:SC_TARGET+',0');$s.Save()\"");
    return RunHidden(ps, 60000);
}

static void DoInstall(void)
{
    WCHAR dir[MAX_PATH], target[MAX_PATH];

    if (g_busy) return;
    if (!g_root[0]) DetectDshRoot(); /* retry: dsh may have appeared after start */
    if (!g_root[0]) {
        MessageBoxW(g_hwnd, g_zh ? L"未检测到 DSH 安装，无法完成安装。"
                                 : L"DSH installation not detected; install aborted.",
                    g_zh ? L"安装" : L"Install", MB_OK | MB_ICONWARNING);
        return;
    }

    g_busy = TRUE;
    EnableButtons(FALSE);
    LOGT(L"开始安装...", L"Installing...");
    SetProgress(10);

    wsprintfW(dir, L"%s\\%s", g_root, PROG_DIR);
    CreateDirectoryW(dir, NULL);
    wsprintfW(target, L"%s\\%s", dir, PROG_NAME);

    if (WritePayload(target) != 0) {
        LOGT(L"写入失败：%s", L"Failed to write: %s", target);
        g_busy = FALSE;
        EnableButtons(TRUE);
        return;
    }
    LOGT(L"已写入：%s", L"Written: %s", target);
    SetProgress(70);

    if (CreateDesktopShortcut(target, dir) != 0)
        LOGT(L"创建桌面快捷方式失败。", L"Failed to create the desktop shortcut.");
    else
        LOGT(L"已创建桌面快捷方式。", L"Desktop shortcut created.");
    SetProgress(100);

    LOGT(L"安装完成。", L"Installation complete.");
    g_busy = FALSE;
    EnableButtons(TRUE);
}

/* ---------------- uninstall ---------------- */

static void DeletePath(const WCHAR *path)
{
    WCHAR list[1600];
    int len = lstrlenW(path);
    lstrcpyW(list, path);
    list[len + 1] = 0; /* double null terminator for SHFileOperationW */
    SHFILEOPSTRUCTW fo;
    memset(&fo, 0, sizeof(fo));
    fo.hwnd = g_hwnd;
    fo.wFunc = FO_DELETE;
    fo.pFrom = list;
    fo.fFlags = FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT;
    SHFileOperationW(&fo);
}

static void DeleteDesktopLinks(void)
{
    WCHAR desk[MAX_PATH];
    WCHAR p[MAX_PATH];
    if (SHGetFolderPathW(NULL, CSIDL_DESKTOPDIRECTORY, NULL, SHGFP_TYPE_CURRENT, desk) != S_OK)
        return;
    /* plain DeleteFileW: more reliable than SHFileOperationW here */
    wsprintfW(p, L"%s\\%s.lnk", desk, SHORTCUT_NAME);
    DeleteFileW(p);
    wsprintfW(p, L"%s\\%s", desk, WEBURL_NAME);
    DeleteFileW(p);
}

static void DoUninstall(BOOL delData)
{
    if (g_busy) return;
    if (!g_root[0])
        LOGT(L"未检测到 DSH 根目录，仅尝试删除桌面快捷方式。",
             L"DSH root not detected; only desktop shortcuts will be removed.");

    g_busy = TRUE;
    EnableButtons(FALSE);
    LOGT(L"开始卸载...", L"Uninstalling...");
    SetProgress(10);

    DeleteDesktopLinks();
    SetProgress(40);

    if (g_root[0]) {
        WCHAR dir[MAX_PATH];
        wsprintfW(dir, L"%s\\%s", g_root, PROG_DIR);
        DeletePath(dir);
        LOGT(L"已删除程序目录：%s", L"Removed program directory: %s", dir);
    }
    SetProgress(70);

    if (delData) {
        WCHAR loc[MAX_PATH];
        if (SHGetFolderPathW(NULL, CSIDL_LOCAL_APPDATA, NULL, SHGFP_TYPE_CURRENT, loc) == S_OK) {
            WCHAR p[MAX_PATH];
            wsprintfW(p, L"%s\\%s", loc, APPDATA_DIR);
            DeletePath(p);
        }
        if (g_root[0]) {
            WCHAR p[MAX_PATH];
            wsprintfW(p, L"%s\\%s\\%s", g_root, PROG_DIR, DATA_DIR);
            DeletePath(p);
        }
        LOGT(L"已删除数据目录。", L"Data directories removed.");
    }
    SetProgress(100);

    LOGT(L"卸载完成。", L"Uninstall complete.");
    g_busy = FALSE;
    EnableButtons(TRUE);
}

/* ---------------- window ---------------- */

/* subclassed log EDIT: Ctrl+A selects the whole log */
static LRESULT CALLBACK EditLogProc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp)
{
    if (msg == WM_KEYDOWN && wp == L'A' && (GetKeyState(VK_CONTROL) & 0x8000)) {
        SendMessageW(hwnd, EM_SETSEL, 0, -1);
        return 0;
    }
    if (g_editProc != NULL)
        return CallWindowProcW(g_editProc, hwnd, msg, wp, lp);
    return DefWindowProcW(hwnd, msg, wp, lp);
}

static LRESULT CALLBACK WndProc(HWND hwnd, UINT msg, WPARAM wParam, LPARAM lParam)
{
    switch (msg) {

    case WM_CREATE:
        g_log = CreateWindowExW(WS_EX_CLIENTEDGE, L"EDIT", L"",
                                WS_CHILD | WS_VISIBLE | WS_VSCROLL |
                                ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL,
                                16, 80, 592, 284, hwnd, NULL, g_hinst, NULL);
        /* subclassed EDIT: Ctrl+A selects all */
        g_editProc = (WNDPROC)SetWindowLongPtrW(g_log, GWLP_WNDPROC,
                                                (LONG_PTR)(void *)EditLogProc);
        SendMessageW(g_log, WM_SETFONT,
                     (WPARAM)CreateFontW(-15, 0, 0, 0, FW_NORMAL, FALSE, FALSE, FALSE,
                                         DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
                                         CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY,
                                         FIXED_PITCH, L"Consolas"), TRUE);
        g_hwndProg = CreateWindowExW(0, PROGRESS_CLASSW, NULL,
                                     WS_CHILD | WS_VISIBLE,
                                     16, 380, 592, 18, hwnd,
                                     (HMENU)(INT_PTR)IDC_PROGRESS, g_hinst, NULL);
        SendMessageW(g_hwndProg, PBM_SETRANGE32, 0, 100);
        SendMessageW(g_hwndProg, PBM_SETPOS, 0, 0);
        CreateWindowExW(0, L"BUTTON", g_zh ? L"安装" : L"Install",
                        WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                        16, 414, 130, 30, hwnd, (HMENU)(INT_PTR)IDC_INSTALL, g_hinst, NULL);
        CreateWindowExW(0, L"BUTTON", g_zh ? L"卸载" : L"Uninstall",
                        WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                        156, 414, 170, 30, hwnd, (HMENU)(INT_PTR)IDC_UNINSTALL, g_hinst, NULL);
        CreateWindowExW(0, L"BUTTON", g_zh ? L"卸载并删除数据" : L"Uninstall & Delete Data",
                        WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                        336, 414, 160, 30, hwnd, (HMENU)(INT_PTR)IDC_UNINSTALLDATA, g_hinst, NULL);
        CreateWindowExW(0, L"BUTTON", g_zh ? L"退出" : L"Exit",
                        WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                        506, 414, 100, 30, hwnd, (HMENU)(INT_PTR)IDC_EXIT, g_hinst, NULL);
        return 0;

    case WM_COMMAND:
        switch (LOWORD(wParam)) {
        case IDC_INSTALL:       DoInstall();        break;
        case IDC_UNINSTALL:     DoUninstall(FALSE); break;
        case IDC_UNINSTALLDATA: DoUninstall(TRUE);  break;
        case IDC_EXIT:          if (!g_busy) DestroyWindow(hwnd); break;
        }
        return 0;

    case WM_CONTEXTMENU: /* right-click on the log: copy the selection */
        {
            DWORD sel = (DWORD)SendMessageW(g_log, EM_GETSEL, 0, 0);
            if (LOWORD(sel) != HIWORD(sel))
                SendMessageW(g_log, WM_COPY, 0, 0);
        }
        return 0;

    case WM_ERASEBKGND: /* full client area is painted white here */
    {
        HBRUSH b = CreateSolidBrush(RGB(255, 255, 255));
        RECT rc;
        GetClientRect(hwnd, &rc);
        FillRect((HDC)wParam, &rc, b);
        DeleteObject(b);
        return 1;
    }

    case WM_PAINT:
    {
        PAINTSTRUCT ps;
        HDC hdc = BeginPaint(hwnd, &ps);
        RECT rc;
        HFONT old;
        GetClientRect(hwnd, &rc);

        /* deep blue title bar 0..60 */
        {
            HBRUSH tb = CreateSolidBrush(RGB(0x4D, 0x6B, 0xFE));
            RECT tr = { 0, 0, rc.right, 60 };
            FillRect(hdc, &tr, tb);
            DeleteObject(tb);
        }
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, RGB(255, 255, 255));
        old = (HFONT)SelectObject(hdc,
              CreateFontW(-22, 0, 0, 0, FW_BOLD, FALSE, FALSE, FALSE,
                          DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
                          CLEARTYPE_QUALITY, DEFAULT_PITCH,
                          g_zh ? L"Microsoft YaHei UI" : L"Segoe UI"));
        {
            RECT tr = { 12, 0, rc.right, 60 };
            DrawTextW(hdc, g_zh ? L"dsh-local-shell 安装程序" : L"dsh-local-shell Installer",
                      -1, &tr, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        }
        DeleteObject(SelectObject(hdc, old));

        EndPaint(hwnd, &ps);
        return 0;
    }

    case WM_CLOSE:
        if (!g_busy) DestroyWindow(hwnd);
        return 0;

    case WM_DESTROY:
        PostQuitMessage(0);
        return 0;
    }
    return DefWindowProcW(hwnd, msg, wParam, lParam);
}

int WINAPI wWinMain(HINSTANCE hInst, HINSTANCE hPrev, PWSTR lpCmd, int nShow)
{
    INITCOMMONCONTROLSEX icc;
    WNDCLASSW wc;
    MSG msg;

    (void)hPrev;
    (void)lpCmd;
    g_hinst = hInst;
    g_zh = (PRIMARYLANGID(GetUserDefaultUILanguage()) == LANG_CHINESE);

    icc.dwSize = sizeof(icc);
    icc.dwICC = ICC_PROGRESS_CLASS;
    InitCommonControlsEx(&icc);

    memset(&wc, 0, sizeof(wc));
    wc.lpfnWndProc = WndProc;
    wc.hInstance = hInst;
    wc.hIcon = LoadIconW(hInst, MAKEINTRESOURCEW(101));
    wc.hCursor = LoadCursorW(NULL, IDC_ARROW);
    wc.hbrBackground = NULL; /* background handled by WM_ERASEBKGND */
    wc.lpszClassName = L"DshLocalShellInstallWndClass";
    if (!RegisterClassW(&wc)) return 1;

    g_hwnd = CreateWindowExW(0, wc.lpszClassName,
                             g_zh ? L"dsh-local-shell 安装程序" : L"dsh-local-shell Installer",
                             WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
                             CW_USEDEFAULT, CW_USEDEFAULT, 640, 480,
                             NULL, NULL, hInst, NULL);
    if (!g_hwnd) return 1;

    ShowWindow(g_hwnd, nShow);
    UpdateWindow(g_hwnd);

    g_logBuf[0] = 0;
    DetectDshRoot();

    while (GetMessageW(&msg, NULL, 0, 0) > 0) {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    return (int)msg.wParam;
}