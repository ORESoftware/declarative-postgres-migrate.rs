#!/usr/bin/env bash
# Install dpm's seven second-class-citizen cross-check tools.
# Each is optional at runtime; dpm reports which are missing.
set -uo pipefail

have() { command -v "$1" >/dev/null 2>&1; }
say() { printf '==> %s\n' "$*"; }

# migra (Python)
if have migra; then say "migra: already installed"
elif have pipx; then say "migra: pipx install"; pipx install migra && pipx inject migra 'setuptools<81' psycopg2-binary
elif have pip3; then say "migra: pip3 --user install"; pip3 install --user migra psycopg2-binary
else say "migra: SKIPPED (need pipx or pip3)"; fi

# pgdiff + pg-schema-diff (Go)
if have go; then
  have pgdiff || { say "pgdiff: go install"; go install github.com/joncrlsn/pgdiff@latest; }
  have pg-schema-diff || { say "pg-schema-diff: go install"; go install github.com/stripe/pg-schema-diff/cmd/pg-schema-diff@latest; }
  say 'remember: export PATH="$HOME/go/bin:$PATH"'
else
  say "pgdiff/pg-schema-diff: SKIPPED (need go)"
fi

# atlas, apgdiff, flyway, liquibase (Homebrew; use your package manager elsewhere)
if have brew; then
  have atlas     || { say "atlas: brew install";     brew install ariga/tap/atlas; }
  have apgdiff   || { say "apgdiff: brew install";   brew install apgdiff; }
  have flyway    || { say "flyway: brew install";    brew install flyway; }
  have liquibase || { say "liquibase: brew install"; brew install liquibase; }
else
  say "atlas/apgdiff/flyway/liquibase: SKIPPED (no brew — see each project's install docs)"
fi

say "installed cross-checkers:"
for t in migra pgdiff atlas pg-schema-diff liquibase apgdiff flyway; do
  if have "$t"; then printf '  ✅ %s\n' "$t"; else printf '  ⬜ %s\n' "$t"; fi
done
