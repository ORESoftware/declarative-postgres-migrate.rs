#!/usr/bin/env bash
# Build release binaries for the targets this host can compile, tar them with
# license/readme, generate sha256 sums, and publish a GitHub release.
# Usage: scripts/publish/github.sh [vX.Y.Z]   (default: v<Cargo.toml version>)
set -euo pipefail
cd "$(dirname "$0")/../.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
tag="${1:-v$version}"

host_target=$(rustc -vV | sed -n 's/^host: //p')
targets=("$host_target")
# On Apple Silicon also cross-build the Intel binary (SDK supports both).
if [ "$host_target" = "aarch64-apple-darwin" ]; then
  rustup target add x86_64-apple-darwin >/dev/null 2>&1 || true
  targets+=("x86_64-apple-darwin")
fi

outdir="target/release-artifacts/$tag"
rm -rf "$outdir"; mkdir -p "$outdir"

built_any=false
for target in "${targets[@]}"; do
  echo "==> building $target"
  if ! cargo build --release --target "$target"; then
    echo "==> SKIPPING $target (no std for this target on this toolchain — use rustup to add it)"
    continue
  fi
  built_any=true
  staging=$(mktemp -d)
  cp "target/$target/release/dpm" "$staging/"
  cp readme.md LICENSE .cli-flags.toml "$staging/"
  asset="dpm-$tag-$target.tar.gz"
  tar -czf "$outdir/$asset" -C "$staging" .
  (cd "$outdir" && shasum -a 256 "$asset" > "$asset.sha256")
  rm -rf "$staging"
done
[ "$built_any" = true ] || { echo "no targets built"; exit 1; }

echo "==> creating GitHub release $tag"
if gh release view "$tag" >/dev/null 2>&1; then
  gh release upload "$tag" "$outdir"/* --clobber
else
  gh release create "$tag" "$outdir"/* \
    --title "dpm $tag" \
    --notes "declarative-postgres-migrate $tag — see readme.md for usage.

Install:
\`\`\`
curl -fsSL https://raw.githubusercontent.com/declarative-migrations/declarative-postgres-migrate.rs/main/scripts/install.sh | bash
\`\`\`"
fi
gh release view "$tag" --json assets -q '.assets[].name'
