#!/usr/bin/env bash
# Cut a release: bump the version everywhere it lives, commit, tag, push.
# The pushed tag triggers .github/workflows/release.yml, which builds all four
# platforms and publishes the GitHub Release the installers download from.
#
#   scripts/release.sh 0.2.0            # bump, commit, tag v0.2.0, push
#   scripts/release.sh 0.2.0-rc.1       # prerelease (never becomes "latest")
#   scripts/release.sh 0.2.0 --no-push  # do everything locally, push yourself
#
# Version lives in: package.json, src-tauri/tauri.conf.json, the workspace
# [workspace.package] in Cargo.toml, and the versioned path dependencies in
# src-tauri/Cargo.toml. Cargo.lock is refreshed to match.
set -euo pipefail

die() { echo "release: $*" >&2; exit 1; }

version="${1:-}"
push=1
[[ "${2:-}" == "--no-push" ]] && push=0

[[ -n "$version" ]] || die "usage: scripts/release.sh <X.Y.Z[-pre]> [--no-push]"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || die "'$version' is not semver"

cd "$(dirname "$0")/.."
tag="v$version"

[[ -z "$(git status --porcelain)" ]] || die "working tree is not clean"
git rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "tag $tag already exists"

branch="$(git rev-parse --abbrev-ref HEAD)"
if [[ "$branch" != "main" && -z "${SLUICE_RELEASE_ANY_BRANCH:-}" ]]; then
  die "on '$branch', not main (set SLUICE_RELEASE_ANY_BRANCH=1 to override)"
fi

current="$(sed -n 's/^  "version": "\(.*\)",$/\1/p' package.json | head -n1)"
echo "release: $current -> $version"

# Portable in-place sed (BSD and GNU).
sedi() { sed -i.bak "$1" "$2" && rm -f "$2.bak"; }

sedi "s/^  \"version\": \"[^\"]*\",/  \"version\": \"$version\",/" package.json
sedi "s/^  \"version\": \"[^\"]*\",/  \"version\": \"$version\",/" src-tauri/tauri.conf.json
sedi "/^\[workspace.package\]/,/^\[/ s/^version = \"[^\"]*\"/version = \"$version\"/" Cargo.toml
sedi "s/version = \"[^\"]*\", path = \"..\/crates\//version = \"$version\", path = \"..\/crates\//" src-tauri/Cargo.toml

# Refresh the workspace members' entries in Cargo.lock without touching deps.
cargo update --workspace --offline >/dev/null 2>&1 || cargo update --workspace >/dev/null

# Sanity: every manifest now agrees.
for f in package.json src-tauri/tauri.conf.json; do
  grep -q "\"version\": \"$version\"" "$f" || die "failed to bump $f"
done
grep -q "^version = \"$version\"" Cargo.toml || die "failed to bump Cargo.toml"

git add package.json src-tauri/tauri.conf.json Cargo.toml src-tauri/Cargo.toml Cargo.lock
git commit -m "Release $tag"
git tag -a "$tag" -m "Sluice $version"

if (( push )); then
  git push origin "$branch"
  git push origin "$tag"
  echo "release: pushed $tag — follow the build at:"
  echo "  https://github.com/AHP15/Sluice/actions/workflows/release.yml"
else
  echo "release: tagged $tag locally. Publish with:"
  echo "  git push origin $branch && git push origin $tag"
fi
