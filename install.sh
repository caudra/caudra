#!/bin/sh
set -eu

REPO="caudra/caudra"
BINARY="caudra"

github_curl() {
    token="${GITHUB_TOKEN:-${GH_TOKEN:-}}"
    if [ -n "${token}" ]; then
        curl -fsSL \
            -H "Authorization: Bearer ${token}" \
            -H "Accept: application/vnd.github+json" \
            -H "User-Agent: caudra-install" \
            "$@"
    else
        curl -fsSL \
            -H "Accept: application/vnd.github+json" \
            -H "User-Agent: caudra-install" \
            "$@"
    fi
}

is_windows() {
    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*) return 0 ;;
        *) return 1 ;;
    esac
}

# Works for both pretty-printed and single-line GitHub API JSON.
latest_tag() {
    github_curl "https://api.github.com/repos/${REPO}/releases/latest" \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
        | head -n 1
}

default_install_dir() {
    if is_windows; then
        if [ -n "${LOCALAPPDATA:-}" ]; then
            printf '%s\n' "${LOCALAPPDATA}/caudra"
        else
            printf '%s\n' "${HOME}/.local/bin"
        fi
    else
        printf '%s\n' "${HOME}/.local/bin"
    fi
}

path_has_dir() {
    dir="$1"
    case ":${PATH}:" in
        *":${dir}:"*) return 0 ;;
        *) return 1 ;;
    esac
}

warn_path() {
    dir="$1"
    if path_has_dir "${dir}"; then
        return 0
    fi
    echo "note: ${dir} is not in PATH; add it to your shell config, e.g.:"
    echo "  export PATH=\"${dir}:\$PATH\""
}

warn_shadowed() {
    dest="$1"
    resolved="$(command -v "${BINARY}" 2>/dev/null || true)"
    if [ -n "${resolved}" ] && [ "${resolved}" != "${dest}" ]; then
        echo "note: '${BINARY}' resolves to ${resolved}, which shadows ${dest};"
        echo "  remove it or reorder PATH so the new install comes first"
    fi
}

