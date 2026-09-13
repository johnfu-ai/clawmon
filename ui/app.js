/* clawmon frontend — plain JS on the Tauri global API (no bundler needed) */
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

/* The backend polls on its own schedule and pushes a snapshot after every
   pass — there is deliberately no timer here. A hidden window's timers are
   throttled by WebView2, which is exactly the state (tray / pet) the monitor
   has to keep working in. */
let snapshot = { sessions: [], warning: null };
let snapshotAt = Date.now();

function fmtIdle(sec) {
  if (sec == null) return "—";
  if (sec < 60) return t("idle.sec", { n: sec });
  if (sec < 3600) return t("idle.min", { n: Math.floor(sec / 60) });
  const h = Math.floor(sec / 3600);
  const m = Math.floor((sec % 3600) / 60);
  return t("idle.hour", { h, m });
}

function fmtCountdown(sec) {
  if (sec == null || sec <= 0) return t("cd.now");
  const h = Math.floor(sec / 3600);
  const m = Math.floor((sec % 3600) / 60);
  const s = sec % 60;
  const pad = (n) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
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
  snapshot = { sessions, warning };
  snapshotAt = Date.now();

  const banner = $("banner");
  if (warning) {
    banner.textContent = `⚠ ${warning}`;
    banner.classList.remove("hidden");
  } else {
    banner.classList.add("hidden");
  }
  $("foot-status").textContent = warning
    ? t("foot.bad")
    : t("foot.ok", { n: sessions.length });

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
    const state = LIGHTS.includes(s.state) ? s.state : "yellow";
    const light = `<span class="light ${state}"></span>`;
    const meta = [
      `<span class="pid">PID ${s.pid}</span>`,
      s.tmuxLabel ? `<span class="tmux">tmux ${escapeHtml(s.tmuxLabel)}</span>`
                  : `<span class="tmux">${t("no.tmux")}</span>`,
      `<span>${t("meta.idle", { s: fmtIdle(s.idleSec) })}</span>`,
      s.tmuxLabel ? "" : `<span class="no-control">${t("monitor.only")}</span>`,
    ].filter(Boolean).join("");

    let countdown = "";
    if (state === "red") {
      if (s.controllable && s.remainingSec != null) {
        // the ticker below keeps this one counting between polls
        countdown = `<span class="countdown" data-countdown="${s.pid}">${
          countdownText(s, s.remainingSec)}</span>`;
      } else if (!s.controllable) {
        countdown = `<span class="countdown sent">${t("cd.notmux")}</span>`;
      } else if (s.sends > 0) {
        countdown = `<span class="countdown sent">${t("cd.limit", { n: s.sends })}</span>`;
      } else {
        countdown = `<span class="countdown sent">${t("cd.disabled")}</span>`;
      }
    }

    const preview = s.preview ? `<div class="session-preview">“${escapeHtml(s.preview)}”</div>` : "";
    const btn = s.controllable
      ? `<button class="act" data-pid="${s.pid}">${t("act.continue")}</button>`
      : `<span class="no-control">${t("no.control")}</span>`;

    return `
      <div class="session">
        <div class="session-top">
          ${light}
          <span class="session-name" title="${escapeHtml(s.cwd)}">${escapeHtml(s.project)}</span>
          <span class="session-state ${state}">${escapeHtml(stateLabel(s.label))}</span>
        </div>
        <div class="session-meta">${meta}</div>
        ${preview}
        <div class="session-bottom">
          ${countdown}
          ${btn}
        </div>
      </div>`;
  }).join("");

  $("summary").textContent = t("sum", {
    g: counts.green,
    y: counts.yellow,
    r: counts.red,
  });
  for (const b of list.querySelectorAll("button.act")) {
    b.addEventListener("click", () => onContinue(Number(b.dataset.pid)));
  }
}

const LIGHTS = ["green", "yellow", "red"];

function escapeHtml(value) {
  return String(value).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  })[c]);
}

/* A red session shows either "…until auto-continue" or, once it has been
   resumed, "resumed N × · …" — both need the send count in front. */
function countdownText(session, remainingSec) {
  const prefix = session.sends > 0 ? t("cd.resumed", { n: session.sends }) : "";
  return prefix + fmtCountdown(remainingSec) + t("cd.until");
}

