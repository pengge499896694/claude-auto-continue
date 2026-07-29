import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { check } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

// ---- Types matching the Rust payloads -------------------------------------
interface SessionItem {
  path: string;
  project: string;
  session_id: string;
  modified_at: number;
  display: string;
}
interface WindowItem {
  hwnd: number;
  title: string;
  executable: string;
  pid: number;
  kind: string;
  kind_label: string;
  display: string;
}
interface Config {
  quiet_seconds: number;
  cooldown_seconds: number;
  stalled_seconds: number;
  max_continues: number;
  mode: string;
  target_kind: string;
  prompt: string;
  check_existing: boolean;
  follow_latest: boolean;
  custom_keywords_enabled: boolean;
  custom_keywords: string[];
  heartbeat_log_enabled: boolean;
  confirm_completion: boolean;
}
interface PairView {
  id: string;
  session_display: string;
  window_label: string;
  target_kind: string;
  continue_count: number;
  status: string;
  status_kind: string;
}
interface InitialPayload {
  config: Config;
  sessions: SessionItem[];
  selected_session: string;
  windows: WindowItem[];
  selected_window: string;
  pairs: PairView[];
  watching: boolean;
}

// ---- Element helpers ------------------------------------------------------
const $ = <T extends HTMLElement = HTMLElement>(id: string) =>
  document.getElementById(id) as T;

const sessionSelect = $<HTMLSelectElement>("session-select");
const windowSelect = $<HTMLSelectElement>("window-select");
const statusDot = $("status-dot");
const statusText = $("status-text");
const logBox = $("log-box");
const toastEl = $("toast");
const btnToggle = $<HTMLButtonElement>("btn-toggle");
const pairsList = $("pairs-list");
const pairsEmpty = $("pairs-empty");

const cfgEls = {
  target: $<HTMLSelectElement>("cfg-target"),
  mode: $<HTMLSelectElement>("cfg-mode"),
  quiet: $<HTMLInputElement>("cfg-quiet"),
  max: $<HTMLInputElement>("cfg-max"),
  cooldown: $<HTMLInputElement>("cfg-cooldown"),
  stalled: $<HTMLInputElement>("cfg-stalled"),
  prompt: $<HTMLTextAreaElement>("cfg-prompt"),
  checkExisting: $<HTMLInputElement>("cfg-check-existing"),
  followLatest: $<HTMLInputElement>("cfg-follow-latest"),
  keywordsEnabled: $<HTMLInputElement>("cfg-keywords-enabled"),
  keywords: $<HTMLTextAreaElement>("cfg-keywords"),
  heartbeat: $<HTMLInputElement>("cfg-heartbeat"),
  confirmCompletion: $<HTMLInputElement>("cfg-confirm-completion"),
};

let watching = false;

// ---- Config <-> UI --------------------------------------------------------
function applyConfig(c: Config) {
  cfgEls.target.value = c.target_kind;
  cfgEls.mode.value = c.mode;
  cfgEls.quiet.value = String(c.quiet_seconds);
  cfgEls.max.value = String(c.max_continues);
  cfgEls.cooldown.value = String(c.cooldown_seconds);
  cfgEls.stalled.value = String(c.stalled_seconds);
  cfgEls.prompt.value = c.prompt;
  cfgEls.checkExisting.checked = c.check_existing;
  cfgEls.followLatest.checked = c.follow_latest;
  cfgEls.keywordsEnabled.checked = c.custom_keywords_enabled;
  cfgEls.keywords.value = (c.custom_keywords || []).join("\n");
  cfgEls.heartbeat.checked = c.heartbeat_log_enabled;
  cfgEls.confirmCompletion.checked = c.confirm_completion;
}

