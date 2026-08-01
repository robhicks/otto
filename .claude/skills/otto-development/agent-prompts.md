# otto-development — Agent Dispatch Prompt Templates

Verbatim prompt bodies for every `Agent`-tool dispatch in `otto-development`. The SKILL.md spine
owns the *decision* logic (when to dispatch, which `model:`/`subagent_type`, status handling,
fix-loop caps, convergence rules); this file owns the prompt *text* you paste.

**Use the template exactly. Do not improvise a dispatch prompt body.** Fill the `<...>` placeholders
from your working memory (spec / plan / task text / report verbatim, plus the repo reminders from
the "Repository Conventions" + "Load-Bearing Invariants" sections of SKILL.md). The `subagent_type`
is named in each template's heading — honor it (the implementer and the review-response subagent are
`general-purpose`; quality + final reviews are `code-reviewer`).

`<ref>` below is the source reference: a GitHub issue (`robhicks/otto#123`), or — on the ticketless
path — the captured task brief.

Unlike `general-development`/`ce-development`, the spec and plan ARE committed repo files
(`docs/superpowers/specs/`, `docs/superpowers/plans/`). Paste the full text inline anyway — a
reviewer must not need to hunt for files — but the repo path is worth giving too so the reviewer can
check against the committed version.

---

## Spec Critique — Phase 1 Step 4 — `subagent_type: general-purpose`

```
Agent tool:
  subagent_type: general-purpose
  description: "Review spec document"
  prompt: |
    You are a spec document reviewer. Verify this spec is complete and ready for planning.

    Spec to review (full text inline; the committed copy is at <docs/superpowers/specs/<slug>-design.md>):
    <PASTE FULL SPEC TEXT>

    Repo context to check against (from otto-development's Load-Bearing Invariants):
    <PASTE THE LOAD-BEARING INVARIANTS SECTION — sensitive-path floor, gated edits, sandbox-only
     bash, inward dependency flow, offline determinism, unauthenticated-by-design static UI route,
     remote-ready trait seams — plus the test/lint commands and plan-by-plan doc conventions>

    Check:
    - Completeness: TODOs, placeholders, "TBD", incomplete sections
    - Consistency: internal contradictions, conflicting requirements
    - Clarity: requirements ambiguous enough to cause someone to build the wrong thing
    - Scope: focused enough for a single plan; respects the repo's workspace crate boundaries
      (protocol → engine-core → impl crates → engine; ui-dioxus workspace-excluded)
    - YAGNI: unrequested features, over-engineering
    - Alignment with the source AC (cite ref <ref>)
    - Whether any design choice would violate the security spine (permission gate, sandbox, auth)

    Only flag issues that would cause real problems during planning. Approve unless there are serious gaps.

    Output:
    ## Spec Review
    Status: Approved | Issues Found
    Issues: - [Section X]: [issue] - [why it matters]
    Recommendations (advisory): - [...]
```

---

## Plan Critique — Phase 2 Step 6 — `subagent_type: general-purpose`

```
Agent tool:
  subagent_type: general-purpose
  description: "Review plan document"
  prompt: |
    You are a plan document reviewer. Verify this plan is complete and ready for implementation.

    Plan to review (full text inline; the committed copy is at <docs/superpowers/plans/<slug>.md>):
    <PASTE FULL PLAN TEXT>

    Spec for reference (full text inline; the committed copy is at <docs/superpowers/specs/<slug>-design.md>):
    <PASTE FULL SPEC TEXT>

    Repo requirements to check against (from otto-development):
    <PASTE THE LOAD-BEARING INVARIANTS + Repository Conventions — test/lint commands
     (cargo test --workspace, cargo clippy --workspace --all-targets, cargo fmt --all),
     out-of-band surfaces (deploy/fly, deploy/install.sh, deploy/homebrew, .github/ CI,
     ui-dioxus bundle guards, feature-gated candle/firecracker builds), plan-by-plan doc
     conventions, "no AI attribution" rule>

    Check: completeness, spec alignment, task decomposition, buildability, and whether each
    task:
    - uses exact file paths in this repo's layout (crates/, ui-dioxus/, deploy/, docs/)
    - orders steps failing-test-first (write failing test → run → implement → run → commit)
    - names the exact verification command per step (cargo test -p <crate> <filter>)
    - includes a final "Format and commit" step (cargo fmt --all + git commit)
    - flags any security-spine or dependency-flow impact
    - flags any out-of-band surface the task touches so Phase 5 verification covers it

    Only flag issues that would cause an implementer to build the wrong thing or get stuck.

    Output:
    ## Plan Review
    Status: Approved | Issues Found
    Issues: - [Task X, Step Y]: [issue] - [why it matters]
    Recommendations (advisory): - [...]
```

