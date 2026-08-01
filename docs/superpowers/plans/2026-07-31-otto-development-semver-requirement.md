# otto-development skill: semver requirement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make semver compatibility a first-class, enforceable requirement of the `otto-development` skill, so every agent run treats wire-type and public-API changes as semver-constrained — additive (semver-minor) by default, with a documented version bump in `Cargo.toml` for any breaking (semver-major) change.

**Architecture:** Docs-only. Encode the requirement at the skill's enforcement points in `SKILL.md` (Non-Negotiable Rule 6, the Repository Conventions `Versioning` row, a fast-path exclusion, Load-Bearing Invariant 8, a Phase 2 plan-task requirement, a Common Rationalizations row, a Red Flag) and strengthen the architect-reviewer template in `agent-prompts.md` to check version-bump compliance. No Rust code, no wire types, no Cargo.toml changes — no version bump applies to this change itself.

**Tech Stack:** Markdown (the skill's own docs).

**Spec:** none — fast-path trivial task (single logical file pair, docs-only, no interface/gate/dependency/deploy surface touched; AC is one sentence).

## Global Constraints

- Docs-only: no Rust, no `cargo fmt`/clippy impact, no workspace change.
- No AI self-attribution in commits.
- The skill's existing phrasing ("Additive changes are semver-minor on the wire types", architect-reviewer's "wire types stay semver-minor") is the floor — the new requirement must not contradict it.

## File Structure

| File | Responsibility |
|---|---|
| `.claude/skills/otto-development/SKILL.md` | **Modify.** Add Non-Negotiable Rule 6; upgrade the `Versioning` conventions row; add the fast-path breaking-change exclusion; add Load-Bearing Invariant 8; add the plan-task semver step; add the rationalization row + red flag. |
| `.claude/skills/otto-development/agent-prompts.md` | **Modify.** Architect-reviewer template: check the version bump lands in the same PR for a breaking change. |

## Task Order & Rationale

One task: the two files are a required mirror pair (the SKILL.md references `agent-prompts.md` templates verbatim), so they land in one commit.

---

### Task 1: Add the semver requirement to otto-development

**Files:**
- Modify: `.claude/skills/otto-development/SKILL.md`
- Modify: `.claude/skills/otto-development/agent-prompts.md`

**Interfaces:**
- Consumes: the skill's existing load-bearing sections (Non-Negotiable Rules, Repository Conventions, Load-Bearing Invariants, fast-path criteria, Phase 2 plan format, Common Rationalizations, Red Flags).
- Produces: an inviolate semver rule (additive = semver-minor, no `Cargo.toml` bump; breaking = semver-major — the 0.x minor-position bump at this repo's pre-1.0 state — with a version bump in the same PR), enforced by the quality and architect reviews (and visible to the independent security reviewer via the `Cargo.toml` bump in the diff).

- [ ] **Step 1: Add Non-Negotiable Rule 6 to `SKILL.md`**
  After Rule 5, add: wire types and every crate's public API stay semver-minor (additive only), which needs no `Cargo.toml` bump; a breaking change (renaming/removing/reordering a field or variant, changing a signature or shape) is semver-major — the 0.x minor-position bump at this repo's pre-1.0 state — must be named in the spec and plan, bump the affected crate(s)' `version` in `Cargo.toml` within the same PR, and be flagged to the architect reviewer (visible to the security reviewer via the bump in the diff); a silent breaking change is a rejected PR.

- [ ] **Step 2: Upgrade the Repository Conventions `Versioning` row**
  Replace "Additive changes are semver-minor on the wire types." with a hard-requirement statement referencing Non-Negotiable Rule 6 (additive-only default; breaking = version bump in the same PR, documented in the spec).

- [ ] **Step 3: Add the fast-path breaking-change exclusion**
  Under the fast-path criteria, add: no breaking change to wire types or a crate's public API (renamed/removed/reordered fields or variants, changed signatures) — breaking changes are semver-major and never fast-path.

- [ ] **Step 4: Add Load-Bearing Invariant 8**
  Wire types and public APIs are semver-constrained: additions stay additive so old receivers parse; renaming/removing/reordering a field or variant breaks the protocol and is semver-major (version bump in the PR + explicit review, never an incidental refactor side-effect).

- [ ] **Step 5: Add the plan-task semver step to Phase 2**
  In "Every task MUST include", add: a `- [ ]` step bumping the affected crate(s)' `version` in `Cargo.toml` (semver-major — the 0.x minor-position bump at this repo's pre-1.0 state) before the final commit, when the task makes a breaking change to a public interface or wire type. Additive changes need no bump.

- [ ] **Step 6: Add the Common Rationalizations row + Red Flag**
  Rationalization: "I'll just rename/remove that protocol field — nothing's released yet" → semver-constrained, version bump in the same PR. Red Flag: "About to rename, remove, or reorder a wire-type field or variant without a version bump in the same PR."

- [ ] **Step 7: Strengthen the architect-reviewer template in `agent-prompts.md`**
  Extend the "wire types stay semver-minor" line: additive only — a breaking change bumps the affected crate version(s) in `Cargo.toml` within this same PR, per Non-Negotiable Rule 6.

- [ ] **Step 8: Review the diff and commit**
  No Rust code, so `cargo fmt --all` is vacuous. Verify the diff touches only the two doc files, then:
  ```bash
  git add .claude/skills/otto-development/SKILL.md .claude/skills/otto-development/agent-prompts.md
  git commit -m "docs: add semver requirement to otto-development skill"
  ```