function collectConfig(): Config {
  const num = (el: HTMLInputElement, fallback: number) => {
    const v = parseFloat(el.value);
    return Number.isFinite(v) ? v : fallback;
  };
  return {
    quiet_seconds: num(cfgEls.quiet, 7),
    cooldown_seconds: num(cfgEls.cooldown, 15),
    stalled_seconds: num(cfgEls.stalled, 60),
    max_continues: Math.max(1, Math.round(num(cfgEls.max, 12))),
    mode: cfgEls.mode.value,
    target_kind: cfgEls.target.value,
    prompt: cfgEls.prompt.value,
    check_existing: cfgEls.checkExisting.checked,
    follow_latest: cfgEls.followLatest.checked,
    custom_keywords_enabled: cfgEls.keywordsEnabled.checked,
    custom_keywords: cfgEls.keywords.value
      .split(/[\n,，]/)
      .map((s) => s.trim())
      .filter((s) => s.length > 0),
    heartbeat_log_enabled: cfgEls.heartbeat.checked,
    confirm_completion: cfgEls.confirmCompletion.checked,
  };
}

// ---- Rendering ------------------------------------------------------------
function renderSessions(sessions: SessionItem[], selected: string) {
  sessionSelect.innerHTML = "";
  if (sessions.length === 0) {
    const opt = document.createElement("option");
    opt.textContent = "未找到 Claude 会话";
    opt.value = "";
    sessionSelect.appendChild(opt);
    return;
  }
  for (const s of sessions) {
    const opt = document.createElement("option");
    opt.value = s.path;
    opt.textContent = s.display;
    sessionSelect.appendChild(opt);
  }
  if (selected) sessionSelect.value = selected;
}

function renderWindows(windows: WindowItem[], selected: string) {
  windowSelect.innerHTML = "";
  const auto = document.createElement("option");
  auto.value = "__auto__";
  auto.textContent = "自动查找（优先匹配会话项目）";
  windowSelect.appendChild(auto);
  for (const w of windows) {
    const opt = document.createElement("option");
    opt.value = String(w.hwnd);
    opt.textContent = w.display;
    windowSelect.appendChild(opt);
  }
  windowSelect.value = selected || "__auto__";
}

function renderPairs(pairs: PairView[]) {
  pairsList.innerHTML = "";
  pairsEmpty.style.display = pairs.length === 0 ? "block" : "none";
  for (const p of pairs) {
    const row = document.createElement("div");
    row.className = "pair-row";

    const main = document.createElement("div");
    main.className = "pair-main";
    const title = document.createElement("div");
    title.className = "pair-title";
    title.textContent = p.session_display;
    const meta = document.createElement("div");
    meta.className = "pair-meta";
    meta.textContent = `→ ${p.window_label}`;
    main.append(title, meta);

    const status = document.createElement("span");
    status.className = "pair-status lv-" + (p.status_kind || "info");
    const countText = p.continue_count > 0 ? `（${p.continue_count} 次）` : "";
    status.textContent = (p.status || "待监听") + countText;

    const del = document.createElement("button");
    del.className = "btn btn-ghost pair-del";
    del.textContent = "移除";
    del.onclick = () => invoke("remove_pair", { id: p.id });

    row.append(main, status, del);
    pairsList.appendChild(row);
  }
}

function setStatus(text: string, kind: string) {
  statusText.textContent = text;
  statusDot.className = "status-dot " + kind;
}

function pushLog(time: string, msg: string, level: string) {
  const line = document.createElement("div");
  line.className = "log-line";
  const t = document.createElement("span");
  t.className = "log-time";
  t.textContent = time;
  const m = document.createElement("span");
  m.className = "lv-" + level;
  m.textContent = msg;
  line.append(t, m);
  logBox.appendChild(line);
  while (logBox.childElementCount > 500) logBox.removeChild(logBox.firstChild!);
  logBox.scrollTop = logBox.scrollHeight;
}

// ---- In-app confirm dialog ------------------------------------------------
const modalMask = $("modal-mask");
const modalTitle = $("modal-title");
const modalBody = $("modal-body");
const modalOk = $<HTMLButtonElement>("modal-ok");
const modalCancel = $<HTMLButtonElement>("modal-cancel");

