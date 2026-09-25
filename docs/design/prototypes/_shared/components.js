/* clawmon prototype — component factory.
 * Data in, DOM out. All four pages × three skins reuse these builders,
 * so a session card cannot drift between pages or themes (Factory pattern).
 * Class contract is owned by base.css (structure) + theme-*.css (skin). */
(function () {
  'use strict';

  function esc(s) {
    return String(s == null ? '' : s)
      .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  function dot(state) { return '<span class="dot ' + state + '"></span>'; }

  function fmtSec(sec) {
    var h = Math.floor(sec / 3600), m = Math.floor(sec % 3600 / 60), s = sec % 60;
    function p(n) { return (n < 10 ? '0' : '') + n; }
    return p(h) + ':' + p(m) + ':' + p(s);
  }

  /* countdown mirrors the engine's tagged kind — the UI only switches on
   * the tag, it never re-derives (same rule as the production wire contract). */
  function countdown(cd) {
    if (!cd) return '';
    if (cd.kind === 'waiting') {
      return '<div class="countdown" data-count="' + cd.remainingSec + '"><span class="cd">' +
        fmtSec(cd.remainingSec) + '</span> 后自动继续 · 已发 ' + cd.sends + '/' + cd.max + '</div>';
    }
    if (cd.kind === 'capped') return '<div class="countdown off">已达上限（' + cd.max + ' 次）· 可手动继续</div>';
    if (cd.kind === 'off') return '<div class="countdown off">已停用自动继续</div>';
    return '';
  }

  function sessionCard(s) {
    var u = s.usage;
    return '<article class="card session" data-state="' + s.state + '">' +
      '<div class="card-head">' + dot(s.state) +
        '<span class="card-title">' + esc(s.project) + '</span>' +
        (s.task ? '<span class="badge task" title="由任务启动">▶ 任务</span>' : '') +
        '<span class="idle">' + esc(s.idle) + '</span>' +
      '</div>' +
      '<div class="reason">' + esc(s.reason) + '</div>' +
      countdown(s.countdown) +
      (u ? '<div class="meta"><span>in ' + u.in + '</span><span>cache ' + u.cache +
           '</span><span>out ' + u.out + '</span><span>' + u.req + ' req</span></div>' : '') +
      (s.preview ? '<div class="preview">' + esc(s.preview) + '</div>' : '') +
      '<div class="card-foot">' +
        '<span class="loc">' + esc(s.loc) + '</span>' +
        (s.state === 'red' && !s.monitorOnly
          ? '<button class="btn sm primary">立即继续</button>' : '') +
      '</div>' +
    '</article>';
  }

  var TASK_BADGE = {
    running:   ['running', '运行中', true],
    launching: ['launching', '启动中…', true],
    idle:      ['', '待启动', false],
    finished:  ['finished', '已结束', false]
  };

  function taskCard(t) {
    var b = TASK_BADGE[t.status] || TASK_BADGE.idle;
    var actions = '';
    if (t.status === 'running' || t.status === 'launching') {
      actions = '<button class="btn sm ghost">打开终端</button>' +
                '<button class="btn sm danger" ' + (t.status === 'launching' ? 'disabled' : '') + '>停止</button>';
    } else {
      actions = '<button class="btn sm primary">' + (t.status === 'finished' ? '再次启动' : '启动') + '</button>' +
                '<button class="btn sm ghost">编辑</button>' +
                '<button class="btn sm ghost">删除</button>';
    }
    return '<article class="card task ' + t.status + '">' +
      '<div class="card-head">' +
        '<span class="task-ico">' + (t.status === 'finished' ? '✓' : '▶') + '</span>' +
        '<span class="card-title">' + esc(t.title) + '</span>' +
        '<span class="idle">' + esc(t.info || '') + '</span>' +
      '</div>' +
      '<div class="path">' + esc(t.cwd) + '</div>' +
      '<div class="cmd">$ ' + esc(t.cmd) + '</div>' +
      (t.sessState
        ? '<div class="task-link">' + dot(t.sessState) +
          '<span>' + esc(t.sessText) + '</span><a href="sessions.html">查看 →</a></div>'
        : '') +
      (t.status === 'launching'
        ? '<div class="task-link"><span class="badge launching pulse">LAUNCH</span></div>' : '') +
      '<div class="actions">' + actions + '</div>' +
    '</article>';
  }

  function quotaRow(q) {
    var level = q.pct >= 90 ? ' crit' : q.pct >= 70 ? ' warn' : '';
    return '<div class="quota-row">' +
      '<div class="quota-label"><span>' + esc(q.name) + '</span>' +
        '<span class="pct' + (level ? ' ' + level.trim() : '') + '">' + q.pct + '%</span></div>' +
      '<div class="bar' + level + '"><i style="width:' + q.pct + '%"></i></div>' +
      '<div class="quota-sub">' + esc(q.used) + ' · ' + esc(q.reset) + '</div>' +
    '</div>';
  }

  function usageRow(r) {
    return '<div class="urow">' +
      '<span class="uname">' + esc(r.name) + '</span>' +
      '<span class="ubar"><i style="width:' + r.w + '%"></i></span>' +
      '<span class="uval">' + r.out + ' out · ' + r.req + ' req</span>' +
    '</div>';
  }

  /* minimal toast (prototype feedback) */
  var toastEl = null, toastTimer = null;
  function toast(msg) {
    if (!toastEl) { toastEl = document.createElement('div'); toastEl.className = 'toast'; document.body.appendChild(toastEl); }
    toastEl.textContent = msg;
    toastEl.classList.add('show');
    clearTimeout(toastTimer);
    toastTimer = setTimeout(function () { toastEl.classList.remove('show'); }, 2000);
  }

  window.UI = {
    esc: esc, dot: dot, fmtSec: fmtSec, toast: toast,
    sessionCard: sessionCard, taskCard: taskCard,
    quotaRow: quotaRow, usageRow: usageRow
  };
})();
