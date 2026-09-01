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
- 🔌 **多 provider** —— 任何 OpenAI 兼容端点（DeepSeek、ollama、vLLM …）以及原生 Anthropic
- 🛠 **流式工具调用** —— `read` / `write` / `edit` / `bash`，在 ratatui TUI 里实时渲染
- 🪝 **Claude Code 风格 hooks** —— `PreToolUse` / `PostToolUse` / `UserPromptSubmit` shell 钩子
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
- **预编译覆盖** `linux-x86_64`、`linux-x86_64-musl`、`macos-aarch64`、
  `windows-x86_64`。Linux ARM 暂无预编译,需自行编译:
  `cargo build --release --target aarch64-unknown-linux-musl`

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
- `nanopi-<ver>-macos-aarch64` —— Apple Silicon（M1+）
- `nanopi-<ver>-windows-x86_64.exe` —— Windows 10/11

macOS Intel 不预编译（GitHub 的 Intel Mac runner 供给紧俏）；有需要自己编：`cargo build --target x86_64-apple-darwin`。

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

Shell hooks 会在工具调用前后触发，对齐 Claude Code 的 `PreToolUse` / `PostToolUse` / `UserPromptSubmit` 协议。配置写在 `~/.nanopi/settings.toml`：

```toml
[[hooks.pre_tool_use]]
matcher = "^bash$"
command = "logger 'nanopi 即将执行 shell'"
```

TOML key 是 snake_case（`pre_tool_use`，不是 `PreToolUse`）。完整协议见 [`docs/v0.5-research.md`](https://github.com/ChrisZhangJin/nanopi/blob/main/docs/v0.5-research.md) 第 6 节。

## 版本

| 版本 | 状态 | 体积 | 说明 |
|---|---|---|---|
| **v0.10.0** | 当前 | 1.6 MB | 自定义 system prompt（`--system-prompt`、`SYSTEM.md`）；显式 `api_kind` 优先于 vendor 嗅探；`-p` 模式工具失败可读；发布产物 UPX 压缩 |
| v0.9.x | 已发布 | ~3.9 MB | 首次运行向导，`/settings` + `/keybindings`，8 个 vendor 分发，重试封装（0.9.2–0.9.3）；v0.9.1 修了 v0.9.0 的 tool-loop bug |
| v0.9.0 | 已发布 | ~4.0 MB | Skills（PI-parity），`--skill`/`--no-skills`，折叠 TUI 卡片，`UserPromptSubmit` hook |
| v0.8.x | 已发布 | ~3.9 MB | 完整 ratatui TUI、`/fork`、`--continue`/`--session`、hooks、JSONL 会话 |
| v0.5.0 | 已发布 | ~3.0 MB | 工具（read/write/edit/bash）、`-p` 模式、JSON 输出、hooks |
| v0.1.0 | 已发布 | 2.4 MB | 单文件 OpenAI 流式演示（保留为 `nanopi_v0_1` 二进制） |

体积都是发布的 musl 产物。从 v0.10.0 起，这个产物经过 UPX 压缩（`make`），所以 1.6 MB
无法和上面几行未压缩的数字直接对比 —— 同一次编译在压缩前是 4.4 MB。

## 路线图

- **v1.0** —— 完整 PI parity：themes、compaction、扩展系统
- Linux aarch64 暂未加入 CI 矩阵（背景段落里有自编指令：见上方）

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

设计与实现笔记见 [`docs/v0.5-research.md`](https://github.com/ChrisZhangJin/nanopi/blob/main/docs/v0.5-research.md) 与 [`docs/PLAN.md`](https://github.com/ChrisZhangJin/nanopi/blob/main/docs/PLAN.md)。

## 致谢

- [Pi](https://github.com/earendil-works/pi) —— nanopi 移植自这份 TypeScript agent
- [Claude Code](https://github.com/anthropics/claude-code) —— hook 协议、`-p` 模式、skills 规范
- [ratatui](https://github.com/ratatui-org/ratatui) 与 [crossterm](https://github.com/crossterm-rs/crossterm) —— TUI 底层

## 许可协议

[MIT](LICENSE) © Chris Zhang
