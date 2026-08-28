# Pi vs nanopi — 差异全景与扩展机制分析

> 最后更新: 2026-08-27  
> 基于 Pi 上游 `packages/coding-agent/` (TypeScript) 与 nanopi `src/` (Rust) 的源码对照。

---

## 一、核心机制同构

两者共享同一套 Agent Loop 设计（见 `docs/v0.5-research.md §2`）:

1. **Tool trait** — 每个工具声明 `name + description + JSON Schema`，注册到 registry
2. **Context** — provider-agnostic 容器，携带 system prompt + messages + tools
3. **Agent Loop** — `stream_turn` → 检测 `finish_reason=ToolCalls` → 执行工具 → 把结果 push 回 context → 循环
4. **Provider 翻译** — 把 Context 翻成各自 wire format（OpenAI `chat/completions` / Anthropic `messages`）

AI「能调用工具」不是魔法，而是 LLM 训练时学了「看到 JSON Schema → 在结构化字段里输出对应 JSON」这条肌肉记忆，协议方（OpenAI/Anthropic）把它固化为 wire 上的结构化字段，agent 框架只是做类型翻译 + 本地执行 + 结果回喂。

---

## 二、差异全景表

### 2.1 工具定义与执行

| 维度 | Pi (TypeScript) | nanopi (Rust) |
|------|----------------|---------------|
| 工具接口 | `AgentTool<TSchema>` 泛型 + TypeBox schema | `trait Tool` + `ToolSpec { parameters: Value }` |
| Schema 格式 | TypeBox（编译时类型安全） | serde_json::Value（运行时 JSON Schema） |
| 执行模式 | 可配置并行/串行，per-tool + 全局 | 总是并行（`join_all`） |
| 工具名规整 | `toClaudeCodeName` / `fromClaudeCodeName`（OAuth 场景） | `canonical_name()`: 小写 + 去 `_tool` 后缀 |
| 流式 args 解析 | 每个 `content_block_delta` 增量调 `parseStreamingJson()` | 累积 `args_buf` 字符串，`content_block_stop` 时一次性 parse |

### 2.2 Agent Loop

| 维度 | Pi | nanopi |
|------|-----|--------|
| 最大迭代 | 无硬限，靠 `stopReason=stop` 自然结束 | `MAX_ITERATIONS = 50` + `STUCK_LIMIT = 3` 防死循环 |
| 取消机制 | event-stream `Aborted` 状态 | `tokio::select!` 抢 cancel token，`kill_on_drop=true` 杀 bash 子进程 |
| Steer / Follow-up | 有 — `getSteeringMessages()` / `getFollowUpMessages()` 可在运行中插话 | 无 — 每个 `run_turn` 是原子 |
| 串行执行截断 | `stopReason === "length"` 时 fail 所有 tool calls | 无特殊截断处理 |
| Hooks | JS 闭包（`beforeToolCall` / `afterToolCall`）| Shell 命令（`pre_tool_use` / `post_tool_use`，Claude Code 协议）|

### 2.3 Context 管理

| 维度 | Pi | nanopi |
|------|-----|--------|
| 上下文裁剪 | `transformContext()` 钩子 + `prepareNextTurn` 自动压缩 | `maybe_compact()` 阈值触发，`compact_now()` 手动触发 |
| 压缩方式 | LLM 摘要，可由扩展自定义（`session_before_compact` 事件可接管） | 固定 LLM 摘要（`agent::compact` 模块） |
| 消息类型 | `Message` 联合类型 + declaration merging 扩展点 | `ContextMessage` enum（User/Assistant/Tool），不可扩展 |

### 2.4 Provider

| 维度 | Pi | nanopi |
|------|-----|--------|
| Provider 数量 | 30+（Anthropic, OpenAI, Google, Bedrock, Azure, OpenRouter...）| 2 个原生（OpenAI-compat + Anthropic），其余靠 vendor 嗅探 |
| Provider 注册 | 扩展可 `pi.registerProvider()` 动态注册 | 不可扩展，`provider::build()` 硬编码两种 |
| 请求头修改 | 扩展可 `before_provider_headers` 事件修改 | 不可 |

### 2.5 Skills 系统

| 维度 | Pi | nanopi |
|------|-----|--------|
| 实现 | Markdown + frontmatter，LLM 读取文件内容 | 同，完全对齐（`SKILL.md` + `name` / `description` frontmatter）|
| 加载位置 | `cwd/.pi/skills/`、`~/.pi/agent/skills/`、`--skill` | `<cwd>/.nanopi/skills/`、`~/.nanopi/skills/`、`--skill` |
| 发现机制 | system prompt 附加 `<available_skills>` | 同 |
| 由扩展提供 | 扩展可通过 `resources_discover` 事件注入额外 skill 路径 | 不可 |

