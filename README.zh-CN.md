# terrarium

一个把模型动作实现为程序而非工具调用的 agent 运行时。默认命令运行持久化的模型驱动 agent 会话；`terrarium run` 是明确的直接 JavaScript 入口。每次 JavaScript 运行都在全新的 QuickJS 笼子中执行，限制 64MB 堆、1MB 栈和硬截止时间，然后返回结构化结果。

[English documentation](README.md)

## 核心方向

Terrarium 是一个有边界的程序运行时：模型输出程序，宿主提供显式能力，每次运行在全新笼子中执行。它不是工具注册表、shell 包装器、操作系统沙箱，也不是多 agent 框架。

### 思考纪律

新增任何能力都必须从第一性原理开始，而不是先设计 API 形状或套用实现模式。按以下顺序回答问题：

1. 这项能力要实现什么用户结果？证明结果所需的最小工作流是什么？
2. 哪些事实和效果必须跨越当前边界？哪些只是临时计算？
3. 每个状态由谁拥有、谁可以改变、何时开始、何时结束？
4. 能让所有权和生命周期显式可见的最小接口是什么？
5. 失败、超时、取消、进程丢失、重启、部分完成和权限拒绝时会怎样？
6. 哪些数据属于模型上下文，哪些属于持久状态，哪些必须留在两者之外？
7. 现有边界能否组合表达这个工作流？如果可以，优先组合，而不是增加新的抽象。

让控制流与数据流分离：结果应说明接下来由谁行动；大数据或敏感数据应通过显式且有界的引用跨越边界。由宿主拥有的事实必须由宿主推导，不能由模型自行报告。以建立正确性所需的最少步数为目标，不要为了消耗或暴露步数上限而优化。没有具体消费者和完整的限额、失败及恢复契约时，不引入新的生命周期、存储层、路由机制或能力；投机性功能不进入公开契约。

当一个用户、模型或未来维护者无需阅读隐藏的实现细节，只根据边界就能判断什么会持久化、什么会释放、接下来谁行动以及不确定性如何处理时，这个设计才是好的。

不变量：

- 模型动作是程序——每次模型回复至多一个 `access` 围栏加恰好一个完整的 `run` 围栏，围栏之外的文字一律不执行。
- 每次运行在全新笼子中执行；可变状态不跨运行传递，凭据永不进入笼子。
- 安全由宿主承担——文件系统模式、冻结的写入范围、执行前预授权、资源限额、取消。Prompt 描述行为，永远不构成安全边界。
- 能力保持显式、最小、有类型、有边界、可观察；错误在边界处暴露，不做静默回退。
- 搜索由组合完成：宿主负责剪枝（gitignore、glob、字面量 `contains`），JavaScript 做最终判断——刻意不提供宿主侧 grep 或正则能力。
- 会话是持久化的仅追加 JSONL 文件。模型请求、运行和授权决策都写入日志；结果未知的运行会被标记，绝不重放，日志只是审计记录，永远不构成授权。
- 核心行为是可移植的宿主代码，不依赖平台特定的外部命令，Linux、macOS 和 Windows 行为一致。

## 协议

一次 agent 轮由多个步骤组成。每次模型回复包含可选的一个 `access` 块和恰好一个闭合的 `run` 围栏；每个成功的程序必须返回一个显式的处置对象：

````text
```access
{"writes": ["/home/me/proj/notes.md"], "reason": "把扫描摘要追加进项目笔记"}
```
```run
const matches = [];
for await (const line of host.fs.scan("/home/me/proj/src", {glob: "*.rs"})) {
  if (line.text.includes("http client")) matches.push({file: line.file, line: line.no});
}
return {to: "model", facts: {matches}};
```
````

`to: "model"` 结束本次 JavaScript 运行并继续同一用户轮。`to: "user"` 结束当前轮并打印消息：

````text
```run
return {to: "user", message: "HTTP client 配置位于 src/llm/。"};
```
````

普通顶层 `return` 会释放本次运行的局部 JavaScript 状态，但不会自行结束轮。格式、解析、遍历、校验、超时及其他可恢复的操作错误，不应自动交还用户；应返回简短 facts 给 `to: "model"`，让下一步修正操作、缩小范围或补充证据。只有结果已经确定，或确实需要用户提供输入、授权或做决定时，才使用 `to: "user"`。只报告错误的 `catch` 块不能结束当前轮。

