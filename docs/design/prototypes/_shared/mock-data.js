/* clawmon prototype — mock data (Module pattern).
 * One sample set shared by every page and every skin, so style comparison
 * compares styles, not data. Numbers deliberately mirror screen-plans.md. */
(function () {
  'use strict';

  window.Mock = {
    summary: { green: 2, blue: 1, yellow: 1, red: 1 },
    statusText: '监控正常',
    usageChip: '5h 17% · 7d 21%',

    sessions: [
      {
        state: 'red', project: 'webapp', task: true,
        reason: '用量限额 429 · 限额将在 21:34 重置',
        idle: '12m',
        usage: { in: '1.1M', cache: '1.2M', out: '89K', req: 42 },
        preview: '额度已用尽，限额将在 21:34:12 重置（It will reset at 21:34）…',
        loc: 'work:0.1 · pid 4123',
        countdown: { kind: 'waiting', remainingSec: 5025, sends: 1, max: 3 }
      },
      {
        state: 'yellow', project: 'parser',
        reason: '等待你回答问题（AskUserQuestion）',
        idle: '3m',
        usage: { in: '302K', cache: '410K', out: '9K', req: 4 },
        preview: '要使用哪种方案？ A 原样保留配置 B 迁移到新格式 C 先输出对比报告',
        loc: 'work:1.2 · pid 5377'
      },
      {
        state: 'blue', project: 'docs-site',
        reason: '回合完成 · 等待下一条指令',
        idle: '5m',
        usage: { in: '655K', cache: '802K', out: '38K', req: 12 },
        loc: 'docs:0.1 · pid 6230'
      },
      {
        state: 'green', project: 'clawmon', task: true,
        reason: '工具运行中 · Bash',
        idle: '8s',
        usage: { in: '980K', cache: '1.0M', out: '61K', req: 31 },
        preview: '$ cargo test -p clawmon-core --test integration -- --ignored',
        loc: 'claw:0.0 · pid 3011'
      },
      {
        state: 'green', project: 'api',
        reason: '模型输出中',
        idle: '0s',
        usage: { in: '210K', cache: '98K', out: '3.2K', req: 2 },
        loc: 'work:0.2 · pid 5518'
      },
      {
        state: 'green', project: 'scripts', monitorOnly: true,
        reason: '仅监控（不在 tmux 中，无法自动继续）',
        idle: '2m',
        loc: 'pid 2277 · 无 tmux'
      }
    ],

    tasks: [
      {
        title: '修复登录 bug', cwd: '~/work/webapp', cmd: 'claude "修复登录重定向"',
        status: 'running', info: '32m',
        sessState: 'red', sessText: '429 等待重置 · 倒计时中'
      },
      {
        title: '重构解析器', cwd: '~/clawmon', cmd: 'claude "重构 detect.py 配对逻辑"',
        status: 'running', info: '4m',
        sessState: 'green', sessText: '工具运行中 · Bash'
      },
      {
        title: '每日构建报告', cwd: '~/work/docs', cmd: 'make report',
        status: 'idle', info: ''
      },
      {
        title: '依赖升级', cwd: '~/work/api', cmd: 'claude "升级所有依赖并修测试"',
        status: 'finished', info: '上次 09-24 21:40'
      }
    ],

    usage: {
      level: 'GLM Coding Plan · pro',
      updated: '刷新于 21:02 · 每 5 分钟自动刷新',
      quotas: [
        { name: '5 小时窗口', pct: 17, used: '32 / 192 积分', reset: '3h 12m 后重置' },
        { name: '7 天窗口',   pct: 21, used: '210 / 1000 积分', reset: '周一 00:00 重置' }
      ],
      sessionUsage: [
        { name: 'webapp',    w: 100, out: '89K', req: 42 },
        { name: 'clawmon',   w: 69,  out: '61K', req: 31 },
        { name: 'docs-site', w: 43,  out: '38K', req: 12 },
        { name: 'parser',    w: 10,  out: '9K',  req: 4 }
      ]
    }
  };
})();
