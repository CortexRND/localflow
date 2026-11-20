//! Prompt queue review through `hub_model::{prompt_rows, review_prompt}`.

use lf_ui::backends::prompts::PromptQueue;
use lf_ui::hub_model::{self, PromptRow, ReviewAction};

use crate::remote::queue_error_text;

/// Load all rows (newest first) or a friendly error.
pub fn load(queue: &dyn PromptQueue) -> Result<Vec<PromptRow>, String> {
    queue
        .list(None)
        .map(|entries| hub_model::prompt_rows(&entries))
        .map_err(|e| queue_error_text(&e))
}

pub fn filter<'a>(rows: &'a [PromptRow], status: Option<&str>) -> Vec<&'a PromptRow> {
    rows.iter()
        .filter(|r| status.is_none_or(|s| r.status == s))
        .collect()
}

pub fn pending_count(rows: &[PromptRow]) -> usize {
    rows.iter().filter(|r| r.status == "pending").count()
}

/// Approve or reject; `Err` is the message to show (the list is reloaded
/// either way by the caller).
pub fn review(queue: &dyn PromptQueue, id: &str, approve: bool) -> Result<(), String> {
    let action = if approve {
        ReviewAction::Approve
    } else {
        ReviewAction::Reject
    };
    match hub_model::review_prompt(queue, id, action) {
        None => Ok(()),
        Some(message) => Err(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::seed_prompt_queue;
    use lf_ui::backends::prompts::UnavailablePromptQueue;

    #[test]
    fn review_flow_and_conflicts() {
        let queue = seed_prompt_queue();
        let rows = load(&queue).unwrap();
        assert_eq!(pending_count(&rows), 2);
        let pending = filter(&rows, Some("pending"))[0].clone();
        review(&queue, &pending.id, true).unwrap();
        // Second click on a stale row: refused with the existing message.
        let again = review(&queue, &pending.id, true).unwrap_err();
        assert!(again.contains("changed"), "{again}");
        let rows = load(&queue).unwrap();
        assert_eq!(pending_count(&rows), 1);
        assert!(review(&queue, "missing", false)
            .unwrap_err()
            .contains("no longer"));
    }

    #[test]
    fn disconnected_queue_is_an_error_not_an_empty_list() {
        assert!(load(&UnavailablePromptQueue).is_err());
    }
}
