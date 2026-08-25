//! In-page context menu for hyperlinks on the 3080 webchat page.
//!
//! The WebView2 default menu is a generic Edge menu: items like "在新窗口中
//! 打开链接" actually shell out to the system browser (wry's NewWindowRequested
//! fallback) and several entries are dead weight. The webchat loads inside the
//! shell's iframe now, so the script rides along as a webview initialization
//! script — WebView2 runs those in every frame, and the script self-guards on
//! `location.origin`, so it installs exactly once per webchat document and
//! no-ops on the shell's own page. It replaces the menu on `<a href>`
//! right-clicks with two honest items: open in the system browser (reusing the
//! same new-window fallback) and copy the link. Non-link right-clicks keep the
//! default menu.

/// Registered at window build time (`initialization_script`); see module docs.
pub(crate) const MENU_SCRIPT: &str = r#"
(function () {
  if (window.__dshLinkMenu) return;
  if (location.origin !== 'http://127.0.0.1:3080') return;
  if (document.readyState === 'loading') return; // retried by the next poll
  window.__dshLinkMenu = true;

  var menu = null;
  function closeMenu() {
    if (menu) { menu.remove(); menu = null; }
  }
  function openInBrowser(url) {
    // New-window request; the shell's fallback opens it in the system browser.
    window.open(url, '_blank', 'noopener');
  }
  function copyLink(url) {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(url);
    }
  }
  function showMenu(x, y, url) {
    closeMenu();
    menu = document.createElement('div');
    menu.setAttribute('style',
      'position:fixed;z-index:2147483647;min-width:180px;padding:4px 0;' +
      'background:#1c1f26;border:1px solid #3a4050;border-radius:8px;' +
      'box-shadow:0 8px 24px rgba(0,0,0,.45);font:13px/1 system-ui,sans-serif;color:#e6e6e6;'
    );
    [['在浏览器中打开', function () { openInBrowser(url); }],
     ['复制链接', function () { copyLink(url); }]].forEach(function (item) {
      var row = document.createElement('div');
      row.textContent = item[0];
      row.setAttribute('style',
        'padding:7px 14px;cursor:pointer;white-space:nowrap;'
      );
      row.addEventListener('mouseenter', function () { row.style.background = '#2a3040'; });
      row.addEventListener('mouseleave', function () { row.style.background = 'transparent'; });
      row.addEventListener('click', function (e) { e.stopPropagation(); closeMenu(); item[1](); });
      menu.appendChild(row);
    });
    document.documentElement.appendChild(menu);
    var w = menu.offsetWidth, h = menu.offsetHeight;
    menu.style.left = Math.min(x, innerWidth - w - 8) + 'px';
    menu.style.top = Math.min(y, innerHeight - h - 8) + 'px';
  }

  // 左键 / Ctrl+点击 `target="_blank"` 外链: 直接交给系统浏览器打开。
  // 背景: tauri-plugin-opener 注入的 init-iife.js(open_js_links_on_click)会在 window
  // 冒泡阶段拦截这类点击并改走 JS 侧 `plugin:opener|open_url` IPC, 而 3080 这个外部
  // 远程页面里没有可靠的 Tauri IPC 桥, 拦截后原生新窗口也被 preventDefault 压掉, 于是
  // 两头落空——左键/Ctrl+点击完全没反应。这里用一个**捕获阶段**处理器抢在 opener 之前
  // 接管, preventDefault 后改走 `window.open` —— 与右键菜单同一条 native→Rust
  // on_new_window→系统浏览器通道(该通道经右键实测可靠), 不依赖任何页面内 Tauri IPC。
  document.addEventListener('click', function (e) {
    // 只接管左键; metaKey/altKey 的其它语义(如 Cmd/Ctrl 开新标签)留给 opener/浏览器
    if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.altKey) return;
    var a = e.target && e.target.closest ? e.target.closest('a[href]') : null;
    if (!a || !a.href) return;
    var isBlankTarget = (a.target || '').toLowerCase() === '_blank';
    var isModifier = e.ctrlKey || e.shiftKey;
    if (!isBlankTarget && !isModifier) return;
    var u;
    try { u = new URL(a.href); } catch (err) { return; }
    if (u.protocol !== 'http:' && u.protocol !== 'https:') return;
    e.preventDefault();
    e.stopPropagation();
    openInBrowser(a.href);
  }, true);
  document.addEventListener('contextmenu', function (e) {
    var a = e.target && e.target.closest ? e.target.closest('a[href]') : null;
    if (!a) return;
    e.preventDefault();
    e.stopPropagation();
    showMenu(e.clientX, e.clientY, a.href);
  }, true);
  document.addEventListener('mousedown', function (e) {
    if (menu && !menu.contains(e.target)) closeMenu();
  }, true);
  window.addEventListener('blur', closeMenu);
  window.addEventListener('resize', closeMenu);
})();
"#;

/// GitHub 请求加速镜像注入: 插件常请求 github.com / raw.githubusercontent.com /
/// api.github.com 等文件与仓库资源, 国内直连易失败。本脚本在每帧注入,
/// 把 fetch / XMLHttpRequest 中的 GitHub URL 重写为 `https://gh-proxy.com/<原URL>`
/// (GitHub 加速镜像前缀), 本地(3080/127.0.0.1)请求不受影响。
pub(crate) const GH_MIRROR_SCRIPT: &str = r#"
(() => {
  if (window.__dsh_gh_mirror) return;
  window.__dsh_gh_mirror = true;
  var MIRROR = 'https://gh-proxy.com/';
  var PREFIXES = [
    'https://github.com/',
    'https://raw.githubusercontent.com/',
    'https://api.github.com/',
    'https://gist.githubusercontent.com/',
    'https://codeload.github.com/',
    'https://objects.githubusercontent.com/'
  ];
  var mirrorize = function (u) {
    if (typeof u !== 'string' || u.indexOf('://') < 0) return u;
    for (var i = 0; i < PREFIXES.length; i++) {
      if (u.indexOf(PREFIXES[i]) === 0) return MIRROR + u;
    }
    return u;
  };
  try {
    var of = window.fetch ? window.fetch.bind(window) : null;
    if (of) {
      window.fetch = function (input, init) {
        if (typeof input === 'string') input = mirrorize(input);
        else if (input && input.url) input = new Request(mirrorize(input.url), input);
        return of(input, init);
      };
    }
  } catch (e) { /* ignore */ }
  try {
    var ox = XMLHttpRequest.prototype.open;
    XMLHttpRequest.prototype.open = function (method, url) {
      arguments[1] = mirrorize(String(url));
      return ox.apply(this, arguments);
    };
  } catch (e) { /* ignore */ }
})();
"#;
