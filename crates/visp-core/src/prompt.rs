use std::path::Path;

use crate::message::Message;

pub struct PromptBuilder;

const USER_QUERY_INSTRUCTION: &str = "\n\n## IMPORTANT: How to ask the user to choose\n\
\n\
When you need the user to make a decision, you MUST use the [USER_QUERY] marker at the very end of your response. \
Do NOT ask the user to choose using plain text - it will not trigger the selection UI.\n\
\n\
### Use regular options when the user should pick from predefined choices:\n\
\n\
[USER_QUERY]\n\
Which approach do you prefer?\n\
- SQLite\n\
- PostgreSQL\n\
[/USER_QUERY]\n\
\n\
### You can also include options with free-form input. An \"Other\" button is always available:\n\
\n\
[USER_QUERY]\n\
What's on your mind?\n\
[/USER_QUERY]\n\
\n\
Or with some suggestions:\n\
[USER_QUERY]\n\
How should we fix this bug?\n\
- Refactor the module\n\
- Add a quick patch\n\
[/USER_QUERY]\n\
\n\
### Rules:\n\
- Marker MUST be at the very end of your response (not in the middle)\n\
- Each option MUST be on its own line starting with `- `\n\
- Options should be REAL choices, not invitations to talk\n\
- Do NOT create \"listening\" options like \"你来说说看\" or \"I'll listen\"\n\
- Only use [USER_QUERY] when you genuinely need user input\n\
- One [USER_QUERY] marker must contain exactly ONE question. If you have multiple questions, ask them one at a time and wait for the user's answer before asking the next.\n\
- An \"Other\" button is always shown for free-form input";

#[allow(clippy::too_many_arguments)]
impl PromptBuilder {
    pub fn build(
        system_template: &str,
        rules: &str,
        history: &[Message],
        working_dir: &Path,
        date_str: &str,
        max_context_tokens: Option<u32>,
        output_tokens: u32,
        trimmer: &dyn crate::context::ContextTrimmer,
    ) -> Vec<Message> {
        let mut system_content = match (system_template.is_empty(), rules.is_empty()) {
            (true, true) => USER_QUERY_INSTRUCTION.trim_start().to_string(),
            (true, false) => format!("## Instructions\n\n{rules}{USER_QUERY_INSTRUCTION}"),
            (false, true) => format!("{system_template}{USER_QUERY_INSTRUCTION}"),
            (false, false) => {
                format!("{system_template}\n\n## Instructions\n\n{rules}{USER_QUERY_INSTRUCTION}")
            }
        };

        let env_context = format!(
            "\n\n## Current Context\n\nDate: {date_str}\nWorking Directory: {}",
            working_dir.display()
        );
        system_content.push_str(&env_context);

        let system_msg = Message::system(system_content);
        let system_tokens = system_msg.estimated_tokens;

        // 过滤 skip_context 消息
        let mut filtered: Vec<Message> = history
            .iter()
            .filter(|m| !m.skip_context)
            .cloned()
            .collect();

        // 上下文裁剪（内部完成 Tool 输出截断）
        if let Some(max_ctx) = max_context_tokens {
            filtered = trimmer.trim(&filtered, max_ctx, system_tokens, output_tokens);
        }

        let mut messages = vec![system_msg];
        messages.extend(filtered);
        messages
    }
}

pub use visp_config::prompt::DEFAULT_SYSTEM_PROMPT;

pub fn user_query_instruction() -> &'static str {
    USER_QUERY_INSTRUCTION
}

#[cfg(test)]
#[path = "prompt_tests.rs"]
mod tests;
