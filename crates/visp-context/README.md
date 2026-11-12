# visp-context — 自适应上下文管理

## 职责

在有限上下文窗口内保留“继续工作所需的信息”，而不是简单丢弃历史。
完整设计见 `docs/design/2026-11-12-context-management.md`。

## 核心能力

### 1. 更准的 token 估算

- 按字符类别估算：ASCII 字母/数字 4 字符/token，ASCII 标点 2 字符/token，
  CJK/emoji 1 字符/token，换行单独计数。
- 每条消息 +3 协议开销；每个 tool call +4 结构开销；每张图片 +1024 预留。
- 工具定义（name/description/schema）计入 system overhead。
- 用 provider 上报的 `UsageInfo.input_tokens` 做**会话级校准**：
  `calibration = max(previous, actual/estimated)`，只向上、限幅 [1.0, 6.0]。
  不依赖模型名称。

### 2. 预算与滞后区间

```
output_reserve = max(max_tokens, min_output_tokens)（≤ 窗口 75%）
available = max_context_tokens − output_reserve − system/tool overhead
budget = available × (1 − safety_margin) / calibration
trigger = budget × compact_trigger_pct   // 默认 80%
target  = budget × compact_target_pct    // 默认 45%
```

达到 trigger 才压缩；压缩到 target，避免每轮抖动。

### 3. 结构化压缩（非丢弃）

```
HEAD（默认 2 轮） | 压缩摘要（system） | TAIL（默认 6 轮）
```

摘要保留：

- Goal（首条用户消息）
- Next step（最后一条 assistant 文本）
- Decisions / Direction changes / Dead ends / Confirmed facts
- Work already done：去重的工具调用日志（`grep(...) (x3) -> error: ...`），
  直接抑制重复调用
- 已持久化的任务记忆

同时：

- 工具输出超过配置字符数时保留前 60% + 后 40%，日志尾部错误不丢。
- 仍超预算时逐级减少 TAIL/HEAD、硬截断，最终保留 goal + summary + last user。
- 输出前修复孤立的 tool call/result，避免 provider 400。

### 4. 持久任务记忆

- `MemoryKind`：goal / fact / decision / dead_end / pivot / work / next
- 压缩时自动抽取并写入 `MemoryStore`（SQLite 表 `context_memory`，与会话库
  同文件、级联删除、幂等）。
- `memory` 工具支持 `record` / `search` / `list`，agent 压缩后仍可查回。
- 无存储/写入失败时降级为内存，不阻塞对话。

### 5. 服务端超长自救

`LlmError::is_context_length_exceeded` 识别 400/413/422 的中英文超长文案。
agent loop 会提高压缩压力、强制压缩、重建 prompt 并重试（默认 2 次），
而不是让会话直接失败。

## 使用

```rust
use std::sync::Arc;
use visp_context::{ContextConfig, DefaultContextTrimmer};

let trimmer = DefaultContextTrimmer::new(
    ContextConfig::default(),
    Some(memory_store), // Arc<dyn visp_core::memory::MemoryStore>
);

// 由 PromptBuilder / agent loop 调用
let trimmed = trimmer.trim_for_session(
    Some(&session_id),
    &history,
    max_context_tokens,
    system_overhead, // 含工具定义
    output_tokens,
);

// provider 返回 usage 后校准
trimmer.observe_usage(Some(&session_id), estimated_input, actual_input);
// 服务端报上下文超长后
trimmer.on_context_overflow(Some(&session_id));
```

## 配置

`daemon.toml` 的 `[agent.context]` 全部可配置且有默认值，详见设计文档 §8。

## 架构

```
visp-core (ContextTrimmer trait / MemoryStore trait / token estimator)
    ↑
visp-context (DefaultContextTrimmer / compaction / calibration)
    ↑
visp-daemon (注入 SQLite memory store、注册 memory 工具)
```
