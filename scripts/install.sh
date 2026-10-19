#!/bin/sh
set -eu

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

detect_downloader() {
    if command -v curl >/dev/null 2>&1; then
        DOWNLOADER="curl"
    elif command -v wget >/dev/null 2>&1; then
        DOWNLOADER="wget"
    else
        die "curl or wget is required to download werkWorker"
    fi
}

detect_checksum_tool() {
    if command -v sha256sum >/dev/null 2>&1; then
        CHECKSUM_TOOL="sha256sum"
    elif command -v shasum >/dev/null 2>&1; then
        CHECKSUM_TOOL="shasum"
    else
        die "sha256sum or shasum is required to verify the werkWorker release"
    fi
}

download_to_file() {
    url=$1
    output=$2

    if [ "$DOWNLOADER" = "curl" ]; then
        curl --proto "=https" --proto-redir "=https" -fL "$url" -o "$output"
    else
        wget -O "$output" "$url"
    fi
}

download_to_stdout() {
    url=$1

    if [ "$DOWNLOADER" = "curl" ]; then
        curl --proto "=https" --proto-redir "=https" -fsSL "$url"
    else
        wget -qO- "$url"
    fi
}

verify_checksum() {
    directory=$1
    checksum_name=$2

    if [ "$CHECKSUM_TOOL" = "sha256sum" ]; then
        (cd "$directory" && sha256sum -c "$checksum_name")
    else
        (cd "$directory" && shasum -a 256 -c "$checksum_name")
    fi
}

validate_archive_listing() {
    archive=$1

    if ! archive_entries=$(tar -tzf "$archive"); then
        die "could not read downloaded release archive"
    fi
    sorted_entries=$(printf '%s\n' "$archive_entries" | LC_ALL=C sort)
    expected_entries=$(printf '%s\n' LICENSE NOTICE README.md werk-worker ww)
    [ "$sorted_entries" = "$expected_entries" ] || die "release archive contains unexpected entries"
    tar -tvzf "$archive" | awk 'substr($0,1,1) != "-" { bad=1 } END { exit bad }' || die "archive contains non-regular files"

}

validate_extracted_archive() {
    directory=$1

    for name in werk-worker ww README.md LICENSE NOTICE; do
        [ -f "$directory/$name" ] || die "release archive did not contain regular file: $name"
        [ ! -L "$directory/$name" ] || die "release archive contained a symbolic link: $name"
    done
}

normalize_version() {
    input=$1

    printf '%s\n' "$input" | LC_ALL=C grep -Eq '^v?[0-9]+\.[0-9]+\.[0-9]+$' || die "invalid stable release version"
    case "$input" in
        v*)
            WERK_WORKER_TAG=$input
            WERK_WORKER_VERSION_NUMBER=${input#v}
            ;;
        *)
            WERK_WORKER_TAG="v$input"
            WERK_WORKER_VERSION_NUMBER=$input
            ;;
    esac
}

detect_platform() {
    os=$(uname -s)
    arch=$(uname -m)

    case "$os:$arch" in
        Linux:x86_64) WERK_WORKER_PLATFORM="linux-x86_64" ;;
        Linux:arm64|Linux:aarch64) WERK_WORKER_PLATFORM="linux-aarch64" ;;
        Darwin:arm64|Darwin:aarch64) WERK_WORKER_PLATFORM="macos-aarch64" ;;
        *) die "unsupported OS/architecture: $os $arch" ;;
    esac
}

configure_artifact() {
    platform=$1
    artifact_name="werkWorker-v${WERK_WORKER_VERSION_NUMBER}-${platform}.tar.gz"
    download_url="https://github.com/${WERK_WORKER_REPO}/releases/download/${WERK_WORKER_TAG}/${artifact_name}"
    checksum_name="$artifact_name.sha256"
    checksum_url="$download_url.sha256"
    archive_path="$tmp_dir/$artifact_name"
    checksum_path="$tmp_dir/$checksum_name"
}

download_artifact_and_checksum() {
    printf 'Downloading %s\n' "$download_url"
    if ! download_to_file "$download_url" "$archive_path"; then
        return 1
    fi
    printf 'Downloading %s\n' "$checksum_url"
    if ! download_to_file "$checksum_url" "$checksum_path"; then
        return 1
    fi
    return 0
}

detect_checksum_tool
detect_platform

WERK_WORKER_REPO=${WERK_WORKER_REPO:-philipbodenbach/werkWorker}
WERK_WORKER_VERSION_INPUT=${WERK_WORKER_VERSION:-latest}

if [ "${WERK_WORKER_INSTALL_DIR+x}" = "x" ]; then
    install_dir=$WERK_WORKER_INSTALL_DIR
else
    [ -n "${HOME:-}" ] || die "HOME is not set; set WERK_WORKER_INSTALL_DIR to choose an install directory"
    install_dir="$HOME/.local/bin"
fi

if [ -z "${WERK_WORKER_ARCHIVE:-}" ]; then detect_downloader; fi
if [ "$WERK_WORKER_VERSION_INPUT" = "latest" ]; then
    [ -z "${WERK_WORKER_ARCHIVE:-}" ] || die "Set WERK_WORKER_VERSION with a local archive"
    latest_json=$(download_to_stdout "https://api.github.com/repos/$WERK_WORKER_REPO/releases/latest")
    latest_tag=$(printf '%s\n' "$latest_json" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | sed -n '1p')
    [ -n "$latest_tag" ] || die "could not resolve latest release for $WERK_WORKER_REPO"
    normalize_version "$latest_tag"
else
    normalize_version "$WERK_WORKER_VERSION_INPUT"
fi

tmp_root=${TMPDIR:-/tmp}
tmp_dir="$tmp_root/werkWorker-install-$$"

mkdir "$tmp_dir" || die "could not create temporary directory: $tmp_dir"
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM

configure_artifact "$WERK_WORKER_PLATFORM"
if [ -n "${WERK_WORKER_ARCHIVE:-}" ]; then
    cp "$WERK_WORKER_ARCHIVE" "$archive_path"
    cp "$WERK_WORKER_ARCHIVE.sha256" "$checksum_path"
else
    download_artifact_and_checksum || die "could not download release archive/checksum"
fi

awk -v name="$artifact_name" 'NF != 2 || $2 != name || length($1) != 64 || $1 !~ /^[0-9a-fA-F]+$/ { bad=1 } END { exit (bad || NR != 1) }' "$checksum_path" || die "invalid checksum manifest"
printf 'Verifying %s\n' "$artifact_name"
verify_checksum "$tmp_dir" "$checksum_name" || die "checksum verification failed for $artifact_name"
validate_archive_listing "$archive_path"

# The CLI installer needs only these files, not the bundled examples.
tar -xzf "$archive_path" -C "$tmp_dir" werk-worker ww README.md LICENSE NOTICE
validate_extracted_archive "$tmp_dir"

mkdir -p "$install_dir"
for binary in werk-worker ww; do
    [ ! -d "$install_dir/$binary" ] || die "destination is a directory: $binary"
    staging=$(mktemp "$install_dir/.werk-worker-install.XXXXXX")
    cp "$tmp_dir/$binary" "$staging"
    chmod 755 "$staging"
    mv -f "$staging" "$install_dir/$binary"
done

printf 'Installed %s\n' "$install_dir/ww"

case ":${PATH:-}:" in
    *":$install_dir:"*) ;;
    *) printf 'Warning: %s is not on PATH. Add it to PATH to run ww from any directory.\n' "$install_dir" >&2 ;;
esac

printf '\nwerkWorker installed successfully.\n\n'
printf 'Run:\n'
printf '  ww --help\n'
