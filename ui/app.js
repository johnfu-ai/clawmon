/* clawmon frontend — plain JS on the Tauri global API (no bundler needed) */
const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);
let pollTimer = null;
let pollInterval = 5;

function fmtIdle(sec) {
  if (sec == null) return "—";
  if (sec < 60) return `${sec} 秒`;
  if (sec < 3600) return `${Math.floor(sec / 60)} 分钟`;
  const h = Math.floor(sec / 3600);
  const m = Math.floor((sec % 3600) / 60);
  return `${h} 小时 ${m} 分`;
}

function fmtCountdown(sec) {
  if (sec == null || sec <= 0) return "即将发送";
  const h = Math.floor(sec / 3600);
  const m = Math.floor((sec % 3600) / 60);
  const s = sec % 60;
  const pad = (n) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(s)} 后自动继续`
               : `${m}:${pad(s)} 后自动继续`;
}

let toastTimer = null;
function toast(msg, isError = false) {
  let el = document.querySelector(".toast");
  if (!el) {
    el = document.createElement("div");
    el.className = "toast";
    document.body.appendChild(el);
  }
  el.textContent = msg;
  el.className = "toast show" + (isError ? " error" : "");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (el.className = "toast"), 2500);
}

function render(sessions, warning) {
  const banner = $("banner");
  if (warning) {
    banner.textContent = `⚠ ${warning}`;
    banner.classList.remove("hidden");
  } else {
    banner.classList.add("hidden");
  }

  const list = $("list");
  const empty = $("empty");
  if (!sessions.length) {
    list.innerHTML = "";
    empty.classList.remove("hidden");
    $("summary").textContent = "";
    return;
  }
  empty.classList.add("hidden");

  const counts = { green: 0, yellow: 0, red: 0 };
  for (const s of sessions) counts[s.state]++;

  list.innerHTML = sessions.map((s) => {
    const rows = [];
    const light = `<span class="light ${s.state}"></span>`;
    const meta = [
      `<span class="pid">PID ${s.pid}</span>`,
      s.tmuxLabel ? `<span class="tmux">tmux ${s.tmuxLabel}</span>`
                  : `<span class="tmux">无 tmux</span>`,
      `<span>空闲 ${fmtIdle(s.idleSec)}</span>`,
      s.tmuxLabel ? "" : `<span class="no-control">仅监控</span>`,
    ].filter(Boolean).join("");

    let countdown = "";
    if (s.state === "red" && s.remainingSec != null) {
      countdown = `<span class="countdown">${fmtCountdown(s.remainingSec)}</span>`;
    } else if (s.state === "red" && s.sends > 0 && s.lastSendAt) {
      countdown = `<span class="countdown sent">已自动继续 ${s.sends} 次</span>`;
    } else if (s.state === "red") {
      countdown = `<span class="countdown sent">已停用自动继续</span>`;
    }

    const preview = s.preview ? `<div class="session-preview">“${escapeHtml(s.preview)}”</div>` : "";
    const btn = s.controllable
      ? `<button class="act" data-pid="${s.pid}">立即继续</button>`
      : `<span class="no-control">不在 tmux 中，无法控制</span>`;

    rows.push(`
      <div class="session">
        <div class="session-top">
          ${light}
          <span class="session-name" title="${escapeHtml(s.cwd)}">${escapeHtml(s.project)}</span>
          <span class="session-state ${s.state}">${s.label}</span>
        </div>
        <div class="session-meta">${meta}</div>
        ${preview}
        <div class="session-bottom">
          ${countdown}
          ${btn}
        </div>
      </div>`);
    return rows.join("");
  }).join("");

  $("summary").textContent =
    `● ${counts.green} 运行 · ● ${counts.yellow} 等待 · ● ${counts.red} 超时`;

  for (const b of list.querySelectorAll("button.act")) {
    b.addEventListener("click", () => onContinue(Number(b.dataset.pid)));
  }
}

function escapeHtml(s) {
  return s.replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  })[c]);
}

async function onContinue(pid) {
  try {
    const msg = await invoke("send_continue", { pid });
    toast(msg);
  } catch (e) {
    toast(String(e), true);
  }
}

async function poll() {
  try {
    const res = await invoke("get_status");
    render(res.sessions, res.warning);
    $("foot-status").textContent =
      res.warning ? "WSL 连接异常" : `WSL 正常 · ${res.sessions.length} 个会话`;
  } catch (e) {
    $("foot-status").textContent = "查询失败";
    toast(String(e), true);
  }
}

function restartTimer() {
  clearInterval(pollTimer);
  pollTimer = setInterval(poll, pollInterval * 1000);
}

/* ---------- settings panel ---------- */

function loadSettingsForm(s) {
  const f = $("settings-form");
  f.pollIntervalSecs.value = s.pollIntervalSecs;
  f.idleGreenSecs.value = s.idleGreenSecs;
  f.blockedAfterSecs.value = s.blockedAfterSecs;
  f.autoContinue.checked = s.autoContinue;
  f.closeToTray.checked = s.closeToTray;
  f.waitHours.value = (s.waitSecs / 3600).toFixed(1);
  f.resumeKeys.value = s.resumeKeys;
  f.maxSends.value = s.maxSends;
  f.retryIntervalMin.value = Math.round(s.retryIntervalSecs / 60);
  f.wslDistro.value = s.wslDistro;
}

async function openSettings() {
  const s = await invoke("get_settings");
  loadSettingsForm(s);
  $("settings").classList.remove("hidden");
  $("list").classList.add("hidden");
  $("empty").classList.add("hidden");
}

function closeSettings() {
  $("settings").classList.add("hidden");
  $("list").classList.remove("hidden");
}

async function saveSettings(ev) {
  ev.preventDefault();
  const f = $("settings-form");
  const num = (v, dflt) => {
    const n = Number(v);
    return Number.isFinite(n) && n > 0 ? n : dflt;
  };
  const s = {
    pollIntervalSecs: num(f.pollIntervalSecs.value, 5),
    idleGreenSecs: num(f.idleGreenSecs.value, 120),
    blockedAfterSecs: num(f.blockedAfterSecs.value, 300),
    autoContinue: f.autoContinue.checked,
    closeToTray: f.closeToTray.checked,
    waitSecs: Math.round(num(f.waitHours.value, 5) * 3600),
    resumeKeys: f.resumeKeys.value.trim() || "Enter",
    maxSends: num(f.maxSends.value, 3),
    retryIntervalSecs: num(f.retryIntervalMin.value, 10) * 60,
    wslDistro: f.wslDistro.value.trim(),
  };
  try {
    await invoke("set_settings", { settings: s });
    pollInterval = s.pollIntervalSecs;
    restartTimer();
    closeSettings();
    toast("设置已保存");
    poll();
  } catch (e) {
    toast(String(e), true);
  }
}

/* ---------- boot ---------- */

window.addEventListener("DOMContentLoaded", async () => {
  $("btn-settings").addEventListener("click", openSettings);
  $("btn-cancel-settings").addEventListener("click", closeSettings);
  $("settings-form").addEventListener("submit", saveSettings);

  try {
    const s = await invoke("get_settings");
    pollInterval = s.pollIntervalSecs;
  } catch { /* keep default */ }

  poll();
  restartTimer();
  setInterval(() => {
    const d = new Date();
    $("foot-clock").textContent = d.toLocaleTimeString("zh-CN", { hour12: false });
  }, 1000);
});
