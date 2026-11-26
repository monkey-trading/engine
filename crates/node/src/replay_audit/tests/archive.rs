//! A custody-backed audit of a log read from an archive's URL: the trust is the log on disk's, so an archive that
//! ends before the vault's finalized checkpoint, as one rolled back does, fails as truncated.

use super::*;
use crate::archive::download;
use crate::archive::origin::serve;
use crate::standby::archive::archive_client;

/// The archive of a log whose completed segments hold the vault's finalized checkpoint replays against the vault's
/// history as the log on disk does; one that lists only the segments before it fails as shorter than the vault's
/// anchor.
#[tokio::test]
async fn a_custody_audit_of_an_archive_that_ends_before_the_finalized_checkpoint_fails() {
    let fixture = Fixture::new("audit-archive-source");
    let dir = scratch_dir("audit-archive-log");
    let mut writer =
        SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, watch::channel(Manifest::default()).0).unwrap();
    for entry in &fixture.entries {
        writer.append(entry).unwrap();
    }
    writer.flush().unwrap();
    writer.seal_open_segment().unwrap();
    drop(writer);
    let client = archive_client(false).unwrap();
    let whole = scratch_dir("audit-archive-whole");
    download(&client, &serve(&dir, None).await.url.parse().unwrap(), &whole).await.unwrap();
    let replayed =
        replay_with_chain(&whole, &fixture.chain, fixture.domain, fixture.signer(), Default::default()).await.unwrap();
    assert_eq!((replayed.commands, replayed.checkpoints), (6, 2));

    let manifest = fs::read_to_string(dir.join(crate::log::MANIFEST)).unwrap();
    let mut lines: Vec<&str> = manifest.lines().collect();
    lines.pop();
    fs::write(dir.join(crate::log::MANIFEST), format!("{}\n", lines.join("\n"))).unwrap();
    let rolled_back = scratch_dir("audit-archive-rolled-back");
    download(&client, &serve(&dir, None).await.url.parse().unwrap(), &rolled_back).await.unwrap();
    let Err(error) =
        replay_with_chain(&rolled_back, &fixture.chain, fixture.domain, fixture.signer(), Default::default()).await
    else {
        panic!("an archive that ends before the vault's finalized checkpoint replayed");
    };
    assert!(format!("{error:#}").contains("shorter than or omits the finalized vault anchor"), "{error:#}");
}
