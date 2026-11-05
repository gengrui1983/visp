# 项目代码行数统计

> 生成时间：2026-11-05；统计范围：排除 `target/`、`.git/`、`assets/`、`node_modules/`

## 总览

**代码文件总数：209　总行数：102,136**

| 语言 | 行数 | 占比 |
|---|---:|---:|
| Rust | 100,700 | 98.6% |
| TOML | 850 | 0.8% |
| Shell | 586 | 0.6% |

## 按模块（crate）统计

| 模块 | 文件数 | 行数 | 占比 |
|---|---:|---:|---:|
| visp-daemon | 25 | 17,059 | 16.7% |
| visp-tui | 20 | 16,044 | 15.7% |
| visp-core | 19 | 14,848 | 14.5% |
| visp-llm | 21 | 8,840 | 8.7% |
| visp-config | 9 | 7,796 | 7.6% |
| visp-agent | 13 | 7,355 | 7.2% |
| visp-codegraph | 15 | 5,905 | 5.8% |
| visp-tools | 18 | 5,167 | 5.1% |
| visp-acp | 11 | 3,977 | 3.9% |
| visp-hooks | 11 | 3,090 | 3.0% |
| visp-mcp | 10 | 2,981 | 2.9% |
| visp-db | 10 | 2,230 | 2.2% |
| visp-fs | 10 | 2,192 | 2.1% |
| visp | 3 | 1,476 | 1.4% |
| visp-command | 5 | 991 | 1.0% |
| visp-context | 2 | 804 | 0.8% |
| （根目录） | 3 | 643 | 0.6% |
| docs | 1 | 430 | 0.4% |
| visp-proto | 3 | 308 | 0.3% |

## 文件明细（按行数降序）

