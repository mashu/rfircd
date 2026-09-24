#!/bin/sh
# Build Linux distribution artifacts from already-compiled binaries.
#
#   cargo build --release
#   ./packaging/linux/build-dist.sh
#
# Or with musl (what CI does):
#   cargo build --release --target x86_64-unknown-linux-musl
#   ./packaging/linux/build-dist.sh --bin-dir target/x86_64-unknown-linux-musl/release --arch x86_64
#
# Writes into dist/:
#   rfircd-<arch>-linux.tar.gz
#   rfircd-<arch>.run
#   rfircd-<arch>.AppImage   (x86_64 only unless --appimagetool is set)

set -eu

ROOT=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
cd "$ROOT"

BIN_DIR="$ROOT/target/release"
ARCH=$(uname -m)
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
OUT="$ROOT/dist"
APPIMAGETOOL=""
SKIP_APPIMAGE=0

usage() {
    cat <<'EOF'
Usage: packaging/linux/build-dist.sh [options]

  --bin-dir DIR        directory containing the three binaries
  --arch ARCH          artifact architecture label (x86_64, aarch64)
  --version VER        override Cargo.toml version
  --out DIR            output directory (default: ./dist)
  --appimagetool PATH  appimagetool binary
  --skip-appimage
EOF
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --bin-dir) BIN_DIR=$(CDPATH= cd -- "$2" && pwd); shift 2 ;;
        --arch) ARCH=$2; shift 2 ;;
        --version) VERSION=$2; shift 2 ;;
        --out) OUT=$2; shift 2 ;;
        --appimagetool) APPIMAGETOOL=$2; shift 2 ;;
        --skip-appimage) SKIP_APPIMAGE=1; shift ;;
        -h|--help) usage ;;
        *) usage ;;
    esac
done

for b in rfircd rfirc-station rfirc-kisshub; do
    if [ ! -x "$BIN_DIR/$b" ]; then
        echo "missing executable: $BIN_DIR/$b" >&2
        echo "build first: cargo build --release" >&2
        exit 1
    fi
done

mkdir -p "$OUT"
STAGING=$(mktemp -d)
trap 'rm -rf "$STAGING"' EXIT

PKG="rfircd-${VERSION}-linux-${ARCH}"
PKGDIR="$STAGING/$PKG"
mkdir -p "$PKGDIR/bin" "$PKGDIR/share/doc"

for b in rfircd rfirc-station rfirc-kisshub; do
    cp "$BIN_DIR/$b" "$PKGDIR/bin/$b"
    chmod 0755 "$PKGDIR/bin/$b"
    if command -v strip >/dev/null 2>&1; then
        strip "$PKGDIR/bin/$b" 2>/dev/null || true
    fi
done

cp rfircd.example.toml "$PKGDIR/share/rfircd.example.toml"
cp packaging/rfircd.service "$PKGDIR/share/rfircd.service"
cp docs/quickstart.md docs/regulatory.md docs/design.md docs/protocol.md LICENSE README.md \
    "$PKGDIR/share/doc/"
cp packaging/linux/install.sh "$PKGDIR/install.sh"
chmod 0755 "$PKGDIR/install.sh"

TAR="$OUT/rfircd-${ARCH}-linux.tar.gz"
tar -C "$STAGING" -czf "$TAR" "$PKG"
echo "wrote $TAR"

# --- .run self-extractor ----------------------------------------------------
RUN="$OUT/rfircd-${ARCH}.run"
{
    cat <<'HEADER'
#!/bin/sh
# rfircd self-extracting installer. Does not overwrite an existing config.
set -eu
usage() {
    cat <<'EOF'
Usage: rfircd-*.run [--prefix DIR] [--system] [--extract DIR]

  --prefix DIR   install root (default: $HOME/.local)
  --system       install to /usr/local (needs root)
  --extract DIR  unpack the payload and stop; do not install
EOF
    exit 2
}

PREFIX=""
SYSTEM=""
EXTRACT=""
while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) PREFIX=$2; shift 2 ;;
        --system) SYSTEM=1; shift ;;
        --extract) EXTRACT=$2; shift 2 ;;
        -h|--help) usage ;;
        *) usage ;;
    esac
