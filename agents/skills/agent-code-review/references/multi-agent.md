# Agent code review: multi-agent mode

Load in multi-agent mode, after the files [agent-code-review](../SKILL.md) lists
before it.

## Review level

The default is **`medium`**. The user may request one or more of `low`,
`medium`, and `high`. A requested level launches only its listed Claude and
Codex models from [ask-agent's review-level
roster](../../ask-agent/SKILL.md#review-level-roster). When multiple levels are requested,
launch the pair listed for every requested level and cross-validate their
reports. The roster defines model IDs and xhigh effort. Reject an invalid
review level rather than silently changing review depth.

## Artifact formats

Each initial report groups findings under `## Critical`, `## High`, `## Medium`, `## Low`, and `## Nit`. Each finding uses:

```markdown
### <Name — short imperative title>
- **Severity:** Critical | High | Medium | Low | Nit
- **Complexity:** Trivial | Small | Medium | Large
- **Confidence:** High | Medium | Low
- **File(s):** `<path>:<line>` (or `<path>:<start>-<end>`)
- **Issue:** 1-3 sentences.
- **Scenario:** the sequence of events that triggers it, or `n/a` for a style finding.
- **Suggested fix:** 1-3 sentences.
```

Each validation uses:

```markdown
### <Reviewer> — <finding title>
- **Verdict:** Confirmed | Rejected | Needs context
- **Evidence:** precise source evidence.
```

Put a high-confidence newly observed finding under `## New findings` using the
initial-report format.

## Process

Prepare one identical independent-review prompt per selected reviewer. It must
name the absolute paths of [agents/AGENTS.md](../../../AGENTS.md),
[development-checklist](../../../rules/development-checklist.md), and
[severity](severity.md), resolved scope, captured diff location, and expected
finding format. It must require the reviewer to read all three files, load every
selected rule and implementation skill, walk every applicable checklist section,
grade each finding with the severity guide, return only complete findings, and
avoid all other review reports. Permit only the named instruction files, captured
diff, and necessary surrounding source at the recorded head SHA; prohibit edits,
Git mutations, tests, fixes, delegation, and output other than the requested
report.

Launch every reviewer in a **fresh** [ask-agent](../../ask-agent/SKILL.md)
session concurrently after saving `.branch.diff` or `.pr.diff`. Give each
reviewer only that captured diff; a delta round ([the review
loop](../SKILL.md#the-review-loop)) gives it `.residual.diff` to review and
`.branch.diff` as context. Do not provide a session ID,
current-conversation context, or another agent's output. Each requested level
launches two independent review processes: one Claude and one Codex. Save each
response as `initial-<reviewer>.md`. Name each `<reviewer>`
`<provider>-<level>`, for example `claude-high` or `codex-low`.

After all independent reviews complete, resume every reviewer's own session
concurrently. Give it the source diff and every `initial-<other-reviewer>.md`,
but never its own report as a file. Require a `Confirmed`, `Rejected`, or
`Needs context` verdict with precise source evidence for every externally
reported finding. Apply the same read-only access scope as the independent
prompt. It may add only a newly observed **high-confidence** finding.
Save the response as `validation-<reviewer>.md`.

Every Codex reviewer validates every Claude report and every Claude reviewer
validates every Codex report. When multiple levels are requested, same-family
reviewers also validate their peers' reports. Do not skip a report because
another reviewer already reached a verdict.

The run also contains:

- `initial-<reviewer>.md` — independent findings.
- `validation-<reviewer>.md` — validation of every other initial report.
- `<reviewer>.session-id`, `<reviewer>.response.{json,jsonl}`, and
  `<reviewer>.stderr` — `ask-agent` bookkeeping.
- `consolidated.md` — distinct candidates with reports and verdicts.

The current agent dedupes candidates by affected code and issue in
`consolidated.md`, preserving reports and validation verdicts. Consensus is
evidence, not proof: retain disagreements and their source evidence. Re-check
consolidated candidates against the code and full conversation context, and run
the fix-notes check from [Review](../SKILL.md#review), before writing the final
report. Preserve the `Reported by` and `Validated by` fields;
rejected and context-dependent candidates remain in `consolidated.md`.

Never edit an initial or validation report after it is written. Never
substitute a lower effort or a different model to save capacity.
