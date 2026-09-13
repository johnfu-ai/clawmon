/* clawmon i18n — every entry is [zh, en]; zh is the default UI language.
   Static HTML keeps its Chinese text inline and carries data-i18n keys;
   applyI18n() swaps the strings whenever the language changes. */
let LANG = "zh";

const T = {
  /* header / empty / banner */
  "app.title": ["Claude WSL 监控", "Claude WSL Monitor"],
  "settings.tooltip": ["设置", "Settings"],
  "empty.title": ["未发现运行中的 Claude Code 会话", "No running Claude Code sessions"],
  "empty.hint": ["在 WSL 终端里启动 claude 即可自动识别", "Start claude in a WSL terminal and it shows up here"],

  /* settings */
  "settings.title": ["设置", "Settings"],
  "set.language": ["语言 / Language", "Language / 语言"],
  "set.poll": ["轮询间隔（秒）", "Poll interval (seconds)"],
  "set.idle": ["活跃判定（秒）——多长时间无输出算“空闲”", "Active window (s) — no output this long counts as idle"],
  "set.blocked": ["超时判定（秒）——等待响应超过该时长视为 API 超时", "Timeout (s) — waiting this long means an API timeout"],
  "set.auto": ["自动继续", "Auto-continue"],
  "set.tray": ["关闭窗口时最小化到托盘（后台继续监控）", "Close to tray (keep monitoring in the background)"],
  "set.wait": ["等待时长（小时）——红灯后多久自动继续", "Wait (hours) — time from red until auto-continue"],
  "set.keys": ["继续按键（tmux 键名，空格分隔）", "Resume keys (tmux key names, space separated)"],
  "set.max": ["最大自动尝试次数", "Max auto attempts"],
  "set.retry": ["重试间隔（分钟）", "Retry interval (minutes)"],
  "set.distro": ["WSL 发行版（留空 = 默认）", "WSL distro (empty = default)"],
  "set.notify": ["桌面通知", "Desktop notifications"],
  "set.notify.red": ["会话变红时", "When a session turns red"],
  "set.notify.continue": ["自动继续已发送时", "When resume keys were sent"],
  "set.notify.recovered": ["红灯解除时", "When a red session recovers"],
  "set.notify.turn": ["回合结束等待输入时", "When a turn finishes, waiting for input"],
  "set.notify.exit": ["会话进程退出时", "When a session process exits"],
  "set.sound": ["提示音（伴随通知）", "Alert sound with notifications"],
  "set.about": ["关于", "About"],
  "set.autoupdate": ["启动时检查更新", "Check for updates at startup"],
  "set.check": ["检查更新", "Check for updates"],
  "set.checking": ["检查中…", "Checking…"],
  "set.install": ["下载并安装", "Download and install"],
  "act.save": ["保存", "Save"],
  "act.close": ["关闭", "Close"],
  "act.continue": ["立即继续", "Continue now"],
  "hint.tmux1": [
    "提示：只有运行在 tmux 里的 Claude Code 才能被自动控制，建议用 ",
    "Tip: only Claude Code sessions inside tmux can be controlled — run ",
  ],
  "hint.tmux2": ["后在里面启动 claude。", ", then start claude inside it."],

  /* footer */
  "foot.connecting": ["连接 WSL…", "Connecting to WSL…"],
  "foot.ok": ["WSL 正常 · {n} 个会话", "WSL OK · {n} sessions"],
  "foot.bad": ["WSL 连接异常", "WSL connection error"],
  "foot.fail": ["查询失败", "Query failed"],

  /* session rows */
  "sum": ["● {g} 运行 · ● {y} 等待 · ● {r} 超时", "● {g} running · ● {y} waiting · ● {r} stuck"],
  "idle.sec": ["{n} 秒", "{n}s"],
  "idle.min": ["{n} 分钟", "{n} min"],
  "idle.hour": ["{h} 小时 {m} 分", "{h} h {m} min"],
  "no.tmux": ["无 tmux", "no tmux"],
  "meta.idle": ["空闲 {s}", "idle {s}"],
  "monitor.only": ["仅监控", "monitor only"],
  "no.control": ["不在 tmux 中，无法控制", "not in tmux, cannot control"],
  "cd.until": ["后自动继续", "until auto-continue"],
  "cd.now": ["即将发送", "sending now"],
  "cd.resumed": ["已继续 {n} 次 · ", "resumed {n} × · "],
  "cd.notmux": ["不在 tmux 中，无法自动继续", "not in tmux, cannot auto-continue"],
  "cd.limit": ["已自动继续 {n} 次（已达上限）", "auto-continued {n} times (limit reached)"],
  "cd.disabled": ["已停用自动继续", "auto-continue disabled"],

  /* toasts */
  "toast.saved": ["设置已保存", "Settings saved"],
  "toast.newversion": ["发现新版本 v{v}，可在设置中安装", "New version v{v} available — install it from Settings"],
  "toast.uptodate": ["已是最新版本", "Already up to date"],
  "toast.installing": ["开始下载更新，完成后应用会自动重启", "Downloading the update; the app restarts when done"],
};

/* the engine's status labels arrive in Chinese; translate for display only */
const LABEL_EN = {
  "运行中": "Running",
  "等待输入": "Waiting for input",
  "工具运行中": "Tool running",
  "等待响应": "Waiting for response",
  "疑似 API 超时": "Likely API timeout",
  "未找到记录": "No transcript",
  "记录未就绪": "Transcript not ready",
};

function t(key, vars) {
  const entry = T[key];
  let s = entry ? entry[LANG === "en" ? 1 : 0] : key;
  if (vars) for (const k in vars) s = s.replaceAll("{" + k + "}", vars[k]);
  return s;
}

function stateLabel(zhLabel) {
  return LANG === "en" ? (LABEL_EN[zhLabel] || zhLabel) : zhLabel;
}

function applyI18n(root = document) {
  document.documentElement.lang = LANG === "en" ? "en" : "zh-CN";
  root.querySelectorAll("[data-i18n]").forEach((el) => (el.textContent = t(el.dataset.i18n)));
  root.querySelectorAll("[data-i18n-title]").forEach((el) => (el.title = t(el.dataset.i18nTitle)));
}

function setLang(lang) {
  LANG = lang === "en" ? "en" : "zh";
  applyI18n();
}
