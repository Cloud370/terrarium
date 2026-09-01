# 进程执行与网络请求

状态:已实现契约。本稿取代 `filesystem-authorization.md` 第 11 节中关于未来进程/网络的推测;该节现指向本稿。`host.proc`(`exec`、`spawn`、`status`、`wait`、`kill`)、`host.net.fetch`、access 块的 `commands` 字段,以及 `run/spawn`、`proc/exit`、`net/request` 会话事件均已安装在当前二进制中。实现位于 `src/proc.rs`、`src/net.rs`,以及 `src/auth.rs` 与 `src/agent.rs` 中的命令生命周期。

## 1. 范围

本提案在 `host.fs` 之外安装两个能力:

- `host.proc` —— 执行外部命令:`exec`、`spawn`、`status`、`wait`、`kill`;
- `host.net` —— `fetch`,一个 HTTP 客户端。

一句话信任模型与文件系统提案一致:**改变本地机器须同意,其余留痕。** 写入与进程创建在 QuickJS 启动前获批;网络请求不改变本地状态,免同意,逐请求入账。

## 2. 心智模型

- cage 永远不拥有长生命周期的东西,只能引用。进程是 host 持有的会话状态,用不透明句柄引用,正如文件用路径引用。
- deadline 约束观察者,不约束被观察者:一个 run 里 await 永不退出的 server,超时的是 run,server 活着。
- 模型需要管理的生命周期区分只有一个,就是选哪个动词:`exec` 随 run,`spawn` 随会话。没有第三种生命周期。
- 进程输出只有一个持久归宿:host 自有的日志文件。模型不选择输出落在哪,不管理缓冲区和游标,读日志用它已经会的 `host.fs.read`。

## 3. 第一性原则

1. 模型提议命令与写入目标;它永远不给自己授权。
2. 命令是结构化记录——解析后的可执行文件加参数数组——永远不是命令行字符串。spawn 路径上没有 shell,因此没有注入面,也没有拆分歧义。
3. 一次用户决策覆盖一个 run 请求的整个集合。部分批准不存在。
4. 批准一条命令即批准该进程剩余寿命内的全部效果。这严格强于批准任何文件写集合——子进程不受 Terrarium 写作用域约束——因此审批显示的质量才是真正的安全边界。
5. 输出需要一条通道,而不是三条。host 自有的追加式日志取代环形缓冲、游标和 overrun 标志;既有的带行号窗口读取 `host.fs.read` 就是抽取协议。
6. journal 记录决策与收据,永远不存流数据。历史文本永远不是授权;会话进程表永远不跨重启。
7. `fetch` 不改变本地机器状态,因此不需要同意,但它是对 OS 用户可读之物的无守卫外泄通道。journal 是检测,不是预防,契约如实写明。
8. 没有能力在没有完整的界限与失效契约之前发布。

## 4. `host.proc`

命名空间沿用 `host.fs` 的风格:平铺的模块函数,接收显式引用,返回普通记录。没有带方法的句柄对象——现有 host 能力都没有,普通记录能直接序列化进 facts,单行签名在 registry 里保持可审阅。

有两个名字是刻意回避的。`run` 是保留字:在本项目里 *run* 指 cage 执行单元(`run` 围栏、`run/start`、`run/result`),把它复用给子进程会模糊协议最核心的术语。`attach` 也回避,因为后续 run 并不获取活连接——它读一条记录和一个文件。

### 4.1 `exec` —— 随 run 的一次性执行

```js
const r = await host.proc.exec("cargo", ["test", "--", "--nocapture"], {cwd: "/code/app"});
// r: {code, stdout, stderr}
```

- 在当前 run 内等待完成。`stdout` 与 `stderr` 分开捕获,各以头+尾有界到 16 KiB,中间标注省略字节数。
- 若 run 先结束——超时、取消或失败——host 杀死子进程的进程组。`exec` 不落文件;捕获的结果是唯一输出通道,run 消失后 journal 收据是唯一痕迹。
- 这是给 build、test、lint 以及一切"当前 run 就要用结果"的命令的动词。

### 4.2 `spawn` —— 随会话的进程

```js
const p = await host.proc.spawn("npm", ["run", "dev"]);
// p: {id, log, output}
```

