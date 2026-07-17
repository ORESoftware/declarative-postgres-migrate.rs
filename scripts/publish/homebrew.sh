#!/usr/bin/env bash
# Publish/refresh the Homebrew formula in the declarative-migrations/homebrew-tap repo.
# Publishes a formula pinned to the release workflow's prebuilt binaries for
# Apple Silicon, Intel macOS, ARM64 Linux, and x86_64 Linux.
# Usage: scripts/publish/homebrew.sh [vX.Y.Z]
set -euo pipefail
cd "$(dirname "$0")/../.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
tag="${1:-v$version}"
repo="declarative-migrations/declarative-postgres-migrate.rs"
tap_repo="declarative-migrations/homebrew-tap"
release_base="https://github.com/$repo/releases/download/$tag"

asset_url() {
  printf '%s/dpm-%s-%s.tar.gz' "$release_base" "$tag" "$1"
}

asset_sha() {
  curl -fsSL "$1" | shasum -a 256 | cut -d' ' -f1
}

echo "==> computing release artifact sha256 values for $tag"
mac_arm_url=$(asset_url aarch64-apple-darwin)
mac_intel_url=$(asset_url x86_64-apple-darwin)
linux_arm_url=$(asset_url aarch64-unknown-linux-gnu)
linux_intel_url=$(asset_url x86_64-unknown-linux-gnu)
mac_arm_sha=$(asset_sha "$mac_arm_url")
mac_intel_sha=$(asset_sha "$mac_intel_url")
linux_arm_sha=$(asset_sha "$linux_arm_url")
linux_intel_sha=$(asset_sha "$linux_intel_url")

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
  desc "Declarative PostgreSQL and CockroachDB schema migration"
  homepage "https://github.com/$repo"
  version "$version"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "$mac_arm_url"
      sha256 "$mac_arm_sha"
    else
      url "$mac_intel_url"
      sha256 "$mac_intel_sha"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "$linux_arm_url"
      sha256 "$linux_arm_sha"
    else
      url "$linux_intel_url"
      sha256 "$linux_intel_sha"
    end
  end

  def install
    bin.install "dpm"
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
