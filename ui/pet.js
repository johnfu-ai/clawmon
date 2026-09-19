/* clawmon desktop pet — click to bring the main window back, drag to move.
   Loads i18n.js (see pet.html) so the pet renders through the same copy
   table as the main window instead of a private one. */
const { invoke } = window.__TAURI__.core;
const { getCurrentWindow } = window.__TAURI__.window;
const { listen } = window.__TAURI__.event;

const root = document.getElementById("pet");
const badge = document.getElementById("badge");

const STATE_KEY = {
  green: "pet.green",
  blue: "pet.blue",
  yellow: "pet.yellow",
  red: "pet.red",
  off: "pet.off",
};

let lastStatus = { red: 0, yellow: 0, blue: 0, green: 0 };

function applyStatus(s) {
  lastStatus = s;
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
  } else if (s.blue > 0) {
    state = "blue";
    count = s.blue;
  }
  root.dataset.state = state;
  root.title = `clawmon — 🔴${s.red} 🟡${s.yellow} 🔵${s.blue} 🟢${s.green}\n${
    t(STATE_KEY[state])}（${t("pet.click")}）`;

  badge.classList.toggle("hidden", count === 0);
  badge.classList.toggle("yellow", state === "yellow");
  badge.classList.toggle("blue", state === "blue");
  badge.textContent = count;
}

/* the backend emits "status" after each poll of the main window */
listen("status", (e) => applyStatus(e.payload));

/* follow language changes made in the main window's settings */
invoke("get_settings").then((s) => {
  setLang(s.language);
  applyStatus(lastStatus);
}).catch(() => {});
listen("settings", (e) => {
  setLang(e.payload.language);
  applyStatus(lastStatus);
});

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
