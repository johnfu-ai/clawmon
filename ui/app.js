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
let lastTasks = [];

/* ---------- unified main nav (single source of truth) ----------
   The nav items exist in exactly one array; every page renders through it.
   No page code may add, remove or reorder items — the unified-nav rule is
   enforced by structure, the same way the design prototypes did it. */
const NAV = [
  { id: "sessions", key: "nav.sessions", ico: "▤" },
  { id: "tasks", key: "nav.tasks", ico: "☰" },
  { id: "usage", key: "nav.usage", ico: "◐" },
  { id: "settings", key: "nav.settings", ico: "⚙" },
];
const PAGES = ["sessions", "tasks", "usage", "settings"];
let activePage = "sessions";

function renderNav() {
  $("nav").innerHTML = NAV.map((n) =>
    `<button class="nav-item${n.id === activePage ? " active" : ""}" data-page="${n.id}"` +
    ` title="${t(n.key)}"><span class="ico">${n.ico}</span><span class="lbl">${t(n.key)}</span></button>`
  ).join("");
}

async function showPage(id) {
  activePage = id;
  for (const p of PAGES) {
    $(`page-${p}`).classList.toggle("hidden", p !== id);
  }
  renderNav();
  // entering settings refreshes the form from the live settings, so a save
  // elsewhere (or a hand-edited settings.json picked up at boot) is reflected
  if (id === "settings") {
    try {
      loadSettingsForm(await invoke("get_settings"));
    } catch (e) {
      toast(String(e), true);
    }
  }
}

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

function fmtTokens(n) {
  if (n >= 1e6) return (n / 1e6).toFixed(1).replace(/\.0$/, "") + "M";
  if (n >= 1e3) return Math.round(n / 1e3) + "K";
  return String(n);
}

function fmtClock(secs) {
  if (!secs) return "";
  const d = new Date(secs * 1000);
  const pad = (n) => String(n).padStart(2, "0");
  const now = new Date();
  const sameDay = d.getFullYear() === now.getFullYear()
    && d.getMonth() === now.getMonth()
    && d.getDate() === now.getDate();
  const hhmm = `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  return sameDay ? hhmm : `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${hhmm}`;
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

/* ---------- sessions page ---------- */

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
    renderUsagePage();
    return;
  }
  empty.classList.add("hidden");

  const counts = { green: 0, blue: 0, yellow: 0, red: 0 };
  for (const s of sessions) counts[s.state]++;

  list.innerHTML = sessions.map((s) => {
    const state = LIGHTS.includes(s.state) ? s.state : "yellow";
    const light = `<span class="light ${state}"></span>`;
    // display-only derivation: a task-launched session lives in a tmux
    // session named clawmon-task-* (the label is "session:window")
    const fromTask = (s.tmuxLabel || "").startsWith("clawmon-task-");
    const taskBadge = fromTask
      ? `<span class="task-badge" title="${escapeHtml(t("tasks.linked"))}">▶ ${escapeHtml(t("tasks.title"))}</span>`
      : "";
    const meta = [
      `<span class="pid">PID ${s.pid}</span>`,
      s.tmuxLabel ? `<span class="tmux">tmux ${escapeHtml(s.tmuxLabel)}</span>`
                  : `<span class="tmux">${t("no.tmux")}</span>`,
      `<span>${t("meta.idle", { s: fmtIdle(s.idleSec) })}</span>`,
      s.usage ? `<span class="tokens" title="${escapeHtml(t("meta.usage.tip"))}">${
          t("meta.usage", {
            i: fmtTokens(s.usage.input),
            c: fmtTokens(s.usage.cacheRead + s.usage.cacheCreation),
            o: fmtTokens(s.usage.output),
            n: s.usage.requests,
          })}</span>` : "",
      s.tmuxLabel ? "" : `<span class="no-control">${t("monitor.only")}</span>`,
    ].filter(Boolean).join("");

    // the backend computes why the countdown row shows what it shows; this
    // is a dumb switch on the tag, never a re-derivation from other fields
    let countdown = "";
    if (s.countdown) {
      const c = s.countdown;
      if (c.kind === "waiting") {
        // the ticker below keeps this one counting between polls
        countdown = `<span class="countdown" data-countdown="${s.pid}">${
          countdownText(s, c.remainingSec)}</span>`;
      } else if (c.kind === "no_tmux") {
        countdown = `<span class="countdown sent">${t("cd.notmux")}</span>`;
      } else if (c.kind === "capped") {
        countdown = `<span class="countdown sent">${t("cd.limit", { n: c.sends })}</span>`;
      } else {
        countdown = `<span class="countdown sent">${t("cd.disabled")}</span>`;
      }
    }

    const preview = s.preview ? `<div class="session-preview">“${escapeHtml(s.preview)}”</div>` : "";
    const btn = s.controllable
      ? `<button class="act" data-pid="${s.pid}">${t("act.continue")}</button>`
      : `<span class="no-control">${t("no.control")}</span>`;

    return `
      <div class="session" data-state="${state}">
        <div class="session-top">
          ${light}
          <span class="session-name" title="${escapeHtml(s.cwd)}">${escapeHtml(s.project)}</span>
          ${taskBadge}
          <span class="session-state ${state}">${escapeHtml(t("reason." + s.reason))}</span>
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
    b: counts.blue,
    y: counts.yellow,
    r: counts.red,
  });
  for (const b of list.querySelectorAll("button.act")) {
    b.addEventListener("click", () => onContinue(Number(b.dataset.pid)));
  }
  renderUsagePage();
}

