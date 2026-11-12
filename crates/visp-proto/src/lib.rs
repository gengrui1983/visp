//! visp-proto —— visp 系统的 gRPC 协议定义层。
//!
//! 本 crate 通过 tonic-build 在编译时将 `proto/visp.proto` 自动生成 Rust 代码，
//! 定义了 CLI 与 Daemon 之间的 gRPC 服务 [`CoderDaemon`] 及双向流 Chat 协议。
//!
//! 生成的代码包含：
//! - 请求/响应消息类型（Session、StatusUpdate 等）
//! - gRPC server/client trait（`coder_daemon_server::CoderDaemon` /
//!   `coder_daemon_client::CoderDaemonClient`）

// tonic 生成的代码中所有 gRPC 方法返回 Result<_, tonic::Status>，
// 而 tonic::Status 体积较大（约 176 字节），会触发 clippy 的
// result_large_err lint（CI 以 -D warnings 运行）。生成的代码
// 无法修改，此处对该模块整体豁免该 lint。
// clippy::double_must_use：新版 clippy 对 async_trait 展开的 Pin<Box<dyn Future>> 误报。
#[allow(clippy::result_large_err, clippy::double_must_use)]
pub mod visp {
    tonic::include_proto!("visp");
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    #[test]
    fn status_update_default_view_only_is_false() {
        let status = visp::StatusUpdate::default();
        assert!(
            !status.view_only,
            "view_only should default to false in proto3"
        );
    }

    #[test]
    fn status_update_with_view_only_true() {
        let status = visp::StatusUpdate {
            view_only: true,
            ..Default::default()
        };
        let encoded = status.encode_to_vec();
        let decoded = visp::StatusUpdate::decode(encoded.as_slice()).unwrap();
        assert!(
            decoded.view_only,
            "view_only should be true after round-trip"
        );
    }

    // ============ /reload 协议（步骤 1a）============

