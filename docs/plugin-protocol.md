# Reef Plugin Protocol Specification

Version: 0.1 (draft)

## 概述

Reef 插件运行在独立子进程中，通过 **JSON-RPC 2.0 over stdin/stdout** 与 host 双向通信。
Host 负责布局、渲染、事件分发；插件只负责业务逻辑，返回结构化内容由 host 统一渲染。

---

## 插件目录结构

```
~/.config/reef/plugins/
└── git/
    ├── reef.json       # 插件清单（必须）
    └── git-plugin      # 可执行文件（reef.json 中 main 指定）
```

内置插件同样遵循此协议，位于 reef 二进制旁边的 `plugins/` 目录。

---

## 插件清单 `reef.json`

```json
{
  "name": "git",
  "displayName": "Git",
  "version": "1.0.0",
  "description": "Git status, diff, stage/unstage",
  "main": "./git-plugin",
  "activationEvents": [
    "onPanel:git.status",
    "onCommand:git.focus"
  ],
  "contributes": {
    "panels": [
      {
        "id": "git.status",
        "title": "Git",
        "slot": "sidebar",
        "icon": "⎇"
      }
    ],
    "keybindings": [
      { "key": "s",           "command": "git.stage",   "when": "panel == git.status" },
      { "key": "u",           "command": "git.unstage", "when": "panel == git.status" },
      { "key": "ctrl+shift+g","command": "git.focus" }
    ],
    "commands": [
      { "id": "git.stage",   "title": "暂存文件" },
      { "id": "git.unstage", "title": "取消暂存" },
      { "id": "git.focus",   "title": "切换到 Git 面板" },
      { "id": "git.commit",  "title": "提交" }
    ]
  }
}
```

### 字段说明

| 字段 | 必须 | 说明 |
|------|------|------|
| `name` | ✓ | 唯一标识符，小写字母+连字符 |
| `version` | ✓ | semver |
| `main` | ✓ | 可执行文件路径（相对于 reef.json） |
| `activationEvents` | ✓ | 触发插件进程启动的事件列表 |
| `contributes.panels` | | 插件注册的面板 |
| `contributes.keybindings` | | 插件注册的快捷键 |
| `contributes.commands` | | 插件注册的命令 |

### Panel Slot 取值

| slot | 说明 |
|------|------|
| `sidebar` | 左侧边栏（可切换的多个 tab） |
| `editor` | 右侧主编辑区 |
| `overlay` | 全屏浮层（如搜索、命令面板） |
| `statusbar` | 状态栏片段 |

### `when` 表达式

| 表达式 | 说明 |
|--------|------|
| `panel == git.status` | 当前焦点在指定 panel |
| `activeFile == "*.rs"` | 当前打开文件匹配 glob |
| `always` | 全局生效（默认） |

---

## 传输层

与 LSP 相同，消息格式：

```
Content-Length: <字节数>\r\n
\r\n
<JSON 内容>
```

- 编码：UTF-8
- 每条消息独立，无状态
- 双向：host 和插件都可以主动发起 request 或 notification

### JSON-RPC 类型

**Request**（需要响应）:
```json
{ "jsonrpc": "2.0", "id": 1, "method": "reef/render", "params": {} }
```

**Response**:
```json
{ "jsonrpc": "2.0", "id": 1, "result": {} }
{ "jsonrpc": "2.0", "id": 1, "error": { "code": -32600, "message": "..." } }
```

**Notification**（不需要响应）:
```json
{ "jsonrpc": "2.0", "method": "reef/event", "params": {} }
```

---

## 生命周期

```
Host                          Plugin
 |                              |
 |── reef/initialize ──────────>|   host 发起，传入能力和初始区域
 |<─ result ────────────────────|   插件返回自己的能力声明
 |                              |
 |── reef/render ───────────────>|  请求渲染（焦点变化、resize 等触发）
 |<─ result (lines) ────────────|
 |                              |
 |── reef/event (key/mouse) ───>|  用户事件
 |<─ result ────────────────────|  consumed: true/false
 |                              |
 |── reef/command ─────────────>|  执行命令
 |<─ result ────────────────────|
 |                              |
 |── reef/shutdown ────────────>|
 |                              |  进程退出
```

---

## 消息定义

### Host → Plugin

#### `reef/initialize`

插件进程启动后，host 立即发送。

```json
{
  "method": "reef/initialize",
  "params": {
    "reefVersion": "0.1.0",
    "capabilities": {
      "renderModel": "styledLines"
    }
  }
}
```

**Response:**
```json
{
  "result": {
    "pluginInfo": {
      "name": "git",
      "version": "1.0.0"
    }
  }
}
```

---

#### `reef/render`

Host 请求插件渲染指定 panel。

```json
{
  "method": "reef/render",
  "params": {
    "panelId": "git.status",
    "area": { "width": 40, "height": 30 },
    "focused": true
  }
}
```

