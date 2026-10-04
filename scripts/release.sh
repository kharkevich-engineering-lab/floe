#!/usr/bin/env bash
# semantic-release prepareCmd (see .releaserc): refuse to publish a release that is
# missing an architecture.
#
# Cargo.toml and Cargo.lock are deliberately not rewritten: nothing would commit the
# change (no @semantic-release/git, so main needs no protection bypass) and nothing
# builds from it afterwards. The version lives only in the tag and in FLOE_VERSION;
# the workspace version is a placeholder.
#
# Nothing is compiled here either. The binaries were built natively per architecture by
# the release workflow's `build` matrix (with FLOE_VERSION already baked in) and
# downloaded into dist/; this runner is x86_64 and could only produce half of them.
set -euo pipefail

VERSION=${1:?Usage: release.sh <version>}
DIST="${DIST:-dist}"
TARGETS="${TARGETS:-x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu}"

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
