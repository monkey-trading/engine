//! The scenario's local Anvil child process, killed on drop.

use std::process::Child;

pub(crate) struct Anvil(pub Child);
impl Drop for Anvil {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
