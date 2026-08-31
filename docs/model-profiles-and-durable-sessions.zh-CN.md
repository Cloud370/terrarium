# 模型配置档与持久会话

状态：已实现的版本 1 行为。本文档定义当前产品与运行时语义。

## 1. 为什么需要这个

Terrarium 在与真实模型配合使用之前，需要两项能力：

1. 具名的模型配置档（profile），让用户选择一套完整的模型设置，而不是在每次调用时重复端点细节；
2. 持久会话（durable session），让进程重启不会抹掉对话，也不会重复执行一个效果未知的 JavaScript 程序。

本设计遵循 Terrarium 工作负载的真实形态。一个会话通常只有一轮，很少超过十几轮。大部分活动发生在一轮之内：主模型执行若干步，Terrarium 执行若干程序。

持久化格式应当针对这种形态做优化，而不是针对假想中拥有数百万行的会话。只要能省掉注册表、哈希、迁移和共享对象规则，在每一轮中重复一个小的已解析配置档是可以接受的。

会话文件只有一个用途：忠实地恢复会话。它不是数据库、搜索索引、授权令牌、防篡改审计日志、遥测格式或密钥存储。

## 2. 设计哲学

### 2.1 最小化整个系统

一个组件并不会仅仅因为它把复杂性藏在某个依赖之后就变得简单。Terrarium 把二进制体积、依赖、数据格式、失败模式、迁移规则和运维文件都计入系统整体。

一个仅追加（append-only）的 JSONL 文件使用 Terrarium 已有的序列化能力，并且可以用普通文本工具直接查看。因此版本 1 不引入任何数据库、模式迁移框架、索引或辅助存储服务。

### 2.2 保持用户词汇量小

用户只需选择一个配置档名称；默认命令直接在当前目录启动持久化的模型驱动 agent：

```sh
terrarium --profile main "review this project"
```

直接执行 JavaScript 是独立的开发与集成入口：

```sh
terrarium run -e 'return await host.fs.text("/work/project/Cargo.toml")'
```

供应商和协议始终是宿主配置层面的细节。调用点不存在隐藏的任务分类器、自动模型路由器、回退链，也不存在供应商专属的选项包。

### 2.3 一轮一次模型绑定

一轮恰好解析一个配置档。该已解析配置档在本轮中每次主模型步骤和重试中保持不变。

本版本中，Terrarium 不暴露 `host.llm.call` 或任何其他程序内模型调用原语。一个独立的受托模型最终应当成为一个拥有自己生命周期、对话、预算、取消机制、权限和结果契约的 agent。一个无状态的辅助调用并不是该设计的部分替代品。

移除这个原语带来直接后果：

- 一轮只存储一个已解析配置档，而不是一个目录；
- 主模型看不到配置档目录；
- JavaScript 没有模型选择 API；
- 会话恢复只需跟踪主模型和 JavaScript 运行。

### 2.4 存储事实，而非框架对象

会话工作根目录是会话生命周期内稳定的资源身份。版本 1 不持久化附件注册表、写入范围、虚拟路径别名或目录级 ACL。根本属于另一个目录的任务应启动另一个会话。

每次调用选择一个文件系统模式——`read-only`、`planned-write`（agent 默认）或 `full-access`——在 `planned-write` 下还可附加操作者 `--allow-write DIR|FILE` 范围。这些由可信宿主选择，在本次调用内固定不变，但不会写入日志。`planned-write` 中，每次运行的写入通过 `access` 块预授权，详见 `filesystem-authorization.zh-CN.md`。`--full-access` 允许模型使用当前操作系统用户可见的真实绝对路径，包括会话工作根之外的路径。JavaScript 不会展开 `~`；runtime state 会标明工作根。路径被拒绝时应将其视为授权结果，不得猜测其他路径或虚构范围。

生成的主机契约将 `host.fs.list(dir)` 暴露为按名称排序的对象数组，字段为 `name`、`type` 和 `size`。`type` 可以是 `file`、`directory`、`symlink` 或 `other`；普通文件的 `size` 是字节数，其他类型为 `null`。程序应直接检查这些字段，不要解析展示字符串。递归统计时一次列出一个目录；仅对 `type` 为 `directory` 的条目继续递归，并且只累加 `type` 为 `file` 的 `size`。应记录遍历错误；如果任何必要目录无法列出，就必须报告结果不完整，不得提交确定的完整总数。

日志（journal）存储：

- 每个用户轮次；
- 该轮使用的确切系统提示词和已解析配置档；
- 主模型请求的尝试与结果；
- JavaScript 运行的开始与结果边界；
- 展示给模型的确切观察结果；
- 每轮的终态。

