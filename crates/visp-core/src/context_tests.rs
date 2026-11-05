use super::*;
use crate::message::Message;

struct MockTrimmer;

impl ContextTrimmer for MockTrimmer {
    fn trim(
        &self,
        history: &[Message],
        _max_context_tokens: u32,
        _system_overhead: u32,
        _output_tokens: u32,
    ) -> Vec<Message> {
        history.to_vec()
    }
}

#[test]
fn test_context_trimmer_trait_object() {
    let trimmer: Box<dyn ContextTrimmer> = Box::new(MockTrimmer);
    let result = trimmer.trim(&[], 1000, 0, 100);
    assert!(result.is_empty());
}

#[test]
fn test_context_trimmer_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<MockTrimmer>();

    let trimmer: Box<dyn ContextTrimmer + Send + Sync> = Box::new(MockTrimmer);
    let _arc = std::sync::Arc::new(trimmer);
}
