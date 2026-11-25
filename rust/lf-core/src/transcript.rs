use serde::{Deserialize, Serialize};

/// One STT segment inside a clip; seconds from clip start.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TimedText {
    pub start: f64,
    pub end: f64,
    pub text: String,
}
