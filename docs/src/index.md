# One conversation. Many hosts.

<p class="book-intro">Myco is a coding agent for your local workspace and remote hosts over SSH. Its inference and agent libraries also work as building blocks for your own applications.</p>

<div class="book-paths">
  <a class="book-path" href="guide/getting-started.html"><strong>Use Myco</strong>Install, configure a model, and start your first session.</a>
  <a class="book-path" href="manual/overview.html"><strong>Read the manual</strong>The same reference articles that Myco gives its agents.</a>
  <a class="book-path" href="developers/index.html"><strong>Build with Myco</strong>Reuse inference, supply tools, or embed a headless agent.</a>
</div>

## Start where you are

The **user guide** explains the workflow: selecting models, giving project
guidance, working across hosts, and keeping useful context through long sessions.

The **bundled manual** is included verbatim from Myco's runtime articles. Agents
have access to those articles too. Each manual page carries a note explaining
where the installed copy lives.

**Build with Myco** covers the public Rust interfaces and their ownership and
cancellation contracts. It includes runnable examples and links to the
[generated API reference](developers/reference.md). The
[evaluations section](developers/evaluations.md) is reserved for work in progress.

## About this edition

This site is built from `main`. It describes that source revision, which may be
ahead of the latest published crate. For the runtime contract of your installed
binary, use `myco --version` and `myco --help overview`.

Search the book with the search button or **S**. The theme menu includes light
and dark reading modes. Rust's generated reference has its own symbol search.
