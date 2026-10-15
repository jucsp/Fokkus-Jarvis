#!/usr/bin/env bash
# Gate SEC-01 (CA-0.3): falla si el whisper.cpp vendorizado es < 1.9.5.
#
# Uso: scripts/check-whisper-version.sh [ruta/a/whisper.cpp/CMakeLists.txt]
# Sin argumento revisa vendor/whisper-rs-sys/whisper.cpp/CMakeLists.txt.
# Con --self-test comprueba que el gate rechaza una versión vulnerable.
set -euo pipefail

MIN_MAJOR=1
MIN_MINOR=9
MIN_PATCH=5

root="$(cd "$(dirname "$0")/.." && pwd)"

read_version() {
    local cmake="$1"
    local major minor patch
    major=$(sed -nE 's/^set\(WHISPER_VERSION_MAJOR ([0-9]+)\).*/\1/p' "$cmake")
    minor=$(sed -nE 's/^set\(WHISPER_VERSION_MINOR ([0-9]+)\).*/\1/p' "$cmake")
    patch=$(sed -nE 's/^set\(WHISPER_VERSION_PATCH ([0-9]+)\).*/\1/p' "$cmake")
    if [[ -z "$major" || -z "$minor" || -z "$patch" ]]; then
        # Formato antiguo (<= 1.8.x): project("whisper.cpp" VERSION x.y.z)
        local v
        v=$(sed -nE 's/^project\("whisper.cpp" VERSION ([0-9]+\.[0-9]+\.[0-9]+)\).*/\1/p' "$cmake")
        [[ -n "$v" ]] || return 1
        IFS=. read -r major minor patch <<<"$v"
    fi
    echo "$major $minor $patch"
}

check() {
    local cmake="$1" major minor patch
    if ! read -r major minor patch < <(read_version "$cmake"); then
        echo "FALLA: no se pudo leer la versión de whisper.cpp en $cmake" >&2
        return 1
    fi
    if (( major > MIN_MAJOR
        || (major == MIN_MAJOR && minor > MIN_MINOR)
        || (major == MIN_MAJOR && minor == MIN_MINOR && patch >= MIN_PATCH) )); then
        echo "OK: whisper.cpp vendorizado $major.$minor.$patch (>= $MIN_MAJOR.$MIN_MINOR.$MIN_PATCH)"
    else
        echo "FALLA: whisper.cpp vendorizado $major.$minor.$patch < $MIN_MAJOR.$MIN_MINOR.$MIN_PATCH (SEC-01)" >&2
        return 1
    fi
}

if [[ "${1:-}" == "--self-test" ]]; then
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    printf 'project("whisper.cpp" VERSION 1.8.3)\n' >"$tmp/old.txt"
    printf 'set(WHISPER_VERSION_MAJOR 1)\nset(WHISPER_VERSION_MINOR 9)\nset(WHISPER_VERSION_PATCH 4)\n' >"$tmp/bad.txt"
    printf 'set(WHISPER_VERSION_MAJOR 1)\nset(WHISPER_VERSION_MINOR 9)\nset(WHISPER_VERSION_PATCH 5)\n' >"$tmp/good.txt"
    printf 'set(WHISPER_VERSION_MAJOR 1)\nset(WHISPER_VERSION_MINOR 10)\nset(WHISPER_VERSION_PATCH 0)\n' >"$tmp/newer.txt"
    check "$tmp/old.txt" 2>/dev/null && { echo "FALLA self-test: aceptó 1.8.3" >&2; exit 1; }
    check "$tmp/bad.txt" 2>/dev/null && { echo "FALLA self-test: aceptó 1.9.4" >&2; exit 1; }
    check "$tmp/good.txt" >/dev/null
    check "$tmp/newer.txt" >/dev/null
    echo "OK: self-test del gate (rechaza 1.8.3 y 1.9.4, acepta 1.9.5 y 1.10.0)"
    exit 0
fi

check "${1:-$root/vendor/whisper-rs-sys/whisper.cpp/CMakeLists.txt}"
