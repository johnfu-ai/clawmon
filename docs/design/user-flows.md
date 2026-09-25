# clawmon 用户流程与页面导航（v2 设计）

| 项目 | 内容 |
|------|------|
| 版本 | 1.0 · 2026-09-25 |
| 关联 | [PRD](../prd.md) §FR5.2/FR10 · [屏幕规划](screen-plans.md) |
| 图表 | Mermaid |

---

## 1. 信息架构：页面导航关系

```mermaid
flowchart LR
    subgraph resident [常驻形态（无窗口也在线）]
        TRAY[托盘]
        PET[宠物 Clawd]
        NOTIF[Windows 通知]
    end
    MAIN[主窗口]
    P1[会话页]
    P2[任务页]
    P3[用量页]
    P4[设置页]
    MAIN -->|主导航·统一| P1 & P2 & P3 & P4
    TRAY -->|左键| MAIN
    PET -->|点击| MAIN
    NOTIF -->|点击| MAIN
    P2 -->|运行中任务「查看」| P1
```

**导航不变量（硬性，来自用户约束 1）**：

1. 主导航栏固定在顶栏正下方，四项「会话 / 任务 / 用量 / 设置」，
   所有页面同一位置、同一次序、同一行为；
2. 导航栏由共享骨架渲染（原型 `_shared/layout.js`；产品中为 app.js 同层），
   页面代码**不得**增删导航项或改写其位置；
3. 仅高亮态随当前页变化；导航徽章（如红灯数）可全局刷新；
4. 最小窗宽 340px：四项等分，不换行、不横向滚动。

## 2. F1 监控闭环（v0.x 已有，v2 保持不变）

```mermaid
flowchart TD
    A[WSL 内 claude 会话] -->|5s 轮询| B[detect.py 发现 + 配对转录]
    B --> C[engine 分类 四色]
    C -->|绿/蓝| D[仅展示]
    C -->|黄| E[提醒等用户操作]
    C -->|红| F{可继续?}
    F -->|不在 tmux| G[仅监控标签 不倒计时]
    F -->|tmux 内| H[倒计时 重置时间+30s 或 wait_secs]
    H --> I[tmux send-keys 继续按键]
    I -->|恢复| J[脱离红色 周期清零 + recovered 通知]
    I -->|未恢复| K[按 retry_interval 重试 至 max_sends]
```

关键不变量：发送前锁内记账（FR3.4）；不确定状态永不红（FR2）。

## 3. F2 新建任务（v2 新增）

```mermaid
flowchart TD
    S[任务页 · 新建任务] --> F[表单：标题 / 目录 / 命令]
    F -->|前端校验| V{目录为绝对路径<br/>命令非空 ≤512 字符?}
    V -->|否| F
    V -->|是| SAVE[保存 tasks.json · sanitize]
    SAVE --> LIST[列表出现新任务 · 待启动]
    NOTE -.保存不校验目录存在性.-> SAVE
```

设计决策：**保存宽松、启动严格**——目录可以先建好任务再创建；
真正执行时才暴露路径错误（任务转「已结束」+ toast 提示）。

## 4. F3 启动任务 → 监控接管（v2 核心流程）

```mermaid
flowchart TD
    L[点击 启动] --> LOCK[引擎锁内登记 launching<br/>重叠点击被吸收]
    LOCK --> TMUX[tmux new-session -d -s clawmon-task-id -c cwd command]
    TMUX -->|失败| ERR[任务 已结束 + 错误 toast]
    TMUX -->|成功| NEXT[下一轮轮询 ≤5s]
    NEXT --> CONFIRM[检测到任务 tmux 会话 → running]
    CONFIRM --> SESS[会话页出现对应会话行<br/>与普通会话同权：四色灯/自动继续/用量]
    SESS --> RUN{会话运行}
    RUN -->|429/卡死| AUTO[走 F1 自动继续]
    RUN -->|命令退出| FIN[tmux 会话消失 → 任务 已结束<br/>exited 通知照常]
```

## 5. F4 打开终端（人工接管，可选）

```mermaid
flowchart LR
    A[任务行 · 打开终端] --> B{Windows Terminal?}
    B -->|有| C[wt.exe 新窗格<br/>wsl.exe -- tmux attach -t name]
    B -->|无| D[wsl.exe 窗口化 attach]
    C & D --> E[用户肉眼查看 / 手动操作]
    E -->|关闭窗格| F[detach · 会话继续在 tmux 里跑]
```

要点：终端进程与 clawmon 解耦（无 CREATE_NO_WINDOW、独立进程）；
关闭终端 = detach，任务不中断；监控**不依赖**此按钮。

## 6. F5 查看用量

```mermaid
flowchart LR
    A[底部用量芯片 悬停] --> B[简要：百分比 + 重置时间]
    C[主导航 → 用量页] --> D[完整：5h/周额度条<br/>积分明细 · 套餐等级]
    D --> E[每会话 token 排行<br/>in / cache / out / req]
```

数据每 5 分钟由独立线程刷新；失败保留上次值；仅展示，永不参与分类。

## 7. F6 设置变更

```mermaid
flowchart LR
    A[设置页修改] --> B[保存 → Rust sanitize 钳制]
    B --> C[广播 settings 事件]
    C --> D[主窗口即时生效<br/>轮询间隔/语言]
    C --> E[宠物即时生效<br/>语言]
```

## 8. 页面 ↔ 流程矩阵

| 页面 | 承载流程 | 页面内主动作 | 空态 |
|------|----------|--------------|------|
| 会话 | F1 | 立即继续（红）、跳转任务（任务会话） | 「没有发现 claude 会话」+ tmux 使用提示 |
| 任务 | F2 F3 F4 | 新建、启动、打开终端、停止、编辑、删除 | 「任务清单为空」+ 新建引导 |
| 用量 | F5 | 悬停明细（无跳转） | 「未配置 GLM 端点或已关闭」 |
| 设置 | F6 | 保存 / 还原 | —（表单常驻） |
