#!/bin/bash

set -e -u

if [[ $# -lt 1 ]]; then
  echo "Usage: $0 <major|minor|patch>" >&2
  exit 1
fi

level="$1"

cargo set-version --bump "${level}"

version="$(cargo get package.version)"
debian_package_name="cargo-for-each"
debian_package_revision="$(cargo metadata --format-version 1 --no-deps | jq -r -C '.packages[] | select(.name == "cargo-for-each") | .metadata.deb.revision')"

git cliff --prepend CHANGELOG.md -u -t "cargo-for-each_${version}"
git cliff --config cliff-debian.toml --prepend changelog -u -t "cargo-for-each_${version}" --context --output context.json
jq < \
context.json \
  --arg debian_package_name "${debian_package_name}" \
  --arg debian_package_revision "${debian_package_revision}" \
  '.[0] += { "extra": { "debian_package_name": $debian_package_name, "debian_package_revision": $debian_package_revision }}' \
  >full_context.json
git cliff --config cliff-debian.toml --prepend changelog -u -t "cargo-for-each_${version}" --from-context full_context.json
tail -n +2 changelog | sponge changelog
rm context.json full_context.json

rumdl fmt CHANGELOG.md

cargo build

git add changelog CHANGELOG.md Cargo.toml Cargo.lock

git commit -m "chore(release): Release version ${version}"

git tag "cargo-for-each_${version}"

for remote in $(git remote); do
  git push "${remote}"
  git push "${remote}" "cargo-for-each_${version}"
done
cargo publish --dry-run
