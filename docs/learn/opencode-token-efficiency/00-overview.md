# OpenCode 1.18.31 vs 2.0.9：为什么同样的问题 v2 更省 token

> 分析对象：两份源码快照 `../opencode-1.18.31`（无 `.git`）、`../opencode-2.0.9`
> 分析方式：直接源码比对（子 agent 委派因 API key 失效不可用）
> 结论日期：2026-09

## 0. 结论速览

v2 更省 token 不是单一改动，而是**六处机制叠加**。按影响排序：

| # | 机制 | 1.18.31 | 2.0.9 | 性质 |
|---|---|---|---|---|
| A | System prompt | 单模型大 prompt（8–15KB）整段发 | 756B 基座 + 按工具条件插入 + 极小 provider 补丁 | 真实减少 |
| B | 工具 schema | 所有工具逐个暴露 | 长尾工具收进单个 CodeMode `execute` | 真实减少 |
| C | 指令文件 | 每个 step 重发 AGENTS.md 全文 | 初始一次 + 之后只发增量 | 真实减少 |
| D | 压缩 | 粗估触发、每次重总结全文 | 真实 usage 锚点、增量 checkpoint、provider 原生窗口、丢旧图片 | 真实减少 |
| E | 缓存 | 分散、条件式 | 默认 `auto`、4 断点、effort 不打断缓存 | 降成本为主 |
| F | 历史读取 | 全量 | 只从最近压缩边界读 | 真实减少 |

> 前四项 + F 是**真实减少发送量**；E 是**降低计费与"非缓存 input"观感**。G（工具输出治理）为次要项。

---

## A. System prompt 断崖式缩小

### v1
`packages/opencode/src/session/system.ts` 按模型选一个**整段** prompt 文件，位于
`packages/opencode/src/session/prompt/`：

| 模型 | 文件 | 大小 |
|---|---|---|
| Claude | `anthropic.txt` | 8.2 KB |
| GPT | `gpt.txt` | 9.3 KB |
| Gemini | `gemini.txt` | 15.4 KB |
| Copilot GPT-5 | `copilot-gpt-5.txt` | 14.2 KB |
| Beast | `beast.txt` | 11.0 KB |

内容含大量 TodoWrite 示例、示例对话、工具策略长文。

### v2
- 基座：`packages/core/src/session/runner/prompt/system.txt`，仅 **756 B**。
- 动态拼装：`packages/core/src/session/system-prompt.ts` 只在对应工具存在时插入条件指引：
  - `shell` → 2 行（优先专用工具、禁止用分隔符串命令）
  - `write` → 1 行
  - `edit` → 1 行
- provider 差异 → `packages/core/src/plugin/system-prompt/*.txt`，其中 `anthropic.txt` 只有 **300 B**（v1 为 8212 B）。

**效果**：Anthropic 场景 system 部分约 `2K tokens → 0.3K tokens`，每一轮都省。

---

## B. 工具目录收拢（CodeMode 默认开启）

### v1
`packages/opencode/src/tool/registry.ts`：内建工具、MCP 工具、插件工具**各自一份 JSON schema** 发给模型；
工具描述来自 `tool/*.txt`（合计约 15KB）。CodeMode 需 `flags.experimentalCodeMode` 才启用，且只包 MCP。

### v2
`packages/core/src/tool.ts:234-262`：
- `codemode !== false` 的工具（MCP、browser、opencode 管理、插件等）全部收进**单个 `execute` 工具**的 catalog；
- 只有 12 个核心工具直连（`options: { codemode: false }`）：
  `read`、`write`、`edit`、`glob`、`grep`、`shell`、`patch`、`question`、`skill`、`subagent`、`webfetch`、`websearch`。

**效果**：接入 MCP server / 浏览器工具后，tool schema 从「N 份」变「1 份」，一次省数千 token。

---

## C. 指令文件不再每轮重发

### v1
`packages/opencode/src/session/prompt.ts:1257-1269`：**每个 LLM step** 都调用 `instruction.system()`，
把 AGENTS.md / CLAUDE.md / CONTEXT.md / `config.instructions` / 远程 URL **全文**塞进 system 数组。
`instruction.ts` 中的 `claims` 只对「read 时发现的下级目录指令文件」去重，全局指令集不去重。

### v2
`packages/core/src/session/instruction-state.ts`：
- 每个 session 存基线（`previous`/`current` values）；
- 初始同步只发一次；
- 之后只发 **diff 增量**（`Instructions.renderUpdate`，见 `instructions/index.ts:171`）。

**效果**：长会话里 AGENTS.md 不再每轮重复。这对"规则文件很大"的项目尤为关键。

---

## D. 压缩（compaction）机制全面升级