---

## Implementer Dispatch — Phase 3 Step A — `subagent_type: general-purpose`

```
Agent tool:
  subagent_type: general-purpose
  description: "Implement Task N: <name>"
  prompt: |
    You are implementing Task N: <name>

    ## Task Description
    <FULL TEXT of the task pasted inline — do not reference the plan file>

    ## Context
    <2-4 sentences: where this fits, dependencies on prior tasks, architectural notes,
    and the repo reminders for THIS task: the test/lint commands (cargo test --workspace,
    cargo test -p otto-<crate> <filter>, cargo clippy --workspace --all-targets, cargo fmt --all),
    the Load-Bearing Invariants that apply (gated edits, sensitive-path floor, sandbox-only bash,
    inward dependency flow, offline determinism, unauthenticated-by-design --ui-dir static route),
    any out-of-band surface this task touches (deploy/fly, install.sh, homebrew, CI, bundle guards,
    feature-gated builds), and the "no AI attribution in commits" rule>

    ## AUTONOMOUS MODE — IMPORTANT

    You are running inside an autonomous pipeline. Do NOT ask clarifying questions.
    There is no developer available to answer mid-run.

    Instead:
    - When the task is ambiguous, pick the most reasonable interpretation given the
      surrounding code and the spec. Document the assumption in your report.
    - If the assumption is high-risk (could plausibly be wrong in a way the developer
      would care about), report DONE_WITH_CONCERNS and list the assumption explicitly.
    - Only return BLOCKED if you genuinely cannot proceed without information that
      cannot be reasonably inferred (e.g., a missing API key, an undocumented external
      contract). Do NOT return BLOCKED for stylistic ambiguity.

    ## Your Job
    1. Work ONLY from the worktree at: <worktree path> (.worktrees/<branch>). Never touch the main
       checkout. Do not commit or push to main directly.
    2. Follow the task's TDD steps in order: failing test → run → implement → run → commit.
       Use the repo's actual test/lint commands (given in Context).
    3. Use exact file paths and commands from the task. Do not invent your own.
    4. Run `cargo fmt --all` before every Rust commit (rustfmt is pinned in rust-toolchain.toml).
    5. Self-review before reporting (completeness, quality, YAGNI, testing).
    6. Commit per the task's step-by-step instructions, using the repo's commit format
       (`<scope>: <subject>`). Never add AI/Co-Authored-By attribution to a commit message.
    7. You are working inside otto's own repository: the deterministic offline test suite must
       stay green — no network or API keys required by default.

    ## Report Format
    - Status: DONE | DONE_WITH_CONCERNS | BLOCKED | NEEDS_CONTEXT
    - Files changed (with commit SHAs)
    - Test results (command + outcome)
    - Assumptions made (with one-line rationale each)
    - Concerns or blockers (if any)
```

---

## Spec Compliance Review — Phase 3 Step C — `subagent_type: general-purpose`

```
Agent tool:
  subagent_type: general-purpose
  description: "Spec compliance: Task N"
  prompt: |
    You are reviewing whether an implementation matches its specification.

    ## What Was Requested
    <FULL TEXT of the task — same as implementer received>

    ## What Implementer Claims They Built
    <implementer's report verbatim>

    ## CRITICAL: Do Not Trust The Report
    Read the actual code at the commit SHAs they listed. Verify line-by-line.

    Check:
    - Missing requirements (claimed implemented but actually skipped)
    - Extra work (built features not requested)
    - Misinterpretations (right feature, wrong way)
    - Repo-specific gotchas from otto-development's conventions:
      - dependency flow strictly inward (engine-core must never depend on a concrete impl crate)
      - no gate/sensitive-path-floor/sandbox weakening; Coder edits gated fail-closed
      - `OTTO_*`/API-key reads behind build_router, not in core logic
      - `ui-dioxus/` stays workspace-excluded; `cargo build/test --workspace` must not require dx
      - no AI attribution in commits/comments/docs
      - out-of-band surfaces the task touched are flagged for Phase 5 (fly image, install.sh,
        homebrew, CI, bundle guards, feature-gated builds)

    Report:
    - ✅ Spec compliant
    - ❌ Issues found: [list with file:line refs]
```

---

## Code Quality Review — Phase 3 Step E — `subagent_type: code-reviewer`

