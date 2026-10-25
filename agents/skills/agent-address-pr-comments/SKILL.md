---
name: agent-address-pr-comments
description: >
  Use when asked to work through the review comments on a GitHub PR — "address the PR comments",
  "address review feedback", "reply to the review", "handle the comments on #123",
  "agent-address-pr-comments", "address-pr-comments". Verifies each open thread and actionable
  top-level comment against the code, agrees the triage with the user, fixes what is real, in
  scope, and worth changing, declines the rest with a reason, updates the PR description, pushes,
  and posts only approved replies. The counterpart to `agent-review-pr`: that skill writes
  comments, this one answers them.
---

# agent-address-pr-comments

Take the review feedback on a pull request, decide what each item deserves, do the work, and
answer it. Fixes are committed to the PR branch and pushed; replies are drafted, approved by the
user, and only then posted.

**Core principle:** verify before you act. A reviewer can be wrong, and so can you. A comment is a
request, not an instruction: a true claim can still sit outside this PR or not be worth changing.
Agree the triage with the user before changing code; commit, push, and replies wait for a second
approval once the fixes are made.

## When to use

- A PR has review feedback to triage, fix, and answer in one pass.
- Not for *writing* review comments — use `agent-review-pr` or `agent-code-review`.

Modes: the open PR for the current branch (default), or an explicit `#42`, `PR 42`, or PR URL.

## Isolation

Work in the current working tree, on the PR's head branch: this skill edits, commits, and pushes.
Create a worktree only when the user asks, through
[agent-create-worktree](../agent-create-worktree/SKILL.md), attached to the PR's head branch with
its original name. Before touching anything, confirm the checkout is on that branch, clean, and in
sync with its remote; if not, stop and tell the user.

Use `gh`, authenticated as the user, with a write-capable credential. Never print or persist a
secret value.

## Gathering the feedback

Load [no-guessing-current-state](../../rules/no-guessing-current-state.md) and fetch live state,
every page. `{owner}` and `{repo}` are filled in by `gh`.

```bash
# Inline review threads, with resolution state and every comment.
gh api graphql --paginate -F owner='{owner}' -F repo='{repo}' -F pr="$PR" -f query='
query($owner: String!, $repo: String!, $pr: Int!, $endCursor: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $pr) {
      reviewThreads(first: 100, after: $endCursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id isResolved isOutdated path line
          comments(first: 100) {
            pageInfo { hasNextPage }
            nodes { databaseId author { login } body url createdAt }
          }
        }
      }
    }
  }
}'
# Top-level PR comments and review summary bodies.
gh api --paginate "repos/{owner}/{repo}/issues/$PR/comments"
gh pr view "$PR" --json author,reviews --jq '.reviews[] | select(.body != "")'
```

A thread whose `comments.pageInfo.hasNextPage` is true has more comments than the first page: fetch
the rest before reading it, so its last comment really is the last one.

```bash
gh api graphql --paginate -F id="$THREAD_ID" -f query='
query($id: ID!, $endCursor: String) {
  node(id: $id) { ... on PullRequestReviewThread {
    comments(first: 100, after: $endCursor) {
      pageInfo { hasNextPage endCursor }
      nodes { databaseId author { login } body url createdAt }
    }
  } }
}'
```

The items to answer are:

- **Each unresolved review thread**, read as a whole conversation. Skip a thread whose last comment
  is already an answer from the PR author with no reviewer comment after it; list it in the report
  as already answered.
- **Each top-level comment or review body that asks for a change.** Others are listed in the report
  without a reply.

Read every comment in full. Keep each thread's root comment `databaseId` and `url`.

## Triage

For every item, read the code it points at and answer three questions in order. A `no` stops the
change:

1. **Is the claim true?** Check the code, not the wording.
2. **Does it belong to this PR?** A comment can be right about the repo and still be about code
   this diff does not touch.
3. **Is the change worth making?** A true, in-scope claim can still buy nothing — churn, a
   speculative abstraction, a preference the repo does not follow elsewhere.

