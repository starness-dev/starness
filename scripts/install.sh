#!/bin/sh
set -eu

REPO="${STARNESS_REPO:-starness-dev/starness}"
INSTALL_DIR="${STARNESS_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${STARNESS_VERSION:-latest}"

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Darwin)
    case "$ARCH" in
      arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
      x86_64)        TARGET="x86_64-apple-darwin" ;;
      *)             echo "Unsupported macOS architecture: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  Linux)
    case "$ARCH" in
      x86_64|amd64)  TARGET="x86_64-unknown-linux-gnu" ;;
      aarch64|arm64) TARGET="aarch64-unknown-linux-gnu" ;;
      *)             echo "Unsupported Linux architecture: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  *)
    echo "Unsupported OS: $OS (supported: macOS, Linux)" >&2
    exit 1
    ;;
esac

if [ "$VERSION" = "latest" ]; then
  BASE_URL="https://github.com/${REPO}/releases/latest/download"
  ARCHIVE="starness-${TARGET}.tar.gz"
else
  BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"
  ARCHIVE="starness-${TARGET}.tar.gz"
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

echo "Downloading ${ARCHIVE} from ${REPO} (${VERSION})..."
curl -fsSL "${BASE_URL}/${ARCHIVE}" -o "${TMP_DIR}/${ARCHIVE}"
curl -fsSL "${BASE_URL}/checksums.sha256" -o "${TMP_DIR}/checksums.sha256"

EXPECTED="$(grep "  ${ARCHIVE}\$" "${TMP_DIR}/checksums.sha256" | awk '{print $1}')"
if [ -z "$EXPECTED" ]; then
  echo "Checksum entry for ${ARCHIVE} not found in checksums.sha256" >&2
  exit 1
fi

if command -v shasum >/dev/null 2>&1; then
  ACTUAL="$(shasum -a 256 "${TMP_DIR}/${ARCHIVE}" | awk '{print $1}')"
elif command -v sha256sum >/dev/null 2>&1; then
  ACTUAL="$(sha256sum "${TMP_DIR}/${ARCHIVE}" | awk '{print $1}')"
else
  echo "Neither shasum nor sha256sum found; cannot verify download." >&2
  exit 1
fi

if [ "$EXPECTED" != "$ACTUAL" ]; then
  echo "SHA256 mismatch for ${ARCHIVE}!" >&2
  echo "  expected: ${EXPECTED}" >&2
  echo "  actual:   ${ACTUAL}" >&2
  exit 1
fi

tar -xzf "${TMP_DIR}/${ARCHIVE}" -C "${TMP_DIR}"
mkdir -p "${INSTALL_DIR}"
install -m 755 "${TMP_DIR}/starness" "${INSTALL_DIR}/starness"

echo "Installed starness to ${INSTALL_DIR}/starness"
"${INSTALL_DIR}/starness" --version

case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    echo ""
    echo "Add ${INSTALL_DIR} to your PATH:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    ;;
esac