Capture commit boundaries first (the SKILL.md step E owns this):
`BASE_SHA = git rev-parse HEAD~<N>` (N = commits this task produced), `HEAD_SHA = git rev-parse HEAD`.

```
Agent tool:
  subagent_type: code-reviewer
  description: "Quality: Task N"
  prompt: |
    Review the code changes between <BASE_SHA> and <HEAD_SHA>.

    Plan/requirements: Task N (full text inline; the plan is committed at
    docs/superpowers/plans/<slug>.md):
    <FULL TEXT of task>

    Check standard code-quality concerns plus:
    - One clear responsibility per file?
    - Units decomposed for independent testing?
    - Following file structure from the plan?
    - Did this change create or grow files significantly beyond what the task required?
    - Repo-specific conventions (from CLAUDE.md and otto-development's Load-Bearing Invariants):
      - Rust idioms: tests live next to code (#[cfg(test)] mod tests), wiremock for HTTP,
        tempfile for fs, ScriptedProvider for agent tests
      - trait seams Send + Sync + async; orchestrator holds trait objects
      - deterministic offline default preserved
      - security spine: gated edits, sensitive-path floor, sandbox-only bash
      - no AI attribution in comments/docs

    Report: Strengths, Issues (Critical / Important / Minor), Assessment.
```

---

## Final Code Review — Phase 3 Step H — `subagent_type: code-reviewer`

```
Agent tool:
  subagent_type: code-reviewer
  description: "Final review: <slug>"
  prompt: |
    Final review of the complete implementation.

    Plan (full text inline; committed at docs/superpowers/plans/<slug>.md):
    <PASTE FULL PLAN TEXT>
    Spec (full text inline; committed at docs/superpowers/specs/<slug>-design.md):
    <PASTE FULL SPEC TEXT>
    Branch: <branch-name>
    Diff range: <merge-base-with-trunk>..HEAD

    Verify:
    - All plan tasks are implemented end-to-end
    - The implementation actually achieves the spec's success criteria
    - No dead code, leftover debug, or skipped tests
    - Test coverage is reasonable for what was built
    - Repo convention compliance (from CLAUDE.md + otto-development's Load-Bearing Invariants):
      inward dependency flow, security spine intact, offline determinism preserved,
      ui-dioxus workspace-excluded, commit format `<scope>: <subject>`, no AI attribution,
      out-of-band surfaces (fly/install.sh/homebrew/CI/bundle/feature builds) handled and
      flagged for Phase 5 verification

    Report: Strengths, Issues, Overall assessment (Ready to merge / Needs work).
```

---

## Mandatory Review Trio — Phase 4 Step 8 — `subagent_type: rust-pro` / `architect-reviewer` / `security-auditor`

Every PR gets all three reviews, no exceptions (Non-Negotiable Rules 4–5). Capture the diff range
first: `git log --oneline <merge-base>..HEAD` from inside the worktree, or use `gh pr diff <PR>`.
Dispatch all three in the same parallel batch, each reading the actual diff, not the summary.

### Rust expert — `subagent_type: rust-pro`

```
Agent tool:
  subagent_type: rust-pro
  description: "Rust review: <scope>: <subject>"
  prompt: |
    You are the mandatory Rust-expert reviewer for PR #<N> on robhicks/otto (<ref>).

    Review the actual diff:
      gh pr diff <N>
      gh pr view <N> --json commits,files,title,body

    Check against otto's Rust conventions (from CLAUDE.md):
    - Idiomatic Rust: ownership/lifetimes, error handling, no panics on untrusted input
    - Trait seams are Send + Sync + async; the orchestrator holds trait objects, never concrete impls
    - Tests live next to code (#[cfg(test)] mod tests); wiremock for HTTP, tempfile for fs,
      ScriptedProvider for agent tests
    - Determinism is a test invariant: the offline default path must stay reproducible; anything
      reading OTTO_* / a provider API key belongs behind build_router, never in core logic
    - The permission gate / sensitive-path floor is never weakened; Coder edits stay gated fail-closed;
      bash is registered only with a sandbox backend
    - cargo fmt --all is clean; clippy-clean
    - No AI attribution in commits, comments, or docs

    Report: Strengths, Issues (Critical / Important / Minor), Assessment (Approve / Request changes).
```

### Architect — `subagent_type: architect-reviewer`

