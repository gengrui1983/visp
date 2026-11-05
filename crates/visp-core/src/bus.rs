//! 总线事件载荷与发布契约（设计 §5 D3 v3.1）。
//!
//! 总线载荷泛化为 [`BusEvent`]：显示域（TUI 消费）与 hook 域（hook 执行器消费）
//! 共用同一总线。`AgentEvent` 保持纯显示语义。

use crate::agent::AgentEventFrame;

/// 总线事件载荷。
#[derive(Clone)]
pub enum BusEvent {
    /// 显示域事件帧，由 TUI 消费。
    Frame(AgentEventFrame),
    /// hook 域事件，由 hook 执行器消费。
    Hook(visp_hooks::HookEvent),
}

/// 事件发布者：向总线投递 [`BusEvent`]。
pub trait EventPublisher: Send + Sync {
    fn publish(&self, event: BusEvent);
}

#[cfg(test)]
#[path = "bus_tests.rs"]
mod tests;
