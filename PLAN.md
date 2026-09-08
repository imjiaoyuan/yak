# PLAN: llm → Rust 版 the reference

目标形态：`llm` 成为和 the reference 同构的极简终端 coding harness —— 交互 REPL 为主入口，
四个内置工具，扩展靠用户自装的插件包。本文件是迁移期的设计依据；完成后并入 CLAUDE.md。

## 1. 定位与不变式

- 单二进制、同步、4 crate 依赖红线不变。
- the reference 的哲学照抄：**核心小，扩展外置**。the reference 用 TS 进程内扩展；Rust 版用
  "出进程扩展宿主"（第 4 节），能力换一种方式实现，哲学一致。
- 参考物：`the behavioral spec`（行为 spec，只读）。
- `~/.llm` 用户目录名保留（已文档化的 deviation）。

## 2. CLI 形态

agent 就是 `llm` 本身，**没有 `agent` 子命令、不留别名**：

```bash
llm                        # 交互 REPL（= the reference）
llm "task text"            # one-shot agent 跑完即退出
llm -c                     # 继续最近 session（= the reference -c）
llm -r                     # 浏览历史 session（取代 llm logs 浏览器入口）
llm --session <id前缀>     # 指定 session 续接
llm --fork <id前缀>        # 从既有 session 分叉新 session（fork_thread 已有）
llm --no-session           # 临时模式，不落盘
llm -m MODEL [--thinking low|high]
llm --yolo / --no-tools
llm -a PATH|-|URL / --at PATH MIME   # 多模态输入保留（输入侧）
llm install|remove|list    # 包管理（git-only，见 4.4；无 update 命令，重跑 install 即刷新）
llm --version / llm help
```

删除的 CLI 面：`prompt`、`logs`（被 -r//resume//session//export 吸收）、
`models`、`login`/`logout`（降为 REPL 内命令 + 库函数）、`agent` 子命令、
`--out` 媒体输出、`--schema` 全家、`-t/--save` 模板旗标。

## 3. REPL 命令（定稿）

保留：`/help /model /thinking /login /logout /resume /clear /status /tree /compact
/export /reload /settings /skills /skill:<name> /memory /yolo /init /tools /exit`

- `/exit` 退出（**保留 /exit，不设 /quit**；ctrl+c 两连击同样退出）。
- `/clear` 清空当前 session 上下文（不引入 /new）。
- `/status` 显示当前 session 状态（模型、thinking 档、轮数、token、session 文件）。
- `/tree` 简化版：列出当前 session 的 turns，选中某条 user 消息从那里截断续接
  （不需要完整 session 树）。
- `/compact [prompt]` 手动压缩，支持自定义指令。
- `/export [file]` session 导出为 .md 或 .jsonl。
- `/reload` 重载扩展、skills、prompts、上下文文件（respawn 扩展宿主）。
- `/settings` 用 $EDITOR 打开 config.json。
- 未知 `/name` 仍走 prompts 模板 → 扩展注册命令 → 纯任务文本的回退。

明确不要：`/name`、`/fork`（REPL 版；`--fork` CLI 旗标保留）、`/copy`、`/new`、
`/quit`、`/ask`、`/mcp`、`/update`、主题系统、订阅 OAuth。

## 4. 插件系统

### 4.1 能力面（对齐 the reference ExtensionAPI 的可出进程子集）

- **自定义工具**：注册进工具表，审批矩阵里 Exec 级（默认询问，per-tool 策略可放行）。
- **斜杠命令**：注册 `/name`。
- **事件钩子**：`tool_call`（可拒绝/改写参数——权限门与路径保护由此实现）、
  `tool_result`、`turn_start`、`turn_end`、`agent_start`、`agent_end`、`input`。
- **不做**：UI 组件/自定义编辑器/热键注册（出进程做不到）、provider 注册、
  主题（明确不要）。

### 4.2 扩展宿主协议（`agent/ext.rs`，取代 mcp.rs + script_tool.rs + user_tools.rs）

- 发现目录：`~/.llm/extensions/`、`.llm/extensions/`（cwd 向上走），
  项目覆盖用户（同名）；包目录里的 `extensions/` 一并挂载。
- 扩展 = 可执行文件（任意语言，shebang 决定）。启动时 spawn，握手：

```jsonc
→ {"id":1,"type":"initialize","params":{"version":"x","cwd":"...","config":{}}}
← {"id":1,"result":{"tools":[{"name","description","parameters":{...schema}}],
                    "commands":["stats"],"events":["tool_call","turn_end"]}}
```