### 2.6 会话管理

| 维度 | Pi | nanopi |
|------|-----|--------|
| 格式 | JSONL（相同思路） | JSONL，对齐 Pi 的 `SessionEntry` 结构 |
| 扩展可写入 | `pi.appendEntry()` 持久化自定义条目 | 不可 |
| 分支管理 | `session_before_fork` / `session_before_tree` 事件，扩展可接管摘要 | 无 |

### 2.7 UI / 交互

| 维度 | Pi | nanopi |
|------|-----|--------|
| TUI 框架 | 自研渲染层 | ratatui + crossterm |
| 自定义渲染 | 扩展可 `registerMessageRenderer` / `registerEntryRenderer` / `registerMarkdownTransformer` | 不可 |
| 键盘快捷键 | 扩展可 `pi.registerShortcut()` 注册 | 不可（固定的 `/slash` 命令）|
| 输入扩展 | `input` 事件可拦截/转换用户输入 | `UserPromptSubmit` hook（shell 命令）|
| 对话框 | `ctx.ui.select()` / `confirm()` / `input()` / `editor()` | 无 |
| 通知 | `ctx.ui.notify()` | 无 |

---

## 三、Pi 扩展机制深度分析

### 3.1 什么是扩展

Pi 的扩展是一个 **TypeScript 模块，导出一个工厂函数**：

```typescript
// 扩展的唯一入口
export default function myExtension(pi: ExtensionAPI) {
    // 在这里注册工具、事件、命令、快捷键...
}
```

扩展通过以下方式加载：

| 路径 | 说明 |
|------|------|
| `cwd/.pi/extensions/` | 项目级扩展（自动发现）|
| `~/.pi/agent/extensions/` | 用户级扩展（自动发现）|
| `--extension <path>` | 命令行显式加载（可重复）|
| `--no-extensions` | 关闭自动发现（显式 `--extension` 仍有效）|

### 3.2 ExtensionAPI 的能力全景

扩展通过 `pi: ExtensionAPI` 对象获得以下能力：

#### A. 事件订阅（30+ 事件）

```typescript
pi.on("tool_call", async (event, ctx) => {
    if (event.toolName === "bash" && event.input.command.includes("rm -rf")) {
        return { block: true, reason: "Dangerous command blocked" };
    }
});
```

**完整事件清单**（按类别）:

| 类别 | 事件 | 能力 |
|------|------|------|
| **会话** | `session_start`, `session_shutdown`, `session_info_changed` | 感知生命周期 |
| | `session_before_switch`, `session_before_fork` | 可取消 |
| | `session_before_compact` | 可接管压缩、取消、提供自定义摘要 |
| | `session_before_tree`, `session_tree` | 分支导航前/后 |
| **Agent** | `before_agent_start` | 注入消息 + 替换 system prompt |
| | `agent_start`, `agent_end`, `agent_settled` | 循环生命周期 |
| | `turn_start`, `turn_end` | 每轮生命周期 |
| **Provider** | `context` | 替换整个 messages 数组 |
| | `before_provider_request` | 替换整个 HTTP payload |
| | `before_provider_headers` | 修改请求头 |
| | `after_provider_response` | 响应后处理 |
| **消息** | `message_start`, `message_update`(逐 token), `message_end` | 消息级拦截，`message_end` 可替换最终消息 |
| **工具** | `tool_call` | 拦截执行 + 修改参数 |
| | `tool_result` | 修改结果内容 / isError |
| | `tool_execution_start/update/end` | 全局工具执行监控 |
| **输入** | `input` | 转换/消费/拦截用户输入 |
| **其他** | `project_trust` | 决定项目信任 |
| | `resources_discover` | 注入额外 skill/prompt/theme 路径 |
| | `model_select`, `thinking_level_select` | 模型/思维级别变更通知 |
| | `user_bash` | `!` / `!!` 前缀命令 |

#### B. 注册工具

```typescript
pi.registerTool({
    name: "my_tool",
    label: "My Custom Tool",
    description: "...",
    parameters: Type.Object({ url: Type.String() }),
    execute: async (toolCallId, params, signal, onUpdate, ctx) => {
        return { content: "result", isError: false };
    },
    renderCall: (args, theme, context) => <Component />,
    renderResult: (result, options, theme, context) => <Component />,
});
```

