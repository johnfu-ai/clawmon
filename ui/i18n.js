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
  "set.usage": ["显示 GLM 套餐用量（底部状态栏）", "Show GLM plan usage (status bar)"],
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

  /* session rows — green = no action needed, blue = turn complete,
     yellow = blocked on the user */
  "sum": ["● {g} 正常 · ● {b} 已完成 · ● {y} 待你输入 · ● {r} 超时",
    "● {g} OK · ● {b} done · ● {y} awaiting you · ● {r} stuck"],
  /* the engine's classification tags (SessionView.reason, snake_case) —
     keying on the tag keeps the vocabulary owned by the state machine that
     produces it; a forgotten entry shows the raw key instead of the wrong
     language */
  "reason.active": ["运行中", "Running"],
  "reason.no_transcript": ["未找到记录", "No transcript"],
  "reason.transcript_stale": ["记录未就绪", "Transcript not ready"],
  "reason.tool_running": ["工具运行中", "Tool running"],
  "reason.waiting_subagent": ["等待 Subagent", "Waiting for subagent"],
  "reason.turn_complete": ["回合完成", "Turn complete"],
  "reason.waiting_input": ["等待输入", "Waiting for input"],
  "reason.waiting_response": ["等待 API 响应", "Waiting for API"],
  "reason.response_timed_out": ["疑似 API 超时", "Likely API timeout"],
  "reason.usage_limited": ["额度已用尽", "Usage limit reached"],
  "idle.sec": ["{n} 秒", "{n}s"],
  "idle.min": ["{n} 分钟", "{n} min"],
  "idle.hour": ["{h} 小时 {m} 分", "{h} h {m} min"],
  "no.tmux": ["无 tmux", "no tmux"],
  "meta.idle": ["空闲 {s}", "idle {s}"],
  "meta.usage": ["in {i} · cache {c} · out {o} · {n} req", "in {i} · cache {c} · out {o} · {n} req"],
  "meta.usage.tip": ["本会话 token 用量（输入/缓存/输出）与 API 请求数",
    "Session token usage (input/cache/output) and API requests"],

  /* GLM plan usage chip */
  "usage.5h": ["5h {p}%", "5h {p}%"],
  "usage.wk": ["7d {p}%", "7d {p}%"],
  "usage.tip.5h": ["5 小时额度：{cur}/{total} 积分 · {time} 重置",
    "5-hour quota: {cur}/{total} credits · resets {time}"],
  "usage.tip.wk": ["周额度：{cur}/{total} 积分 · {time} 重置",
    "Weekly quota: {cur}/{total} credits · resets {time}"],
  "usage.level": ["套餐 {level}", "{level} plan"],
  "monitor.only": ["仅监控", "monitor only"],
  "no.control": ["不在 tmux 中，无法控制", "not in tmux, cannot control"],
  "cd.until": ["后自动继续", "until auto-continue"],
  "cd.now": ["即将发送", "sending now"],
  "cd.resumed": ["已继续 {n} 次 · ", "resumed {n} × · "],
  "cd.notmux": ["不在 tmux 中，无法自动继续", "not in tmux, cannot auto-continue"],
  "cd.limit": ["已自动继续 {n} 次（已达上限）", "auto-continued {n} times (limit reached)"],
  "cd.disabled": ["已停用自动继续", "auto-continue disabled"],

  /* unified main nav — rendered from one array in app.js, shared by every
     page; keys live here so a language switch re-renders the nav too */
  "nav.sessions": ["会话", "Sessions"],
  "nav.tasks": ["任务", "Tasks"],
  "nav.usage": ["用量", "Usage"],
  "nav.settings": ["设置", "Settings"],

  /* tasks page (FR10) */
  "tasks.title": ["任务", "Task"],
  "tasks.count": ["（{n}）", "({n})"],
  "tasks.act.new": ["+ 新建", "+ New"],
  "tasks.new": ["新建任务", "New task"],
  "tasks.editing": ["编辑任务", "Edit task"],
  "tasks.empty.title": ["任务清单为空", "No tasks yet"],
  "tasks.empty.hint": ["把常用的 claude 任务存成一键启动", "Save your usual claude launches as one-click tasks"],
  "tasks.status.idle": ["待启动", "Idle"],
  "tasks.status.launching": ["启动中…", "Launching…"],
  "tasks.status.running": ["运行中", "Running"],
  "tasks.status.finished": ["已结束", "Finished"],
  "tasks.act.launch": ["启动", "Start"],
  "tasks.act.relaunch": ["再次启动", "Start again"],
  "tasks.act.stop": ["停止", "Stop"],
  "tasks.act.edit": ["编辑", "Edit"],
  "tasks.act.delete": ["删除", "Delete"],
  "tasks.act.terminal": ["打开终端", "Open terminal"],
  "tasks.confirm.stop": ["停止该任务的 tmux 会话？", "Stop this task's tmux session?"],
  "tasks.confirm.delete": ["删除该任务（不影响正在运行的会话）？", "Delete this task (a running session is left alone)?"],
  "tasks.linked": ["关联会话", "Linked session"],
  "tasks.view": ["查看 →", "View →"],
  "tasks.last.run": ["上次 {time}", "last {time}"],
  "tasks.form.title": ["标题", "Title"],
  "tasks.form.cwd": ["目录（WSL 绝对路径）", "Directory (WSL absolute path)"],
  "tasks.form.cwd.bad": ["目录必须是 WSL 绝对路径（/ 或 ~ 开头）", "Directory must be absolute (/ or ~)"],
  "tasks.form.command": ["命令", "Command"],
  "tasks.form.hint": ["例：claude \"…\" · make test · 多命令直接换行", "e.g. claude \"…\" · make test"],
  "tasks.form.save": ["保存任务", "Save task"],
  "act.cancel": ["取消", "Cancel"],
  "toast.saved.task": ["任务已保存", "Task saved"],

  /* usage page */
  "usage.title": ["套餐额度", "Plan quota"],
  "usage.sessions": ["会话 token 用量", "Session token usage"],
  "usage.sessions.hint": ["本转录累计", "per transcript"],
  "usage.win.5h": ["5 小时窗口", "5-hour window"],
  "usage.win.wk": ["7 天窗口", "7-day window"],
  "usage.empty": ["未配置 GLM 端点，或已在设置中关闭", "No GLM endpoint, or disabled in settings"],

  /* toasts */
  "toast.saved": ["设置已保存", "Settings saved"],

  /* pet (pet.js loads this same table) */
  "pet.green": ["一切正常", "All good"],
  "pet.blue": ["有会话已完成回合", "A session finished its turn"],
  "pet.yellow": ["有会话在等你输入", "Sessions awaiting your input"],
  "pet.red": ["有会话疑似卡死", "Session may be stuck"],
  "pet.off": ["WSL 连接异常", "WSL unreachable"],
  "pet.click": ["点击打开主窗口", "click to open the main window"],
};

function t(key, vars) {
  const entry = T[key];
  let s = entry ? entry[LANG === "en" ? 1 : 0] : key;
  if (vars) for (const k in vars) s = s.replaceAll("{" + k + "}", vars[k]);
  return s;
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