/* The backend speaks every few seconds; tick the countdowns in between so the
   seconds actually move. */
function tickCountdowns() {
  const elapsed = Math.floor((Date.now() - snapshotAt) / 1000);
  for (const s of snapshot.sessions) {
    if (s.state !== "red" || !s.controllable || s.remainingSec == null) continue;
    const el = document.querySelector(`[data-countdown="${s.pid}"]`);
    if (el) el.textContent = countdownText(s, s.remainingSec - elapsed);
  }
}

async function onContinue(pid) {
  try {
    const msg = await invoke("send_continue", { pid });
    toast(msg);
  } catch (e) {
    toast(String(e), true);
  }
}

/** Render the backend's latest snapshot without waiting for the next poll. */
async function refresh() {
  try {
    const res = await invoke("get_status");
    render(res.sessions, res.warning);
  } catch (e) {
    $("foot-status").textContent = t("foot.fail");
    toast(String(e), true);
  }
}

/* ---------- settings panel ---------- */

function loadSettingsForm(s) {
  const f = $("settings-form");
  f.language.value = s.language || "zh";
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
  f.notifyRed.checked = s.notifyRed;
  f.notifyContinue.checked = s.notifyContinue;
  f.notifyRecovered.checked = s.notifyRecovered;
  f.notifyTurnEnd.checked = s.notifyTurnEnd;
  f.notifyExit.checked = s.notifyExit;
  f.soundAlerts.checked = s.soundAlerts;
  f.autoUpdate.checked = s.autoUpdate;
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
  // unlike the others, 0 hours is a valid choice ("resume as soon as it turns
  // red"), so it must not fall back to the default
  const hours = Number(f.waitHours.value);
  const s = {
    language: f.language.value === "en" ? "en" : "zh",
    pollIntervalSecs: num(f.pollIntervalSecs.value, 5),
    idleGreenSecs: num(f.idleGreenSecs.value, 120),
    blockedAfterSecs: num(f.blockedAfterSecs.value, 300),
    autoContinue: f.autoContinue.checked,
    closeToTray: f.closeToTray.checked,
    waitSecs: Math.round(
      (Number.isFinite(hours) && hours >= 0 ? Math.min(hours, 24) : 5) * 3600),
    resumeKeys: f.resumeKeys.value.trim() || "Enter",
    maxSends: num(f.maxSends.value, 3),
    retryIntervalSecs: num(f.retryIntervalMin.value, 10) * 60,
    wslDistro: f.wslDistro.value.trim(),
    notifyRed: f.notifyRed.checked,
    notifyContinue: f.notifyContinue.checked,
    notifyRecovered: f.notifyRecovered.checked,
    notifyTurnEnd: f.notifyTurnEnd.checked,
    notifyExit: f.notifyExit.checked,
    soundAlerts: f.soundAlerts.checked,
    autoUpdate: f.autoUpdate.checked,
  };
  try {
    await invoke("set_settings", { settings: s });
    closeSettings();
    toast(t("toast.saved"));
    // the backend re-reads the poll interval on every pass, nothing to restart
  } catch (e) {
    toast(String(e), true);
  }
}

/* ---------- boot ---------- */

window.addEventListener("DOMContentLoaded", async () => {
  $("btn-settings").addEventListener("click", openSettings);
  $("btn-cancel-settings").addEventListener("click", closeSettings);
  $("settings-form").addEventListener("submit", saveSettings);

  // language first, so the very first paint already uses it
  try {
    setLang((await invoke("get_settings")).language);
  } catch (_) { /* zh stays */ }
  listen("settings", (e) => setLang(e.payload.language));

  // every poll the backend makes ends up here
  listen("sessions", (event) => render(event.payload.sessions, event.payload.warning));
  await refresh();

  // WebView2 stops running this page's scripts while the window is hidden
  // (tray, or minimized to the pet), so pick up the latest snapshot when the
  // window comes back instead of trusting whatever is still on screen.
  window.addEventListener("focus", refresh);
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) refresh();
  });

  setInterval(tickCountdowns, 1000);
  setInterval(() => {
    const d = new Date();
    $("foot-clock").textContent = d.toLocaleTimeString("zh-CN", { hour12: false });
  }, 1000);
});
