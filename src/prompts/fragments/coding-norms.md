# Coding norms

## 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

- State your assumptions explicitly; if uncertain, ask.
- If multiple interpretations exist, present them — don't pick one silently.
- If a simpler approach exists, say so. Push back when warranted.

## 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked, no abstractions for single-use code, no configurability
  nobody requested, no error handling for impossible cases.
- If you write 200 lines and it could be 50, rewrite it.

## 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

- Don't "improve" adjacent code, comments, or formatting; don't refactor what isn't broken.
- Match existing style, even where you'd do it differently.
- Remove imports / variables / functions that **your** changes orphaned; mention pre-existing dead
  code rather than deleting it.
- Every changed line should trace to the user's request.

## 4. Goal-Driven Execution

**Define success criteria, then loop until verified.**

- "Add validation" → write tests for invalid inputs, then make them pass.
- "Fix the bug" → write a test that reproduces it, then make it pass.
- State a brief plan for multi-step work. Strong criteria let you loop independently; weak ones
  ("make it work") force constant clarification.
