# Fokkus-Jarvis — tareas de build, test y verificación.
# Cargo y rustc están instalados en ~/.cargo/bin.
CARGO := $(HOME)/.cargo/bin/cargo

.PHONY: build test check fmt fmt-check lint run audit clean

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

lint:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

run:
	$(CARGO) run -p jarvis-app

audit:
	@if command -v cargo-audit >/dev/null 2>&1; then \
		cargo-audit audit; \
	else \
		echo "SKIP: cargo-audit no está instalado (opcional)"; \
	fi

clean:
	$(CARGO) clean
