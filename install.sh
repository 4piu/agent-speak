#!/bin/sh
set -eu

repository="4piu/agent-speak"
archive_prefix="agent-speak"
programs="agent-speak"
provider_slug=""

usage() {
    echo "usage: install.sh [--version vX.Y.Z] [--install-dir PATH] [--skip-audio-check] [--uninstall [--purge]]" >&2
}

version="${VERSION:-}"
install_dir="${INSTALL_DIR:-${XDG_BIN_HOME:-$HOME/.local/bin}}"
uninstall=false
purge=false
skip_audio_check=false
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
        --skip-audio-check)
            skip_audio_check=true
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

pipewire_is_active() {
    if command -v wpctl >/dev/null 2>&1 && wpctl status >/dev/null 2>&1; then
        return 0
    fi
    if command -v systemctl >/dev/null 2>&1 &&
        systemctl --user is-active --quiet pipewire.service >/dev/null 2>&1; then
        return 0
    fi
    return 1
}

dpkg_package_is_installed() {
    [ "$(dpkg-query -W -f='${Status}' "$1" 2>/dev/null || true)" = "install ok installed" ]
}

pipewire_alsa_is_installed() {
    if command -v pacman >/dev/null 2>&1; then
        pacman -T pipewire-alsa >/dev/null 2>&1
        return
    fi
    if command -v dpkg-query >/dev/null 2>&1; then
        dpkg_package_is_installed pipewire-alsa
        return
    fi
    if command -v rpm >/dev/null 2>&1; then
        rpm -q pipewire-alsa >/dev/null 2>&1
        return
    fi
    [ -e /etc/alsa/conf.d/99-pipewire-default.conf ] ||
        [ -e /usr/share/alsa/alsa.conf.d/99-pipewire-default.conf ]
}

apt_package_has_candidate() {
    apt-cache policy "$1" 2>/dev/null |
        sed -n 's/^[[:space:]]*Candidate:[[:space:]]*//p' |
        grep -qv '^\(none\)$'
}

debian_alsa_runtime_is_installed() {
    dpkg_package_is_installed libasound2t64 ||
        dpkg_package_is_installed libasound2
}

debian_alsa_runtime_package() {
    if apt_package_has_candidate libasound2t64; then
        printf '%s\n' libasound2t64
    elif apt_package_has_candidate libasound2; then
        printf '%s\n' libasound2
    fi
}

install_arch_audio_packages() {
    packages=$1
    echo "Agent Speak uses ALSA for playback; this Arch/Manjaro system is missing:$packages" >&2
    echo "The installer can run: sudo pacman -S --needed$packages" >&2

    if [ ! -r /dev/tty ] || [ ! -w /dev/tty ]; then
        echo "no interactive terminal is available; install those packages manually" >&2
        return
    fi

    printf "Install the missing audio packages now? [y/N] " >/dev/tty
    answer=
    IFS= read -r answer </dev/tty || true
    case "$answer" in
        y|Y|yes|YES|Yes)
            if [ "$(id -u)" -eq 0 ]; then
                # Package names are fixed constants assembled below; intentional splitting
                # passes each package as a separate pacman argument.
                # shellcheck disable=SC2086
                pacman -S --needed $packages </dev/tty
            elif command -v sudo >/dev/null 2>&1; then
                # shellcheck disable=SC2086
                sudo pacman -S --needed $packages </dev/tty
            else
                echo "sudo is unavailable; install manually with: pacman -S --needed$packages" >&2
            fi
            ;;
        *)
            echo "skipped audio package installation" >&2
            ;;
    esac
}

install_debian_audio_packages() {
    packages=$1
    echo "Agent Speak uses ALSA for playback; this Debian-based system is missing:$packages" >&2
    echo "The installer can run: sudo apt-get install$packages" >&2

    if [ ! -r /dev/tty ] || [ ! -w /dev/tty ]; then
        echo "no interactive terminal is available; install those packages manually" >&2
        return
    fi

    printf "Install the missing audio packages now? [y/N] " >/dev/tty
    answer=
    IFS= read -r answer </dev/tty || true
    case "$answer" in
        y|Y|yes|YES|Yes)
            if [ "$(id -u)" -eq 0 ]; then
                # Package names are selected from fixed constants above.
                # shellcheck disable=SC2086
                apt-get install $packages </dev/tty
            elif command -v sudo >/dev/null 2>&1; then
                # shellcheck disable=SC2086
                sudo apt-get install $packages </dev/tty
            else
                echo "sudo is unavailable; install manually with: apt-get install$packages" >&2
            fi
            ;;
        *)
            echo "skipped audio package installation" >&2
            ;;
    esac
}

check_linux_audio_dependencies() {
    [ "$(uname -s)" = Linux ] || return

    if command -v pacman >/dev/null 2>&1; then
        missing_packages=""
        if ! pacman -T alsa-lib >/dev/null 2>&1; then
            missing_packages="$missing_packages alsa-lib"
        fi
        if pipewire_is_active && ! pipewire_alsa_is_installed; then
            missing_packages="$missing_packages pipewire-alsa"
        fi
        if [ -n "$missing_packages" ]; then
            install_arch_audio_packages "$missing_packages"
        elif pipewire_is_active; then
            echo "verified ALSA playback support and the PipeWire ALSA bridge" >&2
        else
            echo "verified the ALSA runtime; PipeWire is not active in this session" >&2
        fi
        return
    fi

    if command -v dpkg-query >/dev/null 2>&1 &&
        command -v apt-get >/dev/null 2>&1 &&
        command -v apt-cache >/dev/null 2>&1; then
        missing_packages=""
        unresolved_dependencies=false
        if ! debian_alsa_runtime_is_installed; then
            alsa_package="$(debian_alsa_runtime_package)"
            if [ -n "$alsa_package" ]; then
                missing_packages="$missing_packages $alsa_package"
            else
                echo "the ALSA runtime was not found and no installable libasound package was detected" >&2
                unresolved_dependencies=true
            fi
        fi
        if pipewire_is_active && ! pipewire_alsa_is_installed; then
            missing_packages="$missing_packages pipewire-alsa"
        fi
        if [ -n "$missing_packages" ]; then
            install_debian_audio_packages "$missing_packages"
        elif [ "$unresolved_dependencies" = true ]; then
            echo "audio dependencies remain unresolved; refresh the APT package index and retry" >&2
        elif pipewire_is_active; then
            echo "verified ALSA playback support and the PipeWire ALSA bridge" >&2
        else
            echo "verified the ALSA runtime; PipeWire is not active in this session" >&2
        fi
        return
    fi

    if pipewire_is_active; then
        if pipewire_alsa_is_installed; then
            echo "verified the PipeWire ALSA bridge" >&2
        else
            echo "PipeWire is active but its ALSA bridge was not found" >&2
            echo "install your distribution's pipewire-alsa package before using Agent Speak" >&2
        fi
    fi
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

if [ "$skip_audio_check" != true ]; then
    check_linux_audio_dependencies
fi

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
