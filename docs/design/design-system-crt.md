# 设计系统 C — CRT 终端复古

| 项目 | 内容 |
|------|------|
| 定位 | 终端原住民仪表盘：磷光屏、等宽、直角，与 tmux/WSL 视觉同源 |
| 原型 | `prototypes/crt/`（theme-crt.css） |

---

## 1. 设计原则

1. **一切文本等宽**——这里没有「等宽例外」，数字与文字同一节奏；
2. 标签大写 + 字距（`text-transform: uppercase; letter-spacing: .08em`），
   正文保持中文原样；
3. 直角、细边框、微扫描线：像一台还在服役的绿色单色终端；
4. 磷光绿是主色也是强调色；四色灯维持语义不变（安全语义 > 风格纯度）。

## 2. Design Tokens

### 色彩

| Token | 值 | 用途 |
|-------|-----|------|
| `--bg` | `#0a0e0a` | 窗口底（磷光黑） |
| `--panel` | `#10160f` | 面板 |
| `--panel-hover` | `#16211a` | 悬停 |
| `--border` | `#1f2b1f` | 1px 分隔 |
| `--text` | `#b8e6bf` | 主文本（磷光绿白） |
| `--bright` | `#e8ffe9` | 高亮文本（标题/active） |
| `--muted` | `#5f8a66` | 次要文本 |
| `--green` / `--blue` / `--yellow` / `--red` | `#45f882` / `#52c5ff` / `#ffd447` / `#ff5f56` | 四色灯 |
| `--accent` | `#45f882` | 主色（=green 磷光） |
| `--accent-ink` | `#04120a` | accent 上的字 |
| `--danger-bg` / `--danger-ink` | `#2a1414` / `#ff8d84` | 横幅/toast |

对比度：text/bg ≈ 11.4:1（AAA）；muted/bg ≈ 4.6:1（AA，仅用于 ≥11px 辅助字）。

### 字体与字号

| Token | 值 |
|-------|-----|
| `--font-ui` = `--font-mono` | `"Cascadia Mono", Consolas, "Courier New", monospace` |
| 字号 | 11 / 12 / **13** / 15（正文 13；中文在等宽下视觉偏大，正文字号不再放大） |

### 间距 / 圆角 / 层次

| Token | 值 |
|-------|-----|
| spacing | 4 / 8 / 12 / 16 / 24 |
| `--radius` | **2px**（全部组件统一近直角）；徽章也是 2px 方标 |
| 阴影 | 无投影分层，一律 1px border；状态点光晕 `0 0 8px var(--color)`（磷光余辉） |
| 扫描线 | body::after `repeating-linear-gradient(0deg, transparent 0 2px, rgba(0,0,0,.14) 2px 3px)`，pointer-events:none |
| 动效 | 120ms steps(2) 式硬切换偏好；红灯 1.2s blink（方块闪烁） |

## 3. 组件规范

| 组件 | 规范 |
|------|------|
| 主导航 | 顶栏与导航合并区下方 1px 分隔；项渲染为 `> 会话_` 形态（`::before` 加 `>`，active 加光标 `_` 闪烁）；active 文字 `--bright` |
| 顶栏汇总 | `●n` 方角描边胶囊（等宽），hover 显示全称 |
| 会话卡片 | panel 底 + 1px border，直角；首行 `● project …… 状态缩写`；红灯整卡 border 变 red 40% |
| 状态点 | 8px 方块（非圆）+ 磷光晕；红灯方块闪烁 |
| 徽章 | 方标：`RUN`/`IDLE`/`DONE`/`LAUNCH` 大写等宽 10px + 对应色边框 |
| 按钮 | 26px 高直角：primary=磷光绿底深字；ghost=transparent+border；danger=红边红字透明底；文字大写 |
| 输入 | 深底 + border；focus：accent 边 + 内侧 1px 亮线（无扩散阴影） |
| 开关 | 方形轨道，开=[■]，渲染成 `[■]` / `[ ]` 字符风格（真实实现仍是控件） |
| 进度条 | 8px 高由 `▓░` 字符纹理构成或纯色块；阈值变色规则同前 |
| 倒计时 | 等宽 15px `HH:MM:SS`；最后 60s yellow + blink |
| 横幅/Toast | `--danger-bg` + 1px 红边，前缀 `ERR:` |
| 空态 | `~ 没有发现 claude 会话` 等宽居中，`~` 前缀 |

## 4. 布局骨架

同一骨架（36/40/横幅/滚动/28），但导航与卡片全部直角对齐，页面像一张
终端截图；卡片间距 8 不变（等宽字形已足够密）。

## 5. Do / Don't

- ✅ 语义四色优先于「单色磷光纯度」——绿灯可以不是主磷光绿。
- ✅ 装饰用字符（`>` `_` `▓` `ERR:`）只出现在前缀/后缀位置。
- ❌ 不加第二层发光（文本不加 text-shadow，只状态点发光）——全屏辉光
  伤可读性也伤 GPU。
- ❌ 中文不做大写变换（`uppercase` 只作用于拉丁字符与数字标签）。
- ❌ 不模拟 CRT 弯曲/色散/噪点动画——「复古的克制」是专业感来源。
