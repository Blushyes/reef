# Reef

> **AI 时代的最小开发者终端**
> **The minimal dev terminal for the AI coding era**

**AI 写代码之后，IDE 九成的功能都不需要了。Reef 是剩下的一成。**

<a href="./README.md"><img alt="English" src="https://img.shields.io/badge/lang-EN-blue?style=flat-square"></a>
<a href="./README.zh-CN.md"><img alt="简体中文" src="https://img.shields.io/badge/lang-%E4%B8%AD%E6%96%87-red?style=flat-square"></a>

---

## 为什么做这个

当 AI 接管了大部分写代码的工作，IDE 对人真正有用的界面就剩下几样：浏览文件、查看文件、搜索、走查 git diff、把活儿交付出去。Reef 只做这些，别的都不做。

没有补全，没有 linter，没有 language server，连编辑器都没有。写代码用你顺手的 AI 工具；来这里只负责读、看、提交——在本地，或者远程 SSH 到哪台机器上都行。

## 当前功能

### 四个标签

- **Files**：文件树 + 只读预览——代码带语法高亮、图片内联渲染（自动检测 Kitty / iTerm2 / 半块字符三种协议之一）、二进制文件显示友好的元信息卡片。
- **Search**：工作目录内容级搜索（基于 ripgrep，遵循 `.gitignore`），右栏活预览；列表 / 输入两态切换，整行水平滚动，结果上限 1000 条。
- **Git**：status 带每个文件的 `+N −M` 统计；按文件**或按文件夹**暂存 / 取消暂存（键盘或双击）；unified / side-by-side diff；compact / full-file 上下文；语法高亮；带确认的单条还原和一次性全部还原；带确认横条的推送 / 强制推送（`--force-with-lease`）。
- **Graph**：commit 图（DAG）+ 引用标签，内联 commit 详情与单文件 diff。vim 风格的 visual 模式（按 `V`）选一段 commit 区间并渲染整段的合并 diff；`Shift+↑/↓` 或 `Shift+Click` 可以不进 visual 模式直接扩选。

### 浮层

- <kbd>Space</kbd> <kbd>P</kbd>：模糊路径快速打开文件。
- <kbd>Space</kbd> <kbd>F</kbd>：全局内容搜索浮层（跟 Search 标签共用后端，<kbd>Alt</kbd>+<kbd>Enter</kbd> 可以把当前查询 pin 到标签里）。
- <kbd>Ctrl</kbd>+<kbd>O</kbd>：hosts 选择器，列出 `~/.ssh/config` 里的 SSH 主机加上最近用过的记录；选中即热切换，不用重启 Reef。
- <kbd>h</kbd>：帮助。

### 远程 SSH

连上远程机器，整个 Files / Search / Git / Graph UI 照搬——同一套键盘，同一套功能——靠一个小小的 `reef-agent` 守护进程通过 SSH stdio 跑。

```bash
reef --ssh user@host            # 打开远端 $HOME（agent 自动安装）
reef --ssh user@host:/path      # 打开远端的 /path
```

- 自动部署：Reef 嗅探远端 OS/arch，把版本一致的 `reef-agent` 装上去——优先从 GitHub Releases 拉，拉不到就从本地 build 上传。
- 一条 SSH ControlMaster 会话包办全部：JSON-RPC over stdio 走数据，`scp` 走拖拽上传，`ssh -t` 走 `$EDITOR` 透传。
- 内容搜索、diff、graph、暂存/取消暂存、文件树增删改名——远程全部等效于本地。

### 文件操作

- 在文件树里创建、改名、移到回收站、硬删除——工具栏、右键菜单、键盘（`F2`、`d` / `Del`、`Shift+D` / `Shift+Del`）都走得通。
- **把 Finder / 资源管理器里的文件拖进终端**，进入 *place 模式*——点一个目录就把文件复制到那里，`Esc` 或右键取消。走 SSH 时用已有的 ControlMaster 上传。

### 使用体验

- **键盘优先**，鼠标只出现在真正值得的地方——拖动分隔条、双击切换暂存、在光标下的面板里滚动、右键文件树弹菜单。
- **vim 风格的面板内搜索**：`/` 和 `?` 开提示符，`n` / `N` 跳匹配，提示符里支持 `Alt`/`Ctrl` 的词级编辑。
- **导航键**：`↑` / `↓`、`j` / `k`、`Ctrl+P` / `Ctrl+N` / `Ctrl+K` / `Ctrl+J` 在每个标签里都能移动选择。
- **选择 & 剪贴板**：`v` 关掉鼠标捕获让终端原生选择接管；或者直接在预览里拖选，Reef 通过 OSC 52 写进系统剪贴板（不用辅助二进制，SSH 上也行）。
- **自动主题**：OSC 11 探测终端明暗主题；语言自动检测（English / 简体中文）。
- **记住偏好**：diff 布局、diff 模式、最近的 SSH 主机等跨会话保留。