它不引入持久绑定对象、绑定哈希、目录、每个实体的独立 ID、分支、投影或迁移，除非有具体的消费方。

事件序列号在一个日志内天然唯一，内部引用即使用它。

### 2.5 优先本地重复，而非全局间接

每一轮存储其完整的已解析配置档和确切系统提示词。会话很短，因此这只是少量重复的 JSON。作为回报：

- 每一轮都是自包含的；
- 打开中的轮不需要配置文件；
- 配置变更不会悄悄改变某一轮；
- 无需绑定注册表、规范哈希或配置档去重。

### 2.6 只持久化恢复所需的边界

在会话级别，只有两种操作需要预写（write-ahead）记录：

1. 一次主模型请求尝试；
2. 一次 JavaScript 运行。

一次请求尝试必须在联系供应商之前被记录。一次运行必须在 JavaScript 执行之前被记录。

纯转换与其产生的结果一起存储。解析助手响应不产生独立事件。格式化一次运行的观察结果也不产生独立事件。

### 2.7 重试的是步骤，而非历史

一步是一次逻辑上的主 agent 决策。它可能进行尝试 1，在出现可重试失败后进行尝试 2。两次尝试使用相同的冻结对话和已解析配置档。

失败的尝试保留在日志中，但绝不进入模型可见的对话历史。尝试 2 不消耗新的步骤。版本 1 没有第三次尝试。

HTTP 传输层不进行任何隐藏重试。一条已记录的尝试最多授权一次网络派发。

### 2.8 绝不重放效果不确定的本地操作

主模型请求可以再次尝试，因为它不会重复本地文件系统效果，尽管可能重复供应商侧的工作或费用。

JavaScript 程序则不同。如果 `run/start` 已持久化但 `run/result` 缺失，程序可能已经更改了文件。Terrarium 记录一个未知结果，并告知模型去检查当前状态。它绝不会再次执行该源代码。

## 3. 术语

**配置（Config）** 是一份已加载的 TOML 文档，包含供应商、配置档和一个默认配置档。

**供应商（Provider）** 提供一个网络基础 URL 和一个可选的环境变量名，凭据从该变量读取。

**协议（Protocol）** 是内置的请求/响应编解码器。它负责端点路径构造、认证形态、请求编码、响应解码以及推理力度（reasoning-effort）映射。

**配置档（Profile）** 是一个具名的模型调用预设。它组合了一个供应商、协议、上游模型 ID、可选的输出令牌上限和可选的推理力度。

**已解析配置档（Resolved profile）** 是将一个配置档针对一份配置解析后得到的非机密调用规格。一轮直接存储这个值。

**会话（Session）** 是存储在一个 JSONL 文件中的一个持久对话。

**轮（Turn）** 以一条用户消息开始，并在显式交还用户、取消、步骤数耗尽或终态失败时结束。成功的 `{to: "model", facts: {...}}` 会保持本轮打开。一轮完成并不意味着会话完成。

**步（Step）** 是一轮内一次逻辑上的主 agent 决策。一步在模型响应以及程序结果、协议观察或可恢复错误观察之后结束。`{to: "model", facts: {...}}` 会在同一轮开始下一步；`{to: "user", message: "..."}` 会交还用户并结束本轮。终态的模型失败可以在步骤成功之前结束本轮。

**尝试（Attempt）** 是一步的一次持久化尝试。它最多授权一次网络派发。如果进程在记录尝试之后停止，日志无法知道派发是否发生，因此该尝试视为已消耗，其供应商侧结果未知。

**运行（Run）** 是由一次成功的模型响应选出的一个带围栏（fenced）的 Terrarium JavaScript 程序。

**状态重建（State reconstruction）** 指把已存储的事件归约为会话状态和模型可见对话。它绝不意味着重新发出一个已完成的请求或重新执行历史上的 JavaScript。

## 4. 范围

版本 1 包含：

- 一个带版本的 TOML 配置文件；
- 具名的供应商和配置档；
- 一个内置的 OpenAI Chat Completions 协议；
- 仅支持文本输入和文本输出；
- 可选的 `low`、`medium` 或 `high` 推理力度；
- 轮开始时的显式配置档选择；
- 一轮内使用固定不变的已解析配置档；
- 每个会话一个仅追加的 JSONL 文件；
- 一个会话内多个用户轮；
- 每个主 agent 步骤至多一次重试；
- 被中断的未闭合轮的继续；
- 保守恢复，绝不重复结果不确定的运行；
- 通过现有的身份解析边界校验并恢复会话工作根。

版本 1 排除未使用的能力元数据。在 Terrarium 具备上下文管理或会消耗这些值的非文本负载之前，不设上下文窗口、图像模态或视频模态字段。

## 5. 配置

### 5.1 发现