扩展工具与内置工具无缝混合，LLM 看到的是同一个 tools 列表。

#### C. 注册斜杠命令

```typescript
pi.registerCommand("llama", {
    description: "Manage llama.cpp router models",
    handler: async (args, ctx) => {
        const model = await ctx.ui.select("Choose model", options);
        ctx.ui.notify("Done!", "info");
    },
});
```

#### D. 注册 Provider

```typescript
pi.registerProvider("my-proxy", {
    baseUrl: "https://proxy.example.com",
    apiKey: "$PROXY_API_KEY",
    api: "anthropic-messages",
    models: [{ id: "claude-sonnet-4-20250514", ... }]
});
```

#### E. 其他

- `pi.registerShortcut("ctrl+l", { ... })` — 键盘快捷键
- `pi.registerFlag("plan", { type: "boolean" })` — 自定义 CLI flag
- `pi.registerMessageRenderer()` / `registerMarkdownTransformer()` — UI 扩展
- `pi.sendMessage()` / `pi.sendUserMessage()` — 消息注入
- `pi.exec()` — shell 命令执行
- `pi.events` — 跨扩展 EventBus

### 3.3 内置扩展

Pi 目前只有一个内置扩展：`llama.cpp`（`packages/coding-agent/src/extensions/llama/index.ts`），标记为 `hidden: true`，注册了一个 `/llama` 命令用于管理本地模型。

---

## 四、nanopi 能否支持扩展机制？

### 4.1 可行性评估

| 扩展能力 | nanopi 现状 | 实现难度 | 说明 |
|----------|------------|----------|------|
| **加载外部代码** | 无 | ⚠️ 高 | Rust 没有运行时 eval；Pi 靠 jiti 动态加载 TS，nanopi 需要用 WASM 插件（如 wasmtime）或 shell 脚本。**WASM 是最干净的路线**。 |
| **事件系统** | 有 shell hooks（`pre_tool_use`/`post_tool_use`/`user_prompt_submit`/`session_start`/`session_end`）| 低 | 只需在 `loop_.rs` 对应位置加更多 `run_hooks()` 调用，事件类型已经是 `HookEvent` enum。 |
| **注册工具** | 无 | 中 | 需要设计 `ExternalTool`：WASM 调用或 shell 脚本，返回 JSON。`ToolRegistry` 已有 `register()` 接口。 |
| **注册斜杠命令** | 无 | 低 | 需要在 TUI 的命令解析器加一个插件点，命令来自 hook 输出。 |
| **注册 Provider** | 无（`provider::build()` 硬编码）| 高 | 需要把 `build()` 改成注册模式，或用 WASM provider adapter。 |
| **UI 扩展** | 无（TUI 是 ratatui 固定布局）| 非常高 | ratatui 不支持热插 widget；需要重构渲染层。 |
| **Steer/Follow-up** | 无 | 中 | 需要在 `run_turn` 循环里加一个 `mpsc::Receiver<InjectMessage>` 通道。 |

### 4.2 Pi 的扩展运行时：jiti，不是 WASM

需要明确一点：**Pi 上游完全不用 WASM**，扩展加载走的是 **jiti + Node.js require**。

证据：

- `packages/coding-agent/src/core/extensions/loader.ts:2` 标题直写：「Extension loader - loads TypeScript extension modules using jiti」
- `loader.ts:17` import jiti：`import { createJiti } from "jiti/static"`
- `loader.ts:76` 直接用 Node.js 的 `createRequire`：「`const require = createRequire(import.meta.url);`」
- grep `vm / vm2 / isolated-vm / worker_threads / sandbox / eval / Function(` —— 返回零结果

Pi 的扩展加载路径：

```
用户 .ts 文件 ──→ jiti 运行时转译为 JS
                  ↓
                Node.js require / import() 在 Pi 主进程里直接执行
```

Pi 仓库里出现的唯一 WASM 是 `photon_rs_bg.wasm` —— 那是 **photon-node 图像处理库**自带的，不是扩展机制用的。

Pi 不用 WASM 是合理的：**Pi 已经有 Node.js / Bun 在宿主机上跑**，jiti 在那之上加一层 TS 运行时转译几乎零开销，可以直接 import `@earendil-works/pi-coding-agent` 等宿主 SDK 包。对 Pi 来说这是最高 ROI 的方案。

但对 nanopi 不成立：