function confirmDialog(
  message: string,
  opts: { title?: string; okText?: string; cancelText?: string } = {}
): Promise<boolean> {
  modalTitle.textContent = opts.title ?? "确认";
  modalBody.textContent = message;
  modalOk.textContent = opts.okText ?? "确定";
  modalCancel.textContent = opts.cancelText ?? "取消";
  modalMask.classList.add("show");

  return new Promise<boolean>((resolve) => {
    let done = false;
    const finish = (result: boolean) => {
      if (done) return;
      done = true;
      modalMask.classList.remove("show");
      modalOk.onclick = null;
      modalCancel.onclick = null;
      modalMask.onclick = null;
      document.removeEventListener("keydown", onKey);
      resolve(result);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") finish(false);
      else if (e.key === "Enter") finish(true);
    };
    modalOk.onclick = () => finish(true);
    modalCancel.onclick = () => finish(false);
    modalMask.onclick = (e) => {
      if (e.target === modalMask) finish(false);
    };
    document.addEventListener("keydown", onKey);
    modalOk.focus();
  });
}

let toastTimer: number | undefined;
function toast(msg: string, type = "info") {
  toastEl.textContent = msg;
  toastEl.className = "toast show " + type;
  clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => {
    toastEl.className = "toast " + type;
  }, 2600);
}

function setWatching(on: boolean) {
  watching = on;
  if (on) {
    btnToggle.textContent = "■ 停止监听";
    btnToggle.className = "btn btn-danger";
  } else {
    btnToggle.textContent = "▶ 开始监听";
    btnToggle.className = "btn btn-primary";
  }
}

// ---- Command wrappers -----------------------------------------------------
async function refreshSessions() {
  const p = await invoke<{ sessions: SessionItem[]; selected: string }>("refresh_sessions");
  renderSessions(p.sessions, p.selected);
}
async function refreshWindows() {
  const p = await invoke<{ windows: WindowItem[]; selected: string }>("refresh_windows");
  renderWindows(p.windows, p.selected);
}

// ---- Event wiring ---------------------------------------------------------
function wireEvents() {
  $("btn-refresh-sessions").onclick = () => refreshSessions();
  $("btn-open-folder").onclick = () => invoke("open_session_folder");
  $("btn-refresh-windows").onclick = () => refreshWindows();
  $("btn-bind").onclick = () => invoke("bind_after_countdown");
  $("btn-log").onclick = () => invoke("open_log");

  sessionSelect.onchange = () => invoke("select_session", { path: sessionSelect.value });
  windowSelect.onchange = () => invoke("select_window", { value: windowSelect.value });

  $("btn-add-pair").onclick = async () => {
    // Persist the current client-type choice so the new pair adopts it.
    await invoke("save_config", { config: collectConfig() });
    await invoke("add_pair");
  };

  btnToggle.onclick = async () => {
    if (watching) {
      await invoke("stop_watch", { reason: null });
    } else {
      const ok = await invoke<boolean>("start_watch", { config: collectConfig() });
      if (!ok) toast("请先选择有效的 Claude 会话", "error");
    }
  };

  $("btn-analyze").onclick = async () => {
    await invoke("save_config", { config: collectConfig() });
    await invoke("analyze_now");
  };

  $("btn-test").onclick = async () => {
    const ok = await confirmDialog(
      "测试会真实切换到目标窗口（VS Code 或终端），并发送当前续跑提示词。是否继续？",
      { title: "测试发送", okText: "发送", cancelText: "取消" }
    );
    if (!ok) return;
    await invoke("save_config", { config: collectConfig() });
    await invoke("test_send");
  };

  // Persist config on any change.
  for (const el of Object.values(cfgEls)) {
    el.addEventListener("change", () => {
      invoke("save_config", { config: collectConfig() });
    });
  }
}

async function listenBackend() {
  await listen<{ time: string; msg: string; level: string }>("log", (e) =>
    pushLog(e.payload.time, e.payload.msg, e.payload.level)
  );
  await listen<{ text: string; kind: string }>("status", (e) =>
    setStatus(e.payload.text, e.payload.kind)
  );
  await listen<{ on: boolean }>("watch", (e) => setWatching(e.payload.on));
  await listen<{ sessions: SessionItem[]; selected: string }>("sessions", (e) =>
    renderSessions(e.payload.sessions, e.payload.selected)
  );
  await listen<{ windows: WindowItem[]; selected: string }>("windows", (e) =>
    renderWindows(e.payload.windows, e.payload.selected)
  );
  await listen<{ type: string; msg: string }>("toast", (e) =>
    toast(e.payload.msg, e.payload.type)
  );
  await listen<{ pairs: PairView[]; watching: boolean }>("pairs", (e) => {
    renderPairs(e.payload.pairs);
    setWatching(e.payload.watching);
  });
}

