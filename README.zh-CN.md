# Reef

> **AI 时代的最小开发者终端**
> **The minimal dev terminal for the AI coding era**

**AI 写代码之后，IDE 九成的功能都不需要了。Reef 是剩下的一成。**

<a href="./README.md"><img alt="English" src="https://img.shields.io/badge/lang-EN-blue?style=flat-square"></a>
<a href="./README.zh-CN.md"><img alt="简体中文" src="https://img.shields.io/badge/lang-%E4%B8%AD%E6%96%87-red?style=flat-square"></a>

---

## 为什么做这个

当 AI 接管了大部分写代码的工作，IDE 对人真正有用的界面就剩下四样：浏览文件、查看文件、搜索、走查 git diff。Reef 只做这些，别的都不做。

没有补全，没有 linter，没有 language server，连编辑器都没有。写代码用你顺手的 AI 工具；来这里只是为了读代码和提交。

## 当前功能

- **Files 标签**：文件树 + 只读预览
- **Git 标签**：status、stage / unstage（键盘或双击）、unified / side-by-side diff、compact / full-file 上下文、带确认的还原（discard）、带确认的推送 / 强制推送（`--force-with-lease`）
- **Graph 标签**：commit 图（DAG）、引用标签、可选中行、内联 commit 详情与单文件 diff
- **键盘优先**，鼠标只在合适的地方出现——拖动分隔条、双击切换暂存、在光标下的面板里滚动
- **记住偏好**：diff 的布局和模式跨会话保留

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
cargo build --release

# 在任意 git 仓库里运行：
cd your-git-repo
/path/to/reef/target/release/reef
```

Reef 在任何目录都能启动。不在 git 仓库里时，Git 和 Graph 标签显示 "Not a git repository" 占位，Files 标签仍然可以浏览当前目录。

## 快捷键

### 全局

| 按键 | 功能 |
| --- | --- |
| `q`、`Ctrl+C` | 退出 |
| `1` / `2` / `3` | 切到 Files / Git / Graph 标签 |
| `Tab` | 循环切换标签 |
| `Shift+Tab` | 切换聚焦的面板 |
| `h` | 帮助 |
| `v` | 关/开鼠标捕获（让终端原生选择文本） |

### Files 标签

| 按键 | 功能 |
| --- | --- |
| `↑`/`↓`、`j`/`k` | 导航 |
| `PgUp` / `PgDn` | 翻页 |
| `Enter` | 展开/折叠目录，或用 `$EDITOR` 打开文件 |
| `r` | 重建文件树 |

### Git 标签

| 按键 | 功能 |
| --- | --- |
| `s` / `u` | 暂存 / 取消暂存 |
| `d` → `y` | 还原未暂存的文件（需确认） |
| `r` | 刷新 |
| `t` | 树形 / 扁平 切换 |
| `m` | unified / side-by-side |
| `f` | compact / full-file |

### Graph 标签

| 按键 | 功能 |
| --- | --- |
| `↑`/`↓`、`j`/`k` | 移动选中的 commit |
| `m` / `f` | commit 文件 diff 的布局 / 模式 |
| `t` | 树形 / 扁平 显示变更文件 |

## 当前状态

Alpha 阶段。单进程 Rust 二进制——没有插件，没有 IPC。Files、Git、Graph 全部 host 原生。
