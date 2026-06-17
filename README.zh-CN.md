# Reef

> **AI 时代的最小开发者终端**

AI 写代码之后，IDE 九成的功能都不需要了。Reef 是剩下的一成。

<a href="./README.md"><img alt="English" src="https://img.shields.io/badge/lang-EN-blue?style=flat-square"></a>
<a href="./README.zh-CN.md"><img alt="简体中文" src="https://img.shields.io/badge/lang-%E4%B8%AD%E6%96%87-red?style=flat-square"></a>

单二进制的终端工作台，专门用来浏览、走查、提交——本地或者 SSH 远端。不带编辑器、不带补全。代码用你顺手的 AI 写；这里只管剩下的事。

<p align="center">
  <img src="https://github.com/Blushyes/reef/releases/download/v0.33.0/reef.png" alt="Reef" width="900">
</p>

## 功能

- **Files**：文件树 + 只读预览；代码语法高亮、图片内联渲染（Kitty / iTerm2 / 半块字符自动检测）。
- **SQLite 浏览器**：打开任意 `.db` 文件，浏览 schemas、tables、views、indexes、triggers；无框线现代化数据网格，列名按类型着色，index / trigger 有结构详情卡片。
- **Search**：基于 ripgrep 的工作目录内容搜索，右栏活预览，遵循 `.gitignore`。
- **Git**：每文件 / 每文件夹的 `+N −M` 统计；暂存 / 取消暂存 / 还原 / 推送（含 `--force-with-lease`）；unified 或 side-by-side diff。
- **Graph**：commit DAG + 引用标签，内联 diff，visual 模式选区间。
- **远程 SSH**：一套 UI、一套键位；自动部署的 `reef-agent` 守护进程通过 ControlMaster 接管 RPC、文件上传、`$EDITOR` 透传。
- **拖拽上传**：把 Finder / 资源管理器里的文件拖进终端（远程也行）。
- **自动主题**（OSC 11 探测）与语言自动检测，偏好跨会话保留。

键盘优先，鼠标只出现在真正值得的地方。应用里按 `h` 看全部键位。

## 安装

```bash
npx @reef-tui/cli                # 直接运行
npm install -g @reef-tui/cli     # 或全局安装
```

支持：macOS (arm64, x64)、Linux (arm64, x64)、Windows (x64)。也可以 `cargo build --release` 从源码构建。

```bash
reef                             # 打开当前目录
reef --ssh user@host             # 远端 $HOME（agent 自动安装）
reef --ssh user@host:/path       # 远端 /path
```

## 当前状态

Alpha。单 Rust 二进制，没有插件。本地和远程功能一致。
