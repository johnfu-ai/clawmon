# clawmon — Claude Code WSL 状态监控

一个 Windows 桌面小程序（Tauri 2），用于监控运行在 **WSL** 里的多个
Claude Code 终端会话，用 **红 / 黄 / 绿灯** 展示状态，并在 **API 用量耗尽
导致的长等待**（典型为 5 小时）结束后自动向终端发送按键，让任务继续。

## 工作原理

```
┌──────────── Windows ────────────┐      ┌──────── WSL ────────┐
│  Tauri 2 (Rust + WebView2)      │      │                      │
│  ┌──────────┐   wsl.exe   ┌─────┴────┐ │  ┌────────────────┐  │
│  │ 轮询线程  │ ─────────▶ │ python3  │─┼─▶ │ claude 进程     │  │
│  │ (后端)    │ ◀───────── │ 检测脚本  │ │  │ ~/.claude/     │  │
│  └────┬─────┘   JSON      └─────┬────┘ │  │  projects/*.jsonl│ │
│       │ 事件推送 → 红绿灯 UI      │      │  └────────────────┘  │
│       │ tmux send-keys (wsl.exe)──────┼──▶ 目标终端 pane       │
└─────────────────────────────────┘      └──────────────────────┘
```

轮询跑在后端线程里，UI 只是订阅者。放在 WebView 里用 JS 定时器是不行的：
窗口一旦最小化到宠物或收进托盘，WebView2 会把隐藏窗口的定时器限流到大约
每分钟一次——而「窗口不在前台也要继续监控和自动继续」正是这个程序的核心。

- **常驻检测进程**：检测脚本常驻 WSL（`--serve` 模式，每轮一行请求 /
  一行 JSON 应答），而不是每轮轮询重新拉起一次 `wsl.exe`。常驻进程死掉、
  超时或输出无法解析时，自动降级为老的一次性管道模式，下一轮再重拉常驻
  进程，因此常驻路径的任何故障都不会损失一次轮询。
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
  | 🟡 | 找不到会话记录 | 刚启动尚未写盘，或进程不属于任何记录 |
  | 🟡 | 记录的写入时间早于进程启动 | 配对到的是一份旧记录，不作数 |
  | 🔴 | 最后一条是 user 且空闲 > 超时阈值（默认 5 分钟） | 疑似 API 用尽/卡死 |

  后两条是**红灯的前提**：只有确认这份记录确实属于该进程，才会判定超时。
  否则一个刚启动、还没写记录的会话会在第一轮就被判红，甚至几小时后被误发
  一次按键。拿不准时一律显示黄灯——误判的代价是往一个无关终端里敲回车。
- **自动继续**：会话变红后开始计时，到达设定等待时长（默认 **5 小时**，
  对应 Claude Code 用量重置窗口）后，通过 `tmux send-keys -t <pane> Enter`
  向该终端发送继续按键；若仍未恢复，每隔一段时间重试（默认 10 分钟，最多
  3 次）。所有参数可在设置里改。每次尝试在调度的那一刻就记入档案，因此
  两次重叠的轮询不会重复发按键；不在 tmux 里的会话不会倒计时（显示「不在
  tmux 中，无法自动继续」）。

> **重要**：只有运行在 **tmux** 里的 Claude Code 才能被控制（Linux 已禁用
> TIOCSTI，从外部直接向 pts 注入按键不可行）。不在 tmux 里的会话只能
> 监控（UI 中标为「仅监控」）。

### 通知与事件

引擎在两次轮询之间做边沿检测，以下状态变化可以选择性弹出 Windows 通知
（可逐项开关，默认开启「变红 / 自动继续 / 恢复 / 退出」，「回合结束」
默认关闭以免打扰），并可伴随一声系统提示音：

| 事件 | 默认 | 说明 |
|------|------|------|
| 会话变红 | 开 | 进入疑似卡死状态，将按设置自动继续 |
| 自动继续已发送 | 开 | 显示发送的按键与第几次尝试 |
| 红灯解除 | 开 | 未经干预自行恢复运行（用量窗口重置等） |
| 回合结束 | 关 | claude 跑完一轮、等待下一步输入时 |
| 会话退出 | 开 | claude 进程消失（正常结束或崩溃） |

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

产物在仓库根目录的 `target/release/`（`clawmon.exe` 可直接拷走使用）。

### 在 WSL 里驱动本机 Windows 编译

Windows 侧装好 Rust（`%USERPROFILE%\.cargo`）后，WSL 会话可以直接驱动
本机工具链出 exe（不经交叉编译，和真机编译完全一致）：

```bash
scripts/build-local.sh
```