- `id` 是会话内不透明句柄(`"p1"`、`"p2"`、……)。它是唯一跨 run 的引用,像文件路径一样随 facts 传递。
- `log` 是 host 自有的追加式文件的绝对路径,位于会话状态目录下(`.../terrarium/sessions/<sid>/procs/<id>.log`)。标准输出与标准错误共用一条交错时间线,因为关联二者才是调试需求,分离管道化是非目标。文件上限 4 MiB;到达上限后 host 停止追加并写入最后一行标记。头部永不改写,行号永久稳定。
- `output` 是 `log` 累积内容的活异步可迭代视图,产出 `{no, text}`——`host.fs.scan` 的惯例。它只存在于发起 spawn 的那个 run 内;后续 run 用 `host.fs.read(log, from, to)` 读 `log`,`from` 是上次消费的下一行。活迭代器与文件是同一条流的两个视图,不是两条通道。
- 这是给必须活过 run 的东西的动词:dev server、watcher、守护进程。

### 4.3 `status`、`wait`、`kill`

```js
host.proc.status(id)            // -> {id, log, running, code}
await host.proc.wait(id)        // -> 最终记录;受 run deadline 约束
host.proc.kill(id)              // -> 最终记录;优雅终止整个进程组
host.proc.kill(id, {force: true})
```

- `status` 只查当前会话的内存表。未知或来自 resume 前的句柄报 `process_lost`——表不持久,历史 journal 文本不是授权。
- `wait` 阻塞到退出为止,受当前 run 的 deadline 约束。deadline 到时,死的是 run,不是进程。
- `kill` 终止整个进程组(Unix)或 Job(Windows):默认优雅,`{force: true}` 强杀;Windows 上两种形式都是终止 Job 对象,该平台没有单独的优雅形式。杀死已退出的进程是幂等的,返回其最终记录。
- 死条目留在表里供 `status`/`kill` 尸检。表最多容纳 8 个活进程、共 16 个条目(死条目 LRU);满了 `spawn` 以可见错误拒绝。host 永远不悄悄杀旧进程腾位子。

### 4.4 刻意缺席

没有 `stdin` 写入(交互式解释器加 stdin 注入会让被批准的 argv 不再是"将要执行什么"的完整摘要;等真实消费者工作流出现时再回来,并回答它自己的授权问题)。模型不能设环境变量。kill 之外没有发信号能力。进程间没有管道。见第 11 节。

## 5. 生命周期与所有权

host 会话持有进程表,内存态。子进程在各自独立的进程组(Unix)或 kill-on-close 的 Job Object(Windows)中创建。

| 事件 | 对进程的影响 |
|---|---|
| run 返回或失败 | 无 |
| run 超时或被杀 | `exec` 子进程随 run 死;`spawn` 的进程不受影响 |
| 显式 `kill` | 进程组被终止 |
| 会话正常结束 | 杀死全部活进程;日志作为普通会话文件保留 |
| host 崩溃或被杀 | 尽力而为:Windows 上 Job Object 连带杀死;Unix 上 Linux 有 `PDEATHSIG` 时生效——否则进程可能成为孤儿 |
| 会话恢复(resume) | 表已消失;旧句柄报 `process_lost`;日志作为文件仍可读 |

崩溃清理如实陈述而非担保:在 Linux 之外的 Unix 上崩溃后,spawn 的进程可能活过它的会话。journal 记录每个进程的 pid,用户可以手动收割残留。被担保的是反面:resume 永远不复活进程,任何历史文本永远不充当授权。

## 6. 输出模型

| 数据 | 通道 | 界限 | 寿命 |
|---|---|---|---|
| 一次性结果 | `exec` 返回值 | 每流 16 KiB,头+尾 | 该 run |
| 持久时间线 | spawn 日志文件 | 4 MiB,之后截断标记 | 会话的文件 |
| 蒸馏结论 | `facts` | 16 KiB | journal |

模型选择的是动词,不是存储策略。读日志与读源文件是同一项技能:`host.fs.read(log, 120, 180)` 返回带行号的行;后续 run 从下一行号续读。截断标记是一行可见文本,不是静默缺口。如果模型想把命令输出放进项目里,它蒸馏日志并通过已授权的写入路径写蒸馏物——facts 纪律由结构强制,而不是靠劝告。

journal 永远不存流数据。它记录三种收据:创建时的 `run/spawn`(解析后的可执行文件、argv、cwd、pid、句柄、日志路径)、退出时的 `proc/exit`(句柄、退出码、约 1 KiB 尾部)、每次 fetch 的 `net/request`。派发后失败、超时或被取消的请求仍以 status 0 记账——字节可能已经离开本机。收据按批次有上限,超限时以 `receipts/truncated` 标记事件计数被丢弃的收据,审计轨迹从不静默截断。