| Outcome | What it means | What you owe the item |
| --- | --- | --- |
| **Fix** | Right, in scope, and worth it. | Make and verify the local change; report it and draft the reply. |
| **Already fixed** | Later work on the branch resolved it. | Reply saying what happens now. |
| **Not a defect** | The claim does not hold. | Reply with the evidence that disproves it. |
| **Pre-existing** | Real, but not introduced by the PR. | Show it on the base branch (`git show <base>:<path>`); offer a follow-up. |
| **Out of scope** | True and worth doing, but not about this PR. | Say so and offer a follow-up. |
| **Not worth doing** | True and in scope, but earns nothing. | Say what you decided and why, in one sentence. |
| **Needs a decision** | Real and the user's call, or neither side can be evidenced. | State what evidence is missing and ask the user. |

- Prove "pre-existing" and "not a defect" with evidence; when you cannot prove the claim or
  disprove it, the outcome is `Needs a decision`, never a silent fix or a silent dismissal.
- When the reviewer is right and you argued otherwise, say so plainly and correct anything built on
  the mistake, including the PR description.
- Do only what this PR is about ([minimal-change](../../rules/minimal-change.md)), and justify every
  change on its own — closing a thread is not a justification.
- A suggested fix — a `suggestion` block or a `Suggested fix` line — is one option, not the
  instruction. Weigh it against the other ways to remove the failure and make the best one.
- A follow-up becomes a GitHub issue only with the user's explicit approval.

## Reply format

**One short sentence: that it is fixed, and how.** Pushing back owes a reason and still fits in two
sentences.

```
Fixed — every chain read is bounded, so a hung endpoint errors instead of stalling.
```

```
Same two-call sequence is already on main, so it predates this PR. Happy to file it separately.
```

```
The wrapper would have one caller here, so leaving the call inline. Shout if you want it anyway.
```

Replies follow [writing-style](../../rules/writing-style.md). No commit SHAs, branch names, severity
tags, headings, lists, code blocks, restating the comment, or AI signature.

## Fixing and verifying