## 安装

### 通过 npm（推荐）

```bash
# 直接运行，不装
npx @reef-tui/cli

# 或者全局装
npm install -g @reef-tui/cli
reef
```

会自动选择当前平台对应的原生二进制。
支持：macOS (arm64, x64)、Linux (arm64, x64)、Windows (x64)。

### 从源码构建

```bash
cargo build --release

# 在任意 git 仓库里运行：
cd your-git-repo
/path/to/reef/target/release/reef
```

Reef 在任何目录都能启动。不在 git 仓库里时，Git 和 Graph 标签显示 "Not a git repository" 占位，Files 标签仍然可以浏览当前目录。如果你在非仓库目录启动，且没有历史 SSH 主机，Reef 会自动弹出 hosts 选择器，让你始终有个可见的下一步。

## 快捷键

### 全局

| 按键 | 功能 |
| --- | --- |
| `q`、`Ctrl+C` | 退出 |
| `1` / `2` / `3` / `4` | 切到 Files / Search / Git / Graph 标签 |
| `Tab` | 循环切换标签 |
| `Shift+Tab` | 切换聚焦面板 |
| `h` | 帮助 |
| `Space p` | 模糊路径快速打开文件 |
| `Space f` | 全局内容搜索浮层 |
| `Ctrl+O` | hosts 选择器（打开 SSH 连接） |
| `v` | 关/开鼠标捕获（让终端原生选择文本） |
| `↑` / `↓`、`j` / `k`、`Ctrl+P` / `Ctrl+N` | 导航 |
| `Ctrl+K` / `Ctrl+J` | 导航（备用） |
| `PgUp` / `PgDn` | 翻页 |
| `←` / `→`、`Shift+←` / `Shift+→` | 水平滚动 |
| `Home` / `End` | 重置 / 跳到末尾 |
| `/` · `?` · `n` / `N` | 预览 / diff 里的 vim 风格查找 |
| 拖文件进终端 | 进入 place 模式 |

### Files 标签

| 按键 | 功能 |
| --- | --- |
| `Enter` | 展开/折叠目录，或用 `$EDITOR` 打开文件 |
| `e` | 用 `$EDITOR` 打开选中文件 |
| `r` | 重建文件树 |
| `F2` | 重命名选中项 |
| `d` / `Del` / `⌫` | 移到回收站（需确认） |
| `Shift+D` / `Shift+Del` | 硬删除（需确认） |
| 右键 | 弹出文件树右键菜单 |

### Search 标签

左栏有两种模式。默认是列表模式：全局快捷键（`h`、`q`、数字键等）照常响应；按 `/` 或 `i` 聚焦输入，`Esc` 退回列表。打字实时刷新结果。

| 按键 | 功能 |
| --- | --- |
| `/` 或 `i` | 聚焦搜索输入 |
| `Esc` | 回到列表模式 |
| `Enter`、双击 | 切到 Files 标签并定位文件（匹配行高亮） |
| `r` | 重跑当前查询 |
| `Alt+←` / `Alt+→`、`Ctrl+W` / `Ctrl+U` | 提示符里的词级编辑 |

### Git 标签

| 按键 | 功能 |
| --- | --- |
| `s` / `u` | 暂存 / 取消暂存（选中文件夹时整个文件夹下的文件一起走） |
| `d` → `y` | 还原未暂存的文件 / 文件夹（需确认） |
| `Enter` / `e` | 用 `$EDITOR` 打开选中文件 |
| `r` | 刷新 |
| `t` | 树形 / 扁平 切换 |
| `m` | unified ↔ side-by-side |
| `f` | compact ↔ full-file |

### Graph 标签

| 按键 | 功能 |
| --- | --- |
| `↑` / `↓`、`j` / `k` | 移动选中的 commit |
| `V` | 进入 visual 模式（选一段 commit 区间） |
| `↑` / `↓` 或 `PgUp` / `PgDn`（visual 中） | 扩展区间 |
| `Shift+↑` / `Shift+↓`、`Shift+Click` | 不进 visual 模式直接扩选 |
| 在 visual 中点击 | 设区间另一端 |
| `Esc` | 退出 visual 模式 / 清空区间 |
| `m` / `f` | commit 文件 diff 的布局 / 模式 |
| `t` | 树形 / 扁平 显示变更文件 |

## 当前状态

Alpha。单进程 Rust 二进制——本地路径没有插件、没有 IPC。Files、Git、Graph 都是 host 原生；远程靠一个很薄的 `reef-agent` 守护进程通过 SSH stdio 驱动。一套代码、一个 UI、一套键位——本地和远程的功能一致。
