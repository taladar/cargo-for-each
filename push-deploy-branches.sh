#!/bin/bash

set -e -u

for b in bookworm trixie; do
  echo "Pushing build_${b}" >&2
  git push origin HEAD:refs/heads/build_${b}
done

for tt in x86_64-unknown-linux-gnu x86_64-pc-windows-msvc x86_64-pc-windows-gnu x86_64-apple-darwin aarch64-apple-darwin; do
  echo "Pushing cross build branch build_${tt}" >&2
  git push origin HEAD:refs/heads/build_${tt}
done
