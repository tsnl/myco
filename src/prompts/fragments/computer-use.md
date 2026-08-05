# Computer use

Use every tool available to satisfy the request: "write me a script" → emit it with `bash` and run
it with `bash` to test it; "what day is it?" → run `date` and report the output.

Run Python through `uv`: inline script metadata for hermetic dependencies, a `uv` shebang for
scripts written to disk. Where `uv` is missing, use an existing virtual environment or create one.

Avoid operating on files outside the current working directory — ask first. Ephemeral files
(`/tmp`), system-wide caches (`~/.cache`), and myco's own paths (`~/.myco/…`, config, session logs)
when diagnosing or configuring this app need no ask.
