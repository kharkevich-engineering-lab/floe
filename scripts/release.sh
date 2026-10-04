#!/usr/bin/env bash
# semantic-release prepareCmd (see .releaserc): stamp the version into the workspace and
# refuse to publish a release that is missing an architecture.
#
# Nothing is compiled here. The binaries were built natively per architecture by the
# release workflow's `build` matrix (with FLOE_VERSION already baked in) and downloaded
# into dist/; this runner is x86_64 and could only produce half of them.
set -euo pipefail

VERSION=${1:?Usage: release.sh <version>}
DIST="${DIST:-dist}"
TARGETS="${TARGETS:-x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu}"

echo "Setting workspace version to ${VERSION}..."
VERSION="${VERSION}" python3 - <<'PY'
import os
import re
from pathlib import Path

version = os.environ["VERSION"]

# [workspace.package] version in Cargo.toml — every crate inherits it (version.workspace = true).
path = Path("Cargo.toml")
text = path.read_text(encoding="utf-8")
section = re.search(r"(?ms)^\[workspace\.package\]\n(.*?)(?=^\[|\Z)", text)
if not section:
    raise SystemExit("release: no [workspace.package] section in Cargo.toml")
body, n = re.subn(
    r'(?m)^version\s*=\s*"[^"]*"\s*$', f'version = "{version}"', section.group(1), count=1
)
if n != 1:
    raise SystemExit("release: no version key in [workspace.package]")
start, end = section.span(1)
path.write_text(text[:start] + body + text[end:], encoding="utf-8")

# Cargo.lock: the workspace members are exactly the packages without a `source` (path
# packages). Rewriting them here is what `cargo update --workspace --offline` would do,
# without needing the pinned toolchain or a registry cache on the release runner.
lock = Path("Cargo.lock")
packages = lock.read_text(encoding="utf-8").split("[[package]]")
bumped = []
for i, pkg in enumerate(packages[1:], start=1):
    if "\nsource = " in pkg:
        continue
    name = re.search(r'(?m)^name = "([^"]+)"$', pkg)
    pkg, n = re.subn(r'(?m)^version = "[^"]*"$', f'version = "{version}"', pkg, count=1)
    if name and n == 1:
        bumped.append(name.group(1))
        packages[i] = pkg
if not bumped:
    raise SystemExit("release: found no workspace packages in Cargo.lock")
lock.write_text("[[package]]".join(packages), encoding="utf-8")
print(f"Cargo.lock: {', '.join(sorted(bumped))} -> {version}")
PY

# A release without one of its architectures is not a release: the GitHub assets would
# silently omit it and an operator on that machine would find out at install time.
echo "Checking release artefacts in ${DIST}/..."
missing=0
for target in ${TARGETS}; do
  tarball="${DIST}/floe-${VERSION}-${target}.tar.gz"
  for f in "${tarball}" "${tarball}.sha256"; do
    if [[ ! -s "${f}" ]]; then
      echo "release: ${f} is missing (the workflow's build job for ${target} did not deliver it)" >&2
      missing=1
    fi
  done
  if [[ -s "${tarball}" && -s "${tarball}.sha256" ]]; then
    (cd "${DIST}" && sha256sum -c "$(basename "${tarball}").sha256")
  fi
done
if [[ "${missing}" -ne 0 ]]; then
  exit 1
fi

echo "Release ${VERSION} ready:"
ls -l "${DIST}"/floe-"${VERSION}"-*
