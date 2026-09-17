# live

Claude Code and Codex read live, as `arsy-compat` resolves them on every
launch: `input/home` is the operator's home and `input/workspace` an untrusted
checkout. `DOCS_TOKEN` is set and `TICKETS_URL` is not.

Pinned in `expected/resolved.json`:

- precedence: the repository's `.mcp.json` owns `docs`, switched off, over the
  operator's own `docs`;
- trust: every repository server is off in an untrusted checkout, and its Codex
  server may not put `DOCS_TOKEN` into a header even once trusted;
- secrets: env and header names appear, values never do;
- permissions: `Bash(git push:*)` cannot be held exactly and asks instead;
  a repository's `allow` keeps workspace authority;
- models: Claude's alias `opus` is noted, not guessed; the project's full id
  and Codex's model are hints.
