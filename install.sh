#!/bin/sh
set -eu
LC_ALL=C
export LC_ALL

REPO="caudra/caudra"
BINARY="caudra"

github_api() {
    case "$1" in
        "https://api.github.com/repos/${REPO}/releases"*) ;;
        *) err "refusing non-GitHub API origin" ;;
    esac
    token="${GITHUB_TOKEN:-${GH_TOKEN:-}}"
    if [ -n "${token}" ]; then
        curl -q -fsS --proto '=https' --connect-timeout 10 --max-time 30 --max-filesize 4194304 \
            -H "Authorization: Bearer ${token}" \
            -H "Accept: application/vnd.github+json" \
            -H "User-Agent: caudra-install" \
            -w '%{http_code}' "$@"
    else
        curl -q -fsS --proto '=https' --connect-timeout 10 --max-time 30 --max-filesize 4194304 \
            -H "Accept: application/vnd.github+json" \
            -H "User-Agent: caudra-install" \
            -w '%{http_code}' "$@"
    fi
}

download() {
    curl -q -fsSL --proto '=https' --proto-redir '=https' --max-redirs 3 \
        --connect-timeout 10 --max-time 300 --max-filesize "${3:-2147483648}" "$1" -o "$2" || return 1
    [ "$(wc -c < "$2")" -le "${3:-2147483648}" ] || err "release response too large"
}

is_windows() {
    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*) return 0 ;;
        *) return 1 ;;
    esac
}

