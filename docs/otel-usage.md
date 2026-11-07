# visp 中 OpenTelemetry（OTel）使用调查

> 调查日期：2026-10-10。本文梳理 visp 仓库中所有与 OpenTelemetry 相关的代码：依赖、初始化、配置、埋点、数据流、测试与文档索引。

## 1. 总览

- OTel **仅在 `visp-daemon` 进程内启用**，通过 `tracing-opentelemetry` 把 `tracing` span 桥接为 OTel span，经 OTLP 导出（默认 gRPC/tonic，可选 HTTP）。
- `visp-agent` 仅使用 OTel API 做父链重建（`tracing-opentelemetry` + `opentelemetry`），不装配 exporter。
- 其余 crate（core/llm/tools/context/hooks/mcp/fs/config/tui/acp/launcher）**不依赖 OTel**，最多使用纯 `tracing`。
- **未使用 OTel metrics API**（无 Meter/Counter/Histogram），指标为 daemon 内自实现 `MetricsLayer` 进程内聚合 + 日志事件输出。
- 无编译期 feature flag；启停完全由运行时配置控制（`observability.enabled` 总开关 + `observability.otlp.enabled` 独立开关）。

数据链路：

```
tracing 宏（span/event）
  └─ Registry
      ├─ OpenTelemetryLayer（可选，otlp.enabled）        → OTLP exporter（gRPC 默认 / HTTP）→ Collector / Langfuse
      ├─ ParentLinkLayer（W3C span_id ↔ tracing Id 映射）
      ├─ MetricsLayer（进程内聚合，span 关闭时发 summary 事件）
      └─ fmt layer（json|pretty，文件或 stdout）
```

## 2. 依赖

由工作区根 `Cargo.toml` 的 `[workspace.dependencies]` 统一管理，各 crate 以 `{ workspace = true }` 引用：

| 依赖 | 版本/feature | 位置 |
|---|---|---|
| `opentelemetry` | 0.32 | `Cargo.toml:48` |
| `opentelemetry_sdk` | 0.32 | `Cargo.toml:49` |
| `opentelemetry-otlp` | 0.32，features：`grpc-tonic`、`http-proto`、`tls` | `Cargo.toml:50` |
| `tracing-opentelemetry` | 0.33 | `Cargo.toml:51` |
| `tracing` | 0.1 | `Cargo.toml:43` |
| `tracing-subscriber` | 0.3，features：`env-filter`、`registry`、`fmt`、`json` | `Cargo.toml:44` |

引用方：

- `crates/visp-daemon/Cargo.toml:37-40` — `opentelemetry`、`opentelemetry_sdk`（features=["testing"]，供测试用 `InMemorySpanExporter`）、`tracing-opentelemetry`、`opentelemetry-otlp`。
- `crates/visp-agent/Cargo.toml:20-21` — `tracing-opentelemetry`、`opentelemetry`；`:29` dev-dep `opentelemetry_sdk`（testing）。

注意：**未引入 `opentelemetry-semantic-conventions`**，代码中的 `gen_ai.*` 字段均为手写字符串而非该 crate 常量。

## 3. 初始化与装配

全部位于 `crates/visp-daemon/src/observability/`（`mod.rs`、`init.rs`、`otlp.rs`、`parent_link.rs`、`metrics_layer.rs`）。

### 3.1 入口

`src/main.rs:194-199` 在启动时调用：

```rust
let _observability_guard = crate::observability::init::init_observability(
    &config.observability,
    &config.daemon.log_level,
);
```

guard 持有到 `run()` 结束，靠 `Drop` 收尾。

### 3.2 subscriber 装配（`init.rs:88` `init_observability`）

装配顺序（`init.rs:167-191`）：

```
registry → OpenTelemetryLayer? → ParentLinkLayer → MetricsLayer → fmt(json|pretty)
```

- OTel layer 仅当 `observability.otlp.enabled == true` 时构建（`init.rs:148-156`），否则为 `None`。
- `OpenTelemetryLayer::new(tracer).with_context_activation(true)`（`init.rs:152`）。
- 通过 `try_init()` 安装为全局 subscriber（覆盖 tokio 工作线程，见 `observability_global_subscriber.rs` 测试）。
- `observability.enabled == false` 时直接返回 no-op guard（`init.rs:89-97`）。

### 3.3 tracer provider（`otlp.rs:20` `build_tracer_provider`）

