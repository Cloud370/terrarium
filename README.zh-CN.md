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

让控制流与数据流分离：结果应说明接下来由谁行动；大数据或敏感数据应通过显式且有界的引用跨越边界。由宿主拥有的事实必须由宿主推导，不能由模型自行报告。以建立正确性所需的最少步数为目标，不要为了消耗或暴露步数上限而优化。没有具体消费者和完整的限额、失败及恢复契约时，不引入新的生命周期、存储层、路由机制或能力。

当一个用户、模型或未来维护者无需阅读隐藏的实现细节，只根据边界就能判断什么会持久化、什么会释放、接下来谁行动以及不确定性如何处理时，这个设计才是好的。

不变量：

- 模型动作是程序——每轮恰好一个完整的 `run` 块，块外内容一律不执行。
- 能力保持显式、最小、有类型、有边界、可观察；错误在边界处暴露，不做静默回退。
- 安全由宿主承担——挂载范围、`:rw` 写入、资源限额、取消。Prompt 描述行为，永远不构成安全边界。
- 可变状态不跨运行传递，凭据永不进入笼子。
- 核心行为是宿主代码，不依赖平台特定的外部命令。
- `host.fs.walk` 与 `host.fs.scan` 共享同一个遍历引擎——walk 流式产出一棵树的文件条目，scan 流式产出它的行——宿主侧剪枝加普通 JavaScript 过滤仍是唯一的搜索机制。
- 会话是持久化的仅追加 JSONL 文件。模型请求和 JavaScript 运行在派发或执行之前跨越持久化边界；结果不确定的运行绝不重放。

新增任何能力之前先回答：哪个真实工作流需要它；它的限额、取消、失败状态和权限是什么；在 Linux、macOS、Windows 上行为是否一致；现有能力能否表达它？优先选择最小的边界，投机性功能不进入公开契约。

## 协议

一次 agent 轮由多个步骤组成。每次模型回复必须包含一个闭合的 `run` 围栏；每个成功的程序必须返回一个显式的处置对象：

````text
```run
const matches = [];
for await (const line of host.fs.scan("/proj/src", {glob: "*.rs"})) {
  if (line.text.includes("http client")) matches.push({file: line.file, line: line.no});
}
return {to: "model", facts: {matches}};
```
````

`to: "model"` 结束本次 JavaScript 运行并继续同一用户轮。`to: "user"` 结束当前轮并打印消息：

````text
```run
return {to: "user", message: "HTTP client 配置位于 src/llm.rs。"};
```
````

普通顶层 `return` 会释放本次运行的局部 JavaScript 状态，但不会自行结束轮。格式、解析、遍历、校验、超时及其他可恢复的操作错误，不应自动交还用户；应返回简短 facts 给 `to: "model"`，让下一步修正操作、缩小范围或补充证据。只有结果已经确定，或确实需要用户提供输入、授权或做决定时，才使用 `to: "user"`。只报告错误的 `catch` 块不能结束当前轮。

解析器对每条回复只接受恰好一个完整的 `run` 围栏。缺失、未闭合或出现多个 `run` 围栏都是协议错误——解析器绝不执行第一个块后静默忽略其余块。开栏必须是一行独立的 ```` ```run ````，闭栏必须是一行独立的 ```` ``` ````；行内三反引号既不开栏也不闭栏。围栏外的文字不会执行。没有基于文本的完成标记。

每次运行都按同一种 async function body 语义执行，因此顶层 `return` 和 `await` 在所有程序中都合法。结果保留 JSON 值的结构，不会先格式化成字符串。

## 为什么是程序

- 一个完整工作单元在每个步骤中执行，上下文用于承载发现，而不是工具调用往返。
- 重试、分支和并发直接使用 JavaScript 的普通语言构造。
- 宿主能力面保持很小：有边界的文件系统能力，以及显式的模型/用户处置对象。主模型由可信的外层循环调用；JavaScript 没有模型调用原语。
- 每次运行使用全新笼子，失败不会污染下一次运行。

## 笼子

- 每次运行限制 64MB 堆、1MB 栈和一个硬截止时间。agent 模式默认 10 秒；单次运行默认 2 秒。首行 `// timeout-ms: N` 可将 agent 单次运行提高到最多 300 秒。
- stdout 每次最多捕获 16KB。宿主文件读取使用有界行窗口或有界全文通道。
- 文件系统只能访问启动时声明的挂载；写入必须使用 `:rw`。路径越界和解析后越出挂载根的符号链接会被拒绝；scan 从不跟随符号链接。
- API 凭据只留在宿主进程环境中，不会暴露给 JavaScript。

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
terrarium --profile main --read-only "find the unused dependencies"
```

如果一次调用需要读取工作目录之外的真实绝对路径，可以选择 full access：

```sh
terrarium --full-access "读取 ~/chat/landscape-monitor"
```

如果只需要授权一个更窄的目录，可声明一个在本次调用的所有运行中都有效的挂载：

```sh
terrarium --read-only \
  --mount /landscape-monitor="$HOME/chat/landscape-monitor" \
  "读取 landscape-monitor"
