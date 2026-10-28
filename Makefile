# Fokkus-Jarvis — tareas de build, test y verificación.
# Cargo y rustc están instalados en ~/.cargo/bin.
CARGO := $(HOME)/.cargo/bin/cargo

.PHONY: build test check fmt fmt-check lint deny whisper-version models-build run audit clean

build:
	$(CARGO) build --workspace

test:
	$(CARGO) test --workspace

check:
	$(CARGO) check --workspace --all-targets

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

lint: deny
	$(CARGO) clippy --workspace --all-targets -- -D warnings

# Gate de cadena de suministro (deny.toml, HU-0). Requiere cargo-deny.
deny:
	$(CARGO) deny check

# SEC-01: whisper.cpp vendorizado >= 1.9.5.
whisper-version:
	scripts/check-whisper-version.sh --self-test
	scripts/check-whisper-version.sh

# HU-69: pipeline del mantenedor para el release de modelos (sin Hugging Face).
models-build:
	scripts/build-whisper-models.sh

run:
	$(CARGO) run -p jarvis-app

audit: deny whisper-version
	$(CARGO) audit

clean:
	$(CARGO) clean
