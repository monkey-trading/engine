//! Checks an account proof from the node's `GET /v1/accounts/{address}/proof` against a state root
//! taken from a signed checkpoint, without the node, and prints the balance and positions it proves.
//!
//! For a local dev node with a self-signed certificate (`--insecure` is dev-only):
//! `curl --fail --silent --show-error --insecure https://127.0.0.1:8080/v1/accounts/<address>/proof |
//!     cargo run -p engine-core --example verify_proof -- <root>`

use std::io::Read;

use anyhow::{Context, bail};
use engine_core::eip712::B256;
use engine_core::merkle::AccountProof;

/// Reads the proof from stdin and checks it against the root given as the only argument.
/// Error cases:
/// - no root argument, or one that is not 32 bytes of `0x` hex
/// - stdin cannot be read, or is not an account proof
/// - the proof does not verify against the root
fn main() -> anyhow::Result<()> {
    let root: B256 = std::env::args()
        .nth(1)
        .context("usage: verify_proof <root> < proof.json")?
        .parse()
        .context("parsing the root")?;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).context("reading the proof")?;
    let proof: AccountProof = serde_json::from_str(&input).context("parsing the proof")?;
    if !proof.verify(root) {
        bail!("the proof of {} does not verify against {root}", proof.account);
    }
    let leaf = &proof.leaf;
    println!(
        "verified against {root}: account {} at seq {}, balance {}, withdrawal nonce {}",
        proof.account, proof.seq, leaf.balance, leaf.withdrawal_nonce
    );
    for (held, market) in proof.positions.iter().zip(&proof.markets) {
        let mark = market.leaf.mark.map_or("none".to_owned(), |mark| mark.to_string());
        println!("  market {}: qty {}, entry {}, mark {mark}", held.market, held.position.qty, held.position.entry);
    }
    Ok(())
}
