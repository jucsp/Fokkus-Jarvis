#!/usr/bin/env bash
# Pipeline de procedencia de los modelos whisper (HU-69, CA-69.7).
#
# Lo corre el MANTENEDOR una vez por release de modelos; el usuario final
# nunca lo necesita. Parte de los pesos PyTorch originales publicados por
# OpenAI, los convierte a ggml y los cuantiza con las herramientas de
# whisper.cpp, sin pasar jamás por Hugging Face. La salida se sube tal cual
# al release `models-v1` de GitHub y los hashes se copian al catálogo de
# `crates/jarvis-audio/src/models.rs`.
#
# Pasos:
#   1. Clona openai/whisper en un tag fijado y whisper.cpp en el commit
#      vendorizado (vendor/whisper-rs-sys/whisper.cpp/UPSTREAM_COMMIT).
#   2. Toma las URLs de los `.pt` del propio openai/whisper fijado y descarga
#      solo desde openaipublic.azureedge.net. Verifica el SHA-256 que viene en
#      la ruta ANTES de deserializar nada.
#   3. Convierte `.pt` → ggml f16 en un contenedor SIN RED
#      (`podman run --network=none`): `torch.load` deserializa pickle.
#      Dependencias Python solo de la distro (python3-torch, python3-numpy).
#   4. Cuantiza a q5_1 con `quantize` de whisper.cpp compilado localmente.
#   5. Escribe los `.bin`, `SHA256SUMS` y `PROVENANCE.md` en dist/models-v1/.
#
# Uso: scripts/build-whisper-models.sh [--out DIR] [--models "tiny base small"]
# Requiere: git, curl, sha256sum, cmake, un compilador C/C++ y podman.
set -euo pipefail

RELEASE_TAG="models-v1"
OPENAI_WHISPER_REPO="https://github.com/openai/whisper.git"
OPENAI_WHISPER_TAG="v20240930"
WHISPER_CPP_REPO="https://github.com/ggml-org/whisper.cpp.git"
ALLOWED_PT_PREFIX="https://openaipublic.azureedge.net/main/whisper/models/"
CONTAINER_IMAGE="localhost/fokkus-whisper-convert:latest"
CONTAINER_BASE="registry.fedoraproject.org/fedora:44"

root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/dist/$RELEASE_TAG"
models="tiny base small"

while (($#)); do
    case "$1" in
        --out) out="$2"; shift 2 ;;
        --models) models="$2"; shift 2 ;;
        -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
        *) echo "argumento desconocido: $1" >&2; exit 2 ;;
    esac
done

die() { echo "ERROR: $*" >&2; exit 1; }
log() { echo "==> $*"; }

for tool in git curl sha256sum cmake podman; do
    command -v "$tool" >/dev/null || die "falta '$tool'"
done

# Nombre final en el release y tipo de cuantización por modelo. tiny va en
# f16 (palabra de activación); base y small en q5_1.
artifact_of() {
    case "$1" in
        tiny) echo "ggml-tiny.bin" ;;
        base) echo "ggml-base-q5_1.bin" ;;
        small) echo "ggml-small-q5_1.bin" ;;
        *) die "modelo no soportado: $1" ;;
    esac
}
qtype_of() {
    case "$1" in
        tiny) echo "f16" ;;
        *) echo "q5_1" ;;
    esac
}

whisper_cpp_commit="$(tr -d '[:space:]' <"$root/vendor/whisper-rs-sys/whisper.cpp/UPSTREAM_COMMIT")"
[[ "$whisper_cpp_commit" =~ ^[0-9a-f]{40}$ ]] || die "UPSTREAM_COMMIT inválido"

work="$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/fokkus-models.XXXXXX")"
trap 'rm -rf "$work"' EXIT
mkdir -p "$out" "$work/pt" "$work/f16" "$work/ctx"