**Response:** → `RenderResult`（见渲染模型）

---

#### `reef/event`

用户输入事件，仅发送给当前焦点 panel 的插件。

```json
{
  "method": "reef/event",
  "params": {
    "panelId": "git.status",
    "event": {
      "type": "key",
      "key": "s",
      "modifiers": []
    }
  }
}
```

```json
{
  "method": "reef/event",
  "params": {
    "panelId": "git.status",
    "event": {
      "type": "mouse",
      "kind": "click",
      "button": "left",
      "column": 5,
      "row": 3
    }
  }
}
```

**Response:**
```json
{ "result": { "consumed": true } }
```

`consumed: false` 时 host 继续处理全局快捷键。

---

#### `reef/command`

Host 通知插件执行某命令（由快捷键或命令面板触发）。

```json
{
  "method": "reef/command",
  "params": {
    "id": "git.stage",
    "args": { "path": "src/main.rs" }
  }
}
```

**Response:**
```json
{ "result": { "success": true } }
```

---

#### `reef/resize`

Notification，终端尺寸变化时广播。

```json
{
  "method": "reef/resize",
  "params": { "width": 220, "height": 50 }
}
```

---

#### `reef/shutdown`

Notification，host 退出前通知插件清理资源。

---

### Plugin → Host

#### `reef/openFile` (Request)

请求 host 在编辑区打开文件。

```json
{
  "method": "reef/openFile",
  "params": {
    "path": "src/main.rs",
    "line": 42,
    "column": 1
  }
}
```

---

#### `reef/notify` (Notification)

在状态栏或 toast 显示消息。

```json
{
  "method": "reef/notify",
  "params": {
    "message": "已暂存 src/main.rs",
    "level": "info"
  }
}
```

`level`: `"info"` | `"warn"` | `"error"`

---

#### `reef/requestRender` (Notification)

插件内部状态变化后主动请求 host 重新渲染自己的 panel（如 git status 刷新后）。

```json
{
  "method": "reef/requestRender",
  "params": { "panelId": "git.status" }
}
```

---

## 渲染模型

插件不直接操作终端，返回 `StyledLine[]`，由 host 用 ratatui 统一渲染。

### `RenderResult`

```json
{
  "result": {
    "panelId": "git.status",
    "lines": [
      {
        "spans": [
          { "text": "⌄ ", "fg": "white", "bold": true },
          { "text": "暂存的更改", "fg": "white", "bold": true },
          { "text": "  2", "fg": "green" }
        ]
      },
      {
        "spans": [
          { "text": "  src/main.rs", "fg": "white" },
          { "text": " M", "fg": "yellow" },
          { "text": " +", "fg": "green", "bold": true }
        ],
        "clickAction": { "command": "git.selectFile", "args": { "path": "src/main.rs" } }
      }
    ],
    "scrollable": true,
    "scrollOffset": 0,
    "totalLines": 15
  }
}
```

### `Span` 字段

| 字段 | 类型 | 说明 |
|------|------|------|
| `text` | string | 显示文本 |
| `fg` | Color? | 前景色 |
| `bg` | Color? | 背景色 |
| `bold` | bool? | 粗体 |
| `dim` | bool? | 暗色 |
| `italic` | bool? | 斜体 |

### Color 格式

- 命名色：`"red"` `"green"` `"yellow"` `"blue"` `"cyan"` `"white"` `"gray"` `"darkGray"`
- RGB：`"#1e1e28"` 或 `[30, 30, 40]`

### `Line` 字段

| 字段 | 类型 | 说明 |
|------|------|------|
| `spans` | Span[] | 行内样式片段 |
| `clickAction` | Action? | 鼠标点击时触发的命令 |

---

## 错误码

| 代码 | 说明 |
|------|------|
| -32700 | Parse error |
| -32600 | Invalid request |
| -32601 | Method not found |
| -32000 | Plugin internal error |
| -32001 | Panel not found |
| -32002 | Command not found |

---

## 内置插件列表（计划）

| 插件 | slot | 说明 |
|------|------|------|
| `file-tree` | sidebar | 文件树导航 |
| `git` | sidebar | Git status / diff / stage |
| `file-viewer` | editor | 文件内容查看（只读） |
| `file-search` | overlay | Ctrl+P 文件名模糊搜索 |
| `grep` | overlay | Ctrl+Shift+F 内容搜索 |

---

## 实现路线

1. **Host 核心**：进程管理、JSON-RPC 收发、panel 路由、keybinding 分发
2. **渲染层**：`StyledLine` → ratatui `Line` 转换
3. **git 插件**：把现有 git 功能拆成独立进程，验证协议
4. **file-tree 插件**：第一个全新插件
5. **file-viewer 插件**：文件内容查看
6. **file-search 插件**：Ctrl+P overlay