常规文件路径为：

```text
$XDG_CONFIG_HOME/terrarium/config.toml
```

在 Unix 上未设置 `XDG_CONFIG_HOME` 时，回退为 `~/.config/terrarium/config.toml`。其他平台使用其常规的每用户配置目录，加 `terrarium/config.toml` 后缀。

配置选择优先级为：

1. 显式 `--config PATH`；
2. 默认的每用户文件（如存在）；
3. 遗留的 `TERRARIUM_LLM_*` 环境变量（在未选中任何 TOML 文件时）。

这些来源是互斥选项，不是可合并的层级。显式或发现的 TOML 文件无效则报错。Terrarium 不会回退到另一个来源。

include、继承、覆盖（overlay）、目录扫描、项目级发现和 `.env` 加载均不在本规范范围内。

### 5.2 TOML 模式

```toml
version = 1
default_profile = "main"

[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"

[providers.local]
base_url = "http://127.0.0.1:11434/v1"

[profiles.main]
provider = "openrouter"
protocol = "openai-chat-completions"
model = "anthropic/claude-sonnet-4"
max_output_tokens = 32000
reasoning_effort = "high"

[profiles.fast]
provider = "openrouter"
protocol = "openai-chat-completions"
model = "google/gemini-flash"
max_output_tokens = 16000
reasoning_effort = "low"

[profiles.local]
provider = "local"
protocol = "openai-chat-completions"
model = "qwen3-coder"
```

`version` 必填，且必须等于 `1`。

`default_profile` 必填，且必须指向一个配置档。

供应商字段：

| 字段 | 必填 | 含义 |
|---|---:|---|
| `base_url` | 是 | HTTP 或 HTTPS 服务根地址，协议在其后追加路径 |
| `api_key_env` | 否 | 包含凭据的环境变量名 |

配置档字段：

| 字段 | 必填 | 含义 |
|---|---:|---|
| `provider` | 是 | 供应商名称 |
| `protocol` | 是 | 内置协议标识符 |
| `model` | 是 | 确切的上游模型 ID |
| `max_output_tokens` | 否 | 正数的请求输出令牌上限 |
| `reasoning_effort` | 否 | `low`、`medium` 或 `high` |

供应商与配置档名称必须匹配：

```text
[A-Za-z0-9][A-Za-z0-9._-]*
```

`base_url` 必须使用 HTTP 或 HTTPS，不得包含凭据、查询或片段，规范化后不以斜杠结尾。

`model` 原样发送上游。`max_output_tokens` 如存在必须为正数。

未知字段视为错误。严格解析能让拼写错误的选项被发现。

当没有选中的轮使用某供应商时，其凭据缺失不会阻碍配置加载。只有当请求需要它时才成为配置错误。省略 `api_key_env` 表示该端点有意不做认证。

遗留环境变量加载器会创建一个名为 `default` 的配置档，使用 `openai-chat-completions`，且不带可选的上限和推理力度。它可能从遗留端点移除末尾的 `/chat/completions` 段来得到供应商基础 URL。其他端点形态不做猜测。

## 6. 协议边界

核心使用供应商中立的值，等价于：

```text
ModelRequest
|-- messages: 有序的文本 role/content 消息
|-- model
|-- max_output_tokens | 可缺省
`-- reasoning_effort | 可缺省

ModelResponse
`-- content: 文本
```

协议负责：

- 追加到供应商基础 URL 之后的路径；
- 认证头形态；
- 请求 JSON 编码；
- 推理力度编码；
- 成功响应的校验与解码。

共享传输层负责：

- HTTP 客户端；
- 单次尝试超时；
- 并发限制；
- 响应体大小限制；
- 凭据查找；
- 有界的错误呈现。

一次传输调用最多尝试一次网络派发，且从不重试。派发前的失败与派发后的失败都属于同一个持久尝试。崩溃之后，Terrarium 不会声称能区分二者。

初始协议标识符为 `openai-chat-completions`。它向如下地址发送请求：

```text
{规范化后的 base_url}/chat/completions
```

仅当 `api_key_env` 存在时才使用 bearer 认证；发送非流式的文本消息；原样发送配置的模型 ID；将可选的 `max_output_tokens` 映射为 `max_tokens`；仅当配置了 `reasoning_effort` 时才发送它。

它从第一个 assistant choice 中读取字符串内容。工具调用、流式、供应商管理的会话状态、多模态内容和私有思维链字段均不支持。

显式的推理力度要么被所选协议编码，要么导致配置档无效。它绝不会被静默丢弃。

## 7. 配置档选择与轮快照

### 7.1 新会话

新会话从以下来源选择其第一个配置档：

1. 显式 `--profile NAME`；
2. 否则使用 `default_profile`。

