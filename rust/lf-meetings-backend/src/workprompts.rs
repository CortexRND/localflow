use lf_providers::LlmProvider;

const SYSTEM: &str = include_str!("workprompts_system.txt");
pub(crate) fn generate(llm: &dyn LlmProvider, notes: &str) -> String {
    let system = SYSTEM.replace("\r\n", "\n").replace('\r', "\n");
    llm.complete(&system, &format!("<meeting_notes>\n{notes}\n</meeting_notes>\n\nDerive the work prompts from these meeting notes, following your instructions exactly."), Some(8192)).unwrap_or_default()
}