const LIGHTS = ["green", "blue", "yellow", "red"];

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
    if (!s.countdown || s.countdown.kind !== "waiting") continue;
    const el = document.querySelector(`[data-countdown="${s.pid}"]`);
    if (el) el.textContent = countdownText(s, s.countdown.remainingSec - elapsed);
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
    lastTasks = await invoke("get_tasks");
    renderTasks(lastTasks);
  } catch (e) {
    $("foot-status").textContent = t("foot.fail");
    toast(String(e), true);
  }
}

/* ---------- tasks page ---------- */

function taskActions(task) {
  switch (task.status) {
    case "running":
    case "launching":
      return `
        <button class="act sm" data-act="terminal" data-id="${task.id}">${t("tasks.act.terminal")}</button>
        <button class="act sm danger" data-act="stop" data-id="${task.id}" ${task.status === "launching" ? "disabled" : ""}>${t("tasks.act.stop")}</button>`;
    default:
      return `
        <button class="act sm primary" data-act="launch" data-id="${task.id}">${
          task.status === "finished" ? t("tasks.act.relaunch") : t("tasks.act.launch")}</button>
        <button class="act sm" data-act="edit" data-id="${task.id}">${t("tasks.act.edit")}</button>
        <button class="act sm" data-act="delete" data-id="${task.id}">${t("tasks.act.delete")}</button>`;
  }
}

function renderTasks(tasks) {
  lastTasks = tasks;
  $("task-count").textContent = tasks.length ? t("tasks.count", { n: tasks.length }) : "";
  const list = $("task-list");
  const empty = $("task-empty");
  if (!tasks.length) {
    list.innerHTML = "";
    empty.classList.remove("hidden");
    return;
  }
  empty.classList.add("hidden");

  list.innerHTML = tasks.map((task) => {
    // display-only join: find the linked session's light from the snapshot
    const linked = task.pid != null
      ? snapshot.sessions.find((s) => s.pid === task.pid) : null;
    const linkedRow = linked
      ? `<div class="task-link">${t("tasks.linked")}：
           <span class="light ${LIGHTS.includes(linked.state) ? linked.state : "yellow"}"></span>
           <span class="task-link-state">${escapeHtml(t("reason." + linked.reason))}</span>
           <a href="#" data-act="view">${t("tasks.view")}</a></div>`
      : "";
    const when = task.lastRunAt ? t("tasks.last.run", { time: fmtClock(task.lastRunAt) }) : "";
    return `
      <div class="task" data-status="${task.status}" data-id="${task.id}">
        <div class="task-top">
          <span class="task-ico">${task.status === "finished" ? "✓" : "▶"}</span>
          <span class="task-title">${escapeHtml(task.title)}</span>
          <span class="task-status ${task.status}">${t("tasks.status." + task.status)}${when ? " · " + when : ""}</span>
        </div>
        <div class="task-path">${escapeHtml(task.cwd)}</div>
        <div class="task-cmd">$ ${escapeHtml(task.command)}</div>
        ${linkedRow}
        <div class="task-actions">${taskActions(task)}</div>
      </div>`;
  }).join("");
}