## 7. `host.net.fetch`

```js
const res = await host.net.fetch("https://api.example.com/repos/x", {
  method: "GET",
  headers: {Authorization: {env: "GITHUB_TOKEN"}},
});
// res: {status, finalUrl, body}
for await (const chunk of res.body) { /* 字符串,lossy UTF-8 */ }
```

- 任意方法(`GET`、`HEAD`、`POST`、`PUT`、`PATCH`、`DELETE`),任意 http/https URL,以操作系统用户身份执行——与文件系统读取"继承 OS 可读视图"是同一条信任决策。
- header 值是字面字符串或 `{env: NAME}` 名字引用,host 侧解析;凭证值永远不进 cage。带 userinfo 或 fragment 的 URL 作为语法拒绝,不作为授权问题。
- 重定向直接跟随(最多 5 次),最终 URL 入账。无 Cookie,无缓存。
- 物理上限归 host 所有:每请求 60 秒(覆盖响应头与响应体消费);响应体上限 8 MiB,超出以可见错误拒绝;请求体上限 1 MiB;最多 4 个并发请求;header 名与值做 CRLF 拒绝;请求 URL 上限 8 KiB。`--offline` 为整个调用关闭该能力。
- 每个请求入账为 `net/request`:方法、最终 URL、状态码、字节数。

为什么免同意:fetch 响应只进 cage 内存;触及本地磁盘必须走已授权的写入路径,本地变更回路由构造闭合。外泄回路*没有*闭合:OS 用户可读的任何东西都能在一个零同意请求里发往任何地方,journal 事后检测而非事前阻止。选择模型提供商早已为读数据做了同样的取舍;本提案把后果说透,而不是暗示安全。需要预防的操作者用 `--offline` 或出口防火墙——宿主关注点,不是 cage 能力。

## 8. 授权

### 8.1 access 块增加一个字段

```json
{"writes": [],
 "commands": [{"exe": "cargo", "argv": ["test", "--", "--nocapture"], "cwd": "/code/app"}],
 "reason": "Verify the fix passes"}
```

空形态保持无条件习惯:`{"writes":[],"commands":[],"reason":""}`。

- `commands` 是最多 8 条记录的数组;每条记录是 `{exe, argv, cwd?}`。`cwd` 缺省为会话工作根。整个块保持在 8 KiB 编码上限与 200 字符 reason 上限内;审批展示完整印出每个参数——8 KiB 上限已经约束了总量——journal 保留精确记录。
- `exe` 由 host 在批准时与调用时各解析一次(PATH 查找、符号链接归一);匹配是解析后身份相等加 argv 逐元素相等加 cwd 相等。journal 记录解析后路径;声明在解析后去重。
- `exec` 与 `spawn` 对照同一组记录检查。不匹配任何声明的调用以 `command_not_authorized` 失败,错误印出完整期望记录,下一轮自行纠正。
- 请求是 run 局部的,作为一整个集合决策,永远不可部分批准,每个 run 重新声明。

### 8.2 模式矩阵

| 模式 | 写入 | 命令 | Fetch |
|---|---|---|---|
| `read-only` | 拒绝 | 拒绝;声明作为纠正性反馈 | 允许,入账 |
| `planned-write` | 每 run 一次决策 | 每 run 一次决策 | 允许,入账 |
| `full-access` | 不提示,入账 | 不提示,入账 | 允许,入账 |

进程创建是写级别的效果——子进程不受写作用域约束——所以 `read-only` 拒绝它。`full-access` 去掉提示但保留全部收据;它的含义是操作者接受了 OS 用户的机器级身份,对每个已安装的能力。

### 8.3 运维预授予

`--allow-exec NAME`(可重复,仅 `planned-write`)只匹配解析后的可执行文件,任意 argv,同时覆盖 `exec` 与 `spawn`;被覆盖的记录不再到达提示,正如 `--allow-write` 作用域减扣写入目标。声明习惯不变:模型永远声明,host 减扣。

运维文档里应有一条诚实的警告:会加载项目代码的可执行文件——构建工具(`cargo`、`npm`、`make`)与解释器(`sh`、`node`、`python`)一样——把工作区变成它的程序。既然模型能通过已授权的写入改进工作区,允许这类可执行文件接近于对它的完全信任。没有黑名单;显示质量与这条规则就是防线。

