# Log archive

Use this runbook to set up the bucket that keeps the public log for good, to check that the trading
server uploads to it, and to act when one of the archive's alerts fires. The log must stay public
and replayable from genesis, and the archive is where it stays once the host no longer holds every
segment. Commands, configuration keys and their output call a running trading server a node.

- [What a deployment needs](#what-a-deployment-needs)
- [Set up the bucket](#set-up-the-bucket)
- [Check the uploads](#check-the-uploads)
- [When `archive_upload_lagging` fires](#when-archive_upload_lagging-fires)
- [When `archive_object_mismatch` fires](#when-archive_object_mismatch-fires)
- [How the archive works](#how-the-archive-works)

The steps use the shell variables that [Variables](reference.md#variables) sets up, and the ones
below.

## What a deployment needs

Each deployment has a bucket of its own: staging's is not production's. Every member of the rotation
set uploads to the deployment's bucket, with the same keys. Each log's objects sit under its custody
chain and vault, `log/<chain id>/<vault>/`, so a new log on a new vault, as at the move to the launch
vault, can share its predecessor's bucket.

| Need | Detail |
| --- | --- |
| A Cloudflare R2 bucket | Its name is `archive.bucket`, and its account's S3 API endpoint, `https://<account id>.r2.cloudflarestorage.com`, is `archive.endpoint`. The node signs its uploads with AWS Signature Version 4 for the region `auto`, so another S3-compatible store works with its own endpoint and region. |
| An indefinite bucket lock on `log/` | Once the lock covers an object, nobody can replace or delete it, whatever keys they hold. Every object the node writes is under `log/`. |
| A custom domain for public reads | `archive.public_url`. Readers, and the node's own check of each upload, read objects there. Do not use the `r2.dev` address, which Cloudflare rate-limits and does not support for production. |
| An API token | Object read and write on this bucket only. Its access key id and secret access key are delivered as `archive_access_key_id` and `archive_secret_access_key`. |

## Set up the bucket

The owner does these steps in the Cloudflare account that holds the bucket.

**Changes the live system.** Each step changes the deployment's storage or its secrets, and needs the
owner's approval.

1. Create the bucket on the **R2 object storage** page of the Cloudflare dashboard.
2. Open the bucket's **Settings** tab. On the **Bucket lock rules** card, select **Add rule**, name
   it, set the prefix `log/`, keep objects locked indefinitely, and select **Save changes**. See
   Cloudflare's [bucket locks](https://developers.cloudflare.com/r2/buckets/bucket-locks/).

   **A rule without the prefix, or with a retention period, does not protect the archive for good.**
   Check the rule's prefix and its retention before you go on.
3. On the same tab, under **Custom Domains**, select **Add**, enter the domain, and connect it. Leave
   its caching at the default. If a rule caches every file on the domain, keep it from caching `404`
   answers under `log/`: the node reads each object back as soon as it creates it, and a cached
   `404` delays the read until the cache expires.
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
   and finalize.
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

**A first release with the archive uploads the whole log from its first range.**
`engine_archive_pending_ranges` falls as the backlog uploads, and `archive_upload_lagging` fires if it
takes longer than `archive.lag_alert`. Nothing needs to be done unless it stops falling.

## When `archive_upload_lagging` fires

**What is happening.** A whole range of the log has waited longer than `archive.lag_alert`, an hour
by default, for its upload to be confirmed, or the node has not reached the bucket for that long to
find where to resume. The wait includes the time the vault takes to finalize a checkpoint past the
range. The host keeps the whole log, so nothing is lost, and the trading server keeps sequencing.
Clients see no change.

1. If `checkpoint_backlog` is open too, the vault has finalized no checkpoint for a while, and ranges
   wait for it. Follow [Withdrawal stalls](withdrawal-stalls.md) first.
2. Read the trading server's console for the failed uploads:

   ```text
   an upload to the archive failed, and is retried in 300s: ...
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

3. Watch `engine_archive_pending_ranges`. Once the cause is fixed, it falls at the next retry, at
   most five minutes later.

**How to tell it is resolved.** The alert resolves, and `engine_archive_pending_ranges` is back to a
few.

**Escalate** to the owner if the bucket refuses the uploads for an hour after you fixed the cause,
since every member keeps the whole log on its host until it is resolved.

## When `archive_object_mismatch` fires

**What is happening.** The object for a range of the log reads back as other entries than the node's
own copy of that range. The bucket lock keeps the node from replacing it, so the node never counts
that range as archived, uploads nothing past it, and its host keeps the log from there on. A restart
stops there again, unless other nodes have since uploaded later ranges that match this node's copy.
The trading server keeps sequencing, and `archive_upload_lagging` follows within `archive.lag_alert`.
Every node builds a range's bytes the same way from its own copy, and uploads it only once the vault
has finalized a checkpoint past it, so a range never changes once it is uploaded. Two nodes that
hold different bytes for one finalized range hold different logs, or one object was written by a
holder of the bucket's keys or by Cloudflare. Treat it as an incident.

1. Keep the evidence, as [Keep the evidence](incident-response.md#keep-the-evidence) describes,
   with the alert's `object` and `first_seq` fields.
2. Read the object:

   ```shell
   curl -sf https://<archive.public_url's host>/<object> -o range.zst
   zstd -t range.zst
   zstd -dc range.zst | jq -c 'select(.entry == "command") | .seq' | head -n 1
   ```

   An object that fails `zstd -t`, or whose first sequence number is not the alert's `first_seq`,
   was not written by a node. Rotate the bucket's API token and deliver the new keys to every
   member, as step 6 of [Set up the bucket](#set-up-the-bucket) describes.
3. Tell the owner, with the evidence. Readers of the archive check a range by replaying it.

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
  vault has finalized a checkpoint past it. Nothing at or below the vault's finalized checkpoint is
  ever rewritten: a takeover cuts the log back to it at most. The node reads the vault's finality
  through `checkpoints.rpc_url` at most once a minute while a range waits for it.
- **What the node counts as archived.** The node uploads each range create-only, so an object
  already at the key is left as it is. It counts the range as archived only once a read from the
  public URL, over TLS that ends in the enclave, decompresses to its own copy of the range.
  The node uploads in order, and a failure retries the same range, waiting twice as long each time
  up to five minutes.
- **Where uploads come from.** The trading server in office uploads its log, and every member
  uploads its copy while it follows. A member finds what the bucket already holds and uploads only
  what it lacks. A restart finds the first range the bucket lacks with a few existence checks, reads
  back the range before it, and uploads on from there.
- **Size.** At 10,000 sequence numbers a range of launch flow is about 5.5 MB. On 30 minutes of launch
  flow at 10,000 accounts, zstd's level 1 compressed the log about 6.1 times, to under a megabyte a
  range, at over a gigabyte a second. Higher levels up to 15 compressed less, level 19 about 6.8 times
  at under 10 MB a second, and gzip about 5.1 times.
