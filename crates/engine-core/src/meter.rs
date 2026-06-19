//! A cheap, shareable accumulator of token usage for one turn. The `MeteringRouter`
//! (in the `router` crate) writes to it as completions pass through; the orchestrator reads
//! the running totals to emit `TokenCostMeter` events.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::types::Usage;

/// Cumulative input/output token counters. `Default` starts at zero.
#[derive(Default)]
pub struct TokenMeter {
    input: AtomicU64,
    output: AtomicU64,
}

impl TokenMeter {
    /// Add one completion's usage to the running totals.
    pub fn add(&self, u: &Usage) {
        self.input
            .fetch_add(u.input_tokens as u64, Ordering::SeqCst);
        self.output
            .fetch_add(u.output_tokens as u64, Ordering::SeqCst);
    }

    /// `(input_tokens, output_tokens)` accumulated so far.
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.input.load(Ordering::SeqCst),
            self.output.load(Ordering::SeqCst),
        )
    }

    /// Total tokens (input + output). Used to gate emission: zero means "no usage yet".
    pub fn total(&self) -> u64 {
        let (i, o) = self.snapshot();
        i + o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_and_snapshots() {
        let m = TokenMeter::default();
        assert_eq!(m.snapshot(), (0, 0));
        assert_eq!(m.total(), 0);
        m.add(&Usage {
            input_tokens: 2,
            output_tokens: 3,
        });
        m.add(&Usage {
            input_tokens: 1,
            output_tokens: 1,
        });
        assert_eq!(m.snapshot(), (3, 4));
        assert_eq!(m.total(), 7);
    }
}