async function onTaskAction(act, id) {
  try {
    switch (act) {
      case "launch":
        toast(await invoke("launch_task", { id }));
        break;
      case "stop":
        if (!confirm(t("tasks.confirm.stop"))) return;
        toast(await invoke("stop_task", { id }));
        break;
      case "delete":
        if (!confirm(t("tasks.confirm.delete"))) return;
        await invoke("remove_task", { id });
        break;
      case "edit": {
        const task = lastTasks.find((x) => x.id === id);
        if (task) openTaskModal(task);
        break;
      }
      case "terminal":
        await invoke("open_task_terminal", { id });
        break;
      case "view":
        showPage("sessions");
        break;
    }
  } catch (e) {
    toast(String(e), true);
  } finally {
    // even error paths re-pull: launch failures land in "finished" on the
    // backend and the UI must not keep showing a stale "launching"
    try {
      renderTasks(await invoke("get_tasks"));
    } catch (_) { /* poll will bring the next one */ }
  }
}

/* --- new/edit task modal --- */
let editingTaskId = null;

function openTaskModal(task = null) {
  editingTaskId = task ? task.id : null;
  $("task-modal-title").textContent = task ? t("tasks.editing") : t("tasks.new");
  const f = $("task-form");
  f.title.value = task ? task.title : "";
  f.cwd.value = task ? task.cwd : "";
  f.command.value = task ? task.command : "";
  $("task-modal").classList.remove("hidden");
  f.title.focus();
}

function closeTaskModal() {
  $("task-modal").classList.add("hidden");
  editingTaskId = null;
}

async function saveTask(ev) {
  ev.preventDefault();
  const f = $("task-form");
  const cwd = f.cwd.value.trim();
  if (!(cwd.startsWith("/") || cwd.startsWith("~"))) {
    toast(t("tasks.form.cwd.bad"), true);
    return;
  }
  try {
    if (editingTaskId == null) {
      await invoke("add_task", {
        title: f.title.value.trim(), cwd, command: f.command.value.trim(),
      });
    } else {
      await invoke("update_task", {
        id: editingTaskId, title: f.title.value.trim(), cwd, command: f.command.value.trim(),
      });
    }
    closeTaskModal();
    renderTasks(await invoke("get_tasks"));
    toast(t("toast.saved.task"));
  } catch (e) {
    toast(String(e), true);
  }
}

/* ---------- GLM plan usage chip + usage page ---------- */

let lastUsage = null;

function usageClass(p) {
  if (p >= 90) return "u-red";
  if (p >= 70) return "u-amber";
  return "u-green";
}