| 路线 | 对 nanopi 的代价 |
|------|-----------------|
| 拷贝 jiti + Node.js 嵌入版 | ~30-50 MB，破坏「零运行时依赖」卖点 |
| 嵌入 Rust JS 运行时 (deno-core、boa) | 5-15 MB，慢 |
| 嵌入 Python 解释器 (rustpy) | 15-30 MB，慢 |
| **WASM** | ~2 MB wasmtime runtime，沙箱隔离，跨语言 |

所以结论是：**Pi 走 jiti 是因为 Pi 已经有 Node.js；nanopi 没有 Node.js，所以是 WASM 反过来更自然**。

Pi 用 WASM 不？不用。**但 WASM 比 jiti 对 nanopi 更合适** —— 同一行代码、不同的执行模型，恰好是「移植」这个动作该做的判断。

### 4.3 推荐路线：分三个阶段

#### 阶段一：扩大 Shell Hook 覆盖面（低代价，立即可行）

nanopi 已经有 shell hook 机制（`src/agent/hook.rs`），只是覆盖面比 Pi 少很多。只需要在 `loop_.rs` 的对应位置加更多 `run_hooks()` 调用：

| Pi 事件 | nanopi 对应位置 | 现状 |
|---------|----------------|------|
| `before_agent_start` | `run_turn` 开头，user message 之前 | ❌ 缺失 |
| `turn_start` / `turn_end` | `run_turn` 每轮开头/结尾 | ❌ 缺失 |
| `tool_call`（可拦截+改参数）| `run_one_tool` 的 `pre_tool_use` | ✅ 已有 |
| `tool_result`（可改结果）| `run_one_tool` 的 `post_tool_use` | ⚠️ 只读，不可修改 |
| `message_end` | `run_turn` 循环里 done 之后 | ❌ 缺失 |
| `context`（替换 messages）| `stream_turn` 调用之前 | ❌ 缺失 |

**改法**：每处加一个 `HookEvent` variant + `run_hooks()` 调用，hook 脚本通过 stdin JSON 接收事件，stdout JSON 返回指令（`{"decision":"block"}` / `{"transform": {...}}`）。不引入新依赖，完全向前兼容。

#### 阶段二：WASM 插件系统（中期，需引入 wasmtime）

对于需要注册工具/命令的场景，shell 脚本不够（JSON 序列化开销大、无法做 async、没有类型安全）。WASM 是最干净的路线：

```toml
# config.toml
[[extensions]]
path = "~/.nanopi/extensions/my_tool.wasm"

# 或用 cargo-component 构建的原生组件
[[extensions]]
path = "~/.nanopi/extensions/my_tool.so"
```

**WASM 接口设计**（参考 wit-bindgen）:

```wit
package nanopi:extension;

interface extension-api {
    // 注册工具
    record tool-spec { name: string, description: string, parameters: string }
    register-tool: func(spec: tool-spec, callback: func(args: string) -> result<string, string>);

    // 注册斜杠命令
    register-command: func(name: string, handler: func(args: string) -> result<string, string>);

    // 事件订阅
    record event { event-type: string, payload: string }
    subscribe: func(event-type: string, handler: func(event) -> option<string>);
}
```

**可行性证据**：`Cargo.toml` 已经有 `lto = true` + `opt-level = "z"`，WASM runtime 只增加 ~2 MB（wasmtime 精简配置），仍在 5 MB 预算内。

#### WASM 插件能否访问网络？

**能**。WASI（WebAssembly System Interface）采用「能力化安全」模型——默认全禁，宿主显式授权哪些能力。

```
┌─────────────────────────────────────┐
│         nanopi (宿主进程)             │
│                                      │
│   wasmtime 运行时                    │
│   ┌──────────────────────────────┐  │
│   │ 插件 WASM (沙箱)             │  │
│   │   想调 connect("db:5432")     │  │
│   │          │                    │  │
│   └──────────┼────────────────────┘  │
│              ▼                        │
│   ┌──────────────────────────────┐  │
│   │ 宿主授权策略                   │  │
│   │  ✓ 允许: 127.0.0.1:5432      │  │
│   │  ✓ 允许: *.internal:*        │  │
│   │  ✗ 拒绝: 0.0.0.0:*          │  │
│   └──────────────────────────────┘  │
└─────────────────────────────────────┘
```