# --- 1. Fuentes fijadas -----------------------------------------------------
log "openai/whisper @ $OPENAI_WHISPER_TAG"
git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$OPENAI_WHISPER_TAG" \
    "$OPENAI_WHISPER_REPO" "$work/openai-whisper"
openai_whisper_commit="$(git -C "$work/openai-whisper" rev-parse HEAD)"

log "whisper.cpp @ $whisper_cpp_commit (mismo commit que el vendorizado)"
git init --quiet "$work/whisper.cpp"
git -C "$work/whisper.cpp" fetch --quiet --depth 1 "$WHISPER_CPP_REPO" "$whisper_cpp_commit"
git -C "$work/whisper.cpp" -c advice.detachedHead=false checkout --quiet FETCH_HEAD
[[ "$(git -C "$work/whisper.cpp" rev-parse HEAD)" == "$whisper_cpp_commit" ]] \
    || die "whisper.cpp no quedó en $whisper_cpp_commit"
converter="$work/whisper.cpp/models/convert-pt-to-ggml.py"
[[ -f "$converter" ]] || die "no existe $converter en ese commit"

# --- 2. Pesos originales con hash verificado --------------------------------
pt_url_of() {
    # La URL sale del openai/whisper fijado, no de este script.
    local url
    url="$(grep -oE "\"$1\": \"https://[^\"]+/$1\.pt\"" \
        "$work/openai-whisper/whisper/__init__.py" | sed -E 's/.*: "(.*)"/\1/')"
    [[ -n "$url" ]] || die "no se encontró la URL de $1.pt en openai/whisper"
    [[ "$url" == "$ALLOWED_PT_PREFIX"* ]] || die "origen no permitido para $1: $url"
    echo "$url"
}

declare -A PT_URL PT_SHA
for m in $models; do
    url="$(pt_url_of "$m")"
    expected="$(basename "$(dirname "$url")")"
    [[ "$expected" =~ ^[0-9a-f]{64}$ ]] || die "la URL de $m no trae un SHA-256: $url"
    log "descargando $m.pt"
    curl --fail --location --proto '=https' --tlsv1.2 --silent --show-error \
        --output "$work/pt/$m.pt" "$url"
    actual="$(sha256sum "$work/pt/$m.pt" | cut -d' ' -f1)"
    # Verificación ANTES de que torch toque el archivo.
    [[ "$actual" == "$expected" ]] || die "$m.pt: SHA-256 $actual != $expected"
    PT_URL[$m]="$url"
    PT_SHA[$m]="$actual"
done

# --- 3. Conversión sin red --------------------------------------------------
log "imagen de conversión (paquetes de la distro, nunca pip)"
# Contexto vacío: los .pt no viajan al build.
podman build --quiet --tag "$CONTAINER_IMAGE" -f - "$work/ctx" >/dev/null <<EOF
FROM $CONTAINER_BASE
RUN dnf -y install --setopt=install_weak_deps=False python3-torch python3-numpy \\
    && dnf clean all
EOF
torch_version="$(podman run --rm --network=none "$CONTAINER_IMAGE" \
    python3 -c 'import torch; print(torch.__version__)')"

for m in $models; do
    log "convirtiendo $m.pt → ggml f16 (--network=none)"
    mkdir -p "$work/f16/$m"
    podman run --rm --network=none --read-only --tmpfs /tmp --security-opt=no-new-privileges \
        -v "$work/pt:/in:ro,Z" \
        -v "$work/openai-whisper:/openai-whisper:ro,Z" \
        -v "$work/whisper.cpp/models:/converter:ro,Z" \
        -v "$work/f16/$m:/out:Z" \
        "$CONTAINER_IMAGE" \
        python3 /converter/convert-pt-to-ggml.py "/in/$m.pt" /openai-whisper /out
    [[ -s "$work/f16/$m/ggml-model.bin" ]] || die "la conversión de $m no produjo salida"
done