解析器对每条回复只接受恰好一个完整的 `run` 围栏，且它前面至多一个 `access` 围栏。`run` 缺失、未闭合、出现多个 `run` 或多个 `access`、`access` 出现在 `run` 之后、`access` 内容不是合法的访问 JSON,都是协议错误——解析器绝不执行第一个块后静默忽略其余块。开栏必须是一行独立的 ```` ```run ```` 或 ```` ```access ````,闭栏必须是一行独立的 ```` ``` ````；行内三反引号既不开栏也不闭栏。围栏外的文字不会执行。没有基于文本的完成标记。

每次运行都按同一种 async function body 语义执行，因此顶层 `return` 和 `await` 在所有程序中都合法。结果保留 JSON 值的结构，不会先格式化成字符串。

## 上下文预算

一次运行有两个数据通道。程序主动提供的数据只有通过 `to: "model"` 的 `facts` 才会进入下一轮模型上下文；局部变量、`print` 输出和 `to: "user"` 消息不会成为下一步的模型 facts。宿主可能自动附加有界的状态、错误和写入回执，作为可信证据；不要在 facts 中重复这些宿主事实。facts 只放与决策直接相关的路径、计数、状态和有界样本。不要返回完整 scan 结果、整份文件内容或大数组；大结果如需保留，写入授权文件后只返回路径、数量和简短摘要。24 KiB 结果上限和 16 KiB facts 上限是硬边界，不是目标值。

## 为什么是程序

- 一个完整工作单元在每个步骤中执行，上下文用于承载发现，而不是工具调用往返。
- 重试、分支和并发直接使用 JavaScript 的普通语言构造。
- 宿主能力面保持很小：有边界的文件系统能力、预授权的进程执行、记入日志的网络请求，以及显式的模型/用户处置对象。主模型由可信的外层循环调用；JavaScript 没有模型调用原语。
- 每次运行使用全新笼子，失败不会污染下一次运行。

## 笼子

- 每次运行限制 64MB 堆、1MB 栈和一个硬截止时间。agent 模式默认 10 秒；单次运行默认 2 秒。首行 `// timeout-ms: N` 可将 agent 单次运行提高到最多 300 秒。
- stdout 每次最多捕获 16KB。宿主文件读取使用有界行窗口或有界全文通道。
- 没有虚拟路径命名空间：程序中的每个路径都是操作系统用户文件视图中的一个绝对路径。读取看到的就是当前 OS 用户可读的内容。写入由本次调用冻结的文件系统授权决定、进程创建由冻结的命令授权决定——`read-only` 拒绝一切写入和一切进程启动，`planned-write` 要求解析后的目标身份命中已批准的精确文件或操作者声明的范围、且每条命令命中本次运行批准的记录之一，`full-access` 只保留路径校验和 OS 自身权限。已存在的符号链接永远不被写入;scan 从不跟随符号链接。
- API 凭据只留在宿主进程环境中，不会暴露给 JavaScript。

## 写入与命令预授权

默认的 `planned-write` 模式下，需要写入或启动进程的运行必须先在 `access` 块中一并声明——至多 32 个精确的绝对文件路径、至多 8 条命令记录（`exe`、精确 `argv`、可选 `cwd`）加一个理由。宿主解析并校验每条路径与命令，减去已被操作者 `--allow-write` 范围或 `--allow-exec` 可执行授权覆盖的部分，把余下部分作为一次整体的允许/拒绝决定在 JavaScript 启动前提交。不存在部分批准；批准只对这一次运行有效，运行结束即作废。被拒绝、取消、非法或不可用（没有交互终端）的请求不会执行代码，而是返回一条有界观察。每个决策——包括 `full-access` 下接受但被忽略的声明——都作为 `run/access` 审计事件写入日志，且永远不会从日志恢复授权。

命令是结构化记录，不是命令行字符串：spawn 路径中没有 shell。批准一条命令就是批准该进程在剩余生命周期内的一切效果——子进程不受 Terrarium 写作用域约束——因此展示精确 argv 与解析后可执行文件的批准提示才是真正的边界。进程回执（`run/spawn`、`proc/exit`）与网络回执（`net/request`）按发生顺序写入日志；日志从不存储流数据。`host.net.fetch` 在所有模式下都无需同意（响应只进入笼子内存），由 `--offline` 禁用；出站是事后可查而非被阻止。参见[进程执行与网络请求](docs/process-and-network.zh-CN.md)。

## 快速开始

在 `config.toml` 中配置一个或多个模型配置档：

```toml
version = 1
default_profile = "main"

[providers.local]
base_url = "http://127.0.0.1:11434/v1"

[profiles.main]
provider = "local"
protocol = "openai-chat-completions"
model = "qwen3-coder"
```

在当前目录启动模型驱动 agent：

```sh
terrarium "review this project"
terrarium --read-only "find the unused dependencies"
```

默认模式是 `planned-write`：每次运行的写入通过 `access` 块预授权，未被预先覆盖的部分由终端提问一次允许/拒绝。要预先授予一个目录或文件而不再提示，可加操作者范围：

```sh
terrarium --allow-write "$HOME/proj/notes" "把项目总结写进 notes/summary.md"
```

