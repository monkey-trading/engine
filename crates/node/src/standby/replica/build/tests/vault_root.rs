//! A member's snapshot start against the roots the vault accepted.

use super::*;
use crate::daily_snapshots::{AcceptedRoots, snapshot_start_against};

/// A snapshot whose checkpoint the vault accepted another root at is an orphan fork's: it is refused before it is
/// fetched, paged, and the older one taken. One at the root the vault accepted there is taken.
#[tokio::test]
async fn a_snapshot_at_a_seq_the_vault_accepted_another_root_at_is_refused_before_it_is_fetched() {
    let (dir, origin) = (scratch_dir("build-snapshot-vault-root"), scratch_dir("build-snapshot-vault-root-origin"));
    write_log(&dir).await;
    let older = publish_at(&dir, &origin, &recorded(), |held| held / 3);
    let newer = publish_at(&dir, &origin, &recorded(), |held| 2 * held / 3);
    let listing = read_manifest(&origin).unwrap().into_iter().find(|listing| listing.seq == newer).unwrap();
    let (client, origins) = (crate::standby::archive::archive_client(false).unwrap(), [serve(&origin).await]);
    let signers: BTreeSet<AccountId> = [signer()].into();
    let (_guard, mut alerts) = capture_alerts();
    for (case, root, taken) in
        [("another root", B256::repeat_byte(0xee), older), ("the listed root", listing.root.parse().unwrap(), newer)]
    {
        let member = scratch_dir(&format!("build-snapshot-vault-root-{}", case.replace(' ', "-")));
        let roots = Roots([(newer, root)].into());
        let bounds = (u64::MAX, None);
        let signed =
            snapshot_start_against(&client, &origins, (&member, &dir), (DOMAIN, &signers), bounds, &roots).await;
        assert_eq!(signed.map(|signed| signed.snapshot.offset.seq), Some(taken), "{case}");
        if taken == older {
            let alert = alerts.try_recv().expect("the refusal is paged");
            assert!(alert["message"].contains("the vault accepted root"), "{alert:?}");
            assert!(!member.join(&listing.file).exists(), "the orphan's snapshot was fetched");
        }
    }
}

// ── Helpers ──

/// The roots a test's vault accepted, by seq.
struct Roots(std::collections::BTreeMap<u64, B256>);

impl AcceptedRoots for Roots {
    async fn root_at(&self, seq: u64) -> Option<B256> {
        self.0.get(&seq).copied()
    }
}
