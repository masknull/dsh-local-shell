import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import appIcon from "./assets/app-icon.png";
import EnvPanel, { type EnvInfo } from "./EnvPanel";
import "./styles/theme.css";
import "./styles/base.css";
import "./styles/boot.css";
import "./styles/panel.css";

/** Rust→frontend lifecycle payloads emitted on the `dsh-status` channel. */
type DshStatus =
  | { status: "starting"; method?: string }
  | { status: "ready"; attached: boolean; method?: string }
  | { status: "notfound" }
  | { status: "error"; message: string };

const REGISTRY_OFFICIAL = "https://registry.npmjs.org";
const REGISTRY_MIRROR = "https://registry.npmmirror.com";

/** Rust-side parallel speed probe of the two npm registries (ms, null=unreachable). */
type NpmProbe = { npmjsMs: number | null; npmmirrorMs: number | null; fastest: string | null };

/** Which tab of the environment-manager panel is open; null = closed. */
type Overlay = null | "env" | "log";

/** ==========================================================================
 *  The shell page.
 *
 *  This page owns exactly one screen: the boot page. The window is undecorated
 *  by CSS only in the sense that the app draws no chrome of its own — the
 *  native title bar is kept (Rust builds the window with `decorations(true)`),
 *  so there is no self-drawn title bar here to keep in sync with the theme.
 *
 *  When DSH becomes ready, Rust navigates the window away to the webchat as a
 *  TOP-LEVEL page (dsh.rs `navigate_webchat`), so this component simply stops
 *  being displayed — there is no iframe and nothing to hide. That is also why
 *  this file contains no theme-switching code: the OS theme reaches the page
 *  through `prefers-color-scheme`, resolved entirely in styles/theme.css.
 * ========================================================================== */
function App() {
  const [status, setStatus] = useState<DshStatus>({ status: "starting" });
  const [customPath, setCustomPath] = useState("");
  const [pathError, setPathError] = useState("");
  const [npmProbe, setNpmProbe] = useState<NpmProbe | null>(null);
  const [overlay, setOverlay] = useState<Overlay>(null);
  /** Env facts, fetched when the panel opens so it shows data immediately. */
  const [envInfo, setEnvInfo] = useState<EnvInfo | null>(null);
  const [envError, setEnvError] = useState("");
  const [refreshing, setRefreshing] = useState(false);

  const refreshEnv = useCallback(() => {
    setRefreshing(true);
    setEnvError("");
    invoke<EnvInfo>("env_info")
      .then(setEnvInfo)
      .catch((e: string) => setEnvError(String(e)))
      .finally(() => setRefreshing(false));
  }, []);

  // Fetch only while the panel is open. Prefetching on mount used to stack up
  // with the `ready` burst and fire a dozen-plus env_info calls per start.
  useEffect(() => {
    if (overlay === "env") {
      refreshEnv();
    }
  }, [overlay, refreshEnv]);

  // Registry speed probe runs once when the notfound chooser appears; the
  // faster source becomes the primary install button, the other stays as an
  // explicit alternative. A failed probe leaves the plain default button.
  useEffect(() => {
    if (status.status !== "notfound" || npmProbe !== null) return;
    invoke<NpmProbe>("dsh_npm_probe")
      .then(setNpmProbe)
      .catch(() => {});
  }, [status.status, npmProbe]);

  useEffect(() => {
    let unlistenStatus: UnlistenFn | undefined;
    let cancelled = false;

    (async () => {
      unlistenStatus = await listen<DshStatus>("dsh-status", (event) => {
        setStatus(event.payload);
      });
      if (cancelled) {
        unlistenStatus();
      }
    })();

    return () => {
      cancelled = true;
      unlistenStatus?.();
    };
  }, []);

  const openLog = useCallback(() => setOverlay("log"), []);

  return (
    <main className="shell">
      <BootView
        status={status}
        customPath={customPath}
        pathError={pathError}
        npmProbe={npmProbe}
        onCustomPathChange={(value) => {
          setCustomPath(value);
          setPathError("");
        }}
        onPathError={setPathError}
        onOpenLog={openLog}
      />

      {overlay !== null && (
        <EnvPanel
          initialTab={overlay}
          info={envInfo}
          error={envError}
          refreshing={refreshing}
          onRefresh={refreshEnv}
          onClose={() => setOverlay(null)}
        />
      )}
    </main>
  );
}

/** ==========================================================================
 *  Boot page — one card, four states.
 *
 *  `starting` and `ready` are transient (DSH is coming up); `notfound` and
 *  `error` are the recovery surfaces and must always offer a way forward.
 * ========================================================================== */
