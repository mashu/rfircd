#!/bin/sh
# Install rfircd binaries and data files. Invoked by the .run installer
# and usable from an extracted tarball:
#   tar xf rfircd-x86_64-linux.tar.gz
#   ./rfircd-*/install.sh
#
#   ./install.sh                 # $HOME/.local
#   ./install.sh --prefix DIR
#   sudo ./install.sh --system   # /usr/local

set -eu

PREFIX=""
SYSTEM=0

usage() {
    cat <<'EOF'
Usage: install.sh [--prefix DIR] [--system]

  --prefix DIR   install root (binaries go to DIR/bin)
  --system       prefix /usr/local (implies running as root)

Default prefix is $HOME/.local. Existing configuration is never overwritten.
EOF
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)
            PREFIX=${2:-}
            shift 2
            ;;
        --system)
            SYSTEM=1
            shift
            ;;
        -h|--help)
            usage
            ;;
        *)
            usage
            ;;
    esac
done

HERE=$(CDPATH= cd -- "$(dirname "$0")" && pwd)

if [ -z "$PREFIX" ]; then
    if [ "$SYSTEM" -eq 1 ]; then
        PREFIX=/usr/local
    else
        PREFIX=${HOME}/.local
    fi
fi

if [ "$SYSTEM" -eq 1 ] && [ "$(id -u)" -ne 0 ]; then
    echo "install.sh: --system needs root (try sudo)" >&2
    exit 1
fi

BINDIR="$PREFIX/bin"
DATADIR="$PREFIX/share/rfircd"
DOCDIR="$PREFIX/share/doc/rfircd"
UNITDIR=""
if [ "$SYSTEM" -eq 1 ]; then
    UNITDIR=/etc/systemd/system
    CONFDIR=/etc/rfircd
else
    UNITDIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
    CONFDIR="${XDG_CONFIG_HOME:-$HOME/.config}/rfircd"
fi

mkdir -p "$BINDIR" "$DATADIR" "$DOCDIR" "$CONFDIR"

for b in rfircd rfirc-station rfirc-kisshub; do
    install -m 0755 "$HERE/bin/$b" "$BINDIR/$b"
done

install -m 0644 "$HERE/share/rfircd.example.toml" "$DATADIR/rfircd.example.toml"
install -m 0644 "$HERE/share/rfircd.service" "$DATADIR/rfircd.service"
if [ -d "$HERE/share/doc" ]; then
    cp -R "$HERE/share/doc/." "$DOCDIR/"
fi

CONFFILE="$CONFDIR/rfircd.toml"
if [ -f "$CONFFILE" ]; then
    echo "kept existing config: $CONFFILE"
elif [ -t 0 ] && [ -t 1 ]; then
    # Interactive and nothing to lose: offer the wizard. It asks the handful
    # of things that cannot be guessed and can generate a TLS certificate.
    # Declining falls through to the example, which is what happened before
    # the wizard existed and is still a fine place to start.
    echo
    printf 'No configuration yet. Answer a few questions to create one? [Y/n] '
    read -r reply || reply=n
    case "$reply" in
        [Nn]*) ;;
        *)
            if "$BINDIR/rfircd" --init -c "$CONFFILE"; then
                CONFIGURED=1
            else
                echo "setup did not finish; falling back to the example config" >&2
            fi
            ;;
    esac
fi

if [ ! -f "$CONFFILE" ]; then
    install -m 0644 "$HERE/share/rfircd.example.toml" "$CONFFILE"
    echo "wrote starter config: $CONFFILE"
    echo "edit radio.callsign (your callsign) before enabling radio.enabled"
    echo "or run: $BINDIR/rfircd --init -c $CONFFILE   (after moving it aside)"
fi

if [ "$SYSTEM" -eq 1 ]; then
    install -m 0644 "$HERE/share/rfircd.service" "$UNITDIR/rfircd.service"
    echo "systemd unit: $UNITDIR/rfircd.service"
    echo "create a system user, put the config at /etc/rfircd.toml (or edit"
    echo "ExecStart), then: systemctl daemon-reload && systemctl enable --now rfircd"
else
    mkdir -p "$UNITDIR"
    # User unit: run from the user's config path, not /etc.
    sed -e "s|/usr/local/bin/rfircd|$BINDIR/rfircd|g" \
        -e "s|/etc/rfircd.toml|$CONFFILE|g" \
        -e '/^User=/d' \
        -e '/^Group=/d' \
        -e 's/^ProtectHome=.*/ProtectHome=no/' \
        "$HERE/share/rfircd.service" > "$UNITDIR/rfircd.service"
    echo "user systemd unit: $UNITDIR/rfircd.service"
    echo "  systemctl --user daemon-reload"
    echo "  systemctl --user enable --now rfircd"
fi

case ":$PATH:" in
    *":$BINDIR:"*) ;;
    *)
        echo
        echo "note: $BINDIR is not on PATH. Add it, or run $BINDIR/rfircd"
        ;;
esac

echo
echo "installed:"
echo "  $BINDIR/rfircd"
echo "  $BINDIR/rfirc-station"
echo "  $BINDIR/rfirc-kisshub"
echo
if [ "${CONFIGURED:-0}" -eq 0 ]; then
    echo "next:  rfircd --check -c $CONFFILE"
fi
echo "guide: https://mashu.github.io/rfircd/"
