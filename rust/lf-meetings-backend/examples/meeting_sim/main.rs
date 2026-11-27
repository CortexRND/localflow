//! `meeting_sim`: run one `evals/meeting_sim` scenario through the Rust meetings
//! backend with a scripted transcript (eval/test only; see `sim.rs`).
//!
//! ```text
//! cargo build -p lf-meetings-backend --example meeting_sim
//! target/debug/examples/meeting_sim --scenario pricing-review --out r1/results/pricing-review.json \
//!     --llm-url http://127.0.0.1:11434 --llm-model llama3.2:3b --cleanup
//! ```
mod sim;

use std::process::ExitCode;

fn main() -> ExitCode {
    match sim::parse_args(std::env::args().skip(1)).and_then(|args| sim::run(&args)) {
        Ok(summary) => {
            println!("{summary}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("meeting_sim: {error:#}");
            ExitCode::FAILURE
        }
    }
}
