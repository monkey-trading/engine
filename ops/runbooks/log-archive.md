# Log archive

Use this runbook to set up the bucket that keeps the public log and its daily snapshots for good, to
check that the trading server uploads to it, and to act when one of the archive's alerts fires. The
log must stay public and replayable from genesis, and the archive is where it stays once the host no
longer holds every segment. With `[daily_snapshots]`, the host keeps 30 days of daily snapshots, and
the bucket keeps every one. Commands, configuration keys and their output call a running trading server a node.

- [What a deployment needs](#what-a-deployment-needs)
- [Set up the bucket](#set-up-the-bucket)
- [Check the uploads](#check-the-uploads)
- [Read the log from the bucket alone](#read-the-log-from-the-bucket-alone)
- [When `archive_upload_lagging` fires](#when-archive_upload_lagging-fires)
- [When `archive_object_mismatch` fires](#when-archive_object_mismatch-fires)
- [When `archive_copy_superseded` fires](#when-archive_copy_superseded-fires)
- [When `archive_segment_missing` fires](#when-archive_segment_missing-fires)
- [Keep a window of the log on the host](#keep-a-window-of-the-log-on-the-host)
- [When `log_segment_missing` fires](#when-log_segment_missing-fires)
- [When `log_window_held` fires](#when-log_window_held-fires)
- [When `log_window_failing` fires](#when-log_window_failing-fires)
- [How the archive works](#how-the-archive-works)

The steps use the shell variables that [Variables](reference.md#variables) sets up, and the ones
below.

## What a deployment needs

Each deployment has a bucket of its own: staging's is not production's. Every member of the rotation
set uploads to the deployment's bucket, with the same keys. Each log's objects sit under its custody
chain and vault, `log/<chain id>/<vault>/`, and its daily snapshots under `snapshots/<chain id>/<vault>/`,
so a new log on a new vault, as at the move to the launch vault, can share its predecessor's bucket.

| Need | Detail |
| --- | --- |
| A Cloudflare R2 bucket | Its name is `archive.bucket`, and its account's S3 API endpoint, `https://<account id>.r2.cloudflarestorage.com`, is `archive.endpoint`. The node signs its uploads with AWS Signature Version 4 for the region `auto`, so another S3-compatible store works with its own endpoint and region. |
| Indefinite bucket locks on `log/` and `snapshots/` | Once a lock covers an object, nobody can replace or delete it, whatever keys they hold. Every object the node writes is under one of the two. |
| A custom domain for public reads | `archive.public_url`. Readers, and the node's own check of each upload, read objects there. Do not use the `r2.dev` address, which Cloudflare rate-limits and does not support for production. |
| An API token | Object read and write on this bucket only. Its access key id and secret access key are delivered as `archive_access_key_id` and `archive_secret_access_key`. |

## Set up the bucket

The owner does these steps in the Cloudflare account that holds the bucket.

**Changes the live system.** Each step changes the deployment's storage or its secrets, and needs the
owner's approval.

1. Create the bucket on the **R2 object storage** page of the Cloudflare dashboard.
2. Open the bucket's **Settings** tab. On the **Bucket lock rules** card, select **Add rule**, name
   it, set the prefix `log/`, keep objects locked indefinitely, and select **Save changes**. Add a
   second rule the same way for the prefix `snapshots/`. See Cloudflare's [bucket
   locks](https://developers.cloudflare.com/r2/buckets/bucket-locks/).

   **A rule without the prefix, or with a retention period, does not protect the archive for good.**
   Check each rule's prefix and its retention before you go on.
3. On the same tab, under **Custom Domains**, select **Add**, enter the domain, and connect it. Leave
   its caching at the default. If a rule caches every file on the domain, keep it from caching `404`
   answers under `log/` and `snapshots/`: the node reads each object back as soon as it creates it,
   and a cached `404` delays the read until the cache expires.
4. On the **R2 object storage** page, under **Account Details**, select **Manage** next to
   **API Tokens**, then **Create Account API token**. Choose the permission **Object Read & Write**,
   scope it to this bucket only, and create it. Copy its **Access Key ID** and **Secret Access Key**:
   the dashboard shows the secret access key only once.
5. Set `archive.endpoint`, `archive.bucket` and `archive.public_url` in each of the deployment's
   configurations under `config/release/`. These values are part of the attested configuration, so
   they reach the trading server in a release, as [Deploy and rollback](deploy-and-rollback.md)
   describes.
6. Add the two keys to the bundle file of every member, and deliver it, as
   [Deliver secrets](key-recovery.md#deliver-secrets) describes:

   ```toml
   [secrets]
   archive_access_key_id = "..."
   archive_secret_access_key = "..."
   ```

   A trading server whose bundle lacks either key runs with its uploads off, raises
   `secret_missing` for the field, and keeps every segment on its host.

**How to tell it is set up.** [Check the uploads](#check-the-uploads) passes on staging before the
first release that names a production bucket.

## Check the uploads

1. Read the archive's metrics from the trading server's metrics listener:

   ```shell
   curl -s http://<metrics listener>/metrics | grep '^engine_archive_'
   ```

   `engine_archive_confirmed_ranges` rises as the vault finalizes checkpoints past the log's ranges,
   and `engine_archive_pending_ranges` stays at a few: a range waits for the vault to finalize a
   checkpoint past its last sequence number, which takes about as long as checkpoints take to post
   and finalize. `engine_archive_tail_seq` is the newest checkpoint whose tail the trading server
   confirmed since it started. The server uploads a tail once its log holds the checkpoint and no
   range is waiting to go up, so the gauge reaches the vault's `latestSeq` soon after each checkpoint
   is posted. A tail that cannot go up raises `archive_upload_lagging` once it has waited
   `archive.lag_alert`:

   ```shell
   cast call $VAULT 'latestSeq()(uint64)' --rpc-url $RPC
   ```
2. Set the public URL, and check that the first range's object is a whole zstd frame holding the
   first 10,000 sequence numbers:

   ```shell
   export ARCHIVE=https://<archive.public_url's host>/log/<custody.chain_id>/<custody.vault, lower case>
   curl -sf $ARCHIVE/00000000000000000000.jsonl.zst -o range.zst
   zstd -t range.zst
   zstd -dc range.zst | head -n 1 | jq .seq
   zstd -dc range.zst | jq -c 'select(.entry == "command") | .seq' | tail -n 1
   ```

   `zstd -t` passes, which checks the frame's content checksum, and the two `jq` commands print `0`
   and `9999`.
3. With `[daily_snapshots]`, check that the newest snapshot the trading server lists is in the bucket
   as it lists it:

   ```shell
   export SNAPSHOTS=https://<archive.public_url's host>/snapshots/<custody.chain_id>/<custody.vault, lower case>
   curl -sf $API/v1/archive/snapshots/manifest.jsonl | tail -n 1 | jq -r '.file, .sha256'
   curl -sf $SNAPSHOTS/<file> | sha256sum
   ```

   The second command prints the hash the first printed. `engine_archive_snapshots_pending` is at most
   one or two, and `engine_archive_snapshots_confirmed_total` rises by one a day. For some minutes
   after the trading server starts, `engine_archive_snapshots_pending` counts every snapshot on the host
   while it reads each back from the bucket, about 13 GB at 100,000 accounts, and the confirmed count
   rises by as many.

**A first release with the archive uploads the whole log from its first range.**
`engine_archive_pending_ranges` falls as the backlog uploads, and `archive_upload_lagging` fires if it
takes longer than `archive.lag_alert`. Nothing needs to be done unless it stops falling.

## Read the log from the bucket alone

Use this when no trading server or mirror serves the log, as when every operator's host is gone and
accounts exit the vault. The bucket cannot say where the log ends, so the vault's latest checkpoint
names it.

1. Read the vault's latest checkpoint, as [Read the vault and the registry](reference.md#read-the-vault-and-the-registry)
   does:

   ```shell
   export SEQ=$(cast call $VAULT 'latestSeq()(uint64)' --rpc-url $RPC | cut -d' ' -f1)
   export ROOT=$(cast call $VAULT 'latestRoot()(bytes32)' --rpc-url $RPC)
   ```

2. Replay the log from the bucket through that checkpoint, with `ARCHIVE` set as step 2 of
   [Check the uploads](#check-the-uploads) sets it:

   ```shell
   $ENGINE replay --log $ARCHIVE/ --seq $SEQ --state-root $ROOT
   ```

   It prints `verified:` with `final root` equal to `$ROOT`.
3. Build an account's exit proofs from it:

   ```shell
   $ENGINE exit-proof --log $ARCHIVE/ --seq $SEQ --state-root $ROOT --account <address>
   ```

**How to tell it worked.** Both commands exit with status 0. The download goes to the system's
temporary directory, which `TMPDIR` sets, and needs room for the whole log.

A reader that fails with `holds neither range <n> ... nor the tail's piece of it` met a bucket that
does not hold the log through that checkpoint: its tail never went up. Check the object's URL the
message names with `curl -sI`; a `200` there means a cache answered a stale `404`, and the read
succeeds once the cache's entry expires.

## When `archive_upload_lagging` fires

**What is happening.** A whole range of the log, or the tail of a checkpoint past the vault's finalized one, has
waited longer than `archive.lag_alert`, an hour by default, for its upload to be confirmed, or the
node has not reached the bucket for that long to find where to resume, or to read its own log for the
checkpoints whose tails it uploads. While a tail waits, the bucket alone does not hold the log
through the vault's latest checkpoint. A range's wait includes the time the vault takes to finalize a
checkpoint past it. With the field `objects` set to `daily_snapshots`, a daily snapshot has waited that
long instead, with no vault to wait for. The host keeps the whole log and every snapshot not yet in the
bucket, so nothing is lost, and the trading server keeps sequencing. Clients see no change.

1. If `checkpoint_backlog` is open too, the vault has finalized no checkpoint for a while, and ranges
   wait for it. Follow [Withdrawal stalls](withdrawal-stalls.md) first.
2. Read the trading server's console for the failed uploads:

   ```text
   an upload to the archive failed, and is retried in 300s: ...
   a pass over the daily snapshots' archive failed, and is retried in 300s: ...
   ```

   The message names the cause:

   | The message ends with | Cause and fix |
   | --- | --- |
   | `the bucket answered 403 Forbidden (SignatureDoesNotMatch)` or `(InvalidAccessKeyId)` | The delivered keys are wrong. Deliver the token's keys again, as step 6 of [Set up the bucket](#set-up-the-bucket) describes, and restart the trading server, since delivered secrets take effect at a restart. |
   | `(AccessDenied)` | The token lacks write access to this bucket. Give it **Object Read & Write** on the bucket. |
   | `(RequestTimeTooSkewed)` or `(ExpiredRequest)` | The host's clock is far from the bucket's, and the node signs uploads at the host's clock. Fix the host's clock. |
   | `reading ...: the origin answered ...`, or `... is not in the bucket` | The custom domain does not serve the bucket, or caches a `404`. Check step 3 of [Set up the bucket](#set-up-the-bucket). |
   | `reading the vault's finalized checkpoint` | The checkpoint endpoint, `checkpoints.rpc_url`, does not answer. Follow [Checkpoint endpoint](feed-and-custody-failures.md#checkpoint-endpoint). |
   | A timeout, or a connection error | The bucket or the host's network is down. Check [Cloudflare's status page](https://www.cloudflarestatus.com) and the host's outbound connections. |
   | `does not match` its listing, or a segment cannot be read | The host changed or removed a segment file. The node uploads nothing past it. Treat it as an incident, as [Incident response](incident-response.md) describes. |
   | `reading the copy for checkpoints` | The node cannot read its own log for the checkpoints whose tails it uploads: the host changed or removed a segment file, the open one included. Treat it as an incident, as [Incident response](incident-response.md) describes. |

3. Watch `engine_archive_pending_ranges`, or for the snapshots `engine_archive_snapshots_pending`.
   Once the cause is fixed, it falls at the next retry, at most five minutes later. When the log's is
   already 0, a tail waits: compare `engine_archive_tail_seq` with the vault's `latestSeq`, as step 1
   of [Check the uploads](#check-the-uploads) does, and it reaches it at the next retry.

**How to tell it is resolved.** The alert resolves, `engine_archive_pending_ranges` is back to a
few, and `engine_archive_tail_seq` is at the vault's `latestSeq`.

**Escalate** to the owner if the bucket refuses the uploads for an hour after you fixed the cause,
since every member keeps the whole log on its host until it is resolved.

## When `archive_object_mismatch` fires

**What is happening.** The object for a range of the log, or for a piece of a checkpoint's tail, reads
back as other entries than the node's own copy of it. The alert's `object` names which: a tail's
pieces are under `tail/`. For a tail's piece, the node uploads no more of that tail, and the bucket
alone does not hold the log through that checkpoint; the ranges and later tails go on, and no
`archive_upload_lagging` follows, so this Warning is the only page. For a range, the bucket lock
keeps the node from replacing it, so the node never counts that range as archived, uploads nothing
past it, and its host keeps the log from there on. A restart
stops there again, unless other nodes have since uploaded later ranges that match this node's copy.
The trading server keeps sequencing, and for a range `archive_upload_lagging` follows within
`archive.lag_alert`.
Every node builds a range's bytes the same way from its own copy, and uploads it only once the vault
has finalized a checkpoint past it, so a range never changes once it is uploaded. Two nodes that
hold different bytes for one finalized range hold different logs, or one object was written by a
holder of the bucket's keys or by Cloudflare. Treat it as an incident.

When the alert's `object` is under `snapshots/`, with a `seq` field, the object of a daily snapshot
reads back as other bytes than the snapshot's line in this trading server's manifest. The host keeps
that snapshot whatever its age and serves it from its disk, and the other days go on. A snapshot goes
up as soon as it is published, before the vault finalizes its checkpoint, so a failed operator's
snapshot of a tail that a promotion set aside can hold the name of its successor's snapshot at the
same offset. Readers refuse that object, since the log holds another checkpoint there. Otherwise,
treat it as an incident.

1. Keep the evidence, as [Keep the evidence](incident-response.md#keep-the-evidence) describes,
   with the alert's `object` and its `first_seq` or `seq` field.
2. Read the object. For a range:

   ```shell
   curl -sf https://<archive.public_url's host>/<object> -o range.zst
   zstd -t range.zst
   zstd -dc range.zst | jq -c 'select(.entry == "command") | .seq' | head -n 1
   ```

   An object that fails `zstd -t`, or whose first sequence number is not the alert's `first_seq`,
   was not written by a node. For a snapshot, read the object's checkpoint:

   ```shell
   curl -sf https://<archive.public_url's host>/<object> | jq -c '.checkpoint | {seq, root, signer}'
   ```

   An object whose checkpoint is at the alert's `seq` and signed by the vault's signer before a
   promotion is a set-aside tail's: keep the evidence, and nothing else needs doing. An object whose
   checkpoint is not at the alert's `seq`, or is signed by no signer the vault recorded, was not
   written by a node. For a range or a snapshot that no node wrote, rotate the bucket's API token and
   deliver the new keys to every member, as step 6 of [Set up the bucket](#set-up-the-bucket) describes.
3. Tell the owner, with the evidence. Readers of the archive check a range by replaying it, and a
   snapshot against its signature and its checkpoint's root.

## When `archive_copy_superseded` fires

**What is happening.** The vault has finalized a checkpoint that this trading server's copy of the log
holds at another root, or does not hold at its sequence number. The copy's history differs from the
vault's after an earlier checkpoint. This is expected for an operator that failed and comes back as
a member: its log holds an unconfirmed tail that a promoted successor's history replaced. The node
uploads none of the copy's ranges past the last finalized checkpoint the copy holds, so the
superseded tail never reaches the archive. The trading server keeps following, and its host keeps the
log.

1. Read the alert's message, which names the finalized seq, the vault's root and the copy's, and the
   console lines around it.
2. If the node is a member that was the operator before a promotion, wait. As it follows the operator
   in effect, it sets the superseded tail aside and rebuilds its copy from the operator's log, as
   [A replaced operator coming back](operator-candidate.md#a-replaced-operator-coming-back) describes. The alert resolves once the
   rebuilt copy holds the vault's checkpoint.
3. If the node is the operator in effect, or the alert does not resolve within an hour, keep the
   evidence, as [Keep the evidence](incident-response.md#keep-the-evidence) describes, and tell the
   owner. A copy that the vault's history superseded while it was in office is an incident.

**How to tell it is resolved.** The alert resolves, and `engine_archive_confirmed_ranges` rises again.

## When `archive_segment_missing` fires

**What is happening.** A reader of the archive needed an object that the bucket answers 404 for, and
no node origin served the segment either. The reader is a standby's or a member's backfill, a
successor's archive sync at a handover, a promoted member's gap fetch or the carry check. Its read
fails rather than ending there, since the bucket holds a prefix of the log and a cache in front of it
can answer a stale 404. The node retries the read where its caller retries: a standby's next start,
a member's next stint, the next handover attempt.

1. Read the alert's `object` field, the console lines around it, and the segment the message names.
2. Check the object at its public URL:

   ```bash
   curl -sI "<object>"
   ```

   A `200` means the 404 came from a cache in front of the bucket. Purge the cache for the key, or
   wait for its entry to expire, and the next read finds the object.
3. With a 404 there too, check the bucket's own listing of the key with the API token, and check
   `engine_archive_confirmed_ranges` on the operator in effect and on every member. A range that no
   node has confirmed was never uploaded: the node in office uploads it once the vault finalizes a
   checkpoint past it, and a member uploads it from its copy while it follows.
4. If a node redirects the segment, `GET /v1/archive/<segment>` answering 308, but no node holds it
   and the bucket lacks its range, the host dropped the segment before its upload, and that node
   pages `log_segment_missing`. Follow [When `log_segment_missing` fires](#when-log_segment_missing-fires).

**How to tell it is resolved.** The reader's next attempt succeeds: the standby follows, the member
builds its replica, or the handover completes. The alert does not resolve itself, and a later failure
for the same object pages again after `alerts.episode_quiet`.

## Keep a window of the log on the host

`archive.local_window` keeps that much of the log, in sequenced time, on the host, and leaves the
rest to the bucket. Without it, the host keeps every segment. The release configurations set 30
days, the history index's retention.

A completed segment leaves the host, oldest first, only when all of these hold:

- the bucket holds every range the segment holds sequence numbers of, as this process's uploader
  counts them once it has found what the bucket holds, so a segment of a range not yet whole or not
  yet finalized stays, whatever else holds its commands;
- every command of it is older than the window, counted back from the first command of the newest
  completed segment: the log's own stamps, never the host's clock;
- it ends before the checkpoint of the newest daily snapshot a start may go on from, and before the
  checkpoint of the newest daily snapshot stamped before the history index's retention whose file
  hashes as its manifest line says, from which the index re-derives a retained command's rows; with
  no daily snapshot, nothing leaves;
- a read of its sequence numbers from the bucket's public URL, made just before it leaves, gives
  back the bytes its manifest line names;
- no reader of the log inside the node, such as the history index catching up, is reading it. A
  stream client's backfill holds no segment: one that reaches a segment that left the host fails,
  and the client reads it from the archive.

The window does not run with `[snapshots]`, whose history index rebuilds from the whole log, so a
configuration with `[snapshots]` keeps every segment, and a production build takes no window under
four days. Every replay of the whole log first puts back every segment the window removed, read
from the bucket, newest first, so a restore that stops part way leaves the host's segments in one
run: a start with no daily snapshot to go on from, a `[snapshots]` start with no cache, a start whose
posted log head is older than the newest daily snapshot's last 65,536 instructions, a member that
builds its replica from its whole copy, and a handover's replay of a copy with no daily snapshot to
go on from. Each needs room on the log's volume for the whole log, as does a member whose copy is
empty, which takes every segment from the archive before its window trims it. A history index that
rebuilds after `history.retention` grew puts back the segments its new retention holds first, at the
start of the node or of the member's copy, from the daily snapshot before the new retention, or the
whole log when the host no longer holds a daily snapshot that old, as once the daily snapshots' own
window removed it; the start refuses under `history_recovery_failed` while the archive cannot give
them back. The uploader reads from the bucket, as its manifest line names it, a segment of a range it
uploads or reads back that the host lacks, and builds the range from it, writing nothing to the log's
directory; it waits at the range while the bucket cannot give it back. A segment the daily snapshots or
the history index keep, from the floors on, that the host lost is put back like any other the host must
keep.

A copy of a log directory that a window trimmed does not replay from genesis on its own. Give the
command that reads it the node config whose `[archive]` names the bucket, and it puts the removed
segments back into the directory first, which then needs room for the whole log:

```shell
$ENGINE replay --log <log directory> --archive-config <node config>
$ENGINE carry --log <log directory> --seq $S --check --archive-config <node config>
```

`engine replay --carry-source <log directory>` takes `--carry-source-archive-config <node config>` for that
log's own bucket prefix.

Run either on a copy of a running node's directory, not on the directory itself, whose window would
remove the segments again.

**What the host holds.** The window, plus up to a day for the daily snapshot before the start of the
history's retention, plus what the bucket does not hold yet: about 31 days of segments at launch
flow, however long the node runs. A rotation member holds only the daily snapshots of its own turns
and those it fetched, so it keeps its copy from the newest of them before the history's retention,
and from the one before its history index's last command, which its next turn's start goes on from
no later than: as much more as the gap between them. A member whose copy was empty keeps it whole
until it holds a daily snapshot from before the history's retention, about 30 days.

**Turn it on.** Set `archive.local_window` in the deployment's configurations under
`config/release/`, and deploy the release, as [Deploy and rollback](deploy-and-rollback.md)
describes. Every member must first run a release that reads the archive and starts from a daily
snapshot. The first pass removes at most 64 segments a minute, each read back from the
bucket first.

**Check it.**

```shell
curl -s http://<metrics listener>/metrics | grep -E '^engine_log_local_|^engine_store_bytes\{store="log"\}'
```

`engine_log_local_first_seq` rises as segments leave, `engine_log_local_bytes` stays flat once the
log is older than the window, and `engine_log_local_held_seconds` stays under two days.

**Roll back.** Remove `local_window` and deploy. The segments already removed stay in the bucket, and
readers take them from there.

## When `log_segment_missing` fires

**What is happening.** The node's log lists a segment that its host does not hold, and that it must
keep: one after the oldest segment on the host, which a restart's read of the log refuses to skip,
one the daily snapshots or the history index keep, from the floors on, or one whose ranges the bucket
does not hold yet. Neither the bucket nor any archive mirror gave it back. The alert names the newest
such segment; the window tries the ones before it once that one is back, and the console logs each it
cannot put back while the episode stays open. A segment after the oldest on the host, or one the
bucket does not hold yet, the host removed or lost, since the window removes a segment only once the
bucket gives it back; one the floors keep may be one the window removed before a floor moved back,
which comes back, and the alert resolves, once the bucket answers. While the bucket lacks the
segment's range, the uploads wait at it and `archive_upload_lagging` follows, and a restart refuses a
log with a gap.

1. Read the segment the alert's message names, and the console lines around it.
2. Check whether a member's copy holds the segment: on a member, `ls <node.log_dir>/<segment>`. A
   member uploads its copy's ranges while it follows, so once the vault finalizes a checkpoint past
   them, the bucket holds them, the node puts the segment back from there, and the alert resolves.
3. If no member holds it, copy the file from a backup into the node's log directory, and check its
   SHA-256 against its manifest line:

   ```shell
   grep '"<segment>"' <node.log_dir>/manifest.jsonl
   sha256sum <node.log_dir>/<segment>
   ```

4. Keep the evidence, as [Keep the evidence](incident-response.md#keep-the-evidence) describes, and
   tell the owner: a host that loses the log's segments is an incident.

**How to tell it is resolved.** The alert resolves, and `engine_log_local_segments_restored_total` rose.

## When `log_window_held` fires

**What is happening.** A reader of the log, or the bucket not giving back a segment's bytes, has kept
the oldest segment on the host past `archive.local_window` for more than two days of sequenced time;
the message names which. The host's log grows by that much more than the capacity plan allows.
Nothing is lost, and clients see no change. A wait for the bucket's uploads raises
`archive_upload_lagging` instead, and a hold by the daily snapshots or the history index pages
nothing: it lasts as long as the gap between the daily snapshots the node holds, which on a rotation
member are its own turns', and `daily_snapshot_failed` pages a node that stops writing them.
`engine_log_local_held_seconds` shows every hold.

| The message ends with | Cause and fix |
| --- | --- |
| `a reader of the log holds it` | The history index is catching up, or a member link is reading that far back. It clears when the reader moves on. |
| `the bucket does not give back the bytes ...` | The console names the read's failure. If the bucket gives back other bytes than the manifest line names, as a copy backfilled from another archive can spell them, compare the object with the segment, keep the evidence, and tell the owner. |

**How to tell it is resolved.** The alert resolves, and `engine_log_local_held_seconds` falls.

## When `log_window_failing` fires

**What is happening.** The window's passes over the log's segments have failed for an hour, so no
segment leaves the host and the host's log grows past its capacity plan. The message names the last
failure. Nothing is lost, and clients see no change.

1. Read the console for `the local window's pass over the log's segments failed`, which names each
   failure.
2. A segment that does not match its manifest line, a log directory the node cannot list or write,
   or one that no longer holds the newest segment its writer names, is the host's: keep the
   evidence, as [Keep the evidence](incident-response.md#keep-the-evidence) describes, and tell the
   owner. The window touches nothing of such a directory, and a restart refuses a log that lacks a
   listed segment.

**How to tell it is resolved.** The alert resolves at the next pass that succeeds, and
`engine_log_local_window_failures_total` stops rising.

## How the archive works

- **What the bucket holds.** The log in ranges of 10,000 sequence numbers. Each object holds the
  lines of every entry whose sequence number is in its range, in log order, compressed with zstd
  into one frame with its content's checksum, at
  `log/<custody chain id>/<vault>/<first sequence number>.jsonl.zst`, the number in 20 digits.
  Range `k` starts at `k × 10,000`, so the keys need no listing. The ranges do not follow any
  node's segment files, which each node cuts for itself, so every node builds the same bytes for a
  range, and a node counts an object that spells the same entries otherwise, as a copy backfilled
  from another archive may, as its own.
- **When a range goes up.** Once a completed segment holds the range's last sequence number, and the
  vault has finalized a checkpoint past it that the node's own copy holds at the vault's root. Nothing
  at or below the vault's finalized checkpoint is ever rewritten in the vault's history: a takeover
  cuts the log back to it at most. A copy can still hold another history after an earlier checkpoint,
  as a failed operator's does until it rebuilds its copy, and checking the copy against the vault's
  root keeps that history out of the archive. The node reads the vault's finality through
  `checkpoints.rpc_url` at most once a minute while a range waits for it, and checks a checkpoint
  against its copy once a completed segment holds it.
- **Tails.** The ranges past the vault's finalized checkpoint are not in the bucket yet, and the vault's
  latest checkpoint is one of the node's checkpoints from the finalized one on: a paced checkpoint is
  signed only once its parent is final, and a handover's, a shutdown's or a protocol activation's may
  come between them. For each of those, the node uploads the checkpoint's tail: pieces shaped and named as
  ranges, at `log/<custody chain id>/<vault>/tail/<sequence number>-<root>/<first sequence
  number>.jsonl.zst`, the numbers in 20 digits and the root as the vault's `latestRoot` prints it,
  from the first range the bucket lacks to the checkpoint's own, whose piece ends with the
  checkpoint's entry. The node finds a checkpoint as soon as its log holds it, and uploads its tail
  then, before the vault finalizes it; a tail still waiting once the vault finalizes a later
  checkpoint is dropped. The root in the key keeps a checkpoint that the vault never
  takes, as a copy's that a promoted successor's history superseded, from every key a reader derives
  from the vault. A node whose copy stopped at a range that reads back otherwise, or that
  `archive_copy_superseded` names, uploads no tail.
- **What the node counts as archived.** The node uploads each range create-only, so an object
  already at the key is left as it is. It counts the range as archived only once a read from the
  public URL, over TLS that ends in the enclave, decompresses to its own copy of the range.
  The node uploads in order, and a failure retries the same range, waiting twice as long each time
  up to five minutes.
- **Where uploads come from.** The trading server in office uploads its log, and every member
  uploads its copy while it follows. A member finds what the bucket already holds and uploads only
  what it lacks. A restart finds the first range the bucket lacks with a few existence checks, reads
  back the range before it, and uploads on from there.
- **How it is read.** Readers take the bucket's objects from its public URL themselves; the
  trading server never relays them. A listed segment that the host no longer holds is redirected,
  with a 308, to the object of the range that holds its first sequence number, and a reader derives
  the keys of the segment's other ranges from it. Backfills read the bucket that their own
  `[archive]` names first, then the node origins, mirrors first. A segment rebuilt from the bucket
  is taken only if it matches its manifest line, and every copy built from the archive replays before
  a node relies on it. `engine replay`, `engine exit-proof` and `engine carry` take a server's URL
  for `--log`, download the log through the server and the bucket, and read it as a log on disk.
  `engine replay` and `engine exit-proof` also read the log from the bucket alone, through the vault's
  latest checkpoint, as [Read the log from the bucket alone](#read-the-log-from-the-bucket-alone)
  shows: the ranges until the first the bucket lacks, then that checkpoint's tail.
- **The daily snapshots.** Each goes up as soon as it is published, as the file the node wrote, at
  `snapshots/<custody chain id>/<vault>/<file>`, uncompressed, so its object is the bytes its manifest
  line names. The upload is create-only and sends only those bytes: a file that differs from its line
  creates nothing. The node counts a snapshot as archived once a read of its object from the public URL
  has its line's length and SHA-256, and only then, once it is stamped more than
  `daily_snapshots.local_window` before the newest, removes the host's copy, keeping the newest of
  those. The manifest keeps every line, and `/v1/archive/snapshots/<file>` answers 308 to the object
  of a snapshot the host no longer holds. `engine replay --snapshots <server URL>` and a member's
  start from a snapshot follow it; a member follows it only to the bucket its own `[archive]` names.
- **Size.** At 10,000 sequence numbers a range of launch flow is about 5.5 MB. The range that holds a
  carried genesis can hold all of it, up to the carry file's bound of 1 GiB, and a reader takes a range
  of that size beside the range's other entries. On 30 minutes of launch
  flow at 10,000 accounts, zstd's level 1 compressed the log about 6.1 times, to under a megabyte a
  range, at over a gigabyte a second. Higher levels up to 15 compressed less, level 19 about 6.8 times
  at under 10 MB a second, and gzip about 5.1 times. The tails hold about the log again, since each
  covers the ranges between the vault's finalized checkpoint and its own, and the bucket lock keeps
  them; [The archive's bucket](sev-snp-host.md#the-archives-bucket) counts the bucket's growth and
  cost.
