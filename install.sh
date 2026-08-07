#!/bin/sh
set -eu

repository="4piu/agent-speak"
archive_prefix="agent-speak"
programs="agent-speak"
provider_slug=""

usage() {
    echo "usage: install.sh [--version vX.Y.Z] [--install-dir PATH] [--uninstall [--purge]]" >&2
}

version="${VERSION:-}"
install_dir="${INSTALL_DIR:-${XDG_BIN_HOME:-$HOME/.local/bin}}"
uninstall=false
purge=false
while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || { usage; exit 2; }
            version="$2"
            shift 2
            ;;
        --install-dir)
            [ "$#" -ge 2 ] || { usage; exit 2; }
            install_dir="$2"
            shift 2
            ;;
        --uninstall)
            uninstall=true
            shift
            ;;
        --purge)
            purge=true
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage
            exit 2
            ;;
    esac
done

case "$install_dir" in
    ""|/)
        echo "refusing unsafe install directory: '$install_dir'" >&2
        exit 2
        ;;
esac
if [ "$purge" = true ] && [ "$uninstall" != true ]; then
    echo "--purge requires --uninstall" >&2
    exit 2
fi

purge_provider_assets() {
    [ -n "$provider_slug" ] || return 0
    case "$(uname -s)" in
        Darwin)
            data_root="$HOME/Library/Application Support/UtterPipe/providers/$provider_slug"
            cache_root="$HOME/Library/Caches/UtterPipe/providers/$provider_slug"
            ;;
        Linux)
            data_root="${XDG_DATA_HOME:-$HOME/.local/share}/utterpipe/providers/$provider_slug"
            cache_root="${XDG_CACHE_HOME:-$HOME/.cache}/utterpipe/providers/$provider_slug"
            ;;
        *)
            echo "asset purge is unsupported on this operating system" >&2
            exit 2
            ;;
    esac
    rm -rf -- "$data_root" "$cache_root"
    echo "removed provider assets for $provider_slug (not recoverable)"
}

if [ "$uninstall" = true ]; then
    for program in $programs; do
        rm -f -- "$install_dir/$program"
        echo "removed $install_dir/$program"
    done
    if [ "$purge" = true ]; then
        purge_provider_assets
    fi
    exit 0
fi

case "$(uname -s):$(uname -m)" in
    Linux:x86_64|Linux:amd64)
        target="x86_64-unknown-linux-gnu"
        ;;
    Darwin:arm64|Darwin:aarch64)
        target="aarch64-apple-darwin"
        ;;
    Darwin:x86_64|Darwin:amd64)
        target="x86_64-apple-darwin"
        ;;
    *)
        echo "no release artifact is published for $(uname -s) $(uname -m)" >&2
        exit 1
        ;;
esac

if [ -z "$version" ]; then
    latest="$(curl -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/$repository/releases/latest")"
    version="${latest##*/}"
fi
if ! printf '%s\n' "$version" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$'; then
    echo "invalid release version: '$version'" >&2
    exit 1
fi

archive="$archive_prefix-$version-$target.tar.gz"
release_url="${RELEASE_BASE_URL:-https://github.com/$repository/releases/download/$version}"
temporary="$(mktemp -d)"
cleanup() {
    rm -rf -- "$temporary"
}
trap cleanup EXIT HUP INT TERM

curl -fL --retry 3 -o "$temporary/$archive" "$release_url/$archive"
curl -fL --retry 3 -o "$temporary/$archive.sha256" "$release_url/$archive.sha256"
(
    cd "$temporary"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c "$archive.sha256"
    else
        shasum -a 256 -c "$archive.sha256"
    fi
)
tar -C "$temporary" -xzf "$temporary/$archive"
package_root="$temporary/$archive_prefix-$version-$target"
mkdir -p "$install_dir"
for program in $programs; do
    install -m 755 "$package_root/$program" "$install_dir/$program"
    echo "installed $install_dir/$program"
done

case ":${PATH:-}:" in
    *":$install_dir:"*) ;;
    *) echo "add $install_dir to PATH before invoking the installed tools" >&2 ;;
esac