脚本把仓库镜像到 `C:\Users\<你>\clawmon-build\`（保留 `target\` 作增量
缓存，排除 `.git`），调用 Windows 的 `cargo.exe build --release`，产物在
`C:\Users\<你>\clawmon-build\target\release\clawmon.exe`。首次全量约
十几分钟，其后增量一两分钟。

## 使用

1. 启动小程序，保持窗口开着（可以最小化、关到系统托盘，后台仍会
   轮询 + 自动继续；托盘图标悬停可见红黄绿灯统计）。
2. 每个 claude 进程显示为一行：项目名、灯色、状态、空闲时长、本会话
   token 用量（`in x · cache x · out x · N req`，悬停有说明；由
   transcript 按 message id 去重累计，含子代理）、最后输出预览、
   tmux 位置。
3. 红灯会闪烁并显示「xx:xx:xx 后自动继续」倒计时；也可以点
   **「立即继续」** 手动发送按键。
4. **桌面宠物**：一只 Claude Code 螃蟹（Clawd）风格的珊瑚橘小螃蟹
   常驻桌面右下角（置顶显示，启动即出现）。它用表情 + 头顶徽章反映
   红黄绿灯（绿=正常眨眼、黄=打瞌睡、红=发抖并显示数量徽章、
   WSL 断连=变灰睡觉），两只钳子会交替挥舞；可拖到任意位置，点一下
   即可弹出主窗口（宠物本身常驻不消失）。最小化主窗口只是把它藏起来，
   监控面完全交给小螃蟹。窗口四角的透明区域点击可穿透，只有螃蟹本体
   拦截鼠标。
5. **GLM 套餐用量**（底部状态栏）：从 `~/.claude/settings.json` 读取
   `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN`（令牌只在 WSL 内部
   使用，不经过 Windows 侧），每 5 分钟查询一次套餐配额接口，显示
   5 小时额度与周额度「5h xx% · 7d xx%」（悬停可见积分明细、重置
   时间与套餐等级；70% 变黄、90% 变红）。查询失败只保留上次结果，
   不影响监控本身；非 GLM 端点会自动隐藏。可在设置里关闭。
6. ⚙ 打开设置（界面与通知支持中文 / English 切换）：

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
| GLM 套餐用量 | 开 | 底部状态栏显示 5 小时 / 周配额百分比（仅 GLM Coding Plan 有意义） |
| 关闭到托盘 | 开 | 关闭窗口时最小化到系统托盘继续监控；托盘左键点击恢复窗口，右键菜单可退出 |
| 语言 | 中文 | 界面与通知语言：中文 / English |
| 通知 ×5 | 见上表 | 变红 / 自动继续 / 恢复 / 回合结束 / 退出，逐项开关 |
| 提示音 | 开 | 通知伴随一声系统提示音 |

## 项目结构

```
core/                 纯逻辑 crate（不依赖 Tauri，可单独测试）
  src/detector.rs      WSL 检测调用（常驻进程 + 一次性降级）+ JSON 解析
  src/engine.rs        状态机（红黄灯判定、倒计时、自动继续调度、事件边沿检测）
  src/settings.rs      设置持久化 + 取值校验
  src/usage.rs         GLM 套餐配额查询调用 + JSON 解析
  src/wsl.rs           wsl.exe 调用封装（Windows）/ 直接调用（Linux 测试）+ 常驻子进程
src-tauri/
  src/detect.py        内嵌的 WSL 侧检测脚本（一次性 / --serve 常驻两种模式）
  src/usage.py         内嵌的 WSL 侧配额查询脚本（读 ~/.claude/settings.json，令牌不出 WSL）
  src/lib.rs           轮询线程 + 用量线程 + 通知 + Tauri 命令胶水层
scripts/
  build-local.sh       WSL 驱动本机 Windows 工具链出 exe（见「构建」一节）
  win-clippy.sh        从 WSL 跑与 CI windows 作业一致的 clippy
ui/                   静态前端（HTML/CSS/JS，无构建步骤）
  i18n.js              中英双语字典（zh 为默认，en 为翻译）
  pet.html/pet.js      桌面宠物（透明置顶小窗，常驻的状态化身）
```

## 已知限制

- 同一目录下并发的 claude 已按进程逐个配对会话记录；但既无
  `--session-id` / `--resume` 命令行线索、又超出首条时间戳配对窗口
  （5 分钟）的恢复会话，仍可能落到最新文件上（此时会显示「记录未就绪」，
  不会被判红）。
- 工具执行期间（最后一条是含 `tool_use` 的 assistant）不会误判红灯；
  但工具刚结束、模型长时间排队响应时，与用量耗尽仍不可区分。
- 自动继续只发按键；若 claude 已退出则按键无效——不过进程退出本身现在
  会被检测到并可弹通知（「会话已结束」）。
- 检测默认走常驻进程（每轮只是一次 stdin/stdout 往返，毫秒级）；常驻
  进程故障时该轮自动降级为一次性 `wsl.exe` 调用（约 0.1–1 秒）。WSL 整体
  未运行时，首次拉起仍会付出一次完整的 WSL 启动开销。

## 开发与测试

纯逻辑在 `core` crate 里，不依赖 Tauri，可直接测试：

```bash
cargo test -p clawmon-core                          # 单元测试
cargo test -p clawmon-core --test integration -- --ignored   # 端到端（需 tmux）
cargo clippy -p clawmon-core --all-targets          # 静态检查
python3 -m py_compile src-tauri/src/detect.py       # 检测脚本
```

以上（含上面那条端到端用例）都会在 CI 里跑，见 `.github/workflows/ci.yml`。

在 Linux/WSL 上验证 Windows 侧代码（可选）。注意要用 clippy 而不是
`cargo check`——check 不跑 lint，曾有过 Windows CI 被 `needless_borrow`
拦下的教训：

```bash
rustup target add x86_64-pc-windows-msvc
scripts/win-clippy.sh    # 等价于 CI windows 作业的 clippy -D warnings
```

（`.tools/bin/llvm-rc` 是一个占位 stub，仅为绕过 `tauri-winres` 对资源
编译器的依赖；真正的 Windows 构建不需要它。）
