#!/usr/bin/env bash
# Publish/refresh the Homebrew formula in the declarative-migrations/homebrew-tap repo.
# Builds a source-based formula pinned to the release tag's tarball, so it
# works on any mac/linuxbrew arch (depends_on rust => build).
# Usage: scripts/publish/homebrew.sh [vX.Y.Z]
set -euo pipefail
cd "$(dirname "$0")/../.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
tag="${1:-v$version}"
repo="declarative-migrations/declarative-postgres-migrate.rs"
tap_repo="declarative-migrations/homebrew-tap"
tarball="https://github.com/$repo/archive/refs/tags/$tag.tar.gz"

echo "==> computing source tarball sha256 for $tag"
sha=$(curl -fsSL "$tarball" | shasum -a 256 | cut -d' ' -f1)

workdir=$(mktemp -d); trap 'rm -rf "$workdir"' EXIT
if gh repo view "$tap_repo" >/dev/null 2>&1; then
  gh repo clone "$tap_repo" "$workdir/tap" -- --depth 1
else
  echo "==> creating tap repo $tap_repo"
  gh repo create "$tap_repo" --public --description "Homebrew tap for declarative-migrations tools" --clone
  mv homebrew-tap "$workdir/tap" 2>/dev/null || gh repo clone "$tap_repo" "$workdir/tap"
fi
mkdir -p "$workdir/tap/Formula"

cat > "$workdir/tap/Formula/dpm.rb" <<RUBY
class Dpm < Formula
  desc "Declarative, ORM-agnostic Postgres schema migration (diff two databases)"
  homepage "https://github.com/$repo"
  url "$tarball"
  sha256 "$sha"
  license "MIT"
  head "https://github.com/$repo.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
    pkgshare.install ".cli-flags.toml"
  end

  test do
    assert_match "dpm", shell_output("#{bin}/dpm version")
    assert_match "declarative postgres migrate", shell_output("#{bin}/dpm help")
  end
end
RUBY

cd "$workdir/tap"
git add Formula/dpm.rb
if git diff --cached --quiet; then
  echo "formula unchanged"
else
  git commit -m "dpm $tag"
  git push origin HEAD
fi
echo "==> published. Install with:"
echo "    brew install declarative-migrations/tap/dpm"
