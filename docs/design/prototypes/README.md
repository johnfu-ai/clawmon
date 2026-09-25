# clawmon UI 原型（v2 设计阶段产物）

三套风格（Slate / Paper / CRT）× 四个页面（会话 / 任务 / 用量 / 设置）的
静态原型。直接用浏览器打开 `index.html` 开始评选。

**这些是设计评审用原型，不是产品代码**：数据是 mock 的，交互只演示
关键行为；文字暂为中文（产品实现须走 `ui/i18n.js` 双语表）。

## 目录

```
prototypes/
├── index.html          风格选择入口（三风格并排比较）
├── _shared/            设计模式层（见下）
│   ├── base.css        结构骨架：布局与组件结构，零颜色
│   ├── layout.js       统一页面骨架（顶栏 + 主导航 + 横幅 + 底栏）
│   ├── components.js   组件工厂（会话卡 / 任务卡 / 徽章 / 进度条…）
│   ├── mock-data.js    模拟数据（会话 / 任务 / 用量）
│   └── theme-*.css     三套主题皮肤（策略：同一结构契约的不同实现）
├── slate/  sessions.html · tasks.html · usage.html · settings.html
├── paper/  （与 slate 页面逐字节相同，仅换主题引用）
└── crt/    （同上）
```

## 设计模式（用户约束 2：「原型设计环节使用设计模式处理」）

| 模式 | 落点 | 解决的问题 |
|------|------|-----------|
| **模板方法 Template Method** | `_shared/layout.js` | 页面只写 `<body data-page>` + `<main>` 内容；顶栏、**主导航**、横幅、底栏由骨架统一注入。导航四项单一来源，任何页面都改不了它——**用户约束 1（所有页面主导航统一）由结构保证，不靠自觉** |
| **策略 Strategy** | `_shared/theme-*.css` | 三套风格实现同一个 token/类名契约，页面换一行 `<link>` 即换肤。选型、换型、混型成本≈0 |
| **工厂 Factory** | `_shared/components.js` | `UI.sessionCard(s)` / `UI.taskCard(t)` 等以数据产出组件 DOM；四个页面、三套皮肤复用同一构造器，杜绝手写漂移 |
| **模块 Module** | `_shared/mock-data.js` | 模拟数据单点维护，三风格看到完全相同的会话/任务样本，评选比较的是风格而不是数据 |
| **观察者 Observer（雏形）** | 页面内倒计时/时钟 interval | 演示「后端推送值校准 + 前端本地走秒」的真实节奏（产品中由 Tauri 事件驱动，原型用 interval 模拟） |

## 约束映射

1. **所有页面主导航栏统一** → 导航 DOM 只存在于 `layout.js` 一处；
   页面 HTML 中没有任何导航标记；高亮由 `data-page` 自动推导。
2. **原型设计用设计模式** → 见上表；`paper/`、`crt/` 的页面文件由
   `slate/` 逐字节复制后仅替换主题引用生成（策略模式的直接体现）。

## 评选建议

1. 打开 `index.html`，先看三风格定位卡片；
2. 进每个风格的 `sessions.html`（信息密度最高、最能暴露风格问题），
   再看 `tasks.html`（v2 新页面）；
3. 关注：倒计时等宽数字、四色灯辨识度、meta 行密度、导航高亮方式；
4. 可以混合选（例如 Paper 底 + Slate 灯色），主题契约支持拆开组合。
