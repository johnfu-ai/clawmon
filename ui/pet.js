/* clawmon desktop pet — click to bring the main window back, drag to move */
const { invoke } = window.__TAURI__.core;
const { getCurrentWindow } = window.__TAURI__.window;
const { listen } = window.__TAURI__.event;

const root = document.getElementById("pet");
const badge = document.getElementById("badge");

const STATE_LABEL = {
  green: "一切正常",
  yellow: "有会话在等待",
  red: "有会话疑似卡死",
  off: "WSL 连接异常",
};

function applyStatus(s) {
  let state = "green";
  let count = 0;
  if (s.warning) {
    state = "off";
  } else if (s.red > 0) {
    state = "red";
    count = s.red;
  } else if (s.yellow > 0) {
    state = "yellow";
    count = s.yellow;
  }
  root.dataset.state = state;
  root.title = `clawmon — 🔴${s.red} 🟡${s.yellow} 🟢${s.green}\n${STATE_LABEL[state]}（点击打开主窗口）`;

  badge.classList.toggle("hidden", count === 0);
  badge.classList.toggle("yellow", state === "yellow");
  badge.textContent = count;
}

/* the backend emits "status" after each poll of the main window */
listen("status", (e) => applyStatus(e.payload));

/* ---------- click vs drag ---------- */
const DRAG_SLOP = 5; // px of movement before a press turns into a window drag
let down = null;

root.addEventListener("mousedown", (e) => {
  if (e.button !== 0) return;
  down = { x: e.screenX, y: e.screenY };
  e.preventDefault();
});

window.addEventListener("mousemove", (e) => {
  if (!down) return;
  const moved = Math.abs(e.screenX - down.x) + Math.abs(e.screenY - down.y);
  if (moved > DRAG_SLOP) {
    down = null; // OS takes over the pointer; no mouseup reaches us
    getCurrentWindow().startDragging();
  }
});

window.addEventListener("mouseup", (e) => {
  if (!down) return;
  const moved = Math.abs(e.screenX - down.x) + Math.abs(e.screenY - down.y);
  down = null;
  if (moved <= DRAG_SLOP) {
    invoke("pet_clicked").catch(console.error);
  }
});

window.addEventListener("contextmenu", (e) => e.preventDefault());