```rust
// sample_rate <= 0.0 时走快速路径：Sampler::AlwaysOff，不建 gRPC 通道（otlp.rs:23-28）
let exporter = if uses_http_protocol(&cfg.protocol) {
    SpanExporter::builder().with_http()      // HTTP
        .with_endpoint(&cfg.endpoint)
        .with_timeout(Duration::from_secs(cfg.timeout_secs)).build()
} else {
    SpanExporter::builder().with_tonic()     // 默认 gRPC/tonic
        .with_endpoint(&cfg.endpoint)
        .with_timeout(Duration::from_secs(cfg.timeout_secs)).build()
};
// BatchSpanProcessor + Sampler::ParentBased(TraceIdRatioBased(sample_rate))
```

- 协议解析：`"http"`（大小写不敏感）走 HTTP，其余一律回退 gRPC（`otlp.rs:79-81`）。
- headers 通过环境变量 `OTEL_EXPORTER_OTLP_HEADERS` 注入（`otlp.rs:36-46`）。
- resource 属性（`otlp.rs:105-117` `build_resource`）：`service.name="visp-daemon"`、`service.version=CARGO_PKG_VERSION`、`host.name`（取 `HOSTNAME`，缺失为 `"unknown"`）、`process.pid`。
- 关停：`ObservabilityGuard::Drop`（`init.rs:296-302`）调用 `provider.shutdown()` 触发 BatchSpanProcessor flush；关停路径（`shutdown.rs`）无显式 `force_flush()`。
- 测试专用：`build_tracer_provider_with_exporter`（`otlp.rs:86`）注入 `InMemorySpanExporter` + `SimpleSpanProcessor`。
- 另有独立验证 binary `src/bin/otel_poc.rs`（不进生产）。

## 4. 配置（visp-config）

定义在 `crates/visp-config/src/config.rs`：

**`ObservabilityConfig`**（`config.rs:621-639`）

| 字段 | 默认值 |
|---|---|
| `enabled` | `true` |
| `level` | `"info"` |
| `format` | `"json"` |
| `parent_link` | `true` |
| `metrics_summary` | `true` |
| `log_file` | `Some("~/.visp/logs")` |
| `otlp` | `OtlpConfig::default()` |
| `langfuse` | `LangfuseConfig::default()` |

**`OtlpConfig`**（`config.rs:717-734`）

| 字段 | 默认值 |
|---|---|
| `enabled` | `false` |
| `endpoint` | `"http://localhost:4317"` |
| `protocol` | `"grpc"` |
| `timeout_secs` | `10` |
| `headers` | `{}` |
| `sample_rate` | `1.0`（反序列化时 clamp 到 [0.0, 1.0]，`config.rs:762-768`） |

**`LangfuseConfig`**（`config.rs:676-700`）/ `LangfuseCaptureConfig`（`config.rs:642-654`）：只控制 `langfuse.*` 语义字段是否写入 span，不含 Langfuse 连接信息（上报走通用 OTLP exporter）。

运行时开关分两级：`observability.enabled`（总开关）与 `observability.otlp.enabled`（OTel 导出独立开关）。示例见 `docs/daemon.example.toml:149-209`。

## 5. 埋点清单

所有 span 由 `tracing` 宏创建（经 OTel layer 转为 OTel span）。**宏内手写 `tracing::info_span!`，未使用 `#[instrument]`**（原因见 `visp-core/src/agent_loop.rs:1647-1649` 注释：让测试 subscriber 可见）。

agent 循环内三类 span（tool / run / iteration）统一由 **`init_span!` 宏**创建（`agent_loop.rs:153-184`，取三宏字段并集，合并自原 `init_span!/run_span!/iter_span!`）：宏内固定声明延后 record 的字段（`trace_id`/`parent_span_id`、全部 `langfuse.*`、`level`、`status_message`、`visp.tool.*`）为 `field::Empty`，公共字段 `session_id`/`session.id` 直接取自 `$sid`；调用方传入创建期已知的立即字段；展开时自动执行 langfuse 传播（`langfuse_enabled` 守卫内记录 `langfuse.observation.type="span"` 并调用 `record_langfuse_trace_fields`）。约束：调用方字段不得与宏内固定 Empty 字段集重名（对未声明字段调用 `record()` 会被静默丢弃）。

### 5.1 Span