### v1（`packages/opencode/src/session/compaction.ts`）
- 溢出判断：触发用上一条 assistant 的真实 usage（`session/overflow.ts:22-34`）；
  `Token.estimate(JSON.stringify(msgs))` 的粗估用于尾部预算选择与溢出恢复预检
  （`compaction.ts:215-221`）。（勘误详见 04 篇 §0。）
- 预算：`usable = context - min(20_000, maxOutput)`（`session/overflow.ts`）。
- 每次压缩对 head **重新总结**；旧工具输出 prune 成 `[Old tool result content cleared]`。
- 常量：`PRUNE_MINIMUM=20_000`、`PRUNE_PROTECT=40_000`、`TOOL_OUTPUT_MAX_CHARS=2_000`。

### v2（`packages/core/src/session/compaction.ts`）
1. **真实用量锚点**：`estimateTokens` 取最后一条 assistant 的 provider 真实 usage
   （`input + cache.read + cache.write + output + reasoning`）作锚，只对锚之后本地估算（`:174-208`）
   → 触发时机贴合真实用量，不会过早/过晚压缩。
2. **增量 checkpoint**：复用已有摘要做
   "Update the existing checkpoint… Newer history takes precedence"（`:372-385`），
   而非每次重写全部历史。
3. **Provider-native compaction**：插件注册 `native` 策略，由 provider 直接返回**加密 checkpoint** 替换窗口
   （`providerContext`，`:501-593`）；旧历史完全不进本地总结，下个请求可跳过。
4. **丢旧图片**：`boundImages`（`session/model-request.ts:134-174`）超阈值把历史图片换成
   `IMAGE_REMOVED` 文本 → 截图多的会话省巨量 token。
5. `retainUsers` 只保留完整真实用户消息，不含合成引导（`:233-256`）。
6. 摘要模板结构化 + 严格"简洁、不复述、删除已解决问题"规则；输出不合法时二次提醒重试。
- 常量：`DEFAULT_BUFFER=20_000`、`DEFAULT_KEEP_TOKENS=15_000`、`OUTPUT_TOKEN_MAX=32_000`、
  `TOOL_OUTPUT_MAX_CHARS=2_000`、`IMAGE_TOKEN_ESTIMATE=1_500`、`PDF_TOKEN_ESTIMATE=2_000`。

---

## E. 缓存从"零散"变"默认集中"

### v2
- `packages/ai/src/cache-policy.ts`：默认策略 `"auto"`，自动放 **4 个缓存断点**
  （`:17-21, 54-135`）：
  1. 最后一个 tool 定义
  2. 首个 system part
  3. 末尾 system part
  4. 会话尾部消息
- `packages/ai/src/effort-updates.ts`：改 reasoning effort 时用 **per-message 标记**，
  而不是改顶层参数，专门避免"打断 prompt cache"。

### v1
`packages/opencode/src/provider/transform.ts:applyCaching` 是条件式、分散的。

**效果**：这条主要降低**计费/有效成本**——cache read 约 10% 价格，长会话体感"省很多"。
v2 还把 `cache.read` / `cache.write` 分开统计（`SessionUsage`），
所以统计界面里「非缓存 input tokens」更小，这正是"看起来更省 token"的观感来源之一。

---

## F. 历史按压缩边界读取

v2 `packages/core/src/session/history.ts:28-90`：只从最近一次成功 compaction 的 `seq` 之后读消息，
不会把旧历史重新拉进请求。

---

## G. 工具输出治理（次要）

v2 `packages/core/src/tool-output.ts`：
- 统一 `MAX_LINES=2_000` / `MAX_BYTES=50 KiB`；
- 超限时把全文落盘到 `tool-output/` 目录，上下文里只留一句
  `... N bytes truncated; full content saved to <path> ...`，模型需要时再 read；
- 7 天自动清理（`file-retention.ts`）。
- 额外做空输出归一（`(no tool output)`）、缺失 tool result 补齐（`ai/src/tool-history.ts`），减少噪声 token。

v1 有类似机制（`tool/truncation-dir.ts` + `outputPath`），但未统一、未做噪声归一。

---

## 验证方法（建议）

同一问题在两版各跑一次，对比：

1. **首轮请求**的 `system + tools` token 量 → 验证 A/B（预计 v2 少 2–5K）。
2. **每轮 usage** 的 `input / output / cache.read / cache.write` 四项分别看
   → 区分"真实少"（D/F）与"计费少"（E）。
3. **带一个 MCP server** 再跑 → 验证 B 的 tool schema 变化。
4. **长会话（>20 轮）** → 验证 C/D，差距会明显拉大。

---

## 一句话总结

v2 省 token = **更小的系统提示与工具目录** + **指令/历史的增量与边界化** +
**更准的压缩（含 provider 原生窗口）** + **默认开启的缓存**。
前四项是真实减少发送量，E 是降低计费与"非缓存 input"的观感。