要预先授予某个可执行文件（任意 argv），或为本次调用禁用网络请求：

```sh
terrarium --allow-write "$HOME/proj" --allow-exec cargo "先加一个失败的测试，再让它通过"
terrarium --offline "审计这个仓库有没有对外通信"
```

可信调试场景可用显式路径移除范围检查：

```sh
terrarium --full-access "读取 ~/chat/landscape-monitor 并汇报"
```

`--full-access` 只保留路径校验加上当前操作系统用户自身的权限——它不绕过 OS 权限，也不是 root 访问。`--read-only`、`--full-access` 与 `--allow-write` 之间的组合会在启动时报错。JavaScript 不会展开 `~`;runtime state 会标明工作根，模型必须使用真实绝对路径。

直接执行 JavaScript 使用独立的 `run` 命令——默认只读，模式标志相同：

```sh
terrarium run -e 'return 1 + 1'
terrarium run --allow-write /tmp/out.json write-report.js
```

agent 会话存储在每用户状态目录中，创建会话时将会话 ID 打印到 stderr；直接运行不会创建会话。

## 命令行

```sh
terrarium [--config PATH] [--profile NAME] [--read-only | --full-access | --allow-write DIR|FILE]... [--allow-exec NAME]... [--offline] [--max-steps N] [--run-timeout-ms N] [消息...]
terrarium --resume SESSION_ID [--read-only | --full-access | --allow-write DIR|FILE]... [--allow-exec NAME]... [--offline] [消息...]
terrarium run [-e SOURCE | FILE] [--read-only | --full-access | --allow-write DIR|FILE]... [--allow-exec NAME]... [--offline] [--timeout-ms N]
```

普通命令始终启动或恢复模型驱动 agent。没有消息参数时，非终端 stdin 可提供消息。`--allow-write` 可重复出现，每次接受一个已存在的绝对目录（递归前缀）或文件（精确目标）；`--allow-exec` 可重复出现，按解析后的可执行身份（裸名称经 `PATH` 解析）预授予任意 argv，同时覆盖 `exec` 与 `spawn`；`--offline` 禁用 `host.net.fetch`。三个模式标志不能组合，`--allow-write`/`--allow-exec` 仅在 `planned-write` 下有效。模式、写入范围和可执行授权只属于本次调用，不会写入会话。agent 在程序返回 `to: "user"` 后以 `0` 退出；使用错误或配置错误时以 `2` 退出。直接运行在程序成功时以 `0` 退出，失败时以 `1` 退出。

## Host API

生成的契约（`--contract`）就是实时能力面的完整文档：

- `host.fs.list(dir)` 将一级目录返回为按名称排序的对象数组，字段为 `name`、`type`（`file`、`directory`、`symlink` 或 `other`）和 `size`；普通文件的 `size` 是字节数，其他类型为 `null`。
- `host.fs.read(path, from, to)` 读取有界行窗口，返回稳定的 `N: text` 行号和续读提示；`to=Infinity` 在窗口预算内读取到 EOF。
- `host.fs.text(path)` 将整个文本文件以 LF 规范化字符串交给程序，不带展示行号；它用于程序变换，不用于展示代码。
- `host.fs.replace(path, oldText, newText[, {all}])` 对写入已授权的文件执行一次精确的定点替换。默认要求恰好一个匹配，未找到或多匹配会明确失败；替换文本按字面量处理，只有明确需要全部替换时才使用 `{all: true}`。旧文本已知时，这是最高效的一次调用编辑路径；未知时先读取或搜索足够上下文。不要只为确认写入而重新读取，run 结果会提供宿主生成的回执。
- `host.fs.scan(path, options)` 从目录树流式读取文本文件行。可选传入 `contains: "literal"`，让 Rust 在跨入 JavaScript 前丢弃不匹配的行；正则、大小写规则、多条件、跨行状态和自定义限制仍由 JavaScript 做最终判断。不传时保持逐行产出。默认尊重 `.gitignore`、跳过隐藏项、二进制和符号链接，并严格校验选项类型。遍历、打开或解码错误会拒绝 scan，不会静默变成空结果。
- `host.fs.walk(path, options)` 从目录树流式产出每个普通文件的 `{file, size}`——scan 的文件级孪生：同样的剪枝、同样的选项；文件从不会被打开。数文件、算总大小用 walk；数 scan 的产出数是在数行。
- `host.fs.write(path, content)` 向写入已授权的目标原子写入文本，返回字节数；批准新文件时包含创建缺失的父目录。run 结果还包含有界的宿主写入回执（`path`、`created`、`changed`、`bytesBefore`、`bytesAfter`、`firstChangedLine`）。
- `host.proc.exec(exe, argv[, {cwd}])` 在当前运行内执行一条命令直到结束，返回 `{code, stdout, stderr}`——每个流按 16 KiB 头尾截取捕获。运行先结束时，子进程的进程组被杀死。这是构建、测试、lint 用的动词。
- `host.proc.spawn(exe, argv[, {cwd}])` 启动会话级进程，返回 `{id, log, output}`：可像文件路径一样跨运行传递的不透明句柄、宿主拥有的 4 MiB 封顶追加日志（用 `host.fs.read` 读取），以及仅供创建运行使用的实时异步迭代视图。进程表至多 8 个活进程、16 个条目；宿主从不为了腾位置而悄悄杀死旧进程。
- `host.proc.status(id)`、`await host.proc.wait(id)`、`host.proc.kill(id[, {force}])` 查询、等待与优雅终止整个进程组。`wait` 受运行截止时间约束（截止杀死的是观察者，不是被观察者）。重启之前的句柄报 `process_lost`；其日志仍可读。
- `host.net.fetch(url[, {method, headers, body}])` 执行一次写入日志的 HTTP 请求——任意方法、仅限 http/https——返回 `{status, finalUrl, body}`，`body` 是字符串块的异步迭代器。请求头的值可以是 `{env: NAME}` 引用，由宿主侧解析；凭据从不进入笼子。限额由宿主拥有：单请求 60 秒、响应上限 8 MiB、至多 4 个并发；重定向至多跟随 5 次并记录最终 URL。