只有被选中的配置档会被解析。凭据值缺失不会阻碍解析，因为凭据只在发起请求时读取。

### 7.2 一轮冻结的内容

每个 `turn/start` 存储：

- 用户消息；
- 发送给模型的确切最终系统提示词；
- 选中的配置档名称；
- 完整的已解析配置档；
- 该轮使用的步数与运行超时限制。

已解析配置档只包含未来请求所需的值：

```text
name
protocol
base_url
api_key_env | 可缺省
model
max_output_tokens | 可缺省
reasoning_effort | 可缺省
```

供应商名称已完成其配置使命，不再重复存储。API 密钥的值永不存储。

已解析配置档有意在每一轮中重复。会话轮数很少，少量重复的 JSON 比绑定注册、内容哈希、共享对象或迁移规则更简单。

### 7.3 后续轮

当一个已完成的会话收到新的用户消息时：

- 带 `--profile NAME`：Terrarium 加载当前配置，解析该配置档，从当前提示词资产渲染系统提示词，并存储新快照；
- 不带 `--profile`：复制上一轮选中的与已解析的配置档和轮限制，并使用当前调用授权的根路径重新渲染新轮 prompt；不读取配置文件；

这使"采用当前配置与提示词资产"成为显式行为。编辑配置或升级提示词文本绝不会重写历史，也不会悄悄改变一个延续中的会话。

恢复时，`--config` 只有在配合 `--profile` 开启新轮时才有效，否则是用法错误。

日志版本号涵盖继续一个会话所需的事件模式、对话投影、协议编解码语义、运行围栏语义和宿主契约。无法遵守该版本的二进制可以查看文件，但必须拒绝执行。

## 8. 会话 JSONL

### 8.1 用途与位置

一个会话就是一个文件：

```text
$XDG_STATE_HOME/terrarium/sessions/<session-id>.jsonl
```

在 Unix 上未设置 `XDG_STATE_HOME` 时，回退为 `~/.local/state/terrarium/sessions`。其他平台使用其常规的每用户状态目录。

该文件包含恢复所需的对话与执行状态。它是普通的应用程序状态。版本 1 不添加数据库、索引、加密、脱敏、日志专属权限策略或辅助元数据文件。

API 密钥的值绝不写入。

### 8.2 头部

第一个物理行是会话头：

```json
{"type":"session","version":1,"id":"ses_01...","workingRoot":{"displayPath":"/work/project","canonicalPath":"/work/project"}}
```

头部不是事件，没有 `seq`。`displayPath` 是本地用户命名空间中的绝对路径；`canonicalPath` 是创建会话时由宿主规范化得到的路径。

恢复时，宿主会重新解析存储目录的身份。如果目录不存在、解析到不同的规范路径、不是目录或无法在当前主机表示，恢复失败。Terrarium 不会静默采用当前 shell 目录、重定向根目录或重写存储路径。存储的根目录不授予权限；当前调用独立决定文件系统模式与写入范围。

### 8.3 事件信封

之后的每个物理行都是一个事件：

```json
{"type":"turn/start","seq":1,"data":{}}
```

信封字段：

| 字段 | 含义 |
|---|---|
| `type` | 事件名称 |
| `seq` | 从 1 开始的连续会话内序列号 |
| `ts` | 可选的追加墙钟时间，epoch 毫秒；字段引入前写入的日志中没有它 |
| `data` | 事件专属对象 |

顺序由序列号而非时间定义。JSON 字段使用 `camelCase`。`ts` 面向运维取证——每步模型延迟和轮次时长可直接从日志读出——绝不投影进模型可见上下文。

未知事件类型、重复序列号、序列号缺口、格式错误的完整行以及无效的事件形态均为错误。已有的完整事件从不被改写。恢复时只允许移除末尾不完整的物理行。

### 8.4 写入

创建会话使用独占文件创建，写入完整头部，并在第一轮开始之前同步文件。崩溃可能留下只有头部的未完成会话；此时没有任何模型请求或 JavaScript 运行跨越边界，因此无需发布协议。

追加一个事件意味着：

1. 序列化一个紧凑的 JSON 对象；
2. 将其作为一行完整行写入，带结尾换行符；
3. 在依据该事件行动之前 flush 并同步文件。

同样的规则适用于每个事件。模型和程序的延迟主导了同步成本，一条统一的规则比耐久性分级更简单。

写日志文件时，一个进程持有独占锁。第二个写入者会失败，而不是交错写入。不需要边车锁文件。进程内部由唯一的日志写入者串行分配序列号。

## 9. 事件模型

版本 1 有六种事件类型：

```text
turn/start
model/request
model/result
run/start
run/result
turn/end
```