// ---- Auto update ----------------------------------------------------------
// Checks GitHub Releases for a newer signed build. On finding one, asks the
// user with the in-app dialog, then downloads + installs and relaunches.
// `silent` suppresses the "already latest / check failed" toasts (used on the
// automatic startup check, so it never nags when there's nothing to do).
async function checkForUpdates(silent: boolean) {
  // Immediate feedback on a manual click, so it never feels like "no response".
  if (!silent) {
    toast("正在检查更新…", "info");
    setStatus("正在检查更新…", "info");
  }
  // The updater's check() fetches latest.json from GitHub. On a bad/blocked
  // network it can hang ~20s; race it against a timeout so the user gets a
  // clear "can't reach GitHub" message quickly instead of a frozen-feeling UI.
  const TIMEOUT_MS = 10000;
  let update;
  try {
    update = await Promise.race([
      check(),
      new Promise<never>((_, reject) =>
        setTimeout(() => reject(new Error("timeout")), TIMEOUT_MS)
      ),
    ]);
  } catch (e) {
    // Results use the persistent status line (not a 2.6s toast) so they don't
    // flash by and get missed.
    const timedOut = e instanceof Error && e.message === "timeout";
    const msg = timedOut
      ? "连接 GitHub 超时，无法检查更新。请检查网络（或代理）后重试。"
      : "检查更新失败：" + e;
    if (!silent) {
      toast(timedOut ? "连接 GitHub 超时" : "检查更新失败", "error");
      setStatus(msg, "err");
    }
    return;
  }
  if (!update) {
    if (!silent) {
      toast("当前已是最新版本", "success");
      setStatus("当前已是最新版本", "info");
    }
    return;
  }

  const notes = (update.body || "").trim();
  const ok = await confirmDialog(
    `发现新版本 ${update.version}（当前 ${update.currentVersion}）。\n\n` +
      (notes ? notes + "\n\n" : "") +
      "是否现在下载并安装？安装完成后程序会自动重启。",
    { title: "发现新版本", okText: "立即更新", cancelText: "以后再说" }
  );
  if (!ok) return;

  try {
    toast("正在下载更新…", "info");
    setStatus("正在下载更新…", "info");
    let downloaded = 0;
    let total = 0;
    await update.downloadAndInstall((ev) => {
      if (ev.event === "Started") {
        total = ev.data.contentLength ?? 0;
      } else if (ev.event === "Progress") {
        downloaded += ev.data.chunkLength ?? 0;
        if (total > 0) {
          const pct = Math.round((downloaded / total) * 100);
          setStatus(`下载更新中… ${pct}%`, "info");
        }
      } else if (ev.event === "Finished") {
        setStatus("更新下载完成，准备安装…", "info");
      }
    });
    await confirmDialog("更新已安装，点击“重启”以使用新版本。", {
      title: "更新完成",
      okText: "重启",
      cancelText: "稍后",
    }).then((doRestart) => {
      if (doRestart) return relaunch();
    });
  } catch (e) {
    toast("更新失败：" + e, "error");
    setStatus("更新失败", "err");
  }
}

async function init() {
  wireEvents();
  await listenBackend();
  const data = await invoke<InitialPayload>("get_initial");
  applyConfig(data.config);
  renderSessions(data.sessions, data.selected_session);
  renderWindows(data.windows, data.selected_window);
  renderPairs(data.pairs || []);
  setWatching(data.watching);

  // Manual "check for updates" button.
  const btnUpdate = document.getElementById("btn-update");
  if (btnUpdate) btnUpdate.onclick = () => checkForUpdates(false);

  // Silent check shortly after startup so it never blocks first paint.
  setTimeout(() => checkForUpdates(true), 3000);
}

window.addEventListener("DOMContentLoaded", init);
