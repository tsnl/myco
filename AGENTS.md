# Contributing to Myco

Favor correctness, understandable abstractions, and diagnosable failures.
Use ownership and types to express invariants; document the checks that must
happen at runtime. `DESIGN.md` describes component boundaries and interfaces.

- Work in a dedicated Git worktree. Use `feat/` or `fix/` branch names, and use
  `gh stack` when working with stacked pull requests.
- Keep changes focused on the requested behavior. Explain material assumptions
  and tradeoffs before committing to an interface.
- Keep dependencies acyclic and responsibilities explicit. Put shared contracts
  below their consumers and application policy at the composition boundary.
- Preserve data integrity and make failures observable. Treat persistence and
  protocol changes as compatibility decisions.
- Prefer small, cohesive modules. Introduce abstractions that make invariants
  or responsibilities clearer, and test their contracts.
- Prefer functions around ten lines, each doing one named operation. Keep
  formatting readable; flat dispatch tables can be longer.
- Group related types and functions into clearly named sections in both public
  interfaces and implementation files. Use short headings in this form:

  ```rust
  //
  // Requests and messages
  //
  ```

- Comments explain constraints and non-obvious decisions. Documentation describes
  the supported behavior, with consistent terminology.
- Test externally observable behavior and failure paths. Run formatting, relevant
  tests, and Clippy for Rust changes; report what was and was not exercised.
- Keep builds offline after fetching dependencies. Do not require credentials or
  external services for the default test suite.
- Do not merge pull requests, publish packages, or deploy without authorization.