`model/request` 的 `seq` 标识该请求。`run/start` 的 `seq` 标识该运行。不再生成单独的请求、运行或绑定 ID。

以下对象形态是规范性的。在日志版本 1 中，未知字段视为错误。描述为“可缺省”的字段直接省略，而不是编码为 `null`，除非 `Kernel::Outcome` 本身的既有形态就使用 `null`。Agent 的 `run/result` 会存储规范化的 tagged disposition；旧日志中的 `answer` 字段只为兼容读取保留，当前 agent 不会再写入。

### 9.1 `turn/start`

```json
{
  "type": "turn/start",
  "seq": 1,
  "time": 1787890000010,
  "data": {
    "message": "Review this project",
    "systemPrompt": "...",
    "profile": {
      "name": "main",
      "protocol": "openai-chat-completions",
      "baseUrl": "https://openrouter.ai/api/v1",
      "apiKeyEnv": "OPENROUTER_API_KEY",
      "model": "anthropic/claude-sonnet-4",
      "maxOutputTokens": 32000,
      "reasoningEffort": "high"
    },
    "limits": {
      "maxSteps": 256,
      "defaultRunTimeoutMs": 10000,
      "maxRunTimeoutMs": 300000
    }
  }
}
```

同一时刻只能有一个打开的轮。此事件在第 1 步开始之前即已持久化。

### 9.2 `model/request`

```json
{
  "type": "model/request",
  "seq": 2,
  "time": 1787890000020,
  "data": {
    "step": 1,
    "attempt": 1
  }
}
```

请求属于当前打开的轮，并使用其存储的配置档。

模型输入是此事件之前日志前缀的确定性对话投影。请求事件和失败的模型结果被排除在该投影之外，因此尝试 2 收到的输入与尝试 1 完全相同。

此事件在其尝试执行至多一次网络派发之前持久化。即便进程在能证明派发发生之前停止，记录该事件也已消耗了这次尝试。

### 9.3 成功的 `model/result`

成功的结果包含完整的助手文本，以及在写入事件之前计算出的纯解析结果。

对于一次有效运行：

```json
{
  "type": "model/result",
  "seq": 3,
  "time": 1787890000080,
  "data": {
    "requestSeq": 2,
    "ok": true,
    "content": "```run\nreturn await host.fs.text('/proj/Cargo.toml')\n```",
    "action": {
      "kind": "run",
      "source": "return await host.fs.text('/proj/Cargo.toml')\n",
      "timeoutMs": 10000
    }
  }
}
```

对于无效的运行围栏，action 存储将展示给模型的确切观察：