| WASI 版本 | 网络能力 | 细粒度 |
|-----------|---------|--------|
| WASI Preview 1 (wasip1) | `sock_*` 系列原始 socket | 粗粒度 — 开或不开 |
| WASI Preview 2 (wasip2) + `wasi:sockets` | 完整 TCP/UDP socket API | 可按 IP/端口/域名授权 |
| **自定义 Host Function**（推荐） | 宿主暴露 `http_get(url)` / `db_query(sql)` 等 | **宿主完全控制 URL/SQL 白名单** |

nanopi 推荐用**自定义 Host Function** —— 最安全、最可控：

```rust
// 宿主侧：暴露给插件的网络函数
fn host_http_get(url: String) -> Result<String, String> {
    // 宿主决定哪些 URL 允许
    if !is_allowed_url(&url) {
        return Err("URL not in allowlist".into());
    }
    reqwest::blocking::get(&url)?.text().map_err(|e| e.to_string())
}

// 插件侧：只能调宿主暴露的函数，无法绕过
fn execute(args: Value) -> String {
    let html = host_http_get("https://api.example.com/data".into())?;
    // 处理 html...
}
```

插件**无法访问原始 socket** —— 它没有 `sys_socket` syscall，所有网络请求必须走宿主给的 `host_http_get`，宿主可以做 URL 白名单、限流、审计日志。这比 shell hook 安全得多（shell 脚本默认有完整网络权限）。

#### 阶段三：Provider 注册 + UI 扩展（长期，需要重构）

这两个需要改动 nanopi 的核心架构：

- **Provider 注册**：`provider::build()` 改成 `ProviderRegistry`，扩展通过 WASM 或 config 注册新 provider。核心改动是把 `build()` 里硬编码的 `OpenAiProvider::new` / `AnthropicProvider::new` 拆成 trait object 工厂。
- **UI 扩展**：ratatui 不支持热插 widget，需要设计一个 widget protocol（WASM 组件声明 widget，TUI 层统一渲染）。这是最大的工程量，建议推迟到 v1.0 之后。

### 4.4 Skills vs Extensions 的边界

Pi 的 Skills 和 Extensions 是**完全不同的系统**：

| 维度 | Skills | Extensions |
|------|--------|------------|
| 格式 | Markdown 文件（`SKILL.md`） | TypeScript 模块（`.ts`/`.js`）|
| 执行 | 不执行代码，LLM 读取文件内容 | jiti 动态执行，有完整 API |
| 能加工具 | 否 | 是 |
| 能拦截行为 | 否 | 是（30+ 事件） |
| nanopi 已实现 | ✅ 完全对齐 | ❌ 完全缺失 |

nanopi 的 Skills 已经和 Pi 完全对齐（`src/resources/` 里的 `SKILL.md` 发现 + `/skill:name` 展开），这是不需要改的部分。

### 4.5 推荐的优先级

| 优先级 | 功能 | 工作量 | 用户价值 |
|--------|------|--------|----------|
| **P0** | 扩展 shell hook 事件覆盖到 `before_agent_start` / `turn_start` / `turn_end` / `message_end` | 1-2 天 | 立即可用，不引入新依赖 |
| **P1** | `post_tool_use` hook 支持 `transform`（修改工具结果） | 半天 | 日志审计、结果过滤 |
| **P2** | WASM 插件系统 — 工具注册 + 斜杠命令 | 2-3 周 | 第三方扩展生态的基石 |
| **P3** | `steer` / `follow-up` 消息注入 | 1 周 | 运行中插话（Pi 用户高频功能）|
| **P4** | Provider 注册 | 2 周 | 多 provider 生态 |
| **P5** | UI 扩展 | 1+ 月 | 需要 TUI 架构重构 |

---

## 五、总结

nanopi 和 Pi 的**核心 Agent Loop 同构**——同样的 Context → Provider 翻译 → LLM 流式响应 → 工具执行 → 结果回喂循环。差异集中在：

1. **扩展性**：Pi 有完整的 TypeScript 扩展系统（30+ 事件、工具/命令/Provider/UI 注册），nanopi 只有 shell hooks 的子集
2. **Provider 矩阵**：Pi 30+ provider，nanopi 2 个
3. **运行中干预**：Pi 有 steer/follow-up，nanopi 没有
4. **UI 可定制性**：Pi 的 TUI 支持扩展渲染，nanopi 的 ratatui 是固定布局

实现扩展机制的**最短路径**是扩大 shell hook 覆盖面（阶段一），不需要新依赖、不改架构、立即可用。WASM 插件系统（阶段二）是中期目标，Provider 注册和 UI 扩展（阶段三）是长期重构。
