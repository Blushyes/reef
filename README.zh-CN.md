# Reef

> **AI 时代的最小开发者终端**
> **The minimal dev terminal for the AI coding era**

**AI 写代码，你审代码；Reef 只做后一半。**

<a href="./README.md"><img alt="English" src="https://img.shields.io/badge/lang-EN-blue?style=flat-square"></a>
<a href="./README.zh-CN.md"><img alt="简体中文" src="https://img.shields.io/badge/lang-%E4%B8%AD%E6%96%87-red?style=flat-square"></a>

---

## 为什么做这个

当 AI 接管了大部分写代码的工作，IDE 对人真正有用的界面就剩下四样：浏览文件、查看文件、搜索、走查 git diff。Reef 只做这些，别的都不做。

没有补全，没有 linter，没有 language server，连编辑器都没有。写代码用你顺手的 AI 工具；来这里只是为了读代码和提交。

## 当前功能

- **Files 标签**：文件树 + 只读预览
- **Git 标签**：status、stage / unstage（键盘或双击）、unified / side-by-side diff、compact / full-file 上下文
- **键盘优先**，鼠标只在合适的地方出现——拖动分隔条、双击切换暂存、在光标下的面板里滚动
- **记住偏好**：diff 的布局和模式跨会话保留

## 架构

Reef 是一个 host 加若干插件。插件是**独立子进程**，通过 stdin/stdout 上的 JSON-RPC 2.0（LSP 风格的 `Content-Length` 帧）与 host 通信。插件返回 `StyledLine[]`，host 用 [ratatui](https://github.com/ratatui/ratatui) 统一渲染。清单格式参考 VSCode 的 `contributes.*`。

名字本身就是架构：一片珊瑚礁由许多彼此独立的小生物共生堆出来。每个插件都是独立进程；拼在一起就是工作区。

完整协议见 [docs/plugin-protocol.md](./docs/plugin-protocol.md)。

## 安装

**通过 npm（推荐）：**

```bash
# 直接运行，无需安装
npx @reef-tui/cli

# 或全局安装
npm install -g @reef-tui/cli
reef
```

会自动选择当前平台对应的原生二进制。
支持：macOS (arm64, x64)、Linux (arm64, x64)、Windows (x64)。

**从源码构建：**

```bash
# 必须 release 构建——内置 git 插件的清单指向 target/release/reef-git。
cargo build --release

# 在任意 git 仓库里运行：
cd your-git-repo
/path/to/reef/target/release/reef
```

当前目录不在 git 仓库里，Reef 会直接退出。

### 插件查找位置

Reef 依次查找三个位置：

1. `<reef 二进制>/plugins/` —— 与二进制一起分发
2. `<工作区>/plugins/` —— 开发模式，从本仓库运行时
3. `~/.config/reef/plugins/` —— 用户插件

每个插件是一个目录，含 `reef.json` 清单和一个可执行文件。

## 快捷键

### 全局

| 按键 | 功能 |
| --- | --- |
| `q`、`Ctrl+C` | 退出 |
| `1` / `2` | 切到 Files / Git 标签 |
| `Tab` | 循环切换标签 |
| `Shift+Tab` | 切换聚焦的面板 |
| `h` | 帮助 |
| `v` | 关/开鼠标捕获（让终端原生选择文本） |

### Files 标签

| 按键 | 功能 |
| --- | --- |
| `↑`/`↓`、`j`/`k` | 导航 |
| `PgUp` / `PgDn` | 翻页 |
| `Enter` | 展开/折叠目录 |
| `r` | 重建文件树 |

### Git 标签

| 按键 | 功能 |
| --- | --- |
| `s` / `u` | 暂存 / 取消暂存 |
| `r` | 刷新 |
| `t` | 树形 / 扁平 切换 |
| `m` | unified / side-by-side |
| `f` | compact / full-file |

## 当前状态

Alpha 阶段。已内置插件：`git`。host 原生：文件树与预览。规划中（作为独立插件）：`file-search`、`grep`、更完整的 `file-viewer`。
