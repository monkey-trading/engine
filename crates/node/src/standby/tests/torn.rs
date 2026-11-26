//! A standby restarted after a crash cut its copy's last line short opens the copy without that line
//! and resumes the stream at the copy's last whole command.

use super::*;

use std::io::Write;

#[tokio::test]
async fn a_copy_whose_last_line_a_crash_cut_short_resumes_at_its_last_whole_command() {
    let dir = scratch_dir("standby-torn-copy");
    let mut writer = open_writer(&dir, 100);
    writer.append(&command(0)).unwrap();
    writer.append(&checkpoint(0, 5)).unwrap();
    writer.flush().unwrap();
    drop(writer);
    // The next command's line, cut short as a process killed mid-write leaves it.
    let next = serde_json::to_vec(&command(1)).unwrap();
    let segment = dir.join("00000000000000000000.jsonl");
    std::fs::OpenOptions::new().append(true).open(&segment).unwrap().write_all(&next[..next.len() / 2]).unwrap();

    let (mut writer, mut copied) = open_copy(&dir, 100, Chain::new(DOMAIN, enclave())).unwrap();
    let (port, server, request) = operator_stream(Vec::new()).await;
    let followed = Followed::from_standby(&standby_section(port));
    let stint = follow(&mut writer, &mut copied, &config(), &followed, Duration::from_millis(300), None, None).await;
    assert!(format!("{:#}", stint.unwrap_err()).contains("delivered no frame"));
    let request = request.await.unwrap();
    assert!(request.contains("from=0"), "the stream resumed at the copy's last whole command: {request}");
    // The cut line is gone, so the next entry the copy takes starts on its own line.
    writer.append(&command(1)).unwrap();
    writer.flush().unwrap();
    drop(writer);
    assert_eq!(entries(&dir), [command(0), checkpoint(0, 5), command(1)]);
    server.abort();
}

/// A standby opens its copy from the copy's last checkpoint, which its checks replayed, so a segment before it that
/// the host damaged is not read and does not stop the start; the copy's head and link are those a whole read gives.
#[tokio::test]
async fn a_copy_opens_from_its_last_checkpoint_past_a_damaged_earlier_segment() {
    let dir = scratch_dir("standby-open-last-checkpoint");
    let mut writer = open_writer(&dir, 2);
    for seq in 0..6 {
        writer.append(&command(seq)).unwrap();
        if seq % 2 == 1 {
            writer.append(&checkpoint(seq, 5)).unwrap();
        }
    }
    writer.flush().unwrap();
    drop(writer);
    let read = |from| open_copy_replayed(&dir, 2, Chain::new(DOMAIN, enclave()), from, || (), |_, _| Ok(()));
    let (_, whole, ()) = read(None).unwrap();
    let first = dir.join("00000000000000000000.jsonl");
    std::fs::write(&first, b"not the segment the manifest lists\n").unwrap();
    let (_, copied) = open_copy(&dir, 2, Chain::new(DOMAIN, enclave())).unwrap();
    assert_eq!(copied.head(), whole.head(), "the copy's head");
    assert_eq!(copied.resume_seq(), whole.resume_seq(), "where the stream resumes");
}
