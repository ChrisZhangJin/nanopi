<div align="center">

# nanopi

**不装 Node，不装 Python，没有 `node_modules`。**

一个可以直接 `scp` 到没有任何运行时的机器上跑的编码 Agent CLI ——
单文件 ~4 MB 静态 Rust 二进制，移植自 [Pi](https://github.com/earendil-works/pi)。
Alpine 能跑，CentOS 6 能跑，`npm install` 跑不通的地方它也能跑。

[![Release](https://img.shields.io/github/v/release/ChrisZhangJin/nanopi?style=flat-square&color=blue)](https://github.com/ChrisZhangJin/nanopi/releases/latest)
[![License](https://img.shields.io/badge/license-MIT-blue.svg?style=flat-square)](LICENSE)
![Binary](https://img.shields.io/badge/binary-~4%20MB-brightgreen?style=flat-square)
![Static musl](https://img.shields.io/badge/static-musl-informational?style=flat-square)
![Rust](https://img.shields.io/badge/rust-stable-orange?style=flat-square&logo=rust&logoColor=white)
[![CI](https://img.shields.io/github/actions/workflow/status/ChrisZhangJin/nanopi/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/ChrisZhangJin/nanopi/actions/workflows/ci.yml)

[English](README.md) · **简体中文**

<br>

<img src="https://raw.githubusercontent.com/ChrisZhangJin/nanopi/main/img/tui.png" alt="nanopi TUI 截图" width="760">

<p><em>Linux 上的 TUI（macOS / Linux 终端）</em></p>

<img src="https://raw.githubusercontent.com/ChrisZhangJin/nanopi/main/img/tui_win.png" alt="nanopi TUI 截图（Windows）" width="760">

<p><em>Windows 上的 TUI —— 在 Windows 10/11 上跑 <code>nanopi.exe</code> 实拍</em></p>

</div>

---

## 为什么用 nanopi？

- 🚫 **零运行时依赖** —— 不装 Node、不装 Python、不用包管理器。
  下载一个文件，`chmod +x`，直接跑
- 🖥 **能塞进老机器** —— glibc 2.12+（CentOS 6），或者用纯静态 musl 版跑
  Alpine 和其他一切环境
- 🪶 **~4 MB 静态二进制** —— musl + LTO + strip（下载体积 1.6 MB，UPX 压缩后）
- 🧬 **PI-parity** —— 对齐 [Pi](https://github.com/earendil-works/pi) 的用户界面：JSONL 会话、hooks、skills、`-p`、`/fork`、`/resume`
- 🔌 **多 provider** —— 任何 OpenAI 兼容端点（DeepSeek、ollama、vLLM …）以及原生 Anthropic；`config.toml` 里的 `provider` / `api_kind` 可以在 base_url 嗅探不够用时显式指定厂商和协议。回复文本开头（前面只有空白字符）出现的 `<think>…</think>` 块——R1 系列模型（R1 本身、各类蒸馏版、QwQ、GLM 等）经由任意 OpenAI 兼容端点提供服务时常见——会被拆出来按思考渲染；出现在正文其他文字之后的 `<think>` 则原样保留为普通文本。`inline_think_tags = true | false` 可覆盖默认值（默认开启）。
- 🛠 **流式工具调用** —— `read` / `write` / `edit` / `bash`，在 ratatui TUI 里实时渲染
- 🪝 **Claude Code 协议的 hooks** —— JSON 走 stdin、退出码 2 表示拒绝的 shell 钩子，事件名沿用 PI 的命名（`tool_execution_start` / `tool_execution_end` / `input` / …）
- 🧠 **Agent Skills** —— 遵循 [官方规范](https://agentskills.io/specification) 的 `SKILL.md` 发现 + `/skill:name` 展开

## 设计初衷 —— 为什么要做 nanopi

Pi 是个好用的编码 agent,但上游选择不支持一些真实用户切实需要的场景:

| 上游 issue | 用户诉求 | 上游状态 |
|---|---|---|
| [pi#8591](https://github.com/earendil-works/pi/issues/8591) | 为 Alpine 提供 musl 静态构建 | not planned |
| [pi#6546](https://github.com/earendil-works/pi/issues/6546) | 解决老 Linux 上的 glibc 版本不匹配 | not planned |
| [pi#6075](https://github.com/earendil-works/pi/issues/6075) | 启动太慢 | not planned |

三个互不相识的人分别要求 musl 构建、老 glibc 兼容和更轻的启动,上游把这三条
都关闭为 *not planned*。这对上游是合理的取舍 —— Pi 面向的是现代机器 —— 但老硬件
这一块就没人管了。**nanopi 正是为这个场景做的 Rust 重写版:**

- **musl 静态构建** —— 零运行时依赖,能在 Alpine 容器里跑
  (CI 矩阵见 [`release.yml`](https://github.com/ChrisZhangJin/nanopi/blob/main/.github/workflows/release.yml))
- **glibc 2.12+(CentOS 6)** —— 动态版覆盖老服务器,musl 版覆盖其余环境
- **~4 MB** —— Rust + LTO + `opt-level = "z"` + `panic = abort` + strip;
  发布的二进制经 UPX 压缩到 1.6 MB
- **预编译覆盖** `linux-x86_64`、`linux-x86_64-musl`、`linux-aarch64-musl`、
  `macos-aarch64`、`windows-x86_64`。每个 Linux target 另有一份编入插件
  运行时的 `-wasm` 变体

## 安装

### 预编译二进制

从 [Releases](https://github.com/ChrisZhangJin/nanopi/releases/latest) 下载：

```bash
# 替换 VERSION 为你想要的 tag（如 v0.9.1）
VERSION=v0.9.1
curl -L -o nanopi \
  "https://github.com/ChrisZhangJin/nanopi/releases/download/${VERSION}/nanopi-${VERSION}-linux-x86_64-musl"
chmod +x nanopi
./nanopi --version
```

每个 release 提供预编译二进制：
- `nanopi-<ver>-linux-x86_64-musl` —— 全静态 Linux，跑在任何地方（推荐）
- `nanopi-<ver>-linux-x86_64` —— 动态 glibc Linux，体积稍小
- `nanopi-<ver>-linux-aarch64-musl` —— 全静态 arm64：树莓派、arm64 服务器，
  以及 Termux 下的 Android
- `nanopi-<ver>-macos-aarch64` —— Apple Silicon（M1+）
- `nanopi-<ver>-windows-x86_64.exe` —— Windows 10/11

以上每个 Linux 产物都另有一份 `-wasm` 双胞胎（如
`nanopi-<ver>-linux-x86_64-musl-wasm`），用 `--features wasm` 构建。只有要跑
`[[extensions]]` WASM 插件时才需要它 —— 它内含 wasmtime 运行时，约 7.2 MiB，
而标准版约 4 MB。

macOS 和 Windows 只发标准版；要插件支持请自行编译 `cargo build --release --features wasm`。

macOS Intel 不预编译（GitHub 的 Intel Mac runner 供给紧俏）；有需要自己编：`cargo build --target x86_64-apple-darwin`。

macOS 下载的二进制没有签名，会被 Gatekeeper 拦住。清掉 quarantine 属性并
临时签名即可：

```bash
xattr -d com.apple.quarantine ./nanopi-<ver>-macos-aarch64 2>/dev/null || true
codesign --force --sign - ./nanopi-<ver>-macos-aarch64
```

### 从源码编译

```bash
# 一次性环境准备
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
source "$HOME/.cargo/env"
rustup target add x86_64-unknown-linux-musl
sudo apt install -y musl-tools build-essential   # Debian/Ubuntu

# 编译
cargo build --release --target x86_64-unknown-linux-musl
./target/x86_64-unknown-linux-musl/release/nanopi --version
```

## 快速上手

```bash
export OPENAI_API_KEY=sk-...
export OPENAI_BASE_URL=https://api.deepseek.com/v1
export OPENAI_MODEL=deepseek-v4-flash

# 交互式 TUI（默认）
nanopi

# 一次性 -p 模式（Claude Code 语义）
nanopi -p "读一下 /etc/hostname 告诉我内容"

# JSON 输出，便于脚本调用
nanopi -p --output json "say hi"

# 从 stdin 读取 prompt
echo "explain this error" | nanopi -p

# 恢复：最近一次会话 / 指定 id / fork
nanopi --continue
nanopi --session <id>
nanopi --fork <id>
```

## 命令行参数

| 参数 | 默认值 | 用途 |
|---|---|---|
| `--base-url` | `https://api.openai.com/v1` | OpenAI 兼容 API 根地址 |
| `--model` | （必填） | 模型 id |
| `--api-key` | `$OPENAI_API_KEY` | Bearer token |
| `-m`, `--message` | （管道 stdin） | 用户消息；第一个位置参数也可。`-p` 模式下可从管道 stdin 读取 |
| `-p`, `--print` | false | 非交互模式 |
| `--output` | `text` | `-p` 输出格式：`text` \| `json` |
| `--continue` | false | 恢复最近一次会话 |
| `--session <id>` | — | 按会话 id 恢复 |
| `--fork <id>` | — | 从现有会话 fork |
| `--no-hooks` | false | 关闭所有 hooks |
| `-a`, `--approve` | false | 本次运行信任项目资源 |
| `-N`, `--distrust` | false | 本次运行不信任项目资源 |
| `--skill <path>` | — | 加载指定 skill 文件/目录（可重复） |
| `-S`, `--no-skills` | false | 关闭 skill 发现 |
| `-C`, `--no-context-files` | false | 关闭 AGENTS.md / CLAUDE.md 自动发现 |
| `--system-prompt <文本\|路径>` | — | 替换内置 system prompt |
| `--append-system-prompt <文本\|路径>` | — | 追加到 system prompt 末尾（可重复） |

## Skills

Nanopi 实现了 [Agent Skills 规范](https://agentskills.io/specification)。往 `~/.nanopi/skills/<name>/` 里丢一个 `SKILL.md`：

```markdown
---
name: greet
description: 亲切地和用户打招呼。用于问候场景。
---
只说 "hi, friend"，别的都不要说。
```

显式调用，或者让模型通过系统 prompt 里自动附加的 `<available_skills>` 列表自行发现：

```bash
/skill:greet             # 把 SKILL.md 展开进消息
/skill:greet in french   # 额外参数会追加到后面
```

**加载位置**（同名以先加载为准）：
- 用户级：`~/.nanopi/skills/`
- 项目级：`<cwd>/.nanopi/skills/`（需通过 `-a` 或已持久化的信任决策授权）
- 命令行：`--skill <path>`（文件或目录；即便 `--no-skills` 也会加载）

## 自定义系统提示

`--system-prompt <文本|路径>` 会替换内置的 identity/guidelines 提示；`--append-system-prompt <文本|路径>`（可重复，多个值之间用一个空行连接）会在它后面追加内容。两个参数都接受字面文本**或**指向已有文件的路径。一旦指定任一参数，对应的文件自动发现就完全关闭 —— 不做任何合并。

不带这些参数时，nanopi 会自动发现：
- `<cwd>/.nanopi/SYSTEM.md`（仅当项目已被 `-a` 或持久化的信任决策授权），否则读 `~/.nanopi/SYSTEM.md` —— 对应 `--system-prompt`。
- `<cwd>/.nanopi/APPEND_SYSTEM.md`（同样的信任规则），否则读 `~/.nanopi/APPEND_SYSTEM.md` —— 对应 `--append-system-prompt`。

项目级覆盖全局级；全局文件不需要信任门（这是你自己的机器，不是克隆下来的仓库）。Context 文件、skills 和「Current working directory: …」这一行仍会在自定义提示之上叠加 —— 只替换 identity/tools/guidelines 那一段。注意：被替换的提示会丢掉自动生成的「Available tools: …」行，部分模型在这一行缺失时会跳过工具调用，所以请在提示里明确写出你期望模型使用的工具。

## Hooks

nanopi 有两套扩展机制，从 v0.12.0 起两者都能看到全部生命周期事件 —— 区别在于拿到事件之后能做什么：

| | 加工具 | 加命令 | 能看事件 | 能否决 / 改写 | 能保状态 | 每次触发的开销 |
|---|---|---|---|---|---|---|
| shell hooks | ✗ | ✗ | ✓（11 个） | ✓ | ✗ —— 每次都是新进程 | fork + exec |
| WASM 插件 | ✓ | ✓ | ✓（11 个，选择性加入，只能观察） | ✗ | ✓ —— `Store` 常驻 | 一次函数调用 |

一句话说清楚：**WASM 插件只能观察，shell hooks 能拒绝** —— 为什么 nanopi 保留两套而不像 PI 那样合并成一个 `ExtensionAPI`，见 [`docs/v0.12-events.md`](https://github.com/ChrisZhangJin/nanopi/blob/main/docs/v0.12-events.md) 第 8 节。

Shell hooks 会在工具调用前后触发，沿用 Claude Code 的 hook *协议*（JSON 走 stdin、退出码 2 表示拒绝、`tool_name` / `tool_input` / `hookSpecificOutput` 等字段）—— 但事件*名字*用的是 PI 的命名，不是 Claude Code 的。配置写在 `~/.nanopi/settings.toml`：

```toml
[[hooks.tool_execution_start]]
matcher = "^bash$"
command = "logger 'nanopi 即将执行 shell'"
```

TOML key 是 snake_case（`tool_execution_start`，不是 `ToolExecutionStart`）。`matcher` 是正则，具体拿什么去匹配取决于事件：工具名、session id、turn 号，或压缩原因。`input` 是唯一一个没有东西可匹配的事件 —— 用户消息不是工具调用 —— 所以那里的 `matcher` 只能是 `"*"`，写别的会在启动时直接报错并说明原因，而不是加载成功却永远不触发。完整协议见 [`docs/v0.5-research.md`](https://github.com/ChrisZhangJin/nanopi/blob/main/docs/v0.5-research.md) 第 6 节。

### v0.12 改名

nanopi 早期借用了 Claude Code 对四个钩子的命名。v0.12 把它们改成 PI 的命名，没有别名、也没有过渡期 —— 配置里如果还用左边的旧 key，启动时直接报错加载失败，错误信息会指出右边的新 key：

| 旧 key（已停用 —— 硬错误） | 新 key |
|---|---|
| `pre_tool_use` | `tool_execution_start` |
| `post_tool_use` | `tool_execution_end` |
| `user_prompt_submit` | `input` |
| `session_end` | `session_shutdown` |

### 生命周期事件（v0.11.0）

除了上面这几个走 Claude Code 协议的钩子，nanopi 还多四个对齐 Pi `before_agent_start` / `turn_start` / `turn_end` / `message_end` 的生命周期钩子：

| Hook key | 触发时机 | 可阻断？ |
|---|---|---|
| `before_agent_start` | 每个 turn 一次，compaction 之后、用户消息进入 context 之前 | 是（提前返回并写入合成消息）|
| `turn_start` | agent 循环每次迭代的开头 | 否（仅通知）|
| `turn_end` | agent 循环每次迭代的末尾 | 否（仅通知）|
| `message_end` | for 循环结束后触发一次 | 否（仅通知）|

四个 hook 的 `matcher` 都对 turn_count 字符串匹配（`^1$` 只匹配第一个 turn），stdin 收到 `{ "turn_count": N, ... }` 加事件特定字段。完整示例在 [`config.toml.example`](https://github.com/ChrisZhangJin/nanopi/blob/main/config.toml.example)。

另外两个在上下文压缩前后触发：`session_before_compact` 和 `session_compact`。都是仅通知，`matcher` 对压缩原因字符串匹配（`threshold` 或 `manual`）。

## WASM 扩展（v0.11.0）

Shell hook 能观察、能否决，但没法给模型加一个可调用的新工具。扩展可以。nanopi 的扩展是一个 WebAssembly 组件 —— 用 Rust、Go、C 或任何能编译到 WASM 的语言写 —— 它导出的工具会和 `bash`、`read` 并列出现在模型的工具列表里。

**这是编译期可选项。** 官方发布的二进制不含 WASM 运行时，所以保持在 ~4 MB；`[[extensions]]` 配置会被忽略并在 stderr 警告。要用的话：

```bash
cargo build --release --features wasm
```

然后在 `config.toml` 里声明组件：

```toml
[[extensions]]
path = "~/.nanopi/extensions/my-tool.wasm"
```

插件需要导出两个函数，另有两个可选（见 [`wit/nanopi-extension.wit`](https://github.com/ChrisZhangJin/nanopi/blob/main/wit/nanopi-extension.wit)）：

| 导出 | 签名 | 用途 |
|---|---|---|
| `list-tools` | `() -> string` | 返回 `{name, description, parameters}` 的 JSON 数组。加载时调用一次。`parameters` 是 JSON Schema，原样交给模型。 |
| `execute-tool` | `(name: string, args-json: string) -> string` | 执行工具，返回 `{"content": "...", "is_error": false}`。 |
| `list-commands` | `() -> string` | *可选。* 返回 `{name, description}` 的 JSON 数组 —— 用户可以敲的 slash 命令。 |
| `execute-command` | `(name: string, args: string) -> string` | *可选。* 执行命令，返回 `{"print": "..."}`、`{"send_user_message": "..."}`、`{"error": "..."}` 三者之一。 |

**工具 vs 命令。** 工具是*模型*决定调用的；命令是*你*敲的。命令出现在 `/` 面板里并标注来源插件，且仅限交互模式 —— `nanopi -p` 没有命令面板，但插件的工具在那里照常可用。

**查看到底加载了什么。** `/tools` 列出模型真正能调用的每个工具，标注 `[builtin]` 或 `[plugin:<名字>]` 并给出来源 `.wasm` 路径。它读的是实时注册表 —— 和交给模型的是同一份 —— 所以插件工具看起来没生效时，先看这里。让模型自己列工具不能替代：它分不清插件工具和内置工具，只会猜。

`print` 直接写进你的 scrollback：模型看不到，也不进会话记录。`send_user_message` 会像你自己输入一样开启一轮对话 —— 但**总是先原样回显**，插件无法在你不知情的情况下替你说话；流式输出中途调用则转为转向（steer）当前这轮，和你自己打字的行为完全一致。`error` 只展示给你，和 trap 一样绝不转发给模型。

两个命令导出放在第二个 WIT world `extension-commands` 里，它 `include` 了第一个。只提供工具的插件继续用 `extension`，源码一行都不用改 —— WIT 无法表达「可选导出」，直接扩宽原来的 world 会让所有已有插件的*源码*编译不过，尽管宿主仍然能加载它们编译好的*二进制*。

**订阅生命周期事件（v0.12.0）。** 第三个、同样是可选加入的 WIT world `extension-events`，`include` 了 `extension-commands`，再加两个导出：

| 导出 | 签名 | 用途 |
|---|---|---|
| `list-events` | `() -> string` | *可选。* 插件想观察的 PI 事件名称，JSON 数组。 |
| `handle-event` | `(event: string, payload-json: string) -> string` | *可选。* 每当某个事件同时满足「这里请求过」和「config 授予过」，就会被调用一次。返回值被忽略 —— 这是纯观察，插件无法通过它否决或改写任何东西。 |

投递需要**两份名单同时点头**：插件的 `list-events` 导出，加上 config 里 `[[extensions]].events` 的授权（完整语法见 `config.toml.example`）。任何一边缺席都不会投递 —— 如果插件请求了但 config 没授权，加载时会报出来，所以「看起来该收到事件却没收到」的插件能知道原因。`handle-event` 拿到的 payload 和 shell hook 在 stdin 上收到的是逐字节相同的 —— 同一份 `HookInput` JSON，同一个构造函数。

这个授权比看起来的要重：`input` 订阅者能看到每一条原始 prompt，`tool_execution_start` 订阅者能看到每次工具调用的全部参数 —— 比 `allow_fs` 的授权面还大。同一个插件如果同时打开 `events` 和 `allow_network = true`，启动时会警告，因为这等于给了它一条能把这些事件内容外泄出去的通道。**不要在事件处理器里发起网络请求** —— `handle-event` 里仍然能调用 `host-http-get`，但一次慢请求或恶意请求的耗时上限是它自己的 10 秒超时叠加下面的事件预算，而不是 epoch 中断能管的范围（epoch 插桩没法打断正在执行的宿主函数）。

`handle-event` 里的 guest 代码只有 2 秒的墙钟预算 —— 比工具调用的 30 秒紧得多，因为事件处理器就卡在每个 turn 的关键路径上，触发频率也高得多。投递策略是**忙则丢弃，绝不阻塞**：如果某个插件的 `Store` 正被一次工具调用占用，这一刻的事件会被直接丢弃，不会排队，所以一个卡住的插件永远不会拖慢整个 agent 循环的其他投递。丢弃的事件按插件计数并写日志。`/tools` 会在「Watching events」这个小节里列出当前订阅了事件的每个插件 —— 同一份清单，原本回答「模型能调用什么」，现在也回答「谁在盯着我」。

可以导入这些宿主函数：

| 导入 | 签名 | 门控 | 用途 |
|---|---|---|---|
| `host-log` | `(level: u8, message: string)` | 始终可用 | 写 nanopi 的 stderr。`0`=trace `1`=info `2`=warn `3`=error。 |
| `host-fs-read` | `(path: string) -> string` | `allow_fs` | 读工作目录内的 UTF-8 文件。返回内容，或以 `error: ` 开头的字符串。 |
| `host-http-get` | `(url: string) -> string` | `allow_network` + `url_allowlist` | 抓取 `http`/`https` URL。返回响应体，或以 `error: ` 开头的字符串。 |
| `host-store-get` | `(key: string) -> string` | `allow_store` | 读该插件在 `key` 下存的值。键不存在时读到 `""`。 |
| `host-store-set` | `(key: string, value: string) -> string` | `allow_store` | 替换该插件在 `key` 下的值。字节落到文件系统后返回 `""`，否则返回以 `error: ` 开头的字符串。 |
| `host-notify` | `(text: string) -> string` | 始终可用 | 往用户的 scrollback 里写一行，前缀由宿主用该插件的名字加上。按轮次限流。 |
| `host-set-context` | `(text: string) -> string` | `allow_context` | 声明一段要进入模型上下文的文字，并标注来自该插件。每次调用**替换**该插件上一次的贡献；传 `""` 表示清除。返回 `""`，否则返回以 `error: ` 开头的字符串。 |
| `host-call-tool` | `(name: string, args-json: string) -> string` | `allow_tools`，逐个工具点名 | 调用 nanopi 的内置工具。真的跑起来的调用返回 `{"content": "...", "is_error": false}`；没跑起来的返回以 `error: ` 开头的裸字符串。 |
| `host-send-user-message` | `(text: string) -> string` | `allow_send_message` | 用这段文字开启或插话一轮对话，效果等同于你自己敲进去。永远会原样回显给你，并标注是哪个插件。返回 `""`，否则返回以 `error: ` 开头的字符串。 |

数据跨边界用 JSON 字符串而不是 WIT record —— 只用一种原始类型，ABI 就小到两边都不需要 codegen 步骤。

完整可运行的例子在 [`examples/wasm-plugin/`](https://github.com/ChrisZhangJin/nanopi/tree/main/examples/wasm-plugin)，含编译命令。[`examples/wasm-plugin-minimal/`](https://github.com/ChrisZhangJin/nanopi/tree/main/examples/wasm-plugin-minimal) 是更小的可直接复制的骨架 —— 两个工具，分成"样板"和"你要替换的部分"两段。

两个**能直接用**而不只是拿来读的插件：[`examples/wasm-plugin-memory/`](https://github.com/ChrisZhangJin/nanopi/tree/main/examples/wasm-plugin-memory) 是持久化的项目记忆 —— 每条记忆是 `.nanopi/memory/` 下一个 markdown 文件，可 diff 可提交；注入模型上下文的只有那份精简索引，全文由模型按需调 `recall` 取。[`examples/wasm-plugin-report/`](https://github.com/ChrisZhangJin/nanopi/tree/main/examples/wasm-plugin-report) 跨会话记录每个工具的耗时与失败，用来回答「刚才为什么慢」—— `tool_execution_end` 带的那个 `duration_ms` 别处都不持久化。

五个插件在 [`examples/README.md`](https://github.com/ChrisZhangJin/nanopi/tree/main/examples) 里有总表 —— 每个插件做什么、需要哪些 grant、目标是哪个 WIT world。

写插件、调试插件、能力门的分步指南在 [wiki](https://github.com/ChrisZhangJin/nanopi/wiki)（中英双语）。

**沙箱。** 组件跑在 wasmtime 里，没有环境权限 —— 插件只能通过你显式开启的宿主函数接触外部。

`host-fs-read` 由 `allow_fs = true` 门控，且路径必须解析到工作目录**内部**。检查前会先 canonicalize，所以 `../` 穿越和指向外部的符号链接都会被拒。（内置 `read` 工具刻意没有这道检查，理由是模型反正能 shell out —— 但插件没有 shell，所以这里的边界是真约束而非安全剧场。）

`host-notify` 不设门控，因为它是输出而不是访问 —— 它碰不到任何东西，也不留下任何东西。它唯一能造成的问题是淹没你的注意力，所以约束它的是限流而不是权限门：超出每轮的额度后，多出来的行会被丢弃，并由一行 `… N more suppressed` 说明丢了多少；被丢弃的那几次调用返回的是 `error: ` 字符串，所以插件不会以为自己说了话其实没说。前缀由宿主根据 `.wasm` 文件名主干加上，payload 永远不会被拿去解析前缀，所以一个插件无法冒充另一个。它和 `host-log` 的区别在于文字最终落在哪里：`host-log` 写的是裸 stderr，会落进 TUI 管理的区域并被下一次重绘擦掉，而 `host-notify` 会滚进历史里留下来。

`host-store-get` / `host-store-set` 由 `allow_store = true` 门控，而且它们收的是**键，不是路径** —— 插件给出一个 map 的键，落到哪个文件完全由宿主决定（`~/.nanopi/extensions/<stem>/store.json`，每个插件一个 JSON 对象）。所以上面那套路径约束在这里既不适用也不需要：插件手上根本没有一条能指向外部的路径。上限是总共 1 MiB、1000 个键、键最长 128 字符；每一次拒绝都在带内返回，并且什么都不写入 —— 不会出现「告诉插件失败了，值却还是进去了」。`host-store-set` 返回 `""` 意味着字节已经到了文件系统：宿主用临时文件加 rename 提交，所以崩溃之后留下的要么是完整的旧文件、要么是完整的新文件，不会是撕裂的半个。两个插件的 `.wasm` 文件名主干相同、且其中任一开了 `allow_store` 时，两个都会加载失败，而不是悄悄共用一个 store。

`host-set-context` 由 `allow_context = true` 门控。它是唯一一个改变**智能体所相信的东西**、而不是插件所知道的东西的 import：插件声明的文字会在每一轮开始时被折进 system prompt。归属头部 —— `[context contributed by extension "memory"]` —— 由宿主写入，因为没有它，模型就分不清插件注入的指令和你自己的指令。每次调用是**替换**而不是追加，所以它是幂等状态，十轮也不会攒出十份；传 `""` 清除。上限是每个插件 4 KiB，按字节算，而且刻意定得小：这段文字在这个会话余下的时间里会进入**每一个**请求。超限的调用在带内被拒，并且**之前那份贡献依然有效** —— 被告知「不行」不会顺带把你已有的东西弄丢。每一次变更都会在你的 scrollback 里公示并点明是哪个插件 —— 否则这份贡献是不可见的，因为你从来看不到 system prompt —— 而且这条公示与插件自己的 `host-notify` 额度**分开记账**，所以插件没法先用噪声把额度耗光、再把它埋掉。与 `allow_network = true` 组合是 nanopi 目前最锋利的一对权限，启动时会告警：那意味着一个远端来源可以塑造智能体的行为。

`host-call-tool` 是**逐个工具**门控的：`allow_tools` 里必须点到这个工具的名字。默认为空，空即拒绝一切，和 `url_allowlist` 同一条规则。之所以按工具而不是按插件，是因为这些工具并不等价：`allow_tools = ["find", "read"]` 让一个索引插件能遍历、能读，但不能写也不能执行；而 `allow_tools = ["bash"]` 就是任意命令执行，它会直接绕过 `allow_fs` 的工作目录约束和 `url_allowlist` 的逐 host 审批 —— 启动时 nanopi 看到 `bash` 会打一条升级过的告警，因为这个权限一给，同一个插件上其他所有权限都成了装饰。只能调内置工具：插件不能调另一个插件的工具，而不属于内置的名字是**加载错误**并列出合法的名字，不是悄悄的空操作。返回形状是这份契约里诚实的那一半：JSON 帧意味着调用真的跑了（包括跑了但失败的），裸的 `error: ` 意味着它根本没发生。没有任何情况会 trap。每次调用上限 30 秒 —— epoch 预算约束的是 guest 代码，抢占不了一个已经在执行的宿主函数 —— 而超时后被丢下的 `bash` 子进程可能比这个期限活得更久，这是一个已知限制，不是一句承诺。你的 `[[hooks.*]]` 照样会跑，所以插件的位阶并不高于你自己的策略；被拦下时返回 `error: blocked by hook: <reason>`。每一次真的跑起来的调用都会在你的 scrollback 里公示，点明是哪个插件调了哪个工具，而且走宿主自己的额度，所以 `host-notify` 的洪水埋不掉它 —— 这是必需的，因为插件的调用**刻意不进**会话记录（模型从未发出过的 `tool_call` 条目，正是当年让会话无法恢复的那个形状），这也意味着 `--continue` 不会显示插件做过什么。`/tools` 会在「Plugin grants」小节里列出每个已加载插件手上有什么；一个什么权限都没拿到的插件也照样有一行，写着 `no grants`。

`host-send-user-message` 由 `allow_send_message = true` 门控，它是 nanopi 唯一一个**花你钱**的能力。其他每一个权限，给的是让插件知道点什么、改变智能体所相信的东西、或者跑一个你自己本来也能跑的工具；这一个则会产生要向你计费的对话轮次。它是唯一一个**单独**就会在启动时告警的权限，不需要搭配第二个权限才够锋利。如果这时有一轮正在跑，这段文字会插进去，在下一个迭代边界作为一条新的用户消息到达 —— 和你自己在流式输出中途敲进去的那一行走同一条路径，这是刻意的：插件不该拿到一套和 `b90b27f` 修好的那套并行的机制。如果没有在跑的轮次，它会开启下一轮，但排在你已经敲下的任何一行**后面**：你亲眼看着落地的那一行，位阶高于软件替你决定要花的预算。它不会被丢掉。

三重边界，全部公示，没有一条是静默的。每个插件同时只能有一条未被消费的消息，而这个标记是在文字真正进入某一轮时才清除的，不是调用返回时。插件不能在自己消息发起的那一轮里再发 —— 按插件算而不是全局，所以一个审计插件仍然能在规则插件发起的那一轮里说话。以及每个插件每个会话最多 20 轮 —— 这一条是规范原本的两条规则没有给出的：一个 `turn_end` 订阅者每轮发一次，可以永远同时满足那两条，因为每一轮的来源都是**新**的一轮，而每条消息又都在下一条之前被消费掉了。这个上限是拦住失控订阅者的兜底，不是预算，而且它能挺过插件的 trap —— 否则 trap 就成了插件再买二十轮的办法。

这段文字永远会原样回显给你并标注来源，没有安静的路径：宿主接受的你会看到，宿主拒绝的插件会在带内拿到一条写明原因的回复。在 `nanopi -p` 下没有可以抵达的轮次循环，所以这个调用是**被拒绝**，而不是被悄悄丢掉 —— 插件绝不该以为自己发出了实际没发出的东西。

在受限网络环境下这一条尤其值得留意：一个既有 `allow_send_message` 又有 `allow_network` 的插件，能把远端拿到的内容直接变成向你计费的对话轮次。如果远端要走代理才通，那条链路上的任何一次超时或重试，都可能变成一轮你没打算花的钱。

`host-http-get` 有两道门：先是 `allow_network = true`，然后 URL 的 host 必须匹配 `url_allowlist`。**空 allowlist 拒绝一切**，所以只把开关打开本身还是什么都访问不到。匹配比对的是解析出的 host 而不是子串 —— allowlist 为 `api.github.com` 时，`https://evil.com/?x=api.github.com` 和 `https://api.github.com@evil.com/` 都会被拒。

entry 是**模式**，不是主机名 —— 一个「抓模型给的任意 URL」的插件根本没有有限的 host 清单可枚举：

| entry | 匹配 |
|---|---|
| `github.com` | 该 host **及其**子域名，任意端口 |
| `*.github.com` | 仅子域名 —— 顶级域 `github.com` 本身会被拒 |
| `*` | 任意 `http`/`https` host |

`*` 是逃生门，而且是真的门：它等于关掉第二道闸，只剩 `allow_network` 一道检查 —— 包括 link-local 元数据端点。只要开着网络又出现 `*`，nanopi 会在启动时打印点名到插件的警告。星号出现在别的位置（`api.*.com`）会被**拒绝**而不是放宽，这样一个笔误不会悄悄把闸门开大。`*` 只放宽 host；scheme 检查是独立的，所以任何模式下 `file://` 都进不了网络能力。只接受 `http`/`https`；请求 10 秒超时，插件因此没法把一轮对话挂死；重定向**不会**跟随 —— 否则一个 3xx 就能把这次抓取带到你从没批准过的 host 上。拒绝和失败都以 `error: ` 前缀的字符串在带内返回给插件，而不是 trap。插件里的 trap 会作为失败的工具调用报给模型 —— 不会拖垮 nanopi；加载失败的 `.wasm` 会被跳过并警告，不阻塞启动。

**失控插件。** 每次工具调用，guest 代码有约 30 秒的墙钟预算，由 wasmtime 的 epoch interruption 强制执行。超时即 trap，作为失败的工具调用报给模型，且插件仍然可用 —— 实例会被重建，一次坏调用不会让它在整个 session 里失效。没有这道闸，一个含死循环的插件会让 nanopi 永久卡死：guest 占着一个真实线程且没有让出点，<kbd>Esc</kbd> 够不到它里面。

这个预算**只作用于 guest 代码**。epoch interruption 是编译进 guest 的插桩，打断不了一个已经在跑的宿主函数 —— 插件阻塞在 `host-http-get` 或 `host-fs-read` 里时，兜底的是那两个函数各自的限制（10 秒请求超时；只接受常规文件加 1 MiB 上限，后者正是 FIFO 不会永久阻塞的原因），而不是 epoch deadline。所以单次调用的最坏情况是「预算 + 一次宿主调用」，不是只有预算。

**同名冲突。** 插件不能注册已存在的工具名。冲突会被报告并跳过，所以插件无法悄悄替换 `bash`。

命令更严格，两条规则确实不同。**工具**冲突是先到先得：已注册的留下，后来的被跳过。**命令**冲突则两边都拒绝 —— 如果两个插件都注册 `/deploy`，谁都拿不到，因为默默挑一个赢家意味着 `/deploy` 执行的是「碰巧先加载」的那个插件。命令名撞上 `/compact` 这类内置命令时同样跳过。每种情况都会打印点名到插件的警告，且不影响该插件的其他命令和任何工具。

插件在启动时加载，以及 `/new`、`/resume`、`/fork`、`/import` 时；从 v0.12.0 起 `/reload` 也会重新读取 `[[extensions]]`，热替换活跃的注册表。

**热重载。** `/reload` 先加载新的 `.wasm`，再注销旧的工具和命令，最后注册新的 —— 所以某个插件的文件如果已经加载不了，它当前正在跑的实例会被保留，`/reload` 会用红字点名说明，而不是让你既没有旧插件也没有新插件。它会汇报做了什么：重载了几个插件、带来多少工具和命令、哪些插件已不在配置里（已注销）、哪些加载失败。授权会从配置重新读取，所以撤销 `allow_network` 在 `/reload` 时即生效；加载失败的插件保留旧的授权行，因为那一行描述的是实际还在运行的东西。

重载落地时已经在飞行中的工具调用会被**拒绝，既不执行也不悄悄换人**：插件的旧实例会认出自己已被替换，返回一个 `error:` 字符串，点名重载是原因 —— 调用开始前检查一次，guest 返回之后再检查一次。真正起作用的是第二次检查：你绝不会被告知某次调用成功了，而它的结果其实来自已经被替换掉的代码。调用已经产生的副作用（写下的文件、发出的 HTTP 请求）依然成立，拒绝信息里会这么说。事件根本不会投递给被替换的实例。

宿主侧的插件状态刻意在重载中存活：插件的 host-store 文件（它们在磁盘上，重载不是恢复出厂设置），以及 `plugin_send` 的循环保护和已经花掉的每会话消息预算（否则 `/reload` 就等于退款，插件可以靠重载自己绕开自己的上限）。唯一的例外是插件对系统提示词的贡献：如果插件已从配置中消失，或失去了 `allow_context`，这份贡献会被丢弃 —— 留着它等于在提示词里放一段没有任何活着的东西能为之负责的文本。

## 版本

| 版本 | 状态 | 体积 | 说明 |
|---|---|---|---|
| **v0.11.0** | 当前 | ~1.6 MB | WASM 扩展，带门控的 `host-fs-read` / `host-http-get`，以及插件注册的 slash 命令；Pi 生命周期钩子（`before_agent_start`、`turn_start`、`turn_end`、`message_end`）；流式中途转向；可配置的工具执行模式 |
| v0.10.0 | 已发布 | 1.6 MB | 自定义 system prompt（`--system-prompt`、`SYSTEM.md`）；显式 `api_kind` 优先于 vendor 嗅探；`-p` 模式工具失败可读；发布产物 UPX 压缩 |
| v0.9.x | 已发布 | ~3.9 MB | 首次运行向导，`/settings` + `/keybindings`，8 个 vendor 分发，重试封装（0.9.2–0.9.3）；v0.9.1 修了 v0.9.0 的 tool-loop bug |
| v0.9.0 | 已发布 | ~4.0 MB | Skills（PI-parity），`--skill`/`--no-skills`，折叠 TUI 卡片，`UserPromptSubmit` hook |
| v0.8.x | 已发布 | ~3.9 MB | 完整 ratatui TUI、`/fork`、`--continue`/`--session`、hooks、JSONL 会话 |
| v0.5.0 | 已发布 | ~3.0 MB | 工具（read/write/edit/bash）、`-p` 模式、JSON 输出、hooks |
| v0.1.0 | 已发布 | 2.4 MB | 单文件 OpenAI 流式演示（保留为 `nanopi_v0_1` 二进制） |

体积都是发布的 musl 产物。从 v0.10.0 起，这个产物经过 UPX 压缩（`make`），所以 1.6 MB
无法和上面几行未压缩的数字直接对比 —— 同一次编译在压缩前是 4.4 MB。v0.11.0 那个数字
是近似值：它测自开发构建，而非已发布的 tag。

## 路线图

不列特性清单。nanopi 是 Pi 的 Rust 轻量实现：目标是用一个小体积静态二进制承载
Pi 的核心能力，而不是把 Pi 做的每件事都对齐。一个特性值不值得进来，看它配不配
得上它增加的体积。

已知缺口：Linux aarch64 暂未加入 CI 矩阵 —— 需自行构建
`cargo build --release --target aarch64-unknown-linux-musl`（见上方）。

## Cargo 国内镜像

写入 `~/.cargo/config.toml`：

```toml
[source.crates-io]
replace-with = "rsproxy-sparse"

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"

[target.x86_64-unknown-linux-musl]
linker = "musl-gcc"
```

## 设计笔记

- **musl + LTO + panic=abort + strip** → 小巧的静态产物；rustls 避开了 OpenSSL 依赖
- **手写 SSE 解析器** —— 不引入 `reqwest-eventsource`，依赖树更干净
- **JSONL 而非 JSON** —— append-only 文件在写入过程中崩溃也能存活
- **Provider 抽象** 在 v0.6 落地：原生 Anthropic + 任意 OpenAI 兼容端点

设计与实现笔记见 [`docs/v0.5-research.md`](https://github.com/ChrisZhangJin/nanopi/blob/main/docs/v0.5-research.md) 与 [`docs/PLAN.md`](https://github.com/ChrisZhangJin/nanopi/blob/main/docs/PLAN.md)；[`docs/claims-and-races.md`](https://github.com/ChrisZhangJin/nanopi/blob/main/docs/claims-and-races.md) 规定 nanopi 可以就自己的行为声称什么，以及支撑这些规则的竞态表；[`docs/plugin-capabilities.md`](https://github.com/ChrisZhangJin/nanopi/blob/main/docs/plugin-capabilities.md) 规定插件的出站能力面，以及让沙箱依然值得的逐工具授权模型。

## 致谢

- [Pi](https://github.com/earendil-works/pi) —— nanopi 移植自这份 TypeScript agent
- [Claude Code](https://github.com/anthropics/claude-code) —— hook 协议、`-p` 模式、skills 规范
- [ratatui](https://github.com/ratatui-org/ratatui) 与 [crossterm](https://github.com/crossterm-rs/crossterm) —— TUI 底层

## 许可协议

[MIT](LICENSE) © Chris Zhang
