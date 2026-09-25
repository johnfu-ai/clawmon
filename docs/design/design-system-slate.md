# 设计系统 A — Slate 深色工具风

| 项目 | 内容 |
|------|------|
| 定位 | 专业常驻工具：暗夜值守搭档，v0 视觉的体系化演进 |
| 原型 | `prototypes/slate/`（theme-slate.css） |

---

## 1. 设计原则

1. 状态色是唯一的高饱和来源，其余一律低饱和——灯是主角；
2. 数字永远等宽 + `tabular-nums`，对齐产生秩序感；
3. 光晕只给状态点，不给容器——发光是信号，不是装饰。

## 2. Design Tokens

### 色彩

| Token | 值 | 用途 |
|-------|-----|------|
| `--bg` | `#10141a` | 窗口底 |
| `--panel` | `#171d26` | 卡片/面板 |
| `--panel-hover` | `#1c2430` | 卡片悬停 |
| `--border` | `#232c39` | 1px 分隔 |
| `--text` | `#d7dde6` | 主文本 |
| `--muted` | `#7b8798` | 次要文本/meta |
| `--green` / `--blue` / `--yellow` / `--red` | `#3ddc84` / `#58a6ff` / `#f5c542` / `#ff5f56` | 四色灯 + 对应文案 |
| `--accent` | `#6aa1ff` | 主按钮/链接/高亮 |
| `--accent-ink` | `#0b1220` | accent 上的文字 |
| `--danger-bg` / `--danger-ink` | `#3a2020` / `#ffb4a8` | 横幅/toast 错误 |

对比度：text/bg ≈ 12.9:1（AAA）；muted/bg ≈ 5.0:1（AA）；
四色在 panel 上均 ≥ 7:1（大字号 AAA）。

### 字体与字号

| Token | 值 |
|-------|-----|
| `--font-ui` | `"Segoe UI", "Microsoft YaHei", system-ui, sans-serif` |
| `--font-mono` | `"Cascadia Mono", Consolas, "Courier New", monospace` |
| 字号 | 11（meta）/ 12（辅助）/ **13（正文）** / 15（卡片标题） |
| 行高 | 1.5；等宽块 1.45 |

### 间距 / 圆角 / 层次

| Token | 值 |
|-------|-----|
| spacing | 4 / 8 / 12 / 16 / 24 |
| `--radius` | 8px（卡片）；6px（按钮/输入）；999px（徽章/胶囊） |
| 卡片阴影 | 无（用 1px border 分层）；悬浮面板 `0 4px 16px rgba(0,0,0,.4)` |
| 状态点光晕 | `0 0 6px var(--color)` |
| 动效 | 150ms ease-out；红灯 2s blink |

## 3. 组件规范

| 组件 | 规范 |
|------|------|
| 主导航 | 40px 高，四项等分；默认 muted 文字，active 项 `--text` + 底部 2px accent 条；hover 变亮 |
| 顶栏汇总 | `●n` 胶囊，数字等宽；仅非零色显示 |
| 会话卡片 | `--panel` 底 + 1px border + radius 8；左列 8px 状态点；悬停 `--panel-hover`；红灯卡片 border 变 `rgba(255,95,86,.35)` |
| 状态点 | 8px 圆 + 光晕；红灯附加 2s 呼吸闪烁 |
| 徽章 | 胶囊：运行中=green 半透明底、待启动=muted、已结束=border 底、任务徽章 `▶` + accent |
| 按钮 | 28px 高：`primary`=accent 底深字；`ghost`=transparent+border；`danger`=red 半透明底；禁用 40% 透明 |
| 输入 | panel 底 + border，focus 时 border 变 accent + `0 0 0 2px rgba(106,161,255,.2)` |
| 开关 | 32×18 轨道，开=accent |
| 进度条 | 6px 高，圆角满；<70% 状态色（green/accent）、≥70% yellow、≥90% red |
| 倒计时 | 等宽 13px；最后 60s 变 yellow 并加粗 |
| 横幅/Toast | `--danger-bg` 底 + `--danger-ink` 字，radius 6 |
| 空态 | 居中 muted 图标 + 一句话 + 主按钮 |

## 4. 布局骨架

单列 flex：顶栏 36 → 导航 40 → 横幅 0/28 → 内容滚动区（卡片间距 8）→
底栏 28。340px 窄窗：meta 行允许折两行，预览 1 行。

## 5. Do / Don't

- ✅ 状态色只表达状态；交互色只用 accent。
- ✅ 长数字用等宽 + 千分位缩写（1.1M / 89K）。
- ❌ 不给卡片加彩色边框（红灯除外——那是信号）。
- ❌ 不用纯黑 `#000` 或纯白 `#fff` 做文本（用 token）。
- ❌ 不引入背景图/模糊/阴影堆叠——常驻窗口要省电省渲染。
