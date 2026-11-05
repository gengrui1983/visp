use super::*;
use crate::agent::{AgentEvent, AgentEventFrame};
use visp_hooks::{
    HookContext, HookEvent, HookEventName, HookPayload, Origin, SessionEndPayload, SessionSource,
};

fn sample_frame() -> AgentEventFrame {
    AgentEventFrame {
        event: AgentEvent::Done,
        session_id: "s1".into(),
        agent_name: "agent".into(),
        parent_session_id: None,
        parent_session_name: None,
    }
}

fn sample_hook() -> HookEvent {
    HookEvent {
        context: HookContext {
            schema: 1,
            hook_event_name: HookEventName::SessionEnd,
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            source: SessionSource::Startup,
            origin: Origin::Tui,
            seq: Some(1),
        },
        payload: HookPayload::SessionEnd(SessionEndPayload {
            reason: "quit".into(),
            exit_code: Some(0),
        }),
    }
}

#[test]
fn frame_variant_constructs_and_matches() {
    let frame = sample_frame();
    let bus = BusEvent::Frame(frame);
    match bus {
        BusEvent::Frame(f) => {
            assert_eq!(f.session_id, "s1");
            assert_eq!(f.agent_name, "agent");
        }
        BusEvent::Hook(_) => panic!("expected Frame variant"),
    }
}

#[test]
fn hook_variant_constructs_and_matches() {
    let hook = sample_hook();
    let bus = BusEvent::Hook(hook);
    match bus {
        BusEvent::Hook(h) => assert_eq!(h.event_name(), HookEventName::SessionEnd),
        BusEvent::Frame(_) => panic!("expected Hook variant"),
    }
}

#[test]
fn bus_event_is_clone() {
    let bus = BusEvent::Frame(sample_frame());
    let cloned = bus.clone();
    match (bus, cloned) {
        (BusEvent::Frame(a), BusEvent::Frame(b)) => {
            assert_eq!(a.session_id, b.session_id);
            assert_eq!(a.agent_name, b.agent_name);
        }
        _ => panic!("expected Frame variants"),
    }
}