- 之后走 newline-delimited JSON（与被删的 mcp.rs 同型：writer/reader 线程 +
  id 关联 pending map，骨架回收复用）：
  - 事件：`{"type":"event","name":"tool_call","params":{...}}`，
    `tool_call` 是请求——扩展可回 `{"decision":"deny","reason":...}` 或
    `{"args":...}` 改写；其余事件只通知。
  - 工具调用：`{"id":N,"type":"call_tool","name","args"}` → `{"id":N,"result"}`（50KB 上限沿用）。
  - 命令：`{"id":N,"type":"run_command","name","args"}` → 文本回显进 REPL。
  - 退出：发 `shutdown`，关 stdin，等进程退；超时 SIGKILL。
- 超时与降级：事件 5s、工具调用 120s（config 可调）；超时杀进程、本会话禁用并 dim 提示。
  stdout 只走协议，stderr 按行 dim 回显（扩展日志）。
- 开关：config `extensions.disabled: [name]`；`/reload` 全部 respawn。
- `LLM_AGENT_DEPTH` 与子代理栈随 sub-agents 一起删。

### 4.3 其余扩展面（纯数据，无需进程）

- **skills**：SKILL.md 机制保留（= the reference skills）。
- **prompt 模板**：`.llm/commands/*.md` 机制保留但改叫 prompts（`~/.llm/prompts/`
  + `.llm/prompts/` 向上走），`/templatename` 展开；core/templates.rs 的替换引擎
  留作内部件。

### 4.4 包（the reference packages 的 git-only 版）

- `llm install git:github.com/user/repo[@ref]` → clone 到 `~/.llm/pkg/<name>`
  （`-l` 装到 `.llm/pkg/`，项目优先）。无 update 命令：重跑 install 刷新。
- `llm remove <name>`、`llm list`（列出包及其携带的 skills/prompts/extensions）。
- 包布局即目录约定：`skills/` `prompts/` `extensions/`，无需 manifest 文件。
- MCP 不回核心：被删的 mcp.rs 逻辑未来以扩展包形式外部复活（the reference 文档同款说法）。

## 5. 存储与会话

- `threads/` 每线程一个 `<ulid>.jsonl` 不变；`--no-session` 时不建文件。
- session 树不做（/tree 用简化版截断续接）；`--fork` 保留 fork_thread 语义。
- config.json 收缩为：`providers`、`models{default,thinking,aliases}`、
  `agent{approval,skills}`、`extensions{disabled}`。`tools`/`mcpServers` 表删除，
  `logging` 布尔删除（session 总是存，--no-session 控制）。
- `history.jsonl`、`LLM.md` 不变。

## 6. 砍除与落地清单（提交序）

每个独立成 commit（imperative、lowercase），README 的 `-h` 块与 CLAUDE.md 同步刷。

1. `remove media output and image/tts model kinds` —— media.rs、catalog 两项、--out。
2. `remove structured output schemas` —— schemas.rs、--schema、DSL。
3. `remove sub-agents` —— task.rs、生命周期六工具、subagents/ 存储、LLM_AGENT_DEPTH、--mode json。
4. `remove the prompt command and template CLI` —— commands/prompt.rs、-t/--save 旗标、
   logs 的 prompt 半区、logging 布尔；commands-dir 保留待改名为 prompts。
5. `simplify the approval matrix` —— 去 danger table/root 硬封/`a` 降级细节，留 tier+ask/yolo。
6. `make the agent the only command` —— bare llm = REPL，one-shot，-c/-r/--session/--fork/--no-session；
   删 prompt/logs/agent 子命令。
7. `move provider lifecycle into the repl` —— /model /thinking /login /logout（models.rs/login.rs 库函数化）。
8. `add session commands` —— /clear /status /resume /tree /export /compact [prompt] /settings /reload。
9. `replace mcp and script tools with the extension host` —— 删 mcp.rs/script_tool.rs/user_tools.rs，
   同 commit 落 agent/ext.rs 握手 + 工具注册 + 审批接线。
10. `add extension events` —— tool_call 门禁等事件语义。
11. `add extension slash commands`。
12. `add git package management` —— install/remove/list。

依赖红线检查：以上全部落在现有 crate 内；ext.rs 复用 mcp.rs 的 stdio 骨架思路，
不引任何新 crate。

## 7. 与 the reference 的已知差距（明示，不藏）

- 扩展出进程：无 UI 组件/自定义编辑器/全局热键，事件粒度到 turn/tool 边界而非 keystroke。
- 无 npm 包生态：包只有 git 一条路。
- 无订阅 OAuth（明确不要）。
- 无主题系统（明确不要，维持近单色 deviation）。
- 无 update/telemetry/share 命令面。
