import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

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
}
interface InitialPayload {
  config: Config;
  sessions: SessionItem[];
  selected_session: string;
  windows: WindowItem[];
  selected_window: string;
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
}

async function init() {
  wireEvents();
  await listenBackend();
  const data = await invoke<InitialPayload>("get_initial");
  applyConfig(data.config);
  renderSessions(data.sessions, data.selected_session);
  renderWindows(data.windows, data.selected_window);
}

window.addEventListener("DOMContentLoaded", init);
