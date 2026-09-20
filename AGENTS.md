# myco

Build a small, understandable agent server and web GUI. Favor correctness,
explicit ownership, and diagnosable failures over framework machinery.

- Work in an isolated worktree. Use `feat/` or `fix/` branch names.
- Implement the current review step in `DESIGN.md`; keep subsequent steps as
  design until reviewed.
- Agents are state machines pumped from outside. Finish one event before the
  next event for that agent; different agents execute concurrently with async I/O.
- Agent state access is injected. The application owns persistence formats;
  agent traces and tool-service records remain separate and linked by operation IDs.
- Tool instances belong to workspaces and can be shared by agents and humans.
- The application exposes HTTP APIs and a web GUI. There is no session concept
  and no interactive CLI in this implementation.
- Keep lower crates independent of applications, storage engines, and UI code.
- Use small modules and actionable errors. Comments explain invariants.
- Add tests for behavioral contracts, especially incomplete streams,
  cancellation, and request/response correlation.
- Keep builds offline after dependency fetch. Run formatting, relevant tests,
  and Clippy before presenting a review step.
- Do not publish packages, deploy, or merge without user authorization.