add_windows_user_path() {
    dir="$1"
    # Convert to Windows path when possible so PATH works outside Git Bash.
    if command -v cygpath > /dev/null 2>&1; then
        win_dir="$(cygpath -w "${dir}")"
    else
        win_dir="${dir}"
    fi
    powershell.exe -NoProfile -Command "
\$dir = '${win_dir}' -replace '/', '\\'
\$sep = [IO.Path]::PathSeparator
\$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (\$null -eq \$userPath) { \$userPath = '' }
\$entries = \$userPath -split [regex]::Escape(\$sep) | Where-Object { \$_ -ne '' }
\$already = \$entries | Where-Object { \$_.TrimEnd('\\') -ieq \$dir.TrimEnd('\\') }
if (\$already) { exit 0 }
\$newPath = if (\$userPath.Trim()) { \"\$userPath\$sep\$dir\" } else { \$dir }
[Environment]::SetEnvironmentVariable('Path', \$newPath, 'User')
Write-Host \"added \$dir to user PATH (restart terminal if caudra is not found)\"
" || true
}

validate_path() (
    path="$1"
    while [ -n "${path}" ]; do
        [ ! -L "${path}" ] || err "refusing symlink: ${path}"
        if [ -e "${path}" ] && [ ! -d "${path}" ] && [ ! -f "${path}" ]; then
            err "refusing special file: ${path}"
        fi
        path="${path%/*}"
    done
)

validate_tree() {
    invalid="$(${elevate:-} find "$1" ! -type f ! -type d -print)" || err "cannot inspect $1"
    [ -z "${invalid}" ] || err "refusing symlink or special file: ${invalid}"
}

install_payload() (
    bundle_stage=""
    binary_stage=""
    old_bundle=0
    new_bundle=0
    old_binary=0
    committed=0
    finish_install() {
        status=$?
        trap - EXIT HUP INT TERM
        recovery_failed=0
        if [ "${committed}" = 0 ]; then
            if [ "${old_binary}" = 1 ]; then
                if ${elevate} test -e "${dest}" || ${elevate} test -L "${dest}"; then
                    recovery_failed=1
                else
                    ${elevate} mv "${binary_stage}/previous" "${dest}" || recovery_failed=1
                fi
            fi
            if [ "${new_bundle}" = 1 ]; then
                ${elevate} mv "${license_dir}" "${bundle_stage}/new" || recovery_failed=1
            fi
            if [ "${old_bundle}" = 1 ]; then
                if ${elevate} test -e "${license_dir}" || ${elevate} test -L "${license_dir}"; then
                    recovery_failed=1
                else
                    ${elevate} mv "${bundle_stage}/previous" "${license_dir}" || recovery_failed=1
                fi
            fi
        fi
        if [ "${recovery_failed}" = 1 ]; then
            echo "error: recovery failed; retained installation data in ${bundle_stage} and ${binary_stage}" >&2
            exit 1
        fi
        for stage in "${bundle_stage}" "${binary_stage}"; do
            [ -n "${stage}" ] || continue
            if [ "${committed}" = 1 ] && ${elevate} test -e "${stage}/previous"; then
                echo "previous installation retained in ${stage}/previous"
            else
                ${elevate} rm -rf "${stage}" || status=1
            fi
        done
        exit "${status}"
    }
    trap finish_install EXIT
    trap 'exit 1' HUP INT TERM
    bundle_stage="$(${elevate} mktemp -d "${license_parent}/.caudra-backup.XXXXXX")" || err "cannot stage licenses"
    binary_stage="$(${elevate} mktemp -d "${INSTALL_DIR}/.caudra-backup.XXXXXX")" || err "cannot stage binary"
    ${elevate} cp -R "${tmp}/licenses" "${bundle_stage}/new" || err "failed to stage licenses"
    ${elevate} cp "${tmp}/${bin_name}" "${binary_stage}/new" || err "failed to stage binary"
    ${elevate} chmod +x "${binary_stage}/new" || err "failed to make staged binary executable"
    validate_tree "${bundle_stage}/new"
    validate_path "${dest}"
    validate_path "${license_dir}"
    if [ -e "${license_dir}" ]; then
        validate_tree "${license_dir}"
        ${elevate} mv "${license_dir}" "${bundle_stage}/previous" || err "cannot retain previous licenses"
        old_bundle=1
    fi
    ${elevate} mv "${bundle_stage}/new" "${license_dir}" || err "failed to publish licenses"
    new_bundle=1
    if [ -e "${dest}" ]; then
        ${elevate} mv "${dest}" "${binary_stage}/previous" || err "cannot retain previous binary"
        old_binary=1
    fi
    ${elevate} mv "${binary_stage}/new" "${dest}" || err "failed to publish binary"
    committed=1
)

main() {
    elevate=""
    need_cmd curl

    if is_windows; then
        # Only x86_64 Windows builds are published; ARM64 runs them under emulation.
        target="x86_64-pc-windows-msvc"
        archive_ext="zip"
        bin_name="${BINARY}.exe"
        need_cmd unzip
    else
        case "$(uname -s)" in
            Linux)  os="unknown-linux-musl" ;;
            Darwin) os="apple-darwin" ;;
            *) err "unsupported OS: $(uname -s)" ;;
        esac

        case "$(uname -m)" in
            x86_64|amd64)   arch="x86_64" ;;
            aarch64|arm64)  arch="aarch64" ;;
            *) err "unsupported architecture: $(uname -m)" ;;
        esac

        target="${arch}-${os}"
        archive_ext="tar.gz"
        bin_name="${BINARY}"
    fi

    INSTALL_DIR="${CAUDRA_INSTALL_DIR:-$(default_install_dir)}"

    tag="${1:-$(latest_tag)}"
    [ -n "${tag}" ] || err "failed to determine latest release tag"

    url="https://github.com/${REPO}/releases/download/${tag}/${BINARY}-${tag}-${target}.${archive_ext}"
    tmp="$(mktemp -d)"
    trap 'rm -rf "${tmp}"' EXIT

    echo "downloading ${BINARY} ${tag} for ${target}..."
    if [ "${archive_ext}" = "zip" ]; then
        github_curl "${url}" -o "${tmp}/caudra.zip"
        unzip -qo "${tmp}/caudra.zip" -d "${tmp}"
    else
        github_curl "${url}" | tar xz -C "${tmp}"
    fi

    [ -f "${tmp}/${bin_name}" ] || err "archive did not contain ${bin_name}"
    [ -s "${tmp}/licenses/manifest.json" ] || err "archive did not contain licenses/manifest.json; legacy archives without a license bundle are not supported"
    [ -s "${tmp}/licenses/ATTRIBUTION.txt" ] || err "archive did not contain licenses/ATTRIBUTION.txt"
    [ ! -L "${tmp}/${bin_name}" ] || err "refusing symlink: ${bin_name}"
    validate_tree "${tmp}/licenses"

    if is_windows && command -v cygpath > /dev/null 2>&1; then
        INSTALL_DIR="$(cygpath -u "${INSTALL_DIR}")" || err "cannot resolve installation directory"
    fi
    case "${INSTALL_DIR}" in
        /*) ;;
        *) INSTALL_DIR="$(pwd)/${INSTALL_DIR}" ;;
    esac
    license_parent="${INSTALL_DIR}/../share/licenses"
    validate_path "${INSTALL_DIR}"
    validate_path "${license_parent}"
    if mkdir -p "${INSTALL_DIR}" "${license_parent}" 2>/dev/null &&
        [ -w "${INSTALL_DIR}" ] && [ -w "${license_parent}" ]; then
        :
    elif command -v sudo > /dev/null 2>&1; then
        echo "installing binary and licenses to ${INSTALL_DIR} (requires sudo)..."
        elevate="sudo"
        sudo mkdir -p "${INSTALL_DIR}" "${license_parent}" || err "cannot create installation directories"
    else
        err "cannot write to ${INSTALL_DIR} and ${license_parent} (set CAUDRA_INSTALL_DIR to a writable directory)"
    fi
    INSTALL_DIR="$(CDPATH= cd -P "${INSTALL_DIR}" && pwd)" || err "cannot resolve installation directory"
    license_parent="$(CDPATH= cd -P "${license_parent}" && pwd)" || err "cannot resolve license directory"
    license_dir="${license_parent}/caudra"
    dest="${INSTALL_DIR}/${bin_name}"
    validate_path "${dest}"
    validate_path "${license_dir}"
    [ ! -e "${dest}" ] || [ -f "${dest}" ] || err "binary destination is not a regular file"
    [ ! -e "${license_dir}" ] || [ -d "${license_dir}" ] || err "license destination is not a directory"
    install_payload

    echo "${BINARY} ${tag} installed to ${dest}"
    echo "licenses installed to ${license_dir}"

    if is_windows; then
        add_windows_user_path "${INSTALL_DIR}"
    else
        warn_path "${INSTALL_DIR}"
        warn_shadowed "${dest}"
    fi
    echo ""
}

need_cmd() {
    command -v "$1" > /dev/null 2>&1 || err "need '$1' (not found)"
}

err() {
    echo "error: $1" >&2
    exit 1
}

main "$@"