```

`--full-access` 将 `/` 映射到当前操作系统用户可见的文件系统，但不会绕过操作系统权限。受限模式下 agent 使用 `/workspace` 以及显式虚拟挂载。JavaScript 不会展开 `~`；prompt 会列出可用根目录，并说明如何处理被拒绝的路径。

直接执行 JavaScript 使用独立的 `run` 命令：

```sh
terrarium run -e 'return 1 + 1'
```

agent 会话存储在每用户状态目录中，创建会话时将会话 ID 打印到 stderr；直接运行不会创建会话。

## 命令行

```sh
terrarium [--config PATH] [--profile NAME] [--read-only | --full-access] [--mount /virtual=real[:rw]] [--max-steps N] [--run-timeout-ms N] [消息...]
terrarium --resume SESSION_ID [--read-only | --full-access] [--mount /virtual=real[:rw]] [消息...]
terrarium run [-e SOURCE | FILE] [--read-only | --full-access] [--mount /virtual=real[:rw]] [--timeout-ms N]
```

普通命令始终启动或恢复模型驱动 agent。没有消息参数时，非终端 stdin 可提供消息。`--mount` 对本次调用的每一次运行都有效。默认访问模式是 `workspace`；`--read-only` 与 `--full-access` 互斥；访问模式和挂载都不会写入会话。agent 在程序返回 `to: "user"` 后以 `0` 退出；使用错误或配置错误时以 `2` 退出。直接运行在程序成功时以 `0` 退出，失败时以 `1` 退出。

## Host API

生成的契约（`--contract`）就是实时能力面的完整文档：

- `host.fs.list(dir)` 将一级目录返回为按名称排序的对象数组，字段为 `name`、`type`（`file`、`directory`、`symlink` 或 `other`）和 `size`；普通文件的 `size` 是字节数，其他类型为 `null`。
- `host.fs.read(path, from, to)` 读取有界行窗口；`to=Infinity` 在窗口预算内读取到 EOF。
- `host.fs.text(path)` 在不超过 64MB 宿主预算时，把整个文本文件读入程序。
- `host.fs.scan(path, options)` 从目录树流式读取文本文件行。默认尊重 `.gitignore`、跳过隐藏项、二进制和符号链接，并严格校验选项类型。遍历、打开或解码错误会拒绝 scan，不会静默变成空结果。
- `host.fs.walk(path, options)` 从目录树流式产出每个普通文件的 `{file, size}`——scan 的文件级孪生：同样的剪枝、同样的选项；文件从不会被打开。数文件、算总大小用 walk；数 scan 的产出数是在数行。
- `host.fs.write(path, content)` 在声明为 `:rw` 的挂载下原子写入文本，返回字节数。

Agent 程序使用上文的 tagged return 协议来继续交给模型或交还用户。不存在 `host.agent.answer` API。

JavaScript 宿主能力面不包含 `host.llm.call`；模型请求属于可信的外层 agent 循环，并记录在会话日志中。

当前模型示例声明以下能力：

- `deepseek-v4-flash`：文本输入、文本输出，不支持图像输入。
- `deepseek-v4-flash-vision-exp`：文本或图像输入、文本输出。

本阶段只声明这些模型能力。外层模型请求仍是文本 payload；图像读取、编码和 artifact 传输尚未实现。

## 配置

推荐使用严格 TOML 配置文件：`$XDG_CONFIG_HOME/terrarium/config.toml`；在 Unix 且未设置 `XDG_CONFIG_HOME` 时使用 `~/.config/terrarium/config.toml`。可用 `--config PATH` 指定其他文件。凭据只通过环境变量名引用，永远不会存入会话。

没有选中 TOML 文件时，仍兼容遗留的 `TERRARIUM_LLM_API_KEY`、`TERRARIUM_LLM_BASE_URL` 和 `TERRARIUM_LLM_MODEL` 环境变量。二进制不会加载 `.env` 文件。

## 仓库结构

- `src/lib.rs`、`src/kernel.rs` —— 可复用 kernel 边界，每次运行一个全新笼子
- `src/main.rs`、`src/cli.rs` —— 进程和终端适配层
- `src/agent.rs` —— 外层 agent 循环和 run 围栏解析器
- `src/fs.rs`、`src/llm.rs`、`src/registry.rs` —— 宿主能力及实时 API 注册表
- `src/prompts/`、`src/runtime/` —— 编译进二进制的模型 prompt 和 JavaScript runtime 资产
- `docs/` —— 设计、协议、配置、安全和集成说明

库向非 CLI 调用方暴露 `Kernel` 和经过校验的 `Mount`。未来 Web UI 应在此库上增加服务适配层，而不是启动二进制或解析 stderr。

## 文档

- [设计方向](docs/design.md)
- [当前协议](docs/protocol.md)
- [配置](docs/configuration.md)
- [安全边界](docs/security.md)
- [Web UI 集成边界](docs/web-ui.md)

## 许可证

[MIT](LICENSE)
