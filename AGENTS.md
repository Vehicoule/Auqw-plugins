# Agent guidelines

Owns: how AI agents (and humans) work in these repositories. [conventions.md](conventions.md) owns naming and structure; [coding-rules.md](coding-rules.md) owns language rules. These docs are copied by hand into `AGENTS.md` at each repo root — the docs here stay authoritative.

## Before you work

1. Read [README.md](../README.md), [product.md](../product.md), and [decisions.md](../decisions.md). The decision log is the source of truth for what is settled — never re-litigate a Decided row in code; change it in the log first, with rationale.
2. Work in slice units ([plan.md](../plan/plan.md)). Open the slice file; implement against its work breakdown; close its exit gates. If a task isn't in a slice, it isn't work — it's scope creep (risk R4).
3. When a plan doc is ambiguous, stop and surface the ambiguity (issue or comment) instead of guessing architecture. Small local choices (naming, test structure) are yours; boundary choices are not.

## Non-negotiables

- **Greenfield:** no code, schemas, or shims copied from the predecessor projects. Their docs are reference material only ([history.md](../history.md)).
- **A slice is not done until the phone plays sound through the new code path** (Slice 4: the desktop).
- **Never mark an exit gate passed without evidence.** Evidence = command output, measurement, or a recorded journey on a real target. Simulator/emulator/dev-server results are labeled provisional.
- **Never bypass the sandbox:** no ambient network, filesystem, or credential access for plugin guests; budgets and the import allowlist are not suggestions.
- **Never expand scope silently.** A new dependency, package, permission, or capability is a decision-log event first.
- **No secrets in code, logs, or fixtures.** Use the redaction helpers; tokens, signed URLs, and cookies never appear raw.

## Workflow

1. Pick the current slice's next open item.
2. Branch per [conventions.md](conventions.md) (trunk-based, `s<slice>/topic`).
3. Implement per [coding-rules.md](coding-rules.md); run the full local gate (fmt, lint, typecheck, tests) before committing.
4. CI must be green before merge; required checks are non-overridable.
5. Update the slice file: check off gates **only** with an evidence note appended to the file's `## Evidence` section (date, target, command/journey, result, numbers).

## Evidence recording format

```text
- 2026-09-18 · Android (Pixel 8, Android 15) · playback corpus runner
  `pnpm --filter mobile corpus` · 28/30 correct, 0 wrong-version, 2 honest-unavailable
  cancellation p95: 210 ms guest-stop / 340 ms HTTP-abort
```

## Documentation rules (all contributors)

1. Each document owns exactly one topic; change a decision in its owning document, never duplicate it.
2. Statuses are **Decided**, **Deferred**, or **Open** — nothing else.
3. A decision records rationale and a reopen condition.
4. Docs stay short: grow by adding concrete artifacts (schemas, sketches, examples), not prose.
5. Evidence beats documentation: "it works on the phone" outranks any claim in these files.

## Self-review checklist before opening a PR

- [ ] Does the diff match the slice item it claims to close — nothing more?
- [ ] Are all new boundaries/ports typed, with cancellation carried through?
- [ ] Are errors typed per the taxonomy — nothing raw crossing a boundary?
- [ ] Tests: unit for logic, property for invariants, fixtures for parsing; device/browser evidence only where gates demand it?
- [ ] Logs redacted; no `TODO` shipped in a closed item; no commented-out code?
- [ ] Docs updated where a decision actually changed (not prose-polished)?