| 文件 | 语言 | 行数 |
|---|---|---:|
| `crates/visp-core/src/agent_loop.rs` | Rust | 7,165 |
| `crates/visp-daemon/src/service.rs` | Rust | 6,279 |
| `crates/visp-config/src/config.rs` | Rust | 4,517 |
| `crates/visp-agent/src/orchestrator_tests.rs` | Rust | 4,100 |
| `crates/visp-tui/src/app_tests.rs` | Rust | 3,628 |
| `crates/visp-core/src/agent.rs` | Rust | 3,095 |
| `crates/visp-llm/src/anthropic.rs` | Rust | 2,978 |
| `crates/visp-llm/src/openai_tests.rs` | Rust | 2,710 |
| `crates/visp-tui/src/app.rs` | Rust | 2,183 |
| `crates/visp-llm/src/openai.rs` | Rust | 1,721 |
| `crates/visp-tui/src/ui.rs` | Rust | 1,659 |
| `crates/visp-agent/src/orchestrator.rs` | Rust | 1,498 |
| `crates/visp-tui/src/event.rs` | Rust | 1,494 |
| `crates/visp-tui/src/tool_ui.rs` | Rust | 1,318 |
| `crates/visp-daemon/src/observability/parent_link.rs` | Rust | 1,249 |
| `crates/visp-daemon/src/hook_runtime.rs` | Rust | 1,248 |
| `crates/visp-codegraph/src/store_tests.rs` | Rust | 1,171 |
| `crates/visp-daemon/src/observability/init.rs` | Rust | 1,110 |
| `crates/visp-daemon/src/observability/metrics_layer.rs` | Rust | 1,045 |
| `crates/visp-core/src/session.rs` | Rust | 1,018 |
| `crates/visp-tui/src/image.rs` | Rust | 1,008 |
| `crates/visp-codegraph/src/parser.rs` | Rust | 967 |
| `crates/visp-tui/src/ui_tests.rs` | Rust | 954 |
| `crates/visp-codegraph/src/query_tests.rs` | Rust | 902 |
| `crates/visp/src/hook_cmd.rs` | Rust | 884 |
| `crates/visp-daemon/tests/observability_e2e.rs` | Rust | 832 |
| `crates/visp-daemon/src/main.rs` | Rust | 826 |
| `crates/visp-tui/src/event_tests.rs` | Rust | 824 |
| `crates/visp-fs/src/runtime_tests.rs` | Rust | 822 |
| `crates/visp-context/src/lib.rs` | Rust | 794 |
| `crates/visp-config/src/trust.rs` | Rust | 776 |
| `crates/visp-tools/src/file_tests.rs` | Rust | 768 |
| `crates/visp-acp/tests/integration.rs` | Rust | 748 |
| `crates/visp-config/src/rules.rs` | Rust | 729 |
| `crates/visp-acp/src/supervisor.rs` | Rust | 723 |
| `crates/visp-fs/src/runtime.rs` | Rust | 723 |
| `crates/visp-tools/src/bash_tests.rs` | Rust | 714 |
| `crates/visp-config/src/hooks.rs` | Rust | 710 |
| `crates/visp-core/src/message.rs` | Rust | 709 |
| `crates/visp-daemon/src/reload_tests.rs` | Rust | 689 |
| `crates/visp-mcp/src/http_client.rs` | Rust | 664 |
| `crates/visp-command/src/lib.rs` | Rust | 655 |
| `crates/visp-acp/src/translate.rs` | Rust | 628 |
| `crates/visp-daemon/src/watch_tests.rs` | Rust | 627 |
| `crates/visp-tools/src/codegraph.rs` | Rust | 613 |
| `crates/visp-tools/src/file.rs` | Rust | 594 |
| `install.sh` | Shell | 586 |
| `crates/visp/src/main.rs` | Rust | 574 |
| `crates/visp-codegraph/src/store.rs` | Rust | 550 |
| `crates/visp-tools/src/bash.rs` | Rust | 526 |
| `crates/visp-codegraph/src/query.rs` | Rust | 518 |
| `crates/visp-daemon/src/hook_map.rs` | Rust | 518 |
| `crates/visp-hooks/src/dispatcher.rs` | Rust | 508 |
| `crates/visp-mcp/src/manager.rs` | Rust | 506 |
| `crates/visp-acp/src/grpc.rs` | Rust | 505 |
| `crates/visp-hooks/tests/contract.rs` | Rust | 504 |
| `crates/visp-mcp/src/client.rs` | Rust | 502 |
| `crates/visp-tui/src/notify_tests.rs` | Rust | 496 |
| `crates/visp-daemon/src/watch.rs` | Rust | 494 |
| `crates/visp-agent/src/agent_loader_tests.rs` | Rust | 487 |
| `crates/visp-mcp/src/get_client.rs` | Rust | 477 |
| `crates/visp-daemon/src/reload.rs` | Rust | 465 |
| `crates/visp-db/src/schema_tests.rs` | Rust | 454 |
| `crates/visp-tools/src/search.rs` | Rust | 452 |
| `crates/visp-config/src/path.rs` | Rust | 446 |
| `crates/visp-core/src/trace_context.rs` | Rust | 441 |
| `crates/visp-db/src/message_repo_tests.rs` | Rust | 437 |
| `crates/visp-config/src/skills.rs` | Rust | 430 |
| `docs/daemon.example.toml` | TOML | 430 |
| `crates/visp-core/src/agent_definition.rs` | Rust | 424 |
| `crates/visp-acp/src/agent.rs` | Rust | 423 |
| `crates/visp-core/src/prompt.rs` | Rust | 405 |
| `crates/visp-hooks/src/event.rs` | Rust | 404 |
| `crates/visp-tui/src/notify.rs` | Rust | 403 |
| `crates/visp-tools/src/search_tests.rs` | Rust | 402 |
| `crates/visp-hooks/tests/schema_tests.rs` | Rust | 394 |
| `crates/visp-codegraph/src/parser_tests.rs` | Rust | 373 |
| `crates/visp-hooks/tests/herdr_hook_script.rs` | Rust | 370 |
| `crates/visp-agent/src/observability_tests.rs` | Rust | 361 |
| `crates/visp-codegraph/src/index.rs` | Rust | 350 |
| `crates/visp-tui/src/connection_tests.rs` | Rust | 347 |
| `crates/visp-core/src/tool.rs` | Rust | 345 |
| `crates/visp-codegraph/src/watcher_tests.rs` | Rust | 345 |
| `crates/visp-mcp/src/tool.rs` | Rust | 341 |
| `crates/visp-core/src/tool_registry.rs` | Rust | 336 |
| `crates/visp-daemon/tests/filewatch_integration.rs` | Rust | 334 |
| `crates/visp-tools/src/fetch_tests.rs` | Rust | 331 |
| `crates/visp-tui/src/connection.rs` | Rust | 328 |
| `crates/visp-acp/src/approval.rs` | Rust | 320 |
| `crates/visp-db/src/session_repo_tests.rs` | Rust | 308 |
| `crates/visp-hooks/src/spawn_handler.rs` | Rust | 308 |
| `crates/visp-tui/src/theme.rs` | Rust | 299 |
| `crates/visp-tools/src/fetch.rs` | Rust | 297 |
| `crates/visp-proto/src/lib.rs` | Rust | 285 |
| `crates/visp-acp/src/sessions.rs` | Rust | 282 |
| `crates/visp-hooks/tests/dispatcher.rs` | Rust | 272 |
| `crates/visp-hooks/tests/spawn_handler.rs` | Rust | 267 |
| `crates/visp-tui/src/selection.rs` | Rust | 264 |
| `crates/visp-daemon/src/observability/otlp.rs` | Rust | 261 |
| `crates/visp-agent/src/agent_loader.rs` | Rust | 253 |
| `crates/visp-codegraph/src/index_tests.rs` | Rust | 246 |
| `crates/visp-tui/src/main.rs` | Rust | 242 |
| `crates/visp-daemon/src/bus.rs` | Rust | 241 |
| `crates/visp-agent/src/builtin_agents.rs` | Rust | 240 |
| `crates/visp-mcp/src/config.rs` | Rust | 238 |
| `crates/visp-db/src/message_repo.rs` | Rust | 228 |
| `crates/visp-core/src/provider.rs` | Rust | 227 |
| `crates/visp-tui/src/client.rs` | Rust | 227 |
| `crates/visp-db/src/session_repo.rs` | Rust | 210 |
| `crates/visp-llm/src/aliyun.rs` | Rust | 208 |
| `crates/visp-daemon/src/shutdown.rs` | Rust | 207 |
| `crates/visp-db/src/store_tests.rs` | Rust | 204 |
| `crates/visp-llm/src/sanitize.rs` | Rust | 204 |
| `crates/visp-daemon/tests/trace_field_export.rs` | Rust | 190 |
| `crates/visp-db/src/schema.rs` | Rust | 187 |
| `crates/visp-tui/src/selection_tests.rs` | Rust | 186 |
| `crates/visp-acp/src/lib.rs` | Rust | 185 |
| `crates/visp-db/src/store.rs` | Rust | 182 |
| `crates/visp-core/src/agent_registry.rs` | Rust | 176 |
| `crates/visp-llm/src/image_util.rs` | Rust | 168 |
| `crates/visp-mcp/src/transport.rs` | Rust | 165 |
| `crates/visp-fs/src/normalize_tests.rs` | Rust | 149 |
| `crates/visp-tui/src/command.rs` | Rust | 144 |
| `crates/visp-daemon/src/bin/otel_poc.rs` | Rust | 143 |
| `crates/visp-core/src/error.rs` | Rust | 142 |
| `crates/visp-codegraph/src/watcher.rs` | Rust | 139 |
| `crates/visp-config/src/prompt.rs` | Rust | 139 |
| `crates/visp-llm/src/sanitize_tests.rs` | Rust | 136 |
| `crates/visp-core/src/provider_metadata.rs` | Rust | 132 |
| `crates/visp-acp/tests/smoke.rs` | Rust | 128 |
| `crates/visp-agent/src/active_agent.rs` | Rust | 127 |
| `crates/visp-tools/src/skill.rs` | Rust | 125 |
| `crates/visp-daemon/tests/observability_global_subscriber.rs` | Rust | 125 |
| `crates/visp-tools/src/codegraph_tests.rs` | Rust | 124 |
| `crates/visp-fs/src/target_tests.rs` | Rust | 122 |
| `crates/visp-codegraph/src/lib_tests.rs` | Rust | 119 |
| `crates/visp-command/src/init.rs` | Rust | 116 |
| `crates/visp-agent/src/active_agent_tests.rs` | Rust | 115 |
| `crates/visp-fs/src/target.rs` | Rust | 115 |
| `crates/visp-command/src/init_skill.rs` | Rust | 112 |
| `crates/visp-codegraph/src/lib.rs` | Rust | 111 |
| `crates/visp-llm/src/mock_tests.rs` | Rust | 108 |
| `crates/visp-llm/src/aliyun_tests.rs` | Rust | 105 |
| `crates/visp-command/src/init_agent.rs` | Rust | 99 |
| `crates/visp-agent/src/observability.rs` | Rust | 98 |
| `crates/visp-core/src/bus.rs` | Rust | 94 |
| `crates/visp-fs/src/degrade_tests.rs` | Rust | 90 |
| `crates/visp-daemon/tests/observability_otlp_e2e.rs` | Rust | 88 |
| `crates/visp-core/src/context.rs` | Rust | 86 |
| `crates/visp-tools/src/skill_tests.rs` | Rust | 76 |
| `crates/visp-fs/src/degrade.rs` | Rust | 76 |
| `crates/visp-fs/src/normalize.rs` | Rust | 71 |
| `crates/visp-llm/src/opencode.rs` | Rust | 62 |
| `crates/visp-mcp/src/error.rs` | Rust | 60 |
| `crates/visp-llm/src/opencode_go.rs` | Rust | 60 |
| `crates/visp-llm/src/bigmodel.rs` | Rust | 60 |
| `Cargo.toml` | TOML | 54 |
| `crates/visp-llm/src/opencode_tests.rs` | Rust | 53 |
| `crates/visp-llm/src/streaming.rs` | Rust | 52 |
| `crates/visp-codegraph/src/graph.rs` | Rust | 50 |
| `crates/visp-daemon/Cargo.toml` | TOML | 48 |
| `crates/visp-llm/src/opencode_go_tests.rs` | Rust | 46 |
| `crates/visp-codegraph/src/graph_tests.rs` | Rust | 43 |
| `crates/visp-tools/src/path_tests.rs` | Rust | 41 |
| `crates/visp-llm/src/bigmodel_tests.rs` | Rust | 41 |
| `crates/visp-llm/src/mock.rs` | Rust | 41 |
| `crates/visp-agent/examples/test_agent_reg.rs` | Rust | 37 |
| `crates/visp-llm/src/streaming_tests.rs` | Rust | 35 |
| `crates/visp-tui/Cargo.toml` | TOML | 33 |
| `crates/visp-config/src/lib.rs` | Rust | 30 |
| `crates/visp-agent/Cargo.toml` | TOML | 30 |
| `crates/visp-tools/src/truncate_tests.rs` | Rust | 29 |
| `crates/visp-hooks/src/env.rs` | Rust | 28 |
| `crates/visp-acp/Cargo.toml` | TOML | 27 |
| `crates/visp-core/src/lib.rs` | Rust | 26 |
| `crates/visp-core/Cargo.toml` | TOML | 25 |
| `crates/visp-tools/Cargo.toml` | TOML | 24 |
| `crates/visp-tools/src/path.rs` | Rust | 24 |
| `crates/visp-llm/Cargo.toml` | TOML | 22 |
| `crates/visp-codegraph/Cargo.toml` | TOML | 21 |
| `crates/visp-mcp/Cargo.toml` | TOML | 20 |
| `crates/visp-tools/src/truncate.rs` | Rust | 19 |
| `crates/visp-config/Cargo.toml` | TOML | 19 |
| `crates/visp-hooks/Cargo.toml` | TOML | 19 |
| `crates/visp-llm/src/util.rs` | Rust | 19 |
| `crates/visp/Cargo.toml` | TOML | 18 |
| `crates/visp-hooks/src/lib.rs` | Rust | 16 |
| `crates/visp-daemon/src/lib.rs` | Rust | 15 |
| `crates/visp-db/Cargo.toml` | TOML | 14 |
| `crates/visp-daemon/src/server.rs` | Rust | 14 |
| `crates/visp-proto/Cargo.toml` | TOML | 12 |
| `crates/visp-fs/Cargo.toml` | TOML | 12 |
| `crates/visp-fs/src/lib.rs` | Rust | 12 |
| `crates/visp-proto/build.rs` | Rust | 11 |
| `crates/visp-llm/src/lib.rs` | Rust | 11 |
| `crates/visp-context/Cargo.toml` | TOML | 10 |
| `crates/visp-command/Cargo.toml` | TOML | 9 |
| `crates/visp-acp/src/main.rs` | Rust | 8 |
| `crates/visp-tools/src/lib.rs` | Rust | 8 |
| `crates/visp-mcp/src/lib.rs` | Rust | 8 |
| `crates/visp-tui/src/client_tests.rs` | Rust | 7 |
| `crates/visp-daemon/src/config.rs` | Rust | 7 |
| `crates/visp-agent/src/lib.rs` | Rust | 6 |
| `crates/visp-db/src/lib.rs` | Rust | 6 |
| `crates/visp-daemon/src/observability/mod.rs` | Rust | 4 |
| `rust-toolchain.toml` | TOML | 3 |
| `crates/visp-agent/src/event_bus.rs` | Rust | 3 |
| `crates/visp-core/src/rules.rs` | Rust | 1 |
| `crates/visp-core/src/skill.rs` | Rust | 1 |

