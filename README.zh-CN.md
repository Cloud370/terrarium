# terrarium

**LLM 的动作是程序,不是工具调用。**模型每轮写一段完整的 ES2020 程序(围栏 `run` 块),内核在笼子里执行(64MB 堆 / 1MB 栈 / 硬超时),返回一个 JSON 结果。能力——`host.fs`、`host.llm`——是程序*内*的 API。控制流(循环、解析、重试、子代理委派)同样下沉到程序里,agent 框架不需要 harness。

[English documentation](README.md)

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
- 写入口只认操作员启动时声明的 `:rw` 挂载。内核做物理,不做判断;策略性拒绝应当写进模型的最终回答,而不是换条路重试。

每个程序内置:`runBlock(code)`(嵌套运行,语义一致)和 `spawnAgent(task, {system?, maxTurns=8})`(全新上下文的子代理)。子代理 = 不同上下文的主 agent —— 一份契约同时教会两者。

## 沙箱限制

每次运行:64MB 堆、1MB 栈、一个硬截止(`--timeout`,默认 2000ms)。死掉的运行(内存耗尽、超时)只杀该次运行 —— 会话存活,下一次运行全新开始。

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
