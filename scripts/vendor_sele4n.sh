#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# vendor_sele4n.sh — vendor the pinned seLe4n ABI crates into vendor/sele4n/ (D8).
#
# Decision D8 (docs/DECISIONS.md) consumes the seLe4n Rust ABI as a git dependency
# pinned to an exact SHA, with a vendored mirror for hermetic, reproducible,
# offline builds. This script *is* that mirror mechanism. Run it at M1
# (plan 09 W22-0), when `topo-backend-sele4n` begins linking the real ABI under
# `--features real-abi`; until then the M0 build stays hermetic and links nothing
# GPL.
#
# The vendored tree under vendor/sele4n/ is upstream GPL-3.0-or-later and lives on
# the seLe4n side of the license boundary (D5).
set -euo pipefail

# The reviewed pin (keep in lockstep with docs/DECISIONS.md §D8 and the optional
# git dependency in crates/topo-backend-sele4n/Cargo.toml).
PIN="57c11054d31b819364d8089268ab38927881ab0b"
REPO="https://github.com/hatter6822/seLe4n"
CRATES=(sele4n-types sele4n-abi sele4n-sys sele4n-hal)

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="${ROOT}/vendor/sele4n"
TMP="$(mktemp -d)"
trap 'rm -rf "${TMP}"' EXIT

echo "[vendor-sele4n] cloning ${REPO} @ ${PIN}"
git clone --quiet "${REPO}" "${TMP}/seLe4n"
git -C "${TMP}/seLe4n" checkout --quiet "${PIN}"

# Confirm we vendored exactly the reviewed commit (defense against a moved ref).
got="$(git -C "${TMP}/seLe4n" rev-parse HEAD)"
if [ "${got}" != "${PIN}" ]; then
  echo "error: checked-out SHA ${got} != pinned ${PIN}" >&2
  exit 1
fi

mkdir -p "${DEST}"
for crate in "${CRATES[@]}"; do
  src="${TMP}/seLe4n/rust/${crate}"
  [ -d "${src}" ] || { echo "error: ${crate} not found in upstream" >&2; exit 1; }
  rm -rf "${DEST:?}/${crate}"
  cp -r "${src}" "${DEST}/${crate}"
done
cp "${TMP}/seLe4n/LICENSE" "${DEST}/LICENSE"
echo "${PIN}" > "${DEST}/PINNED_SHA"

echo "[vendor-sele4n] vendored ${CRATES[*]} into ${DEST} (GPL-3.0-or-later)"
echo "[vendor-sele4n] next: point the git deps in crates/topo-backend-sele4n/Cargo.toml"
echo "                at these vendored paths (or '[patch]' the git source) and enable --features real-abi."
