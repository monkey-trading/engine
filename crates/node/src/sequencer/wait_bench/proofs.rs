//! The proof reads `price_wait_budget` sends beside its prices, whose answers it takes, so the root
//! worker proves each one while the prices are measured: the sequencer forwards no read whose answer
//! no one waits for.

use tokio::sync::oneshot::Receiver;
use tokio::sync::oneshot::error::TryRecvError;

/// Proof reads sent and not yet answered, and how many have been answered with a proof.
pub(super) struct Answers<T> {
    waiting: Vec<Receiver<Option<T>>>,
    proved: usize,
}

impl<T> Default for Answers<T> {
    fn default() -> Self {
        Self { waiting: Vec::new(), proved: 0 }
    }
}

impl<T> Answers<T> {
    /// Waits on `answer`, the reply of a read just sent.
    pub(super) fn push(&mut self, answer: Receiver<Option<T>>) {
        self.waiting.push(answer);
    }

    /// Takes the answers that have come, each of which must be a proof.
    pub(super) fn take(&mut self) {
        self.waiting.retain_mut(|answer| match answer.try_recv() {
            Err(TryRecvError::Empty) => true,
            answer => {
                self.proved += proved(answer);
                false
            }
        });
    }

    /// Waits for every answer still to come, each of which must be a proof, and returns how many reads
    /// were answered with one.
    pub(super) fn finish(mut self) -> usize {
        for answer in self.waiting.drain(..) {
            self.proved += proved(answer.blocking_recv());
        }
        self.proved
    }
}

/// One, for an answer that is a proof; panics on any other.
fn proved<T, E>(answer: Result<Option<T>, E>) -> usize {
    match answer {
        Ok(Some(_)) => 1,
        Ok(None) => panic!("a proof read was answered with none: the worker held no released version"),
        Err(_) => panic!("a proof read went unanswered: turned away or dropped"),
    }
}
