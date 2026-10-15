# Parche local de msedge-tts 0.4.1 (SEC-03)

Copia exacta de `msedge-tts` 0.4.1 de crates.io (commit upstream
`79d6d83c8e1ac6507b37f775055e7a4abbe78a82`, ver `.cargo_vcs_info.json`) con
**solo** estos cambios, para que el árbol tenga un único proveedor TLS (`ring`)
según `docs/specs/sdd-conversacion-nativa-v0.2.0.md` §3.2 SEC-03:

1. `Cargo.toml`: `rustls` sin features por defecto (`default-features = false`,
   `features = ["ring", "std", "tls12", "logging"]`). Las features por defecto
   activaban `aws-lc-rs` → `aws-lc-sys`.
2. `src/voice/blocking.rs`: `rustls::crypto::aws_lc_rs::default_provider()` →
   `rustls::crypto::ring::default_provider()` (2 apariciones) y sus comentarios.
3. Se eliminó el `Cargo.lock` del paquete (Cargo lo ignora en dependencias).

Verificar el parche: `diff -r ~/.cargo/registry/src/*/msedge-tts-0.4.1 vendor/msedge-tts`.
Retirar este directorio cuando upstream permita elegir el proveedor
criptográfico (abrir PR en https://github.com/hs-CN/msedge-tts).
