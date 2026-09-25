/* clawmon prototype — unified page skeleton (Template Method pattern).
 *
 * A page provides ONLY:  <body class="t-<theme>" data-page="sessions|tasks|usage|settings"
 *                        data-banner="optional warning text">  +  <main class="page">…</main>
 * Everything else (topbar, THE unified main nav, banner, statusbar, clock) is
 * injected here. The nav markup exists in exactly one place — no page can
 * restyle, reorder or drop it (user constraint #1, enforced structurally).
 */
(function () {
  'use strict';

  // Single source of the main navigation. Pages never touch this list.
  var NAV = [
    { id: 'sessions', label: '会话', ico: '▤', href: 'sessions.html' },
    { id: 'tasks',    label: '任务', ico: '☰', href: 'tasks.html' },
    { id: 'usage',    label: '用量', ico: '◐', href: 'usage.html' },
    { id: 'settings', label: '设置', ico: '⚙', href: 'settings.html' }
  ];

  function el(html) {
    var t = document.createElement('template');
    t.innerHTML = html.trim();
    return t.content.firstElementChild;
  }

  document.addEventListener('DOMContentLoaded', function () {
    var page = document.body.dataset.page || 'sessions';
    var m = window.Mock || {};
    var s = m.summary || { green: 0, blue: 0, yellow: 0, red: 0 };
    var main = document.querySelector('main.page');

    // ---- topbar: brand + four-color summary (non-zero only) ----
    var chips = [
      ['g', s.green, '正常'], ['b', s.blue, '已完成'],
      ['y', s.yellow, '待输入'], ['r', s.red, '超时']
    ].filter(function (c) { return c[1] > 0; })
     .map(function (c) {
       return '<span class="chip ' + c[0] + '" title="' + c[2] + ' ' + c[1] + '">● ' + c[1] + '</span>';
     }).join('');
    document.body.prepend(el(
      '<header class="topbar">' +
        '<div class="brand"><span class="gem">◆</span><span>clawmon</span></div>' +
        '<div class="summary">' + chips + '</div>' +
      '</header>'
    ));
    Array.prototype.forEach.call(document.querySelectorAll('.summary .chip'), function (chip) {
      chip.addEventListener('click', function () { location.href = 'sessions.html'; });
    });

    // ---- unified main nav (identical on every page; only highlight differs) ----
    var navHtml = NAV.map(function (n) {
      return '<a class="nav-item' + (n.id === page ? ' active' : '') +
        '" data-nav="' + n.id + '" href="' + n.href + '">' +
        '<span class="ico">' + n.ico + '</span><span class="lbl">' + n.label + '</span></a>';
    }).join('');
    document.body.insertBefore(el('<nav class="navbar">' + navHtml + '</nav>'), main);

    // ---- warning banner (global position, page opt-in via data-banner) ----
    if (document.body.dataset.banner) {
      document.body.insertBefore(el(
        '<div class="banner show"><span>⚠</span><span>' + document.body.dataset.banner + '</span></div>'
      ), main);
    }

    // ---- statusbar: state · usage chip · clock ----
    var foot = el(
      '<footer class="statusbar">' +
        '<span class="foot-dot' + (s.warn ? ' warn' : '') + '"></span>' +
        '<span>' + (m.statusText || '监控正常') + '</span>' +
        '<span class="usage-chip" title="GLM 套餐额度">' + (m.usageChip || '') + '</span>' +
        '<span class="foot-clock">--:--</span>' +
      '</footer>'
    );
    document.body.appendChild(foot);
    var clock = foot.querySelector('.foot-clock');
    function tick() {
      var d = new Date();
      function p(n) { return (n < 10 ? '0' : '') + n; }
      clock.textContent = p(d.getHours()) + ':' + p(d.getMinutes());
    }
    tick();
    setInterval(tick, 1000);
  });
})();
