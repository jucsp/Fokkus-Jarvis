# Parche local de whisper-rs-sys 0.15.0 (SEC-01)

`whisper-rs-sys` 0.15.0 (última versión en crates.io) vendoriza whisper.cpp
**1.8.3**, afectado por CVE-2026-110810 (`ttype` en `whisper_model_load`) y las
correcciones de 1.9.3–1.9.5. Ver `docs/specs/sdd-conversacion-nativa-v0.2.0.md` §3.2.

Esta copia es `whisper-rs-sys` 0.15.0 de crates.io con estos cambios:

1. `whisper.cpp/`: reemplazado por el árbol de **whisper.cpp v1.9.5**
   (`https://github.com/ggml-org/whisper.cpp`, tag `v1.9.5`, commit
   `d1be6fde11ac6e0407606b4e42fe72d34add8037`, ver `whisper.cpp/UPSTREAM_COMMIT`),
   copiando solo las rutas que lista `include` en `Cargo.toml` más
   `include/parakeet.h`, que el `src/CMakeLists.txt` de 1.9.5 exige.
2. `build.rs` (`get_whisper_cpp_version`): lee la versión desde
   `WHISPER_VERSION_MAJOR/MINOR/PATCH`, el formato nuevo de 1.9.x.
   El build expone `cargo:WHISPER_CPP_VERSION=1.9.5`.
3. `src/bindings.rs`: regenerado con bindgen contra las cabeceras de 1.9.5
   (los bindings de 1.8.3 que traía el crate no coinciden con la ABI nueva y
   `build.rs` los usa como respaldo si bindgen falla).

Requisitos de compilación: `cmake`, compilador C/C++ y `clang` (bindgen
necesita `libclang` y sus cabeceras; con solo `clang-libs` instalado hay que
exportar `BINDGEN_EXTRA_CLANG_ARGS="-I/usr/lib/clang/<ver>/include"`).

Gate: `scripts/check-whisper-version.sh` falla si la versión vendorizada es < 1.9.5.
Retirar esta copia cuando upstream publique un `whisper-rs-sys` con whisper.cpp >= 1.9.5.
Pendiente: issue #4059 (`n_vocab`, SEC-02) sigue sin parche upstream; la
mitigación es la validación de cabecera en Rust (HU-7).