    /// ReloadConfig 请求/响应两个消息类型已生成且字段可构造。
    #[test]
    fn reload_config_messages_constructible() {
        let request = visp::ReloadConfigRequest {};
        // 空请求序列化后应为空字节。
        assert!(request.encode_to_vec().is_empty(), "空请求应编码为空字节");

        let response = visp::ReloadConfigResponse {
            results: vec![visp::reload_config_response::Item {
                category: "rules".to_string(),
                success: true,
                message: "3 个规则文件".to_string(),
                added: 1,
                modified: 1,
                deleted: 0,
                skipped: 1,
            }],
        };
        let encoded = response.encode_to_vec();
        let decoded = visp::ReloadConfigResponse::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.results.len(), 1);
        let item = &decoded.results[0];
        assert_eq!(item.category, "rules");
        assert!(item.success);
        assert_eq!(item.message, "3 个规则文件");
        assert_eq!(
            (item.added, item.modified, item.deleted, item.skipped),
            (1, 1, 0, 1)
        );
    }

    /// 生成的 client 与 server trait 均暴露 reload_config 方法。
    #[test]
    fn service_traits_expose_reload_config() {
        // 编译期断言：server trait 具备 reload_config 方法（函数体类型检查即可，
        // 无需实际调用）。
        // `tonic::Status` 为 tonic 约定的大 Err 变体，无法装箱。
        #[allow(dead_code, clippy::result_large_err)]
        async fn assert_server_has_reload_config<S: visp::coder_daemon_server::CoderDaemon>(
            service: &S,
            request: tonic::Request<visp::ReloadConfigRequest>,
        ) -> std::result::Result<tonic::Response<visp::ReloadConfigResponse>, tonic::Status>
        {
            service.reload_config(request).await
        }

        // 编译期断言：client 具备 reload_config 方法。
        #[allow(dead_code)]
        fn assert_client_has_reload_config<T>(
            client: &mut visp::coder_daemon_client::CoderDaemonClient<T>,
        ) where
            T: tonic::client::GrpcService<tonic::body::Body>,
            T::Error: Into<tonic::codegen::StdError>,
            T::ResponseBody: tonic::codegen::Body<Data = tonic::codegen::Bytes> + Send + 'static,
            <T::ResponseBody as tonic::codegen::Body>::Error: Into<tonic::codegen::StdError> + Send,
        {
            let future = client.reload_config(visp::ReloadConfigRequest {});
            drop(future);
        }

        // 真正的验证发生在上述两个内部函数的编译期类型检查；
        // 运行时无需断言（避免 always-true 断言）。
    }

    // ============ GetHookStats 协议（步骤 1b-3a）============

    /// `GetHookStats` 请求/响应消息已生成、字段可构造且 round-trip。
    #[test]
    fn get_hook_stats_messages_round_trip() {
        let request = visp::GetHookStatsRequest {
            project: "/tmp/proj".to_string(),
        };
        let decoded =
            visp::GetHookStatsRequest::decode(request.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded.project, "/tmp/proj");

        // 请求可空：缺省（无 project）编码为空字节。
        assert!(
            visp::GetHookStatsRequest::default()
                .encode_to_vec()
                .is_empty(),
            "空请求应编码为空字节"
        );

        let response = visp::HookStatsResponse {
            emitted: 1,
            dropped: 2,
            executed: 3,
            failed: 4,
            timed_out: 5,
            rules: vec![visp::HookRuleSummary {
                id: "r1".to_string(),
                scope: "global".to_string(),
                enabled: true,
            }],
        };
        let decoded = visp::HookStatsResponse::decode(response.encode_to_vec().as_slice()).unwrap();
        assert_eq!(
            (
                decoded.emitted,
                decoded.dropped,
                decoded.executed,
                decoded.failed,
                decoded.timed_out,
            ),
            (1, 2, 3, 4, 5)
        );
        assert_eq!(decoded.rules.len(), 1);
        assert_eq!(decoded.rules[0].id, "r1");
        assert_eq!(decoded.rules[0].scope, "global");
        assert!(decoded.rules[0].enabled);
    }

    /// 生成的 client 与 server trait 均暴露只读 get_hook_stats 方法。
    #[test]
    fn service_traits_expose_get_hook_stats() {
        // 编译期断言：server trait 具备 get_hook_stats 方法。
        // `tonic::Status` 为 tonic 约定的大 Err 变体，无法装箱。
        #[allow(dead_code, clippy::result_large_err)]
        async fn assert_server_has_get_hook_stats<S: visp::coder_daemon_server::CoderDaemon>(
            service: &S,
            request: tonic::Request<visp::GetHookStatsRequest>,
        ) -> std::result::Result<tonic::Response<visp::HookStatsResponse>, tonic::Status> {
            service.get_hook_stats(request).await
        }

        // 编译期断言：client 具备 get_hook_stats 方法。
        #[allow(dead_code)]
        fn assert_client_has_get_hook_stats<T>(
            client: &mut visp::coder_daemon_client::CoderDaemonClient<T>,
        ) where
            T: tonic::client::GrpcService<tonic::body::Body>,
            T::Error: Into<tonic::codegen::StdError>,
            T::ResponseBody: tonic::codegen::Body<Data = tonic::codegen::Bytes> + Send + 'static,
            <T::ResponseBody as tonic::codegen::Body>::Error: Into<tonic::codegen::StdError> + Send,
        {
            let future = client.get_hook_stats(visp::GetHookStatsRequest::default());
            drop(future);
        }

        // 验证发生在上述内部函数的编译期类型检查；运行时无需断言。
    }

    /// oneof 回归：ClientMessage / ServerMessage 的 oneof 变体集合与改动前一致（防误动）。
    #[test]
    fn oneof_variant_sets_unchanged() {
        use visp::client_message::Payload as ClientPayload;
        use visp::server_message::Payload as ServerPayload;

        // 穷尽匹配即编译期断言：新增或删除变体都会导致编译失败。
        fn client_variant(payload: &ClientPayload) -> &'static str {
            match payload {
                ClientPayload::UserInput(_) => "user_input",
                ClientPayload::ConfigUpdate(_) => "config_update",
                ClientPayload::UserResponse(_) => "user_response",
                ClientPayload::Cancel(_) => "cancel",
                ClientPayload::Ack(_) => "ack",
                ClientPayload::JoinSession(_) => "join_session",
            }
        }

        fn server_variant(payload: &ServerPayload) -> &'static str {
            match payload {
                ServerPayload::TextDelta(_) => "text_delta",
                ServerPayload::ToolCall(_) => "tool_call",
                ServerPayload::ToolResult(_) => "tool_result",
                ServerPayload::StatusUpdate(_) => "status_update",
                ServerPayload::Error(_) => "error",
                ServerPayload::Done(_) => "done",
                ServerPayload::UserQuery(_) => "user_query",
                ServerPayload::ThinkingBlock(_) => "thinking_block",
                ServerPayload::UsageInfo(_) => "usage_info",
                ServerPayload::UserMessage(_) => "user_message",
                ServerPayload::ImageBlock(_) => "image_block",
                ServerPayload::ImageError(_) => "image_error",
                ServerPayload::UsageDelta(_) => "usage_delta",
                ServerPayload::VerificationUpdate(_) => "verification_update",
            }
        }

        let client_variants = [
            ClientPayload::UserInput(Default::default()),
            ClientPayload::ConfigUpdate(Default::default()),
            ClientPayload::UserResponse(Default::default()),
            ClientPayload::Cancel(Default::default()),
            ClientPayload::Ack(Default::default()),
            ClientPayload::JoinSession(Default::default()),
        ];
        let client_names: Vec<_> = client_variants.iter().map(client_variant).collect();
        assert_eq!(
            client_names,
            [
                "user_input",
                "config_update",
                "user_response",
                "cancel",
                "ack",
                "join_session",
            ]
        );

        let server_variants = [
            ServerPayload::TextDelta(Default::default()),
            ServerPayload::ToolCall(Default::default()),
            ServerPayload::ToolResult(Default::default()),
            ServerPayload::StatusUpdate(Default::default()),
            ServerPayload::Error(Default::default()),
            ServerPayload::Done(Default::default()),
            ServerPayload::UserQuery(Default::default()),
            ServerPayload::ThinkingBlock(Default::default()),
            ServerPayload::UsageInfo(Default::default()),
            ServerPayload::UserMessage(Default::default()),
            ServerPayload::ImageBlock(Default::default()),
            ServerPayload::ImageError(Default::default()),
            ServerPayload::UsageDelta(Default::default()),
            ServerPayload::VerificationUpdate(Default::default()),
        ];
        let server_names: Vec<_> = server_variants.iter().map(server_variant).collect();
        assert_eq!(
            server_names,
            [
                "text_delta",
                "tool_call",
                "tool_result",
                "status_update",
                "error",
                "done",
                "user_query",
                "thinking_block",
                "usage_info",
                "user_message",
                "image_block",
                "image_error",
                "usage_delta",
                "verification_update",
            ]
        );
    }
}