function BootView({
  status,
  customPath,
  pathError,
  npmProbe,
  onCustomPathChange,
  onPathError,
  onOpenLog,
}: {
  status: DshStatus;
  customPath: string;
  pathError: string;
  npmProbe: NpmProbe | null;
  onCustomPathChange: (value: string) => void;
  onPathError: (message: string) => void;
  onOpenLog: () => void;
}) {
  return (
    <div className="boot">
      <div className="boot-card">
        <img className="boot-mark" src={appIcon} alt="" draggable={false} />

        {status.status === "starting" && (
          <>
            <div className="boot-status">
              <div className="spinner" aria-hidden="true" />
            </div>
            <h1 className="boot-title">
              正在启动 DSH{status.method ? `（${status.method}）` : ""}
            </h1>
            {status.method?.includes("npx") && (
              <p className="boot-desc">首次运行需下载 DSH 包，可能需要几分钟，请耐心等待</p>
            )}
            <LogLink onClick={onOpenLog} />
          </>
        )}

        {status.status === "ready" && (
          <>
            <div className="boot-status">
              <div className="spinner" aria-hidden="true" />
            </div>
            <h1 className="boot-title">正在打开…</h1>
            <LogLink onClick={onOpenLog} />
          </>
        )}

        {status.status === "notfound" && <NotFoundView npmProbe={npmProbe} customPath={customPath} pathError={pathError} onCustomPathChange={onCustomPathChange} onPathError={onPathError} />}

        {status.status === "error" && (
          <>
            <h1 className="boot-title boot-title--danger">DSH 启动失败</h1>
            <pre className="boot-detail-block">{status.message}</pre>
            <div className="boot-actions">
              <button type="button" className="btn" onClick={() => invoke("dsh_retry")}>
                重试
              </button>
              <button
                type="button"
                className="btn btn--secondary"
                onClick={() => invoke("dsh_download")}
              >
                改用 npx 下载启动
              </button>
            </div>
            <LogLink onClick={onOpenLog} />
          </>
        )}
      </div>
    </div>
  );
}

function LogLink({ onClick }: { onClick: () => void }) {
  return (
    <button type="button" className="link-btn boot-log-link" onClick={onClick}>
      查看日志
    </button>
  );
}

/** ==========================================================================
 *  "未找到本机 DSH" — the chooser.
 *
 *  Offers, in order of preference: a one-click global install (fastest, and it
 *  leaves a permanent `dsh` command on PATH), the npx fallback, and a manual
 *  path for users who already have DSH somewhere unusual. Nothing here
 *  downloads without an explicit click.
 * ========================================================================== */
function NotFoundView({
  npmProbe,
  customPath,
  pathError,
  onCustomPathChange,
  onPathError,
}: {
  npmProbe: NpmProbe | null;
  customPath: string;
  pathError: string;
  onCustomPathChange: (value: string) => void;
  onPathError: (message: string) => void;
}) {
  const mirrorFastest = npmProbe?.fastest === "npmmirror";
  const ms = (value: number | null) => (value === null ? "不通" : `${value}ms`);

  const primaryRegistry: string | null = mirrorFastest
    ? REGISTRY_MIRROR
    : npmProbe?.fastest === "npmjs"
      ? REGISTRY_OFFICIAL
      : null; // probe pending/failed: plain npm default
  const secondaryRegistry = mirrorFastest ? REGISTRY_OFFICIAL : REGISTRY_MIRROR;

  const primaryLabel = mirrorFastest
    ? `一键全局安装并启动（已选最快：国内镜像 ${ms(npmProbe?.npmmirrorMs ?? null)}）`
    : npmProbe?.fastest === "npmjs"
      ? `一键全局安装并启动（已选最快：官方源 ${ms(npmProbe.npmjsMs)}）`
      : "一键全局安装并启动（推荐，约 1-3 分钟）";

  const secondaryLabel = mirrorFastest
    ? `改用官方源安装（${ms(npmProbe?.npmjsMs ?? null)}）`
    : `改用国内镜像安装（${ms(npmProbe?.npmmirrorMs ?? null)}）`;

  return (
    <>
      <h1 className="boot-title boot-title--warn">未找到本机 DSH</h1>
      <p className="boot-desc">
        已搜索 PATH（where dsh，含 npm 全局 dsh/dsh.cmd）、应用目录与用户目录，均未发现 DSH
        安装。
      </p>

      <div className="boot-actions">
        <button
          type="button"
          className="btn"
          onClick={() => invoke("dsh_install_npm", { registry: primaryRegistry })}
        >
          {primaryLabel}
        </button>

        {npmProbe !== null && (
          <div className="boot-actions--row">
            <button
              type="button"
              className="btn btn--secondary"
              onClick={() => invoke("dsh_install_npm", { registry: secondaryRegistry })}
            >
              {secondaryLabel}
            </button>
          </div>
        )}

        <button
          type="button"
          className="btn btn--secondary"
          onClick={() => invoke("dsh_download")}
        >
          下载并启动（npx 缓存，备选）
        </button>

        <div className="boot-field">
          <input
            className="input"
            value={customPath}
            placeholder="已知安装位置？粘贴 dsh.cmd 完整路径"
            onChange={(event) => onCustomPathChange(event.target.value)}
          />
          <button
            type="button"
            className="btn btn--secondary"
            onClick={() => {
              invoke("dsh_custom_path", { path: customPath }).catch((error: string) =>
                onPathError(String(error)),
              );
            }}
          >
            使用此路径启动
          </button>
        </div>

        {pathError !== "" && <p className="boot-desc boot-title--danger">{pathError}</p>}

        <div className="boot-actions--row">
          <button
            type="button"
            className="btn btn--secondary"
            onClick={() => invoke("dsh_retry")}
          >
            重新检测
          </button>
          <button
            type="button"
            className="btn btn--secondary"
            onClick={() => invoke("dsh_exit")}
          >
            退出
          </button>
        </div>
      </div>

      <p className="boot-hint">
        全局安装后终端可用 <code>dsh</code> 命令，应用启动最快且无需网络；不想全局装就选 npx
        备选或填路径。
      </p>
    </>
  );
}

export default App;