- Fix each `Fix` item by class, as
  [Fixing an accepted finding](../agent-code-review/SKILL.md#fixing-an-accepted-finding) says: its
  invariant and the sites fixed go in the branch's fix notes and the item's `Changes`, and its reply
  states the invariant. A review round over the fixes follows
  [the review loop](../agent-code-review/SKILL.md#the-review-loop).
- Check every fix against the applicable sections of
  [development-checklist](../../rules/development-checklist.md): a fix must not open a new
  concurrency, crash-recovery, or test gap, and it must remove the failure, not just the flagged
  line.
- Run the local checks for the affected packages per
  [git-workflow](../../rules/git-workflow.md) before the report — the request to address the
  comments approves them, per [context-discipline](../../rules/context-discipline.md) — never the
  whole CI, never a publishing step. Commit after approval, one commit per coherent change.

## Approval report

Print it once the fixes are made and the local checks pass, so it describes what actually changed.
Number every item and keep the numbers in later discussion:

- **Link** — the thread root or comment URL.
- **Comment** — a short plain-language restatement.
- **Severity** — the comment's own `Severity` when it has one, otherwise your assessment with the
  [severity guide](../agent-code-review/references/severity.md), and why.
- **Changes** — the completed local change, with the invariant it restores and every site fixed,
  or `No change` with the outcome and reason. Only `Needs a decision` may defer a change.
- **Reply** — the exact text to post, or `none` for a top-level comment that asks for nothing.

```
1. https://github.com/<org>/<repo>/pull/42#discussion_r3845829157
   Comment: A chain read can hang indefinitely.
   Severity: High — it blocks the update loop.
   Changes: Every chain read is bounded — added a timeout to the block, log, and receipt reads.
   Reply: Fixed — every chain read is bounded, so a hung endpoint errors instead of stalling.
```

After the items, show every proposed commit message, then the exact updated PR description or
`unchanged`. The user approves changes, replies, commit messages, and description together. An
explicit instruction to commit, push, and reply without review counts as that approval; the report
is still printed.

## Updating the PR description

The PR squashes into one commit, so its title and body describe the pushed branch as it now
stands. When a fix adds, removes, or changes behavior the description covers — or corrects a claim
it makes — rewrite the affected title, overview, or bullets in the
[agent-create-pr](../agent-create-pr/SKILL.md#body) format. Describe the result, not the review:
no "addressed review comments" bullets. Keep the issue line.

```bash
gh pr edit "$PR" --body-file <new-body.md>        # add --title "<new title>" only if it changed
```

## Posting

Post the approved text verbatim; if the wording should change, ask again. Every reply goes out in
one review, so the PR gets one notification rather than one per reply. GitHub allows one pending
review per user on a PR: if you already have one, stop and ask the user to submit or delete it.

1. Open a pending review (no `event` field) and keep its `id` and `node_id`. Its body holds the
   answers to top-level comments and review bodies, which cannot anchor to a line; leave the body
   out when there are none.

   ```bash
   gh api --method POST "repos/{owner}/{repo}/pulls/$PR/reviews" -f body="$TOP_LEVEL_ANSWERS" \
     --jq '.id, .node_id'
   ```

2. Stage each thread reply in it, using the thread `id` from the gathering query:

   ```bash
   gh api graphql -F review="$REVIEW_NODE_ID" -F thread="$THREAD_ID" -f body="$REPLY" -f query='
   mutation($review: ID!, $thread: ID!, $body: String!) {
     addPullRequestReviewThreadReply(input: {
       pullRequestReviewId: $review, pullRequestReviewThreadId: $thread, body: $body
     }) { comment { url } }
   }'
   ```

3. Submit it once as a comment:

   ```bash
   gh api --method POST "repos/{owner}/{repo}/pulls/$PR/reviews/$REVIEW_ID/events" -f event=COMMENT
   ```

If a step fails, delete the pending review
(`gh api --method DELETE "repos/{owner}/{repo}/pulls/$PR/reviews/$REVIEW_ID"`), post nothing else,
and report the failure.

## Workflow

1. Resolve the PR; confirm the checkout is on its head branch, clean, and in sync.
2. Gather every open thread and actionable top-level comment, every page.
3. Triage each item against the code.
4. **Print the triage — outcome, planned change, and draft reply per item — and wait.** A request
   to fix without review skips this wait.
5. Make the approved fixes, each by class ([Fixing and verifying](#fixing-and-verifying)); ask
   before anything that widens scope.
6. Verify the fixes and run the local checks. On a feature PR, a fix that changes the feature's
   behaviour also runs its dev scenario again
   ([agent-build-feature](../agent-build-feature/SKILL.md#pr-and-merge)).
7. **Print the approval report and wait. Post nothing yet.**
8. Re-read the PR head, its description, and every thread you will answer; if anything changed,
   re-triage and print a refreshed report.
9. Commit and push.
10. Update the PR description as approved.
11. Post the approved replies as one review.
12. Report one line per item with its link: what was done, what was replied, and why anything was
    not fixed.

## Guardrails

- Verify every claim against the code; do not fix on the reviewer's word or dismiss on your own.
- Never post a reply the user has not approved; only an explicit instruction to reply without
  review replaces the wait.
- Never say "fixed" before the fix is pushed.
- Never leave an open thread or an actionable top-level comment unanswered.
- Never leave the PR description behind the pushed fixes.
- Post every reply in one `COMMENT` review; never one call per reply.
- Never approve, request changes, or resolve a thread — the reviewer resolves their own threads.
- Never widen the PR's scope without asking.
- Never print or persist secrets.
- Never create, edit, or close a GitHub issue without the user's explicit approval, except the
  `bug` issues [the review loop](../agent-code-review/SKILL.md#the-review-loop) files for Mediums
  from round 3 on.
- No AI signature in commits, replies, or the PR description.