```
Agent tool:
  subagent_type: architect-reviewer
  description: "Architect review: <scope>: <subject>"
  prompt: |
    You are the mandatory architectural reviewer for PR #<N> on robhicks/otto (<ref>).

    Review the actual diff and the spec/plan for this change:
      gh pr diff <N>
      gh pr view <N> --json commits,files,title,body
      # Spec/plan (committed repo files): docs/superpowers/specs/<slug>-design.md,
      # docs/superpowers/plans/<slug>.md

    Check against otto's architecture (docs/ARCHITECTURE.md — the intended destination — and the
    latest plan for what is actually shipped):
    - Inward dependency flow preserved: protocol → engine-core → impl crates → engine;
      engine-core must never depend on a concrete impl crate
    - Crate boundaries respected; new capabilities added via AgentCtx accessors or new seams,
      never by widening public surface
    - The change matches the spec/plan it claims to implement; wire types stay semver-minor
    - Trait seams stay remote-ready (Send + Sync + async); extensible payloads stay JSON Value
    - ui-dioxus stays workspace-excluded; workspace build/test never requires dx
    - The change follows the documented design rather than eroding it; deviations are justified

    Report: Strengths, Issues (Critical / Important / Minor), Assessment (Approve / Request changes).
```

### Independent security review — `subagent_type: security-auditor`

> **CRITICAL — this review is blind by design.** The security agent receives **only the diff**. It
> is never given the spec, the plan, the task brief, the PR body summary, the issue, or the
> implementer's report. Its findings must come from the code alone. This independence is a
> Non-Negotiable Rule — do not paste spec/plan context into this dispatch, and do not ask the agent
> to read the docs.

```
Agent tool:
  subagent_type: security-auditor
  description: "Independent security review: <scope>: <subject>"
  prompt: |
    You are the independent security reviewer for PR #<N> on robhicks/otto.

    You receive ONLY the diff — deliberately. Do not read the PR description, the linked issue,
    any design spec or plan, or any implementer summary. Your findings must be derived from the
    code changes alone.

    The diff:
      gh pr diff <N>

    Evaluate the change for security defects, focusing hardest on otto's security spine:
    - Sensitive-path floor: anything that could read, serve, or expose `.env*`, `.ssh/`, `.git/`,
      `.aws/`, or ssh keys; any path-containment or traversal hole
    - Authentication/authorization: the bearer-token checks on /ws, /workspace, /promote, /export,
      and the unauthenticated-by-design static --ui-dir route (which must never be defaulted or
      point at a workspace root)
    - The permission gate: any route that bypasses or weakens gating, any edit applied without an
      explicit Allow, any bash registered without a sandbox backend
    - Secrets/PII: logging, exposing, or persisting secrets; sensitive data crossing a trust boundary
    - Injection: shell/argv injection (mcp-git's leading-dash and URL-scheme rules), SQL, path
      traversal, template injection, untrusted-input panics or overflows
    - Sandbox escapes, unsafe blocks, TLS/auth bypass, algorithm confusion

    Report: Strengths, Issues (Critical / Important / Minor), Assessment (Approve / Request changes).
```

Aggregate all three reports — plus the pr-review-toolkit and Copilot findings — into the single PR
comment grouped by **Critical / Important / Suggestions / Strengths**. Critical/Important findings
from any reviewer must be fixed or explicitly dismissed (with reasoning) before merge.

---

## Review-Response Subagent — Phase 4 Step 9 — `subagent_type: general-purpose`

```
Agent tool:
  subagent_type: general-purpose
  description: "Address PR review feedback"
  prompt: |
    You are addressing PR review feedback on PR #<N> for <ref>.
    Follow otto-development requirements (Phase 4 step 9).

    Your job:
    - Read all unresolved review threads on the PR (including the aggregated rust-pro,
      architect-reviewer, and independent security-auditor findings if they were posted as comments)
    - For each comment: either fix-and-reply ("Fixed in <sha>") or explicitly dismiss
      with reasoning. NEVER silent dismissal.
    - After each reply, resolve the conversation thread via GraphQL:
        gh api graphql -f query='mutation {
          resolveReviewThread(input: {threadId: "<thread_id>"}) {
            thread { isResolved }
          }
        }'
    - Always reply inline to each comment explaining how the feedback was addressed
      (keeps the review thread traceable).
    - For automated reviewers, a flagged false positive should be verified then dismissed
      with reasoning.
    - Do all fix work in the existing worktree (.worktrees/<branch>) and push to the PR
      branch — never commit to main directly.
    - Never add AI attribution to commit messages or replies.
    - If the same thread remains unresolved across multiple subagent runs, escalate
      (do not silently retry).

    Return when all threads are resolved or escalation is needed. The main thread
    receives only the summary (what was fixed, what was dismissed, any escalations).
```
