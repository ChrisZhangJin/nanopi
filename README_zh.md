<div align="center">

# nanopi

**[Pi](https://github.com/earendil-works/pi) 的 Rust 迷你移植 —— 一个 ~4 MB 的编码 Agent CLI，专为老旧 / 低配 Linux 主机而生。**

[![Release](https://img.shields.io/github/v/release/ChrisZhangJin/nanopi?style=flat-square&color=blue)](https://github.com/ChrisZhangJin/nanopi/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/ChrisZhangJin/nanopi/total?style=flat-square)](https://github.com/ChrisZhangJin/nanopi/releases)
[![Stars](https://img.shields.io/github/stars/ChrisZhangJin/nanopi?style=flat-square)](https://github.com/ChrisZhangJin/nanopi/stargazers)
[![License](https://img.shields.io/badge/license-MIT-blue.svg?style=flat-square)](LICENSE)
![Binary](https://img.shields.io/badge/binary-~4%20MB-brightgreen?style=flat-square)
![Rust](https://img.shields.io/badge/rust-stable-orange?style=flat-square&logo=rust&logoColor=white)
![Platform](https://img.shields.io/badge/platform-linux%20%7C%20macOS%20%7C%20windows-lightgrey?style=flat-square)
![Static musl](https://img.shields.io/badge/static-musl-informational?style=flat-square)
[![CI](https://img.shields.io/github/actions/workflow/status/ChrisZhangJin/nanopi/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/ChrisZhangJin/nanopi/actions/workflows/ci.yml)

[English](README.md) · **简体中文**

<br>

<img src="img/tui.png" alt="nanopi TUI 截图" width="760">

</div>

---

## 为什么用 nanopi？

- 🪶 **~4 MB 静态二进制** —— musl + LTO + strip，零运行时依赖
- 🖥 **能塞进老机器** —— glibc 2.12+（CentOS 6）或纯静态 musl
- 🧬 **PI-parity** —— 对齐 [Pi](https://github.com/earendil-works/pi) 的用户界面：JSONL 会话、hooks、skills、`-p`、`/fork`、`/resume`
- 🔌 **多 provider** —— 任何 OpenAI 兼容端点（DeepSeek、ollama、vLLM …）以及原生 Anthropic
- 🛠 **流式工具调用** —— `read` / `write` / `edit` / `bash`，在 ratatui TUI 里实时渲染
- 🪝 **Claude Code 风格 hooks** —— `PreToolUse` / `PostToolUse` / `UserPromptSubmit` shell 钩子
- 🧠 **Agent Skills** —— 遵循 [官方规范](https://agentskills.io/specification) 的 `SKILL.md` 发现 + `/skill:name` 展开

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
| `-m`, `--message` | （stdin） | 用户消息；第一个位置参数也可 |
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

## Hooks

Shell hooks 会在工具调用前后触发，对齐 Claude Code 的 `PreToolUse` / `PostToolUse` / `UserPromptSubmit` 协议。配置写在 `~/.nanopi/settings.toml`：

```toml
[[hooks.pre_tool_use]]
matcher = "^bash$"
command = "logger 'nanopi 即将执行 shell'"
```

TOML key 是 snake_case（`pre_tool_use`，不是 `PreToolUse`）。完整协议见 [`docs/v0.5-research.md`](docs/v0.5-research.md) 第 6 节。

## 版本

| 版本 | 状态 | 体积 | 说明 |
|---|---|---|---|
| **v0.9.1** | 当前 | ~3.9 MB | 修 v0.9.0 tool-loop bug；`MAX_ITERATIONS` 从 16 提到 50 |
| v0.9.0 | 已发布 | ~4.0 MB | Skills（PI-parity），`--skill`/`--no-skills`，折叠 TUI 卡片，`UserPromptSubmit` hook |
| v0.8.x | 已发布 | ~3.9 MB | 完整 ratatui TUI、`/fork`、`--continue`/`--session`、hooks、JSONL 会话 |
| v0.5.0 | 已发布 | ~3.0 MB | 工具（read/write/edit/bash）、`-p` 模式、JSON 输出、hooks |
| v0.1.0 | 已发布 | 2.4 MB | 单文件 OpenAI 流式演示（保留为 `nanopi_v0_1` 二进制） |

## 路线图

- **v1.0** —— 完整 PI parity：themes、compaction、扩展系统
- Linux aarch64 预编译（当前只有 x86_64）

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

设计与实现笔记见 [`docs/v0.5-research.md`](docs/v0.5-research.md) 与 [`docs/PLAN.md`](docs/PLAN.md)。

## 致谢

- [Pi](https://github.com/earendil-works/pi) —— nanopi 移植自这份 TypeScript agent
- [Claude Code](https://github.com/anthropics/claude-code) —— hook 协议、`-p` 模式、skills 规范
- [ratatui](https://github.com/ratatui-org/ratatui) 与 [crossterm](https://github.com/crossterm-rs/crossterm) —— TUI 底层

## 许可协议

[MIT](LICENSE) © Chris Zhang