done

ARCHIVE=$(awk '/^__ARCHIVE_BELOW__/ { print NR + 1; exit 0; }' "$0")
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
tail -n +"$ARCHIVE" "$0" | tar -xz -C "$TMP"

PKGDIR=$(find "$TMP" -mindepth 1 -maxdepth 1 -type d | head -n1)
if [ -n "$EXTRACT" ]; then
    mkdir -p "$EXTRACT"
    cp -R "$PKGDIR"/. "$EXTRACT/"
    echo "extracted to $EXTRACT"
    exit 0
fi

set --
if [ -n "$PREFIX" ]; then
    set -- --prefix "$PREFIX"
fi
if [ -n "$SYSTEM" ]; then
    set -- "$@" --system
fi
exec sh "$PKGDIR/install.sh" "$@"

__ARCHIVE_BELOW__
HEADER
    cat "$TAR"
} > "$RUN"
chmod 0755 "$RUN"
echo "wrote $RUN"

# --- AppImage ---------------------------------------------------------------
if [ "$SKIP_APPIMAGE" -eq 1 ]; then
    exit 0
fi

if [ "$ARCH" != "x86_64" ] && [ "$ARCH" != "aarch64" ]; then
    echo "skipping AppImage (unsupported arch $ARCH)"
    exit 0
fi

if [ -z "$APPIMAGETOOL" ]; then
    if command -v appimagetool >/dev/null 2>&1; then
        APPIMAGETOOL=$(command -v appimagetool)
    else
        TOOL_ARCH=$ARCH
        [ "$TOOL_ARCH" = "aarch64" ] && TOOL_ARCH=aarch64
        [ "$TOOL_ARCH" = "x86_64" ] && TOOL_ARCH=x86_64
        CACHED="$ROOT/packaging/linux/.cache/appimagetool-${TOOL_ARCH}.AppImage"
        mkdir -p "$(dirname "$CACHED")"
        if [ ! -x "$CACHED" ]; then
            URL="https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-${TOOL_ARCH}.AppImage"
            echo "downloading $URL"
            curl -fsSL -o "$CACHED" "$URL"
            chmod 0755 "$CACHED"
        fi
        APPIMAGETOOL=$CACHED
    fi
fi

APPDIR="$STAGING/AppDir"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/rfircd" "$APPDIR/usr/share/doc/rfircd" \
    "$APPDIR/usr/share/icons/hicolor/256x256/apps"

cp "$PKGDIR/bin/"* "$APPDIR/usr/bin/"
cp rfircd.example.toml "$APPDIR/usr/share/rfircd/"
cp packaging/rfircd.service "$APPDIR/usr/share/rfircd/"
cp docs/quickstart.md docs/regulatory.md LICENSE "$APPDIR/usr/share/doc/rfircd/"
cp packaging/linux/AppRun "$APPDIR/AppRun"
chmod 0755 "$APPDIR/AppRun"
cp packaging/linux/rfircd.desktop "$APPDIR/rfircd.desktop"

ICON_PNG="$ROOT/packaging/linux/rfircd.png"
if [ ! -f "$ICON_PNG" ]; then
    echo "missing $ICON_PNG" >&2
    exit 1
fi
cp "$ICON_PNG" "$APPDIR/rfircd.png"
cp "$ICON_PNG" "$APPDIR/usr/share/icons/hicolor/256x256/apps/rfircd.png"

# GitHub-hosted runners (and many containers) have no FUSE.
export APPIMAGE_EXTRACT_AND_RUN=1
export ARCH
APPIMAGE_OUT="$OUT/rfircd-${ARCH}.AppImage"
rm -f "$APPIMAGE_OUT"
"$APPIMAGETOOL" --no-appstream "$APPDIR" "$APPIMAGE_OUT"
chmod 0755 "$APPIMAGE_OUT"
echo "wrote $APPIMAGE_OUT"
