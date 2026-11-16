//! Command mode and polish/prompt transforms (`localflow/transform.py`).

use lf_providers::LlmProvider;

pub const COMMAND_WITH_SELECTION: &str = "You are a writing assistant. Apply the instruction to the selected text. \
Return only the resulting text that should replace the selection, with no \
commentary, no quotes, and no explanations.\n\nInstruction:\n{instruction}\
\n\nSelected text:\n{selection}";
pub const COMMAND_NO_SELECTION: &str = "You are a writing assistant. Write the text the instruction asks for. \
Return only that text, with no commentary, no quotes, and no explanations.\
\n\nInstruction:\n{instruction}";
pub const POLISH: &str = "You are an editor. Rewrite the text to be clear, concise and well-structured \
while keeping the author's meaning, voice and every fact. Fix grammar and \
punctuation. Do not add new information. Return only the rewritten text with \
no commentary, no quotes, and no explanations.\n\nText:\n{text}";
pub const PROMPT_ENGINEER: &str = "You turn rough notes into a clear prompt for an AI assistant. Rewrite the \
text as a well-structured prompt: state the goal, the relevant context, the \
constraints and the desired output format, using short sections or bullet \
points where helpful. Keep every requirement from the original and do not \
invent new ones. Return only the prompt with no commentary.\n\nText:\n{text}";

pub fn command_prompt(instruction: &str, selection: &str) -> String {
    if selection.is_empty() {
        COMMAND_NO_SELECTION.replace("{instruction}", instruction.trim())
    } else {
        COMMAND_WITH_SELECTION
            .replace("{selection}", selection)
            .replacen("{instruction}", instruction.trim(), 1)
    }
}

pub fn transform_prompt(preset: &str, text: &str) -> Result<String, String> {
    let template = match preset {
        "polish" => POLISH,
        "prompt" => PROMPT_ENGINEER,
        other => return Err(format!("unknown transform preset: {other}")),
    };
    Ok(template.replace("{text}", text))
}

fn strip_quotes(s: &str) -> &str {
    s.trim().trim_matches(|c| c == '"' || c == '\'').trim()
}

/// `Transformer.run`: the prompt's first paragraph is the system message.
pub fn run_transform(llm: &dyn LlmProvider, prompt: &str) -> Option<String> {
    let (system, user) = prompt.split_once("\n\n")?;
    let result = match llm.complete(system, user, None) {
        Ok(r) => r,
        Err(error) => {
            log::error!("text transform failed: {error}");
            return None;
        }
    };
    let result = strip_quotes(&result);
    let lower = result.to_lowercase();
    let result = ["text:", "prompt:"]
        .iter()
        .find(|p| lower.starts_with(*p))
        .map_or(result, |p| result[p.len()..].trim_start());
    let result = strip_quotes(result);
    (!result.is_empty()).then(|| result.to_string())
}
