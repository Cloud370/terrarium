# terrarium

一个把模型动作实现为程序而非工具调用的 agent 运行时。每轮模型提交一段完整的 ES2020 JavaScript 程序；内核在全新的 QuickJS 笼子中执行，限制 64MB 堆、1MB 栈和硬截止时间，然后返回一个结构化 JSON 结果。

[English documentation](README.md)

## 协议

一次 agent 会话由一系列程序组成。模型回复必须包含一个闭合的 `run` 围栏：

````text
```run
for await (const line of host.fs.scan("/proj/src", {glob: "*.rs"})) {
  if (line.text.includes("http client")) return `${line.file}:${line.no}`;
}
```
````

普通 `return` 只结束本次运行。只有程序调用 `host.agent.answer(text)` 才会结束整个会话：

````text
```run
host.agent.answer("HTTP client 配置位于 src/llm.rs。");
```
````

解析器只接受完整的 `run` 围栏。未闭合围栏和没有程序的回复都会产生协议错误。没有基于文本的完成标记。

每次运行都按同一种 async function body 语义执行，因此顶层 `return` 和 `await` 在所有程序中都合法。结果保留 JSON 值的结构，不会先格式化成字符串。

## 为什么是程序

- 每轮执行一个完整工作单元，上下文用于承载发现，而不是工具调用往返。
- 重试、分支和并发直接使用 JavaScript 的普通语言构造。
- 宿主能力面保持很小：文件系统、文本型嵌套 LLM 调用，以及显式的会话回答函数。
- 每次运行使用全新笼子，失败不会污染下一次运行。

## 笼子

- 每次运行限制 64MB 堆、1MB 栈和一个硬截止时间。agent 模式默认 10 秒；单次运行默认 2 秒。首行 `// timeout-ms: N` 可将 agent 单次运行提高到最多 300 秒。
- stdout 每次最多捕获 16KB。宿主文件读取使用有界行窗口或有界全文通道。
- 文件系统只能访问启动时声明的挂载；写入必须使用 `:rw`。路径越界和解析后越出挂载根的符号链接会被拒绝；scan 从不跟随符号链接。
- API 凭据只留在宿主进程环境中，不会暴露给 JavaScript。

## 快速开始

```sh
cargo build --release
echo 'return 1+1' | ./target/release/terrarium
```

命令输出一个 JSON 对象：

```json
{
  "ok": true,
  "value": 2,
  "answer": null,
  "stdout": "",
  "error": null,
  "termination": "returned",
  "timed_out": false,
  "elapsed_ms": 1,
  "target": "x86_64-linux",
  "limits": { "memory": "64MB", "stack": "1MB", "timeout_ms": 2000 },
  "mounts": [],
  "llm_usage": { "calls": 0, "cache_hit_tokens": 0, "cache_miss_tokens": 0, "output_tokens": 0 }
}
```

打印挂载项目使用的完整契约：

```sh
./target/release/terrarium --mount /proj=$(pwd) --contract
```

## 命令行

从参数运行一个程序；没有代码参数时从 stdin 读取：

```sh
terrarium [--timeout-ms N] [--mount /virt=real[:rw]]... [--contract] [code]
```

运行外层 agent 循环：

```sh
terrarium agent <任务文件 | 任务文本> [--mount /virt=real[:rw]]... [--max-rounds N] [--run-timeout-ms N]
```

所有时间单位均为毫秒。运行模式退出码为：成功 `0`、程序失败 `1`、用法或配置错误 `2`。agent 模式退出码为：调用 `host.agent.answer` 后 `0`、传输或用法失败 `1`、轮次预算耗尽 `2`。

## Host API

程序内使用 `host.help()` 查看实时能力面，或使用 `--contract` 查看完整生成契约：

- `host.fs.list(dir)` 列出一级目录，包含大小和符号链接条目。
- `host.fs.read(path, from, to)` 读取有界行窗口；`to=Infinity` 在窗口预算内读取到 EOF。
- `host.fs.text(path)` 在不超过 64MB 宿主预算时，把整个文本文件读入程序。
- `host.fs.scan(path, options)` 从目录树流式读取文本文件行。默认尊重 `.gitignore`、跳过隐藏项、二进制和符号链接，并严格校验选项类型。遍历、打开或解码错误会拒绝 scan，不会静默变成空结果。
- `host.fs.write(path, content)` 在声明为 `:rw` 的挂载下原子写入文本，返回字节数。
- `host.llm.call(prompt, system)` 和 `host.llm.chat(messages, system)` 通过配置的 OpenAI 兼容 chat-completions 端点发起文本请求。
- `host.agent.answer(text)` 提交当前 agent 会话回答。从程序返回永远不会提交整个会话。

当前模型示例声明以下能力：

- `deepseek-v4-flash`：文本输入、文本输出，不支持图像输入。
- `deepseek-v4-flash-vision-exp`：文本或图像输入、文本输出。

本阶段只声明能力。实际 `host.llm` 请求仍是文本 payload；图像读取、编码和 artifact 传输尚未实现。

## 配置

| 变量 | 用途 |
|---|---|
| `TERRARIUM_LLM_API_KEY` | agent 模式和 `host.llm` 使用的 API key |
| `TERRARIUM_LLM_BASE_URL` | OpenAI 兼容 chat-completions 端点 |
| `TERRARIUM_LLM_MODEL` | 发给上游的模型 ID，默认 `deepseek-v4-flash` |
| `TERRARIUM_LOG_RUNS` | 设为 `1` 时将执行的程序源码记录到 stderr |

二进制不会加载 `.env` 文件。通过进程环境或外部 secret manager 提供凭据，并将秘密文件放在挂载目录之外。

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
