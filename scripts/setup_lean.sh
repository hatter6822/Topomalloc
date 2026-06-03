#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# setup_lean.sh — install the pinned Lean toolchain (W0-3) via a direct download
# from GitHub releases, with SHA-256 verification.
#
# Adapted from seLe4n's scripts/setup_lean_env.sh (GPL-3.0-or-later upstream;
# this is an independent, simplified reimplementation of the same *technique*).
# We download the toolchain tarball straight from GitHub rather than letting
# `elan` resolve it, because elan's release host (release.lean-lang.org) is
# unreachable from some sandboxed/proxied environments (e.g. Claude-on-web egress
# gateways), whereas github.com is reachable. We then symlink the *real* toolchain
# binaries onto PATH, bypassing the elan shim entirely so `lake`/`lean` never try
# to contact the release host. The version is pinned by `lean-toolchain` and
# matches the version seLe4n uses so the bridge model co-develops without
# toolchain skew (D2).
#
# Idempotent: a fast-path check skips the download if the toolchain is present.
# Usage: scripts/setup_lean.sh [--quiet]
set -euo pipefail

QUIET=0
[ "${1:-}" = "--quiet" ] && QUIET=1
log() { [ "${QUIET}" -eq 1 ] || echo "[setup-lean] $*"; }

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ELAN_HOME_DIR="${ELAN_HOME:-${HOME}/.elan}"
TOOLCHAIN_FILE="${ROOT_DIR}/lean-toolchain"

[ -f "${TOOLCHAIN_FILE}" ] || { echo "error: ${TOOLCHAIN_FILE} not found" >&2; exit 1; }
TOOLCHAIN="$(tr -d '\n\r' < "${TOOLCHAIN_FILE}")"
# "leanprover/lean4:v4.28.0" -> org=leanprover repo=lean4 tag=v4.28.0
TOOLCHAIN_ORG="${TOOLCHAIN%%/*}"
TOOLCHAIN_REPO="${TOOLCHAIN#*/}"; TOOLCHAIN_REPO="${TOOLCHAIN_REPO%%:*}"
TOOLCHAIN_TAG="${TOOLCHAIN##*:}"
VERSION_NUMBER="${TOOLCHAIN_TAG#v}"
# elan normalises "org/repo:tag" -> "org-repo-tag" for toolchain directory names.
TOOLCHAIN_DIR_NAME="$(echo "${TOOLCHAIN}" | sed 's|/|-|g; s|:|-|g')"
TOOLCHAIN_DIR="${ELAN_HOME_DIR}/toolchains/${TOOLCHAIN_DIR_NAME}"
ELAN_BIN_DIR="${ELAN_HOME_DIR}/bin"

# Pinned SHA-256 of the Lean toolchain tarballs (update together with the version
# in `lean-toolchain`). Values for v4.28.0 (must match seLe4n's pin, D2).
SHA_ZST_X86="ceb3a3f844f7aebf63245e2b51c28d5b0ed38942c19f93cf3febd520302160bd"
SHA_ZST_ARM="c865801261c747d4f15d08beca9abc20aca907904abbb284de25a37f4b4558bc"

case "$(uname -m)" in
  x86_64|amd64)  ARCH_SUFFIX="";          EXPECTED_SHA="${SHA_ZST_X86}" ;;
  aarch64|arm64) ARCH_SUFFIX="_aarch64";  EXPECTED_SHA="${SHA_ZST_ARM}" ;;
  *) echo "error: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

# --- Download + extract, unless the toolchain is already present (fast path) ---
if [ -x "${TOOLCHAIN_DIR}/bin/lean" ] && [ -f "${TOOLCHAIN_DIR}/lib/crti.o" ]; then
  log "Lean ${TOOLCHAIN} already installed (fast path)."
else
  command -v curl >/dev/null 2>&1 || { echo "error: curl is required" >&2; exit 1; }
  command -v zstd >/dev/null 2>&1 || { echo "error: zstd is required (apt-get install zstd)" >&2; exit 1; }

  ARCHIVE="lean-${VERSION_NUMBER}-linux${ARCH_SUFFIX}.tar.zst"
  URL="https://github.com/${TOOLCHAIN_ORG}/${TOOLCHAIN_REPO}/releases/download/${TOOLCHAIN_TAG}/${ARCHIVE}"
  TMP="$(mktemp -d)"
  trap 'rm -rf "${TMP}"' EXIT

  log "downloading ${ARCHIVE} ..."
  curl -fsSL "${URL}" -o "${TMP}/lean.tar.zst"

  ACTUAL_SHA="$(sha256sum "${TMP}/lean.tar.zst" | cut -d' ' -f1)"
  if [ "${ACTUAL_SHA}" != "${EXPECTED_SHA}" ]; then
    echo "error: Lean toolchain checksum mismatch (possible corruption/tampering)" >&2
    echo "  expected ${EXPECTED_SHA}" >&2
    echo "  actual   ${ACTUAL_SHA}" >&2
    exit 1
  fi
  log "SHA-256 verified."

  zstd -dqf "${TMP}/lean.tar.zst" -o "${TMP}/lean.tar"
  mkdir -p "${ELAN_HOME_DIR}/toolchains"
  tar -xf "${TMP}/lean.tar" -C "${ELAN_HOME_DIR}/toolchains/"
  rm -rf "${TOOLCHAIN_DIR}"
  mv "${ELAN_HOME_DIR}/toolchains/lean-${VERSION_NUMBER}-linux${ARCH_SUFFIX}" "${TOOLCHAIN_DIR}"
  log "Lean ${TOOLCHAIN} installed to ${TOOLCHAIN_DIR}"
fi

# --- Always (re)point PATH at the REAL toolchain binaries, bypassing the elan
# shim so `lake`/`lean` never contact the (possibly unreachable) release host. ---
mkdir -p "${ELAN_BIN_DIR}" "${ELAN_HOME_DIR}/update-hashes"
for b in lean lake leanc leanmake; do
  [ -x "${TOOLCHAIN_DIR}/bin/${b}" ] && ln -sf "${TOOLCHAIN_DIR}/bin/${b}" "${ELAN_BIN_DIR}/${b}"
done
printf 'version = "12"\ndefault_toolchain = "%s"\n' "${TOOLCHAIN_DIR_NAME}" > "${ELAN_HOME_DIR}/settings.toml"
echo "direct-install" > "${ELAN_HOME_DIR}/update-hashes/${TOOLCHAIN_DIR_NAME}"

# Put the toolchain on PATH for this process, persist it for the session
# ($CLAUDE_ENV_FILE) and for CI ($GITHUB_PATH), and print a hint otherwise.
export PATH="${ELAN_BIN_DIR}:${PATH}"
[ -n "${GITHUB_PATH:-}" ] && echo "${ELAN_BIN_DIR}" >> "${GITHUB_PATH}"
[ -n "${CLAUDE_ENV_FILE:-}" ] && echo "export PATH=\"${ELAN_BIN_DIR}:\${PATH}\"" >> "${CLAUDE_ENV_FILE}"

if "${ELAN_BIN_DIR}/lake" --version >/dev/null 2>&1; then
  log "ready: $("${ELAN_BIN_DIR}/lake" --version 2>&1 | head -1)"
else
  log "note: add ${ELAN_BIN_DIR} to PATH to use lake/lean."
fi
