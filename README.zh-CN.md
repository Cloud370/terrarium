# terrarium

一个把**模型动作实现为程序而非工具调用**的 agent 运行时。LLM 每轮写一段完整的 ES2020 JavaScript 程序;内核在全新的沙箱笼子里执行(64MB 堆 / 1MB 栈 / 硬超时),返回一个 JSON 结果。循环、重试、分支、并行、子代理委派——通常由 agent harness 实现的一切——都是程序*内*的语言构造。harness 不是工具分发器,而是内核:执行、限额、返回 JSON。

[English documentation](README.md)

## 协议

一次会话就是:任务 → 程序 → 最终回答。模型的每条回复恰好是两种东西之一:

- 围栏 `run` 块 —— 一段完整程序;内核执行它,把 JSON 结果作为下一条消息回灌
- `FINAL:` 行 —— 提交的最终回答;这个词之后的内容不会被提取或执行

````
任务:   内核在哪里设置 HTTP 超时?

回复:   ```run
        return host.fs.search("http client", 20).filter(h => h.startsWith("/proj/src/"))
        ```
        (仓库在宿主机侧扫描 —— 文件内容从不进入模型上下文)

结果:   {"round":3,"ok":true,"result":"[\"/proj/src/main.rs:262: …\"]","stdout":"","error":null,"timed_out":false,"elapsed_ms":6}

回复:   FINAL: src/main.rs:262 —— HTTP client 在这里构建,每请求 120 秒超时。
````

为什么用 `run` 围栏而不是 ` ```javascript `?语言标签描述的是*内容*,而模型写语言标签围栏十有八九是为了*展示*代码。`run` 是发给运行时的指令,只有一个含义:执行这段程序。(内核也宽容地接受 ` ```js `/` ```javascript ` 围栏和 `<run>` 标签作为兜底,但契约只教一个记号。况且这方言本来就不是纯 JavaScript:顶层 `return` 与 `await`、`host.*` API、`runBlock`/`spawnAgent` 内置。)

## 为什么是程序,不是工具调用

- 一次往返,一个工作单元:每轮执行的是一个完整计划而不是一次工具调用——上下文花在结果上,不花在轮次往复上。
- 控制流是免费的:重试是 `try/catch`,分支是 `if`,并行是 `Promise.all`,委派是 `spawnAgent(task)`——不需要 harness 先实现某个功能,模型才用得上它。
- 能力面保持极小,因为语言本身就是组合器:`host.fs` 和 `host.llm` 就是全部。
- 失败很便宜:每次运行都是全新笼子;死掉的运行(内存耗尽、超时)只杀该次运行——会话存活,下一次运行全新开始。

## 笼子

- 每次运行:64MB 堆、1MB 栈、一个硬截止。默认 2 秒;agent 模式上限 300 秒,程序可以在首行用 `// timeout-ms: N` 申请更大预算。
- 文件系统访问只能通过启动时声明的挂载(`--mount /virt=real`;仅 `:rw` 可写)。越狱(`..`、经符号链接逃出根)由路径物理拒绝,而非判断——策略性拒绝应当写进最终回答,而不是换条路重试。
- API 密钥只存在于宿主进程环境;沙箱永远看不见。

## 快速开始

```sh
cargo build --release
echo 'return 1+1' | ./target/release/terrarium
```

```json
{
  "ok": true,
  "result": "2",
  "stdout": "",
  "error": null,
  "timed_out": false,
  "elapsed_ms": 1,
  "target": "x86_64-linux",
  "limits": { "memory": "64MB", "stack": "1MB", "timeout_ms": 2000 },
  "mounts": [],
  "llm_usage": { "calls": 0, "cache_hit_tokens": 0, "cache_miss_tokens": 0, "output_tokens": 0 }
}
```

挂载一个项目并打印完整的 agent 契约(也就是你的 LLM 被灌输的全文):

```sh
./target/release/terrarium --mount /proj=$(pwd) --contract
```

## 命令行

运行模式 —— 执行一段程序(代码来自 argv,缺省时读 stdin):

```sh
terrarium [--timeout N] [--mount /virt=real[:rw]]... [--contract] [code]
```

Agent 模式 —— 外层循环驱动 LLM;会话状态在笼子外,每次运行都是一个全新笼子:

```sh
terrarium agent <任务文件 | 任务文本> [--mount /virt=real[:rw]]... [--max-rounds N] [--run-timeout-ms N]
```

## Host API

程序内模型拿到 `host`(运行 `host.help()` 看实时能力面,或 `--contract` 看契约全文):

- `host.fs.{list,read,text,search,write}` —— 缩放式探索:`list`(顺带拿到大小)→ 开窗 `read`;全仓库 `search` 在宿主机侧执行,文件内容不进模型上下文;`text` 把整个文件读进程序做点状编辑;`write` 纯文本、原子、自动建父目录。
- `host.llm.{call,chat}` —— 嵌套 LLM 调用。

每个程序内置:`runBlock(code)`(嵌套运行,语义一致)和 `spawnAgent(task, {system?, maxTurns=8})`(全新上下文的子代理——就是不同上下文的主 agent;一份契约同时教会两者)。

## 配置

| 变量 | 用途 |
|---|---|
| `DEEPSEEK_API_KEY` | `host.llm` 与 agent 模式必需 |
| `TERRARIUM_LLM_BASE_URL` | 覆盖端点(任意 OpenAI 兼容服务) |
| `TERRARIUM_LLM_MODEL` | 覆盖模型 |

密钥只存在于进程环境变量中 —— 沙箱看不见它们。agent 模式下,若环境里没有密钥,terrarium 还会回退到从 `./.env` 文件读取。

## 仓库结构

- `src/main.rs` —— 内核管线:每次运行全新笼子、限额、取消协议
- `src/registry.rs` —— host API 注册表;`host.help()` 与契约都由它生成,永不漂移
- `src/fs.rs`、`src/llm.rs` —— 宿主能力(挂载、LLM 端点)
- `src/agent.rs` —— 外层 agent 循环
- `src/CONTRACT.md`、`src/MAIN.md`、`src/prelude.js` —— 教学契约、角色模板、运行时基础;经 `include_str!` 编译进二进制

## 许可证

[MIT](LICENSE)