write_resolver() {
    cat > "${tmp}/releases.awk" <<'AWK'
function fail(message) { print "error: " message > "/dev/stderr"; exit 1 }
function number(s) { return s ~ /^(0|[1-9][0-9]*)$/ }
function semver(s, a, b, n, i) {
    if (s !~ /^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$/) return 0
    sub(/^v/, "", s)
    n = split(s, b, /\+/)
    if (n == 2) {
        n = split(b[2], a, /\./)
        for (i = 1; i <= n; i++) if (a[i] == "") return 0
    }
    s = b[1]
    sub(/-.*/, "", s)
    split(s, a, /\./)
    for (i = 1; i <= 3; i++) if (!number(a[i])) return 0
    s = b[1]
    if (index(s, "-")) {
        sub(/^[^-]*-/, "", s)
        n = split(s, a, /\./)
        for (i = 1; i <= n; i++)
            if (a[i] == "" || (a[i] ~ /^[0-9]+$/ && !number(a[i]))) return 0
    }
    return 1
}
function preview(s) { sub(/\+.*/, "", s); return index(s, "-") > 0 }
function numeric_cmp(a, b) {
    if (length(a) != length(b)) return length(a) > length(b) ? 1 : -1
    return ("x" a) == ("x" b) ? 0 : (("x" a) > ("x" b) ? 1 : -1)
}
function compare(a, b, ac, bc, ap, bp, an, bn, i, c) {
    sub(/^v/, "", a); sub(/^v/, "", b)
    sub(/\+.*/, "", a); sub(/\+.*/, "", b)
    ap = a; bp = b
    sub(/-.*/, "", a); sub(/-.*/, "", b)
    split(a, ac, /\./); split(b, bc, /\./)
    for (i = 1; i <= 3; i++) { c = numeric_cmp(ac[i], bc[i]); if (c) return c }
    if (ap == a || bp == b) return (ap == a) - (bp == b)
    sub(/^[^-]*-/, "", ap); sub(/^[^-]*-/, "", bp)
    an = split(ap, ac, /\./); bn = split(bp, bc, /\./)
    for (i = 1; i <= an && i <= bn; i++) {
        if (("x" ac[i]) == ("x" bc[i])) continue
        if (number(ac[i]) && number(bc[i])) return numeric_cmp(ac[i], bc[i])
        if (number(ac[i]) != number(bc[i])) return number(ac[i]) ? -1 : 1
        return ("x" ac[i]) > ("x" bc[i]) ? 1 : -1
    }
    return an == bn ? 0 : (an > bn ? 1 : -1)
}
function ws() { while (substr(json, pos, 1) ~ /^[ \t\r\n]$/) pos++ }
function string( s, c, e, h, n, i) {
    if (substr(json, pos++, 1) != "\"") fail("invalid JSON string")
    s = ""
    while (pos <= length(json)) {
        c = substr(json, pos++, 1)
        if (c == "\"") return s
        if (c ~ /[[:cntrl:]]/) fail("control character in JSON")
        if (c == "\\") {
            e = substr(json, pos++, 1)
            if (e == "u") {
                h = substr(json, pos, 4); pos += 4
                if (length(h) != 4 || h ~ /[^0-9a-fA-F]/) fail("invalid JSON escape")
                n = 0
                for (i = 1; i <= 4; i++) n = n * 16 + index("0123456789abcdef", tolower(substr(h, i, 1))) - 1
                c = n >= 32 && n < 127 ? sprintf("%c", n) : "\001"
            } else if (e == "\"" || e == "\\" || e == "/") c = e
            else if (e ~ /^[bfnrt]$/) c = "\001"
            else fail("invalid JSON escape")
        }
        s = s c
    }
    fail("unterminated JSON string")
}
function value(path, depth, c, key, child, n, text) {
    if (depth > 32) fail("JSON nesting limit exceeded")
    ws(); c = substr(json, pos, 1)
    if (c == "{" || c == "[") {
        kinds[path] = c; pos++; ws(); n = 0
        if (substr(json, pos, 1) != (c == "{" ? "}" : "]")) {
            while (1) {
                if (c == "{") {
                    key = string(); ws()
                    if (substr(json, pos++, 1) != ":") fail("invalid JSON object")
                } else key = n
                child = path SUBSEP key
                if (child in kinds) fail("duplicate JSON member")
                value(child, depth + 1); n++; ws()
                if (substr(json, pos, 1) != ",") break
                pos++; ws()
            }
        }
        if (substr(json, pos++, 1) != (c == "{" ? "}" : "]")) fail("invalid JSON container")
        sizes[path] = n
    } else if (c == "\"") { kinds[path] = "string"; values[path] = string() }
    else {
        text = substr(json, pos)
        if (match(text, /^(true|false|null)/)) {
            text = substr(text, 1, RLENGTH); kinds[path] = text; values[path] = text
        } else if (match(text, /^-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?/)) {
            text = substr(text, 1, RLENGTH); kinds[path] = "number"; values[path] = text
        } else fail("invalid JSON value")
        pos += length(text)
    }
}
function release(path, tag, pre, asset, name, base, archive, i, required, found, usable) {
    if (kinds[path] != "{" || kinds[path SUBSEP "tag_name"] != "string" ||
        kinds[path SUBSEP "draft"] !~ /^(true|false)$/ ||
        kinds[path SUBSEP "prerelease"] !~ /^(true|false)$/) fail("invalid release metadata")
    tag = values[path SUBSEP "tag_name"]
    if (kinds[path SUBSEP "draft"] == "true" || !semver(tag)) return
    if (kinds[path SUBSEP "published_at"] != "string" ||
        values[path SUBSEP "published_at"] !~ /^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]Z$/) return
    pre = preview(tag)
    if ((kinds[path SUBSEP "prerelease"] == "true") != pre) return
    archive = "caudra-" tag "-" target "." extension
    base = "https://github.com/caudra/caudra/releases/download/" tag "/"
    usable = kinds[path SUBSEP "assets"] == "["
    for (i = 0; i < sizes[path SUBSEP "assets"]; i++) {
        asset = path SUBSEP "assets" SUBSEP i
        name = values[asset SUBSEP "name"]
        required = name == archive || name == "sha256sums.txt" || name == installer
        if (!required) continue
        if (found[name]++) usable = 0
        if (kinds[asset SUBSEP "name"] != "string" ||
            values[asset SUBSEP "state"] != "uploaded" ||
            kinds[asset SUBSEP "size"] != "number" || values[asset SUBSEP "size"] + 0 <= 0 ||
            values[asset SUBSEP "browser_download_url"] != base name) usable = 0
    }
    if (!found[archive] || !found["sha256sums.txt"] || !found[installer]) usable = 0
    print tag "\t" (pre ? "preview" : "stable") "\t" usable
}
BEGIN {
    if (mode == "tag") exit !semver(tag)
    if (mode == "select") {
        while ((getline line) > 0) {
            split(line, fields, "\t")
            if (seen[fields[1]]++) fail("duplicate release across pages")
            if (channel == "stable" && fields[2] != "stable") continue
            if (best == "" || (channel == "auto" && kind == "preview" && fields[2] == "stable") ||
                ((channel == "preview" || kind == fields[2]) && compare(fields[1], best) > 0)) {
                best = fields[1]; kind = fields[2]; usable = fields[3]
            }
        }
        if (best == "") fail("no published release for requested channel/tag")
        if (usable != "1") fail("selected release lacks required uploaded assets")
        print best; exit
    }
    while ((getline line) > 0) json = json line "\n"
    pos = 1; value("root", 0); ws()
    if (pos <= length(json)) fail("trailing JSON data")
    if (mode == "single") {
        if (values["root" SUBSEP "tag_name"] != tag) fail("release tag mismatch")
        release("root")
    } else {
        if (kinds["root"] != "[" || sizes["root"] > 100) fail("invalid release list")
        print sizes["root"] > countfile
        for (record = 0; record < sizes["root"]; record++) release("root" SUBSEP record)
    }
}
AWK
}