function fmtReset(ms) {
  if (!ms) return "—";
  const d = new Date(ms);
  const now = new Date();
  const pad = (n) => String(n).padStart(2, "0");
  const hhmm = `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  const sameDay = d.getFullYear() === now.getFullYear()
    && d.getMonth() === now.getMonth()
    && d.getDate() === now.getDate();
  // a window resetting on another day needs its date — "18:19" alone would
  // read as tonight, and the weekly quota can be a week out
  return sameDay ? hhmm : `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${hhmm}`;
}

function usageTip(key, u) {
  return t(key, {
    cur: u.consumed.toLocaleString(),
    total: u.total.toLocaleString(),
    time: fmtReset(u.nextResetMs),
  });
}

/** Paint the header chip from the backend's latest quota snapshot. The
    backend pushes a fresh one every few minutes; between pushes this just
    re-renders (e.g. on a language switch, via lastUsage). */
function renderUsage(u) {
  lastUsage = u;
  const chip = $("usage-chip");
  if (!u || (!u.fiveHour && !u.weekly)) {
    chip.classList.add("hidden");
    chip.textContent = "";
    chip.removeAttribute("title");
    renderUsagePage();
    return;
  }
  const parts = [
    u.fiveHour
      ? `<span class="${usageClass(u.fiveHour.percentage)}">${
          t("usage.5h", { p: Math.round(u.fiveHour.percentage) })}</span>`
      : "",
    u.weekly
      ? `<span class="${usageClass(u.weekly.percentage)}">${
          t("usage.wk", { p: Math.round(u.weekly.percentage) })}</span>`
      : "",
  ].filter(Boolean);
  chip.innerHTML = parts.join('<span class="u-sep">·</span>');
  const tips = [
    u.fiveHour ? usageTip("usage.tip.5h", u.fiveHour) : "",
    u.weekly ? usageTip("usage.tip.wk", u.weekly) : "",
    u.level ? t("usage.level", { level: u.level }) : "",
  ].filter(Boolean);
  chip.title = tips.join("\n");
  chip.classList.remove("hidden");
  renderUsagePage();
}

/** The full usage page: the chip's data expanded into quota bars, plus the
    per-session token ranking (display data straight from SessionView.usage —
    it never feeds classification, here or anywhere). */
function renderUsagePage() {
  const quotas = $("usage-quotas");
  if (!quotas) return;
  const u = lastUsage;
  if (!u || (!u.fiveHour && !u.weekly)) {
    $("usage-level").textContent = "";
    quotas.innerHTML = `<p class="hint">${t("usage.empty")}</p>`;
    $("usage-rows").innerHTML = "";
    return;
  }

  $("usage-level").textContent = u.level ? t("usage.level", { level: u.level }) : "";
  const row = (nameKey, lim) => lim ? `
    <div class="quota-row">
      <div class="quota-label"><span>${t(nameKey)}</span>
        <span class="pct ${usageClass(lim.percentage)}">${Math.round(lim.percentage)}%</span></div>
      <div class="bar ${usageClass(lim.percentage)}"><i style="width:${Math.min(100, lim.percentage)}%"></i></div>
      <div class="quota-sub">${escapeHtml(usageTip(nameKey === "usage.win.5h" ? "usage.tip.5h" : "usage.tip.wk", lim))}</div>
    </div>` : "";
  quotas.innerHTML = row("usage.win.5h", u.fiveHour) + row("usage.win.wk", u.weekly);

  const withUsage = snapshot.sessions.filter((s) => s.usage);
  const max = Math.max(1, ...withUsage.map((s) => s.usage.output));
  $("usage-rows").innerHTML = withUsage
    .slice()
    .sort((a, b) => b.usage.output - a.usage.output)
    .map((s) => `
      <div class="urow" title="${escapeHtml(s.cwd)}">
        <span class="uname">${escapeHtml(s.project)}</span>
        <span class="ubar"><i style="width:${Math.round((s.usage.output / max) * 100)}%"></i></span>
        <span class="uval">${fmtTokens(s.usage.output)} out · ${s.usage.requests} req</span>
      </div>`)
    .join("");
}

function escapeHtml(value) {
  return String(value).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  })[c]);
}

/* ---------- settings page ---------- */

function loadSettingsForm(s) {
  const f = $("settings-form");
  f.language.value = s.language || "zh";
  f.pollIntervalSecs.value = s.pollIntervalSecs;
  f.idleGreenSecs.value = s.idleGreenSecs;
  f.blockedAfterSecs.value = s.blockedAfterSecs;
  f.autoContinue.checked = s.autoContinue;
  f.closeToTray.checked = s.closeToTray;
  f.showGlmUsage.checked = s.showGlmUsage;
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
}

async function saveSettings(ev) {
  ev.preventDefault();
  const f = $("settings-form");
  // Every number input is `required` (and HTML-validated), so no empty or
  // invalid value reaches this code. Defaults and bounds are owned by
  // Settings::sanitize on the Rust side — this half only converts display
  // units (hours/minutes → seconds) and forwards raw values.
  const s = {
    language: f.language.value === "en" ? "en" : "zh",
    pollIntervalSecs: Number(f.pollIntervalSecs.value),
    idleGreenSecs: Number(f.idleGreenSecs.value),
    blockedAfterSecs: Number(f.blockedAfterSecs.value),
    autoContinue: f.autoContinue.checked,
    closeToTray: f.closeToTray.checked,
    showGlmUsage: f.showGlmUsage.checked,
    // 0 stays 0: "resume as soon as it turns red" is a valid choice
    waitSecs: Math.round(Number(f.waitHours.value) * 3600),
    resumeKeys: f.resumeKeys.value.trim(),
    maxSends: Number(f.maxSends.value),
    retryIntervalSecs: Number(f.retryIntervalMin.value) * 60,
    wslDistro: f.wslDistro.value.trim(),
    notifyRed: f.notifyRed.checked,
    notifyContinue: f.notifyContinue.checked,
    notifyRecovered: f.notifyRecovered.checked,
    notifyTurnEnd: f.notifyTurnEnd.checked,
    notifyExit: f.notifyExit.checked,
    soundAlerts: f.soundAlerts.checked,
  };
  try {
    await invoke("set_settings", { settings: s });
    toast(t("toast.saved"));
    // the backend re-reads the poll interval on every pass, nothing to restart
  } catch (e) {
    toast(String(e), true);
  }
}

/* ---------- boot ---------- */

window.addEventListener("DOMContentLoaded", async () => {
  $("nav").addEventListener("click", (e) => {
    const item = e.target.closest(".nav-item");
    if (item) showPage(item.dataset.page);
  });
  $("btn-new-task").addEventListener("click", () => openTaskModal());
  $("task-modal-close").addEventListener("click", closeTaskModal);
  $("task-cancel").addEventListener("click", closeTaskModal);
  $("task-form").addEventListener("submit", saveTask);
  $("task-list").addEventListener("click", (e) => {
    const btn = e.target.closest("button[data-act], a[data-act]");
    if (!btn) return;
    e.preventDefault();
    const card = btn.closest(".task");
    onTaskAction(btn.dataset.act, Number(btn.dataset.id ?? card?.dataset.id ?? 0));
  });
  $("settings-form").addEventListener("submit", saveSettings);
  renderNav();

  // language first, so the very first paint already uses it
  try {
    setLang((await invoke("get_settings")).language);
  } catch (_) { /* zh stays */ }
  // a language switch also re-renders nav, tasks and the usage surfaces
  listen("settings", (e) => {
    setLang(e.payload.language);
    renderNav();
    renderTasks(lastTasks);
    renderUsage(lastUsage);
  });

  // every poll the backend makes ends up here
  listen("sessions", (event) => render(event.payload.sessions, event.payload.warning));
  // task statuses ride their own stream (poll reconciles + mutations emit)
  listen("tasks", (event) => renderTasks(event.payload));
  // the usage loop pushes a fresh quota snapshot every few minutes
  listen("usage", (event) => renderUsage(event.payload));
  invoke("get_usage").then(renderUsage).catch(() => {});
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
    // en-GB keeps the 24-hour clock the footer is sized for
    $("foot-clock").textContent =
      d.toLocaleTimeString(LANG === "en" ? "en-GB" : "zh-CN", { hour12: false });
  }, 1000);
});