环境也是这份信任的一部分:子进程完整继承宿主进程环境——包括 Terrarium 按变量名挡在笼子外的全部凭据——任何被批准的可执行文件都能读取这些值,一条被批准的 `sh -c env` 会把它们全部打印进模型随后可读的 spawn 日志。批准提示展示 exe、argv 与 cwd;继承而来的环境在提示里不可见,必须视为批准所接受的一部分。

### 8.4 显示质量

审批提示把每条命令渲染为逐参数的精确 argv、解析后的可执行文件、工作目录与 reason——与文件系统提案给写入的"你所读即所运行"同一保证。没有 shell、不能设环境变量、v1 没有 stdin,被批准的记录就是将要启动之物的完整摘要——唯一例外是继承的宿主环境(见 8.3),批准即整份接受。这才是边界;`command_not_authorized` 之类的下游错误只负责让声明保持诚实。

## 9. 契约、runtime-state 与迁移

- 稳定提示前缀进入下一个版本:"无进程、无网络"的句子替换为两个能力描述;材料按确定性顺序追加;一个版本内既有前缀字节永不改变。
- runtime-state 块增加三行——`Platform`、`Live processes`(活进程的句柄与可执行文件,截断为一行,永远不含输出),能力列表增加 `host.proc`、`host.net`。spawn 日志目录是 host 自有状态,不渲染为可写根,也不需要模型可见的前缀,因为模型永远不构造日志路径。
- 会话校验器必须接受旧的两字段 `run/access` 事件(缺 `commands` 视为空),既有 journal 不加改动即可重放;新事件(`run/spawn`、`proc/exit`、`net/request`)与其他事件同样校验,未知字段拒绝。
- `registry.rs` 仍是模型可见面的单一事实源;两个能力只有在其 registry 行、契约文本、prelude shim 与校验器一起落地时才算存在。

## 10. 模型引导(QuickJS 不是 Node)

prelude 为 `require`、`process`、`Buffer` 安装抛错 shim,报错信息指明 host 侧替代物,永远不 polyfill 能力形状的全局;廉价规范件(`TextEncoder`)保留。没有裸 `fetch` 全局别名。契约教一个分诊:纯计算留在 JavaScript;数据访问用 `host.fs` 与 `host.net.fetch`;真工具链是一条声明的命令。`sh` 只用于组合多个真工具,永不用于包裹单个命令。

错误即课程:命名空间代理报出可用面,`command_not_authorized` 报出期望记录,`process_lost` 报出句柄早于当前会话。每一个都为下一轮自我纠正而设计。

## 11. 非目标

管道与多进程组合;PTY/TTY 与继承 stdio;`stdin` 写入;kill 之外的信号;模型设环境变量;cage 内裸 TCP、DNS、TLS 或代理配置;流式上传(大载荷是 spawn 出来的 `curl`);cage 内 SSE 与 WebSocket(`src/llm/` 传输层的职责);文件系统删除;以及网络授权——逃生门如果将来出现真实消费者,是未来某个 access 块版本的 `requests` 字段,到那时再设计,现在不设计。

## 12. 最小验证工作流

1. **构建/测试**:一个 run,声明 `cargo test`,`exec` 它,摘要进 facts。无文件,无句柄。
2. **Dev server**:run A 声明并 spawn `npm run dev`,迭代 `output` 直到端口行,facts 带 `{proc, log, url}`。run B(全新 cage)从下一行读 `log` 并查 `status`。run C kill。resume 路径:句柄报 `process_lost`,日志仍可读。
3. **抓文档**:一个 run,`fetch` 一个页面,在 JavaScript 里过滤 body 流,facts 有界带回结论——全程无授权提示。

## 13. 实现地图

实现遵循此方向:`src/proc.rs`(进程表、tokio `Command`、进程组与 Job Object、带截断标记的日志写入器);`src/net.rs`(`fetch`,复用 llm 传输层的 HTTP 客户端);`auth.rs` 命令记录的解析、解析与冻结;`agent.rs` access 块扩展;`registry.rs` 能力行;`prelude.js` shim 加 `output` 与 `body` 的流化;契约文本更新与会话校验器向后兼容(旧的两字段 `run/access` 事件原样重放);`filesystem-authorization.md` §11(两种语言)指向本稿。
