#!/bin/sh
# Install helios from GitHub Releases.
#
#   curl -fsSL https://raw.githubusercontent.com/simonspoon/helios/main/install.sh | sh
#
# Options (flags or environment variables):
#   --version <vX.Y.Z>   HELIOS_VERSION   release tag to install (default: latest)
#   --bin-dir <dir>      HELIOS_BIN_DIR   install directory (default: ~/.local/bin)
#   --with-csharp        HELIOS_CSHARP=1  also install the Roslyn C# helper
set -eu

REPO="simonspoon/helios"
VERSION="${HELIOS_VERSION:-latest}"
BIN_DIR="${HELIOS_BIN_DIR:-$HOME/.local/bin}"
WITH_CSHARP="${HELIOS_CSHARP:-}"

die() { printf 'error: %s\n' "$1" >&2; exit 1; }
info() { printf '%s\n' "$1"; }
need() { command -v "$1" >/dev/null 2>&1 || die "$1 is required but not installed"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) [ $# -ge 2 ] || die "--version needs a value"; VERSION="$2"; shift 2 ;;
    --bin-dir) [ $# -ge 2 ] || die "--bin-dir needs a value"; BIN_DIR="$2"; shift 2 ;;
    --with-csharp) WITH_CSHARP=1; shift ;;
    -h|--help) awk 'NR>1 && /^#/ { sub(/^# ?/, ""); print; next } NR>1 { exit }' "$0"; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done

need curl
need uname

case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=darwin ;;
  *) die "unsupported OS: $(uname -s). Windows users: download helios-windows-amd64.exe from https://github.com/$REPO/releases" ;;
esac

case "$(uname -m)" in
  x86_64|amd64) arch=amd64 ;;
  arm64|aarch64) arch=arm64 ;;
  *) die "unsupported architecture: $(uname -m)" ;;
esac

asset="helios-$os-$arch"

if [ "$VERSION" = latest ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

info "Downloading $asset ($VERSION)..."
curl -fsSL "$base/$asset" -o "$tmp/$asset" \
  || die "download failed: $base/$asset"

# Checksums are published alongside the binaries; verify when available.
if curl -fsSL "$base/checksums.txt" -o "$tmp/checksums.txt" 2>/dev/null; then
  if command -v sha256sum >/dev/null 2>&1; then
    sum="$(sha256sum "$tmp/$asset" | cut -d' ' -f1)"
  elif command -v shasum >/dev/null 2>&1; then
    sum="$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)"
  else
    sum=""
  fi
  if [ -n "$sum" ]; then
    want="$(grep " \{1,2\}$asset\$" "$tmp/checksums.txt" | cut -d' ' -f1 || true)"
    [ -n "$want" ] || die "no checksum published for $asset"
    [ "$sum" = "$want" ] || die "checksum mismatch for $asset (expected $want, got $sum)"
    info "Checksum verified."
  fi
fi

mkdir -p "$BIN_DIR"
chmod +x "$tmp/$asset"
mv "$tmp/$asset" "$BIN_DIR/helios"
info "Installed $BIN_DIR/helios"

if [ -n "$WITH_CSHARP" ]; then
  need unzip
  info "Downloading helios-roslyn.zip..."
  curl -fsSL "$base/helios-roslyn.zip" -o "$tmp/helios-roslyn.zip" \
    || die "download failed: $base/helios-roslyn.zip"
  # The helper is looked up next to the helios binary unless HELIOS_ROSLYN is set.
  unzip -oq "$tmp/helios-roslyn.zip" -d "$BIN_DIR"
  info "Installed C# helper into $BIN_DIR"
  command -v dotnet >/dev/null 2>&1 \
    || info "note: the helper needs .NET 8 or later on your PATH (dotnet --version)"
fi

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) info ""
     info "$BIN_DIR is not on your PATH. Add it, e.g.:"
     info "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.zshrc" ;;
esac

info ""
"$BIN_DIR/helios" --version || true