| Span 名 | 位置 | 关键字段 |
|---|---|---|
| `visp.agent.run` | `agent_loop.rs:1668-1676`（经 `init_span!` 宏统一创建） | `session.id`、`session.short_id`、`gen_ai.provider.name`、`visp.agent.kind/depth/parent_session_id`、`visp.span.w3c_id`、`trace_id`、`parent_span_id`；langfuse 版另带 `langfuse.session.id`、`trace.name`、`user.id`、`trace.tags`、`environment`、`release`、`version`、`trace.public`、`trace.metadata`、`observation.type/input/output` |
| `visp.agent.iteration` | `agent_loop.rs:1775-1781`（经 `init_span!` 宏统一创建） | `session_id`、`visp.span.w3c_id`、`iteration.count`（+langfuse 字段） |
| `visp.subagent.spawn` | `visp-agent/src/orchestrator.rs:938-959` | `visp.subagent.name/session_id/call_id/task_id/depth`、`visp.agent.parent_session_id`、`trace_id`、`parent_span_id`、`trace_state`（+langfuse 字段）；子 agent 父链桥接 span |
| `gen_ai.client.operation` | `visp-llm/src/anthropic.rs:546-576`、`openai.rs:1509-1521` | `gen_ai.system`、`gen_ai.request.model`、`gen_ai.operation.name="chat"`、`gen_ai.request.max_tokens/temperature`、`visp.llm.attempt`、`gen_ai.usage.input_tokens/output_tokens`、`gen_ai.usage.cache_read.input_tokens`、`gen_ai.usage.cache_creation.input_tokens`、`gen_ai.response.finish_reasons/model`、`visp.llm.token_limit_hit`、`gen_ai.client.base_url`、`visp.tools.definitions`（+langfuse） |
| `visp.tool.execute` | `agent_loop.rs:1241-1247`（经 `init_span!` 宏统一创建） | `gen_ai.tool.name`、`gen_ai.tool.call.id`、`gen_ai.tool.type="function"`、`gen_ai.operation.name`、`langfuse.observation.type="span"`（langfuse 开启时）、`visp.tool.is_error/duration_ms/arguments`、`level`、`status_message`（langfuse 开启时记录）;duration 于 `agent_loop.rs:1471` 记录，span 经 `.instrument()` 进入工具任务（`:1524`） |

### 5.2 Event

| 事件 | 位置 |
|---|---|
| `visp.agent.cancelled` | `agent_loop.rs:244, 1718, 1817` |
| `visp.agent.iteration_limit` | `agent_loop.rs:271` |
| `visp.agent.completed` | `agent_loop.rs:1892` |
| `gen_ai.client.first_token` | `anthropic.rs:861`、`openai.rs:871` |
| `gen_ai.client.completed` | `anthropic.rs:969`、`openai.rs:1263` |
| `gen_ai.client.retry` | `visp-core/src/agent_loop.rs:441-456`（`call_llm_with_retry` 重试退避循环，唯一发出点，覆盖全部 provider；`reason` ∈ rate_limit/network/server_error，附 `attempt`/`delay_ms`） |
| `gen_ai.client.error`（error 级） | `anthropic.rs:720, 733`、`openai.rs:1665, 1695, 1708` |
| `visp.sse.anthropic` / `visp.sse.openai` | `anthropic.rs:1077`、`openai.rs:1116` |

### 5.3 Metrics：未使用 OTel metrics API

全仓库**没有** `opentelemetry::metrics`、`MeterProvider`、Counter/Histogram/Gauge 的使用（已 grep 验证）。替代方案是自实现的 `MetricsLayer`（`visp-daemon/src/observability/metrics_layer.rs`）：一个 `tracing_subscriber::Layer`，在 `on_close` 中按 `session.id` 进程内聚合 token/成本/工具耗时，在 `visp.agent.run` span 关闭时发一条 `tracing::info!`（`metrics_layer.rs:359-372`，`target="visp.metrics"`，`name="metrics.session.summary"`），字段含 `total_tokens_input/output`、`cache_read_tokens`、`cache_creation_tokens`、`llm_calls`、`tool_calls`、`tool_duration_ms`。**不导出 OTel metric。**

### 5.4 明确未埋点的区域

- `visp-tools`：无任何 span（仅 `tracing` 依赖，无 span 宏）。
- `visp-context`：无 tracing/span（连 `tracing` 依赖都没有）。
- `visp-hooks`：仅少量 `tracing::warn!`（`spawn_handler.rs:188,222,253,257,269`）。
- gRPC 服务层（`visp-daemon/src/service.rs`、`server.rs`）：无 span、无 `#[instrument]`、无 trace context 传播。
- `visp-llm` 内部 reqwest HTTP 客户端：无 OTel HTTP 埋点。