resolve_tag() {
    : > "${tmp}/candidates"
    if [ -n "${tag}" ]; then
        awk -v mode=tag -v tag="${tag}" -f "${tmp}/releases.awk" || err "expected a v-prefixed SemVer tag"
        status="$(github_api "https://api.github.com/repos/${REPO}/releases/tags/${tag}" -o "${tmp}/page")" || err "release lookup failed"
        [ "${status}" = 200 ] || err "release lookup returned HTTP ${status}"
        [ "$(wc -c < "${tmp}/page")" -le 4194304 ] || err "release response too large"
        awk -v mode=single -v tag="${tag}" -v target="${target}" -v extension="${archive_ext}" \
            -v installer=install.sh -f "${tmp}/releases.awk" "${tmp}/page" > "${tmp}/candidates" || err "invalid release response"
    else
        page=1
        while :; do
            [ "${page}" -le 10 ] || err "release discovery exceeded 10 pages; refusing incomplete selection"
            status="$(github_api "https://api.github.com/repos/${REPO}/releases?per_page=100&page=${page}" -o "${tmp}/page")" || err "release discovery failed"
            [ "${status}" = 200 ] || err "release discovery returned HTTP ${status}"
            [ "$(wc -c < "${tmp}/page")" -le 4194304 ] || err "release response too large"
            awk -v target="${target}" -v extension="${archive_ext}" -v installer=install.sh \
                -v countfile="${tmp}/count" -f "${tmp}/releases.awk" "${tmp}/page" >> "${tmp}/candidates" || err "invalid release response"
            [ "$(cat "${tmp}/count")" != 0 ] || break
            page=$((page + 1))
        done
    fi
    tag="$(awk -v mode=select -v channel="${channel}" -f "${tmp}/releases.awk" "${tmp}/candidates")" || err "release selection failed"
}

verify_archive() {
    expected="$(LC_ALL=C awk -v archive="${archive_name}" '
        { sub(/\r$/, "") }
        length($0) < 67 || substr($0, 1, 64) ~ /[^0-9a-fA-F]/ ||
            substr($0, 65, 1) != " " || substr($0, 66, 1) !~ /^[ *]$/ { exit 1 }
        {
            name = substr($0, 67)
            if (name !~ /^[A-Za-z0-9][A-Za-z0-9._+-]*$/ || seen[name]++) exit 1
            if (name == archive) { digest = tolower(substr($0, 1, 64)); count++ }
        }
        END { if (count != 1) exit 1; print digest }
    ' "${tmp}/sha256sums.txt")" || err "invalid, duplicate, or missing archive checksum"
    case "${sha_tool}" in
        sha256sum) actual="$(sha256sum "${tmp}/${archive_name}")" ;;
        shasum) actual="$(shasum -a 256 "${tmp}/${archive_name}")" ;;
        openssl) actual="$(openssl dgst -sha256 "${tmp}/${archive_name}")"; actual="${actual##* }" ;;
    esac
    actual="${actual%% *}"
    [ "${actual}" = "${expected}" ] || err "archive SHA-256 checksum mismatch"
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
    need_cmd awk
    channel=auto
    tag=""
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --channel)
                [ "$#" -ge 2 ] && [ "${channel}" = auto ] || err "usage: install.sh [vVERSION | --channel stable|preview]"
                channel="$2"; shift
                case "${channel}" in stable|preview) ;; *) err "channel must be stable or preview" ;; esac ;;
            --help|-h) echo "usage: install.sh [vVERSION | --channel stable|preview]"; return ;;
            -*) err "unknown option: $1" ;;
            *) [ -z "${tag}" ] || err "only one release tag is allowed"; tag="$1" ;;
        esac
        shift
    done
    [ -z "${tag}" ] || [ "${channel}" = auto ] || err "a release tag cannot be combined with --channel"
    sha_tool=""
    for candidate in sha256sum shasum openssl; do
        if command -v "${candidate}" >/dev/null 2>&1; then sha_tool="${candidate}"; break; fi
    done
    [ -n "${sha_tool}" ] || err "need sha256sum, shasum, or openssl to verify downloads"

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

    tmp="$(mktemp -d)"
    trap 'rm -rf "${tmp}"' EXIT
    write_resolver
    resolve_tag
    archive_name="${BINARY}-${tag}-${target}.${archive_ext}"
    url="https://github.com/${REPO}/releases/download/${tag}"
    label=""
    case "${tag%%+*}" in *-*) label=" (Preview)" ;; esac

    echo "downloading ${BINARY} ${tag}${label} for ${target}..."
    download "${url}/${archive_name}" "${tmp}/${archive_name}" || err "archive download failed"
    download "${url}/sha256sums.txt" "${tmp}/sha256sums.txt" 1048576 || err "checksum download failed"
    verify_archive
    if [ "${archive_ext}" = "zip" ]; then
        unzip -qo "${tmp}/${archive_name}" -d "${tmp}"
    else
        tar xzf "${tmp}/${archive_name}" -C "${tmp}"
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

    echo "${BINARY} ${tag}${label} installed to ${dest}"
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
