# Memory

Load on request before searching, writing, or updating persistent memory, or before moving
information between memory, rules, skills, `AGENTS.md`, or repo docs.

**Rule**: Store durable decisions in the place that owns them — not transcript exhaust.

What to store:

- Record durable facts: decisions, gotchas, regressions, security notes, merged behavior.
- Skip step-by-step logs, temporary hypotheses, command noise, and obvious status churn.
- Never store secrets, raw env values, private keys, tokens, or sensitive user data.
- Final notes should answer: what changed, what was verified, and what not to repeat.

How to use it:

- Search memory before repeating PR / deploy / incident / design work.
- Use memory as past-session recall, not current-state proof. Search and filter first, then fetch
  only the needed details.
- Keep automatic memory context compact. Prefer summaries and token metadata; exclude
  terminal-output noise unless it is specifically needed.

Where knowledge belongs (route to the owner, don't duplicate):

- Rules and guardrails → `agents/rules/`.
- Repeatable workflows → `agents/skills/`.
- Repo facts, hard rules, structure, and source locations → the repo's `AGENTS.md` / `agents/`.
- Public protocol facts → the README and `docs/`; plans and trackers → GitHub Issues and milestones.
- Past events, PR history, incidents, and "where to look next" breadcrumbs → persistent memory.
- Curated gotchas may live in memory only as a compact index pointing at the canonical file.

If two sources conflict, do not merge silently. Name the conflict, verify current state, and update
the canonical source rather than copying stale text into another layer.

Why: useful memory is a compact index of decisions and traps that routes agents to truth. Excess or
misplaced memory competes with repo-owned policy and makes agents slower and less precise.

