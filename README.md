# clawmon — Claude Code WSL 状态监控

一个 Windows 桌面小程序（Tauri 2），用于监控运行在 **WSL** 里的多个
Claude Code 终端会话，用 **红 / 黄 / 绿灯** 展示状态，并在 **API 用量耗尽
导致的长等待**（典型为 5 小时）结束后自动向终端发送按键，让任务继续。

## 工作原理

```
┌──────────── Windows ────────────┐      ┌──────── WSL ────────┐
│  Tauri 2 (Rust + WebView2)      │      │                      │
│  ┌──────────┐   wsl.exe   ┌─────┴────┐ │  ┌────────────────┐  │
│  │ 迷你 UI   │ ─────────▶ │ python3  │─┼─▶ │ claude 进程     │  │
│  │ 红绿灯    │ ◀───────── │ 检测脚本  │ │  │ ~/.claude/     │  │
│  └──────────┘   JSON      └─────┬────┘ │  │  projects/*.jsonl│ │
│       │                         │      │  └────────────────┘  │
│       │ tmux send-keys (wsl.exe)──────┼──▶ 目标终端 pane       │
└─────────────────────────────────┘      └──────────────────────┘
```

- **发现会话**：扫描 `/proc` 中的 claude 进程（PID、工作目录、TTY、是否在
  tmux pane 中），并映射到 `~/.claude/projects/<slug>/*.jsonl` 会话记录。
  同一目录下并发的多个 claude 会各自配对到自己的会话记录（依据命令行里的
  `--session-id` / `--resume`，或进程启动时间与记录首条时间戳的匹配）。
- **判断状态**（基于记录里最后一条写入 + 空闲时长）：
  | 灯 | 条件 | 含义 |
  |----|------|------|
  | 🟢 | 空闲 < 活跃阈值（默认 2 分钟） | 正在干活 |
  | 🟡 | 最后一条是含 `tool_use` 的 assistant | 工具执行中（不算超时） |
  | 🟡 | 最后一条是 assistant 且空闲较久 | 等待用户输入 |
  | 🟡 | 最后一条是 user（等待模型响应）但未超阈值 | 处理中/等待响应 |
  | 🔴 | 最后一条是 user 且空闲 > 超时阈值（默认 5 分钟） | 疑似 API 用尽/卡死 |
- **自动继续**：会话变红后开始计时，到达设定等待时长（默认 **5 小时**，
  对应 Claude Code 用量重置窗口）后，通过 `tmux send-keys -t <pane> Enter`
  向该终端发送继续按键；若仍未恢复，每隔一段时间重试（默认 10 分钟，最多
  3 次）。所有参数可在设置里改。

> **重要**：只有运行在 **tmux** 里的 Claude Code 才能被控制（Linux 已禁用
> TIOCSTI，从外部直接向 pts 注入按键不可行）。不在 tmux 里的会话只能
> 监控（UI 中标为「仅监控」）。

### 推荐的 WSL 使用方式

```bash
tmux new -s work          # 开一个 tmux 会话
cd ~/my-project
claude                    # 在 tmux 里启动 claude
```

## 构建（Windows）

前置要求：

- Rust（stable，`rustup` 安装即可）
- WebView2 Runtime（Windows 11 自带；Windows 10 若无会自动安装）
- WSL 发行版中有 `python3`（Ubuntu 自带）和 `tmux`（`sudo apt install tmux`）

前端是纯静态文件（无 Node 构建步骤），只需 Rust 工具链：

```powershell
cargo install tauri-cli --version "^2"
cargo tauri build          # 产出 exe / msi
# 开发模式：
cargo tauri dev
```

产物在 `src-tauri/target/release/`（`clawmon.exe` 可直接拷走使用）。

## 使用

1. 启动小程序，保持窗口开着（可以最小化或直接关到系统托盘，后台仍会
   轮询 + 自动继续；托盘图标悬停可见红黄绿灯统计）。
2. 每个 claude 进程显示为一行：项目名、灯色、状态、空闲时长、最后输出
   预览、tmux 位置。
3. 红灯会闪烁并显示「xx:xx:xx 后自动继续」倒计时；也可以点
   **「立即继续」** 手动发送按键。
4. ⚙ 打开设置：

| 设置项 | 默认 | 说明 |
|--------|------|------|
| 轮询间隔 | 5 秒 | 每次检测的间隔 |
| 活跃判定 | 120 秒 | 多久无输出算空闲 |
| 超时判定 | 300 秒 | 等待响应超过多久判红灯 |
| 自动继续 | 开 | 红灯到时自动发送按键 |
| 等待时长 | 5 小时 | 变红后多久发送 |
| 继续按键 | `Enter` | tmux 键名，可改如 `C-c Enter` |
| 最大尝试次数 | 3 | 每次卡死周期的自动发送上限 |
| 重试间隔 | 10 分钟 | 发送失败/无效后的重试节奏 |
| WSL 发行版 | 空 | 留空用默认发行版，可填 `Ubuntu` 等 |
| 关闭到托盘 | 开 | 关闭窗口时最小化到系统托盘继续监控；托盘左键点击恢复窗口，右键菜单可退出 |

## 项目结构

```
core/                 纯逻辑 crate（不依赖 Tauri，可单独测试）
  src/detector.rs      WSL 检测调用 + JSON 解析
  src/engine.rs        状态机（红黄灯判定、倒计时、自动继续调度）
  src/settings.rs      设置持久化
  src/wsl.rs           wsl.exe 调用封装（Windows）/ 直接调用（Linux 测试）
src-tauri/
  src/detect.py        内嵌的 WSL 侧检测脚本（python3，经 stdin 管道执行）
  src/lib.rs           Tauri 命令胶水层
ui/                   静态前端（HTML/CSS/JS，无构建步骤）
```

## 已知限制

- 同一目录下并发的 claude 已按进程逐个配对会话记录；但既无
  `--session-id` / `--resume` 命令行线索、又超出首条时间戳配对窗口
  （5 分钟）的恢复会话，仍可能落到最新文件上。
- 工具执行期间（最后一条是含 `tool_use` 的 assistant）不会误判红灯；
  但工具刚结束、模型长时间排队响应时，与用量耗尽仍不可区分。
- 自动继续只发按键；若 claude 已退出则无效（会话行会随之消失）。

## 开发与测试

纯逻辑在 `core` crate 里，不依赖 Tauri，可直接测试：

```bash
cargo test -p clawmon-core                          # 单元测试
cargo test -p clawmon-core --test integration -- --ignored   # 端到端（需 tmux）
```

在 Linux/WSL 上交叉验证 Windows 编译（可选）：

```bash
rustup target add x86_64-pc-windows-msvc
RC=.tools/bin/llvm-rc CC_x86_64_pc_windows_msvc=gcc \
  cargo check --target x86_64-pc-windows-msvc
```

（`.tools/bin/llvm-rc` 是一个占位 stub，仅为绕过 `tauri-winres` 对资源
编译器的依赖；真正的 Windows 构建不需要它。）