Agent 程序使用上文的 tagged return 协议来继续交给模型或交还用户。

模型请求属于可信的外层 agent 循环并写入会话日志；JavaScript 宿主能力面只有上述能力。请求始终是纯文本——图像读取、编码和 artifact 传输尚未实现。

## 配置

推荐使用严格 TOML 配置文件：`$XDG_CONFIG_HOME/terrarium/config.toml`；在 Unix 且未设置 `XDG_CONFIG_HOME` 时使用 `~/.config/terrarium/config.toml`。可用 `--config PATH` 指定其他文件。凭据只通过环境变量名引用，永远不会存入会话。

配置档从三种线上协议中选择一种——`openai-chat-completions`、`openai-responses` 或 `anthropic-messages`（DeepSeek 的 Anthropic 兼容端点通过 `base_url = "https://api.deepseek.com/anthropic"` 使用）。每次调用都以 server-sent events 流式传输，受单次尝试总超时和块间空闲超时约束，两者都可按配置档设置。助手推理随每次结果写入日志，并按各自协议的原生形状在后续请求中回放（Chat Completions 的助手 `reasoning_content`、Responses 的加密推理条目、Anthropic 的签名思考块）。每次请求的 token 用量——净输入、输出、缓存读写——会写入日志，并对照配置档声明的 `context_window` 输出一行上下文预算。

没有选中 TOML 文件时，仍兼容遗留的 `TERRARIUM_LLM_API_KEY`、`TERRARIUM_LLM_BASE_URL` 和 `TERRARIUM_LLM_MODEL` 环境变量。二进制不会加载 `.env` 文件。

## 仓库结构

- `src/lib.rs`、`src/kernel.rs` —— 可复用 kernel 边界，每次运行一个全新笼子
- `src/main.rs`、`src/cli.rs` —— 进程和终端适配层
- `src/agent.rs` —— 外层 agent 循环、access/run 围栏解析器和预授权生命周期
- `src/session.rs` —— 持久化仅追加会话日志
- `src/fs.rs`、`src/proc.rs`、`src/net.rs`、`src/auth.rs`、`src/llm/`、`src/registry.rs` —— 文件系统能力与冻结写入授权、进程表与命令授权、记入日志的网络请求、access 块解析与 `Authorizer` 边界、三协议流式模型传输和实时 API 注册表
- `src/prompts/`、`src/runtime/` —— 编译进二进制的模型 prompt 和 JavaScript runtime 资产
- `docs/` —— 设计、协议、配置、安全和集成说明

库向非 CLI 调用方暴露 `Kernel`、`RunFilesystemAuthority`/`WriteScope` 信任类型，以及供嵌入适配层使用的 `Authorizer` trait。未来 Web UI 应在此库上增加服务适配层，而不是启动二进制或解析 stderr。

## 文档

- [设计方向](docs/design.md)
- [当前协议](docs/protocol.md)
- [文件系统授权](docs/filesystem-authorization.zh-CN.md)
- [进程执行与网络请求](docs/process-and-network.zh-CN.md)
- [配置](docs/configuration.md)
- [安全边界](docs/security.md)
- [模型配置档与持久会话](docs/model-profiles-and-durable-sessions.zh-CN.md)
- [Web UI 集成边界](docs/web-ui.md)

## 许可证

[MIT](LICENSE)