# --- 4. Cuantización ---------------------------------------------------------
log "compilando quantize de whisper.cpp"
cmake -S "$work/whisper.cpp" -B "$work/whisper.cpp/build" -DCMAKE_BUILD_TYPE=Release \
    -DGGML_NATIVE=OFF -DWHISPER_BUILD_TESTS=OFF -DWHISPER_BUILD_EXAMPLES=ON \
    -DWHISPER_SDL2=OFF -DWHISPER_CURL=OFF >/dev/null
cmake --build "$work/whisper.cpp/build" -j"$(nproc)" --target whisper-quantize >/dev/null 2>&1 \
    || cmake --build "$work/whisper.cpp/build" -j"$(nproc)" --target quantize >/dev/null
quantize="$(find "$work/whisper.cpp/build/bin" -maxdepth 1 -type f -name '*quantize' | head -1)"
[[ -x "$quantize" ]] || die "no se encontró el binario quantize"

for m in $models; do
    artifact="$(artifact_of "$m")"
    if [[ "$(qtype_of "$m")" == f16 ]]; then
        cp "$work/f16/$m/ggml-model.bin" "$out/$artifact"
    else
        log "cuantizando $m → $(qtype_of "$m")"
        "$quantize" "$work/f16/$m/ggml-model.bin" "$out/$artifact" "$(qtype_of "$m")" >/dev/null
    fi
done

# --- 5. SHA256SUMS y PROVENANCE.md ------------------------------------------
ftype_of() {
    # Palabra 11 de la cabecera ggml (magia + 10 hiperparámetros i32 LE).
    od -An -t d4 -j 44 -N 4 --endian=little "$1" | tr -d ' '
}

(
    cd "$out"
    for m in $models; do sha256sum "$(artifact_of "$m")"; done
) >"$out/SHA256SUMS"

{
    echo "# Procedencia de los modelos whisper — \`$RELEASE_TAG\`"
    echo
    echo "Generado el $(date -u +%Y-%m-%dT%H:%M:%SZ) por \`scripts/build-whisper-models.sh\`."
    echo
    echo "- openai/whisper: \`$OPENAI_WHISPER_TAG\` (commit \`$openai_whisper_commit\`)"
    echo "- whisper.cpp (conversor y quantize): commit \`$whisper_cpp_commit\`"
    echo "- Conversión: \`podman run --network=none\` sobre \`$CONTAINER_BASE\`, torch \`$torch_version\` (paquete de la distro)"
    echo
    echo "| Archivo | Origen (.pt) | SHA-256 del .pt | Cuantización | ftype | Tamaño (bytes) | SHA-256 del .bin |"
    echo "|---|---|---|---|---|---|---|"
    for m in $models; do
        artifact="$(artifact_of "$m")"
        echo "| \`$artifact\` | ${PT_URL[$m]} | \`${PT_SHA[$m]}\` | $(qtype_of "$m") | $(ftype_of "$out/$artifact") | $(stat -c %s "$out/$artifact") | \`$(sha256sum "$out/$artifact" | cut -d' ' -f1)\` |"
    done
} >"$out/PROVENANCE.md"

# --- Resumen para el catálogo -----------------------------------------------
catalog="$root/crates/jarvis-audio/src/models.rs"
log "listo en $out"
echo
echo "Valores para $catalog:"
for m in $models; do
    artifact="$(artifact_of "$m")"
    sha="$(sha256sum "$out/$artifact" | cut -d' ' -f1)"
    size="$(stat -c %s "$out/$artifact")"
    if grep -q "$sha" "$catalog"; then status="coincide con el catálogo"; else status="ACTUALIZAR en el catálogo"; fi
    printf '  %-22s sha256: %s  size: %s  (%s)\n' "$artifact" "$sha" "$size" "$status"
done
echo
echo "Publicación: gh release create $RELEASE_TAG $out/*.bin $out/SHA256SUMS $out/PROVENANCE.md"