## 6. Trace Context 传播

- **类型**：`visp-core/src/trace_context.rs`（W3C `trace_id`/`span_id`/`trace_flags`/`trace_state` + visp 内部 `parent_span_id`；`:37-50`）。该文件刻意不依赖 tracing/opentelemetry（`:5-6`），保证 core 层纯净。
- **进程内跨边界**：`Envelope.trace_context`（`visp-core/src/agent.rs:250`）与 `AgentMessage::SpawnRequest.trace_context`（`agent.rs:225`）随 mpsc 消息传递；子 agent 工具从 `context.visp_trace_id` + `context.iter_span_w3c_id` 构造（`agent.rs:331-349`）。
- **OTel 父链重建**：orchestrator 收到 SpawnRequest 时优先用其携带的 TraceContext，缺失才 `extract_trace_context()`（`visp-agent/src/orchestrator.rs:361-362`）；`spawn_sub_agent` 用 `rebuild_parent_context`（`visp-agent/src/observability.rs:75-94`）构造远端 SpanContext，并在 `span.enter()` 前 `set_parent`（`orchestrator.rs:964-969`）。
- **`extract_trace_context`**：从当前 OTel span 提取；OTel 未激活时回退 UUID（`visp-agent/src/observability.rs:34-64`）。
- **ParentLinkLayer**（`visp-daemon/src/observability/parent_link.rs:70-115`）：维护 W3C span_id ↔ tracing `span::Id` 双向映射；otel 模式下 `on_enter` 读真实 OTel span ID 写入 `visp.span.w3c_id`（替换 uuid 占位）。
- **跨进程边界（TUI ↔ daemon）无传播**：`crates/visp-proto/proto/visp.proto` 无 trace/traceparent 字段，gRPC 调用间不注入/提取 trace context。传播仅发生在 daemon 进程内部。

前端边界：

- **TUI**（`visp-tui`）：纯 `tracing` 调用，无 OTel 依赖，也未安装 subscriber。
- **ACP**（`visp-acp`）：`tracing_subscriber::fmt()` 输出到 stderr（`visp-acp/src/lib.rs:54-61`），无 OTel；stdout 保留给协议消息。
- **launcher（`visp`）**：无 OTel。

## 7. 测试与文档索引

测试：

- `crates/visp-daemon/tests/observability_e2e.rs` — 端到端 subscriber 栈 + 真实 `run_agent_loop` 的 JSON 断言（span 树、trace_id 传播）。
- `crates/visp-daemon/tests/observability_otlp_e2e.rs` — OTLP collector 不可达时优雅降级。
- `crates/visp-daemon/tests/observability_global_subscriber.rs` — 全局 subscriber 跨 tokio 线程捕获。
- `crates/visp-daemon/tests/trace_field_export.rs` — deferred `span.record()` 字段传入 OTel span attributes。
- `crates/visp-agent/src/observability_tests.rs` — `extract_trace_context`/`rebuild_parent_context`/`set_parent`。
- 单元测试：`init.rs`（装配/开关/格式）、`otlp.rs`（协议/采样/resource）、`parent_link.rs`、`metrics_layer.rs`；`orchestrator_tests.rs`（`visp.subagent.spawn` 父链）；`config.rs:2512-2653`（`OtlpConfig` 默认值/反序列化/sample_rate 夹取）。

文档：

- `docs/otel-langfuse-trace-fields.md` — 导出的 trace/span 结构与 Langfuse 字段说明。
- `docs/design/visp-design-langfuse-otel-compatibility.md` + `docs/plans/visp-plan-langfuse-otel-compatibility.md` — 兼容设计与实施计划。
- `docs/otel-collector-langfuse.example.yaml` — Collector 参考配置（visp 不管理 Collector）。
- `docs/daemon.example.toml:149-209` — 可观测性配置示例。
- `README.md:240, 466` — OTel 集成特性与技术栈描述。

## 8. 已知边界与限制

1. 仅导出 traces，不导出 metrics/logs（metrics 为进程内聚合 + 日志事件）。
2. TUI ↔ daemon 的 gRPC 调用间无 trace context 传播（proto 无相应字段）。
3. gRPC 服务层、工具执行内部、HTTP 客户端均无 span 埋点，可见性止于 agent 循环/LLM 请求/子 agent 三层。
4. 关停时依赖 guard Drop flush，无显式 `force_flush()`。
5. `gen_ai.*` 字段手写，未使用 `opentelemetry-semantic-conventions` crate。