```json
{
  "type": "model/result",
  "seq": 3,
  "time": 1787890000080,
  "data": {
    "requestSeq": 2,
    "ok": true,
    "content": "I could not inspect the project.",
    "action": {
      "kind": "observation",
      "message": "protocol error: no program was executed; send exactly one complete ```run program with no prose or other code block"
    }
  }
}
```

解析和观察格式化在写入结果之前进行，因为它们没有外部副作用。存储它们的输出，省去了 parser-version 和 feedback-formatter 事件的需要。

### 9.4 失败的 `model/result`

```json
{
  "type": "model/result",
  "seq": 3,
  "time": 1787890000080,
  "data": {
    "requestSeq": 2,
    "ok": false,
    "error": {
      "kind": "transport",
      "message": "request timed out",
      "retryable": true
    }
  }
}
```

稳定的错误类别是 `configuration`、`transport`、`http`、`protocol`、`cancelled` 和 `interrupted`。

尝试 1 的可重试失败允许同一步骤进行尝试 2。尝试 2 是最终的，即使其错误仍是可重试的。

### 9.5 `run/start`

```json
{
  "type": "run/start",
  "seq": 4,
  "time": 1787890000090,
  "data": {
    "modelResultSeq": 3
  }
}
```

被引用的模型结果必须属于当前轮，且包含 `action.kind == "run"`。该 action 已存储确切的源代码和超时，因此 `run/start` 不重复它们。

此事件在 JavaScript 开始执行之前持久化。其序列号标识该运行。

### 9.6 `run/result`

正常结果存储完整的内核结果和规范化的 agent disposition。`to: "model"` 还会存储加入主对话的、有界观察；`to: "user"` 存储用户消息但省略 observation：

```json
{
  "type": "run/result",
  "seq": 5,
  "time": 1787890000110,
  "data": {
    "runSeq": 4,
    "status": "completed",
    "outcome": {
      "ok": true,
      "value": null,
      "stdout": "",
      "error": null,
      "termination": "returned",
      "timedOut": false,
      "elapsedMs": 20
    },
    "disposition": {
      "to": "model",
      "facts": {"matches": [{"file": "/work/project/src/llm.rs", "line": 12}]}
    },
    "observation": "{\"turn\":1,\"step\":1,\"to\":\"model\",\"facts\":{\"matches\":[{\"file\":\"/work/project/src/llm.rs\",\"line\":12}]}}"
  }
}
```

`to: "user"` 的 disposition 会存储 `message`，不存储 observation，并触发 `turn/end` 的 `handed_off`。可恢复的运行或协议错误则存储有界的模型观察，不会自动交还用户。

```json
{
  "type": "run/result",
  "seq": 5,
  "time": 1787890000200,
  "data": {
    "runSeq": 4,
    "status": "outcome_unknown",
    "observation": "the previous program may have changed state before the process stopped; it was not repeated, so inspect current state before proceeding"
  }
}
```

`outcome_unknown` 是恢复状态，不是捏造的内核结果。

### 9.7 `turn/end`

显式交还用户时追加 `turn/end`：

```json
{
  "type": "turn/end",
  "seq": 9,
  "data": {
    "reason": "handed_off",
    "handoffRunSeq": 8
  }
}
```

原因有：

- `handed_off`：已完成的 `to: "user"` disposition；
- `step_limit`；
- `failed`；
- `cancelled`。

只有 `handed_off` 需要 `handoffRunSeq`，且它必须指向一个 disposition 目标为 `user` 的已完成运行。旧日志中的 `answered` 和 `answerRunSeq` 仍可读取，但当前 agent 不再写入。

## 10. 对话投影

一次请求的模型对话按顺序包含：

1. 当前轮存储的确切 `systemPrompt`；
2. 来自当前及此前 `turn/start` 事件的用户消息；
3. 成功的 `model/result.content` 值，作为助手消息；
4. `model/result.action.message` 值，作为协议观察；
5. `run/result.observation` 值，作为运行观察或恢复观察。

只考虑请求之前的事件。投影排除：

- 已解析配置档的连接细节；
- 请求与重试元数据；
- 失败的模型结果；
- 时间元数据；
- 凭据——从不存储。

由于一次失败的尝试不产生任何对话消息，且两次尝试之间不会发生其他对话事件，尝试 2 可以重建与尝试 1 完全相同的输入，而无需存储第二份消息副本或上下文标识符。

版本 1 不截断、摘要或压缩历史。如果供应商拒绝了过大的请求，Terrarium 报告该错误。未来的压缩功能必须通过添加显式状态来实现，而不是重写历史。

## 11. 步骤执行与重试

对每个新步骤：

1. 从日志重建模型可见对话；
2. 追加 `model/request`，`attempt: 1`；
3. 尝试至多一次网络派发；
4. 追加 `model/result`；
5. 若为可重试失败，为同一步骤追加尝试 2；
6. 若成功，处理存储的 action；
7. 在其协议观察、运行结果、可恢复错误观察或显式交还用户之后结束该步骤；
8. 仅在需要另一次主 agent 决策时递增步骤。

一步的多次尝试绝不重叠。尝试 2 必须跟随尝试 1 的可重试失败结果。不存在尝试 3。

重试不消耗新的步骤。如果尝试 2 失败，本轮以 `failed` 结束。不可重试的错误同样以 `failed` 结束本轮，但操作者取消以 `cancelled` 结束。

可重试的失败：

- 传输失败；
- HTTP 429；
- HTTP 5xx;
- 请求事件已记录但其结果尚未持久化时的中断。

配置错误、除 429 外的 HTTP 4xx 以及无效的供应商响应不可重试。

超时可能使供应商侧结果未知。尝试 2 可能重复供应商侧的工作或费用。Terrarium 记录这种不确定性，且绝不做第三次尝试。

在一个成功返回 `to: "model"` 的步骤，或发生可恢复的协议/运行错误之后，Terrarium 开始下一步，除非这会超出该轮存储的 `maxSteps`。成功返回 `to: "user"` 时，以 `reason: "handed_off"` 结束本轮。达到步骤上限时追加 `turn/end`，`reason: "step_limit"`。

## 12. 恢复与修复

概念上的 CLI 形式：

```sh
terrarium [--config PATH] [--profile NAME] [--read-only | --full-access | --allow-write DIR|FILE]... <消息...>
terrarium --resume SESSION_ID [--read-only | --full-access | --allow-write DIR|FILE]...
terrarium --resume SESSION_ID [--config PATH] [--profile NAME] [--read-only | --full-access | --allow-write DIR|FILE]... <消息...>
terrarium run [-e SOURCE | FILE] [--read-only | --full-access | --allow-write DIR|FILE]... [--timeout-ms N]
```

普通命令始终是模型驱动 agent。消息参数会拼接成文本；Terrarium 不会把看起来像现有文件的消息重新解释为任务文件。没有消息时，非终端 stdin 可提供消息。文件系统模式标志对本次调用的每一次运行都有效。`--full-access` 下，写入只需通过路径校验并受当前操作系统用户权限约束；JavaScript 不会展开 `~`，因此模型必须使用 runtime state 中标明的实际绝对 home 路径。`terrarium run` 是唯一的直接 JavaScript 入口：它从 `-e`、一个文件或非终端 stdin 读取源码，不创建持久会话，默认只读，并输出一个结构化结果。

- 不带 `--resume`：创建会话并开始第一轮；
- `--resume ID` 不带消息：继续一个打开的轮；
- 无打开的轮时，不带消息的恢复失败；
- 带消息的恢复要求上一轮已闭合，并开启新轮；
- `--profile` 仅在开启新轮时有效；
- 打开中的轮始终使用其存储的配置档和限制；系统提示词字节稳定，当前调用的运行时状态随 user 消息传递；文件系统模式由当前调用独立选择；
- 已完成的轮绝不被重新打开；
- `--read-only`、`--full-access` 与 `--allow-write` 只属于当前调用，从不写入或从日志恢复；
- 创建会话时会话 ID 打印到 stderr，让 stdout 保持为回答通道。

恢复至多移除一个末尾不完整的物理行，校验所有完整事件，归约日志，然后按如下方式处理最终状态。

### 12.1 轮已开始，尚无请求

开始第 1 步。对于更后面的步骤，由前面存储的观察决定下一步的步骤号。

### 12.2 请求没有结果

供应商侧结果未知。恢复追加一个失败的 `model/result`，`kind: "interrupted"`，`retryable: true`。

如果该请求是尝试 1，恢复为同一步骤发起尝试 2。如果是尝试 2，恢复以 `failed` 结束本轮。

### 12.3 请求有失败结果

尝试 1 的可重试失败产生尝试 2。尝试 2 的任何失败以 `failed` 结束本轮。尝试 1 的不可重试失败同样以 `failed` 结束。取消则以 `cancelled` 结束。

### 12.4 成功的模型结果包含运行 action 但没有 `run/start`

程序尚未跨越其执行边界。恢复追加 `run/start` 并首次执行它。

模型请求不会被重复。

### 12.5 `run/start` 没有结果

程序可能已更改文件。恢复绝不再次执行它。

恢复追加 `run/result`，`status: "outcome_unknown"`，附带确切的恢复观察，然后进入下一步，或在步骤上限处闭合。

### 12.6 已完成的运行没有后续

如果 disposition 目标是 `user`，追加 `turn/end`，`reason: "handed_off"`，并带上对应的 `handoffRunSeq`；只打印一次 disposition 中的消息。`to: "model"`、有界的运行错误观察或协议观察都会进入下一步，或在步骤上限处闭合。为兼容旧日志，没有 disposition 但 `outcome.answer` 为字符串的已完成运行，仍以旧的 `answered` 原因闭合。

### 12.7 协议观察没有后续

观察已存储在 `model/result` 中。进入下一步，或在步骤上限处闭合。

## 13. 日志不变量

版本 1 的日志仅在以下条件成立时有效：

- 头部在最前且仅出现一次；
- 事件序列号连续且从 1 开始；
- 至多一个打开的轮；
- 上一轮结束之前不出现下一轮的事件；
- 每个事件引用都指向更早的兼容事件；
- 每个模型请求至多一个结果；
- 主步骤从 1 开始，逐一递增；
- 尝试仅限 1 和 2；
- 尝试 2 必须跟随同一步骤中尝试 1 的可重试失败；
- 每步至多一次尝试成功；
- 一次成功的模型结果恰好包含一个 action；
- 一个运行 action 至多一个 `run/start`；
- 一次运行至多一个结果；
- 成功的 `to: "model"` 运行带有模型观察，且 facts 序列化后不超过 16384 字节；
- `handed_off` 的轮引用一个带有 user disposition 的已完成运行；
- 在下一个 `turn/start` 之前，`turn/end` 之后没有事件。

校验会报告违规的序列号和不变量。Terrarium 不会为了使无效日志可用而静默丢弃完整事件。

## 14. 错误、凭据与普通文件状态

配置错误包含完整字段路径。选择错误包含所请求的配置档和有效名称。会话错误在可用时包含会话 ID 和违规事件的序列号。

供应商错误暴露有界的分类和状态信息，不复制无界的响应体。

API 密钥的值只存在于宿主进程内存和请求头中。它们绝不会出现在 TOML、轮快照、日志事件、JavaScript 全局变量、提示词或常规错误中。环境变量名会被存储，因为它是已解析配置档的一部分。

日志可能包含用户提示词、源代码、路径、模型响应、程序输出和回答。版本 1 不添加日志专属的权限策略、加密、脱敏、签名或防篡改检测。保护普通应用程序状态文件是运行环境的责任，不属于持久会话语义的一部分。

## 15. 验收要求

所有验收测试使用本地 mock HTTP 服务器。不需要任何真实的第三方模型服务。

### 15.1 配置档

- 一个 TOML 文件定义多个供应商和配置档。
- 默认与显式配置档选择是确定性的。
- 供应商定位与协议编码保持分离。
- 未知字段和无效引用在网络活动之前失败。
- 未选中供应商的凭据缺失不阻碍选中配置档的使用。
- 确切的模型 ID 原样发送。
- 一轮存储一个完整的已解析配置档，此后绝不读取可变配置。
- 后续轮不显式选择配置档时，复制上一轮的配置档、提示词和限制。
- 显式选择配置档时，采用当前配置和提示词资产，但不重写历史。

### 15.2 步骤与重试

- 一次主步骤可以进行尝试 1 和尝试 2，且模型输入完全相同。
- 第一次可重试失败产生尝试 2，不消耗新的步骤。
- 失败的尝试不出现在模型可见对话中。
- 一步内至多一次尝试成功。
- 不存在尝试 3，包括重启之后。
- 一条已记录的尝试至多授权一次网络派发。
- 传输层不做隐藏重试。

### 15.3 日志与投影

- 一个会话在单个 JSONL 文件中持久化多轮。
- 重建产生与原始完全一致的用户、助手、协议观察和运行观察顺序。
- 每轮通过其存储的提示词和已解析配置档自包含。
- 无需绑定注册表、配置档目录或绑定哈希。
- API 密钥的值绝不出现在日志中。
- 当前存储的工作根通过身份解析重新校验；
- 文件系统模式与写入范围只属于当前调用，从不写入日志；
- 第二个写入者无法交错写入事件。

### 15.4 恢复

- 末尾不完整的行可以被移除而不丢失其前的事件。
- 格式错误的完整行被拒绝。
- 未配对的尝试 1 被标记为中断，并在同一步骤进入尝试 2。
- 未配对的尝试 2 被标记为中断并终止本轮。
- 没有 `run/start` 的已存储运行 action 执行一次。
- `run/start` 已持久化但没有结果的运行绝不重新执行。
- 已存储的运行结果或协议观察直接继续，不重新生成其文本。
- 已存储的 `to: "user"` disposition 只以 `handed_off` 结束本轮一次。
- 旧的已回答轮保持闭合。

## 16. 显式的非目标

版本 1 不包含：

- `host.llm.call` 或其他程序内模型调用原语；
- 受托 agent 或子 agent；
- SQLite 或其他数据库；
- 会话索引、搜索、列表 UI 或删除 UI；
- 日志专属的文件权限、加密、脱敏、签名或哈希链；
- 持久绑定注册表、绑定哈希、配置档目录或配置档去重；
- 为轮、请求、运行或绑定分配独立的 UUID；
- 隐藏的模型路由或回退配置档；
- 编译进 Terrarium 的供应商/模型目录；
- 配置档继承、合并、include 或覆盖；
- 任意供应商请求头或原始厂商 JSON；
- 上下文窗口元数据或自动上下文管理；
- 图像、视频、音频或工件传输；
- 流式响应或令牌事件；
- 在打开的轮内更换配置档；
- 重放结果未知的 JavaScript；
- 回滚、历史重写或分支；
- 多用户服务授权。

未来的 agent 委托设计必须独立成立。它必须定义生命周期、对话所有权、预算、取消、权限、结构化结果、持久化和递归限制，而不是默认复活一个无状态的辅助调用。

## 17. 最终边界

对用户：

```text
一次性配置供应商与配置档
在定义会话工作根的目录启动 agent
默认使用 planned-write，也可为本次调用选择只读、full access 或操作者写入范围
按会话 ID 恢复，不改变工作根或恢复旧权限
直接 JavaScript 只使用 terrarium run
```

对模型：

```text
使用当前轮的模型绑定
每步发出一个程序
从不选择供应商、配置档、协议或持久化行为
```

对运行时：

```text
Config -> 一个已解析的轮配置档
       -> 步骤 -> 可见的尝试 1 -> 可选的尝试 2 -> 模型结果
               -> 可选的运行 -> 运行结果
       -> 仅追加 JSONL
```

这就是预期的平衡：每轮重复一个小的不可变配置档，让唯一允许的重试在拥有它的步骤上保持可见，只持久化恢复所需的边界，并且除 Terrarium 真正需要的会话行为之外，不添加任何模型调用或存储机制。
