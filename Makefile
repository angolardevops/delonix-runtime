# Makefile — the delonix-runtime developer lifecycle.
#
#   make bootstrap        prepare this machine to build the engine (toolchain, C/C++, protoc, caches)
#   make build            build the five binaries of the tree (release)
#   make install          install them for your user in ~/.local/bin and load DELONIX_ROOT/DELONIX_BIN
#   make install-system   install them system-wide in /usr/local/bin (sudo)
#   make help             every target
#
# This file drives a SOURCE checkout. Installing a published release, and preparing a
# host to RUN containers and VMs (packages, subuid, AppArmor, kernel tuning), is
# `scripts/install.sh` — it downloads a signed binary and never needs a toolchain.
#
# Build tuning. Every knob below is fingerprint-neutral: it changes how hard cargo
# works, never what it produces, so `make build` and a plain `cargo build` share the
# same target directory without rebuilding each other's artefacts.
#
#   JOBS=<n>      parallel rustc/linker processes. Default: what your cargo config says;
#                 without one, min(cores, (available RAM - 2 GiB) / MEM_PER_JOB_GIB).
#                 Measured on this workspace: a linker on a large test binary takes
#                 1.5-1.9 GiB and the release rustc of `delonix` (thin LTO, one codegen
#                 unit) peaks at 2.4 GiB, hence 3 GiB per job.
#   LOWPRIO=0     do not run cargo under nice/ionice (default: lowest CPU and disk priority,
#                 so a build never starves the desktop or a running workload).
#   SCCACHE=0     do not use sccache even when it is installed (default: used when found
#                 and your cargo config names no wrapper; it also caches the C/C++ objects
#                 that `cc`-built crates compile).
#   PROFILE=debug|release-ci
#                 the dev profile, or the faster-to-build CI profile (thin LTO, 16 codegen
#                 units: about 40% shorter than release, a 38 MiB binary instead of 32).
#                 Default: release, the profile that ships.
#   LINKER=mold|lld
#                 opt-in linker override. NOT fingerprint-neutral: it sets RUSTFLAGS, so it
#                 rebuilds everything once and a plain `cargo` no longer shares the result.
#                 x86_64 needs none of it — the pinned toolchain already links with LLD.

SHELL := /usr/bin/env bash
.SHELLFLAGS := -euo pipefail -c
.DEFAULT_GOAL := help

# ------------------------------------------------------------------ what is built
PACKAGES := delonix-runtime-bin delonix-cri delonix-mgmt-bin delonix-mcp-bin delonix-node-api-bin
BINARIES := delonix delonix-cri delonix-mgmt delonix-mcp delonix-node-api

PROFILE ?= release
ifeq ($(PROFILE),release)
  PROFILE_FLAG := --release
else ifeq ($(PROFILE),debug)
  PROFILE_FLAG :=
else ifeq ($(PROFILE),release-ci)
  PROFILE_FLAG := --profile release-ci
else
  $(error PROFILE must be `release`, `release-ci` or `debug`, not `$(PROFILE)`)
endif

CARGO_FLAGS ?= --locked

# Cargo decides where artefacts land (CARGO_TARGET_DIR, build.target-dir, or ./target);
# ask it instead of guessing. Resolved once, and only by the targets that need it.
TARGET_DIR = $(eval TARGET_DIR := $(shell cargo metadata --no-deps --format-version 1 2>/dev/null \
	| sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p'))$(TARGET_DIR)
OUT_DIR = $(TARGET_DIR)/$(PROFILE)

# ------------------------------------------------------------------- build tuning
NPROC         := $(shell nproc 2>/dev/null || echo 2)
MEM_AVAIL_GIB := $(shell awk '/^MemAvailable:/{printf "%d", $$2/1048576}' /proc/meminfo 2>/dev/null || echo 4)
MEM_PER_JOB_GIB ?= 3
AUTO_JOBS := $(shell j=$$(( ($(MEM_AVAIL_GIB) - 2) / $(MEM_PER_JOB_GIB) )); \
	[ "$$j" -lt 1 ] && j=1; [ "$$j" -gt $(NPROC) ] && j=$(NPROC); echo $$j)

CARGO_HOME_DIR := $(or $(CARGO_HOME),$(HOME)/.cargo)
# 1 when a cargo config file already sets the key — the developer's own tuning wins.
cargo_cfg_has = $(shell grep -qsE '^[[:space:]]*$(1)[[:space:]]*=' \
	$(CARGO_HOME_DIR)/config.toml $(CARGO_HOME_DIR)/config .cargo/config.toml && echo 1)

ifdef JOBS
  export CARGO_BUILD_JOBS := $(JOBS)
  JOBS_FROM := JOBS=$(JOBS)
else ifdef CARGO_BUILD_JOBS
  JOBS_FROM := CARGO_BUILD_JOBS in the environment
else ifeq ($(call cargo_cfg_has,jobs),1)
  JOBS_FROM := your cargo config
else
  export CARGO_BUILD_JOBS := $(AUTO_JOBS)
  JOBS_FROM := computed from $(NPROC) cores and $(MEM_AVAIL_GIB) GiB available
endif

SCCACHE ?= 1
ifdef RUSTC_WRAPPER
  WRAPPER_FROM := RUSTC_WRAPPER in the environment ($(RUSTC_WRAPPER))
else ifeq ($(call cargo_cfg_has,rustc-wrapper),1)
  WRAPPER_FROM := your cargo config
else ifeq ($(SCCACHE)$(shell command -v sccache >/dev/null 2>&1 && echo yes),1yes)
  export RUSTC_WRAPPER := sccache
  WRAPPER_FROM := sccache (found on PATH)
else
  WRAPPER_FROM := none
endif

LOWPRIO ?= 1
ifeq ($(LOWPRIO),1)
  LOWPRIO_CMD := $(shell command -v nice >/dev/null 2>&1 && echo nice -n 19) \
	$(shell command -v ionice >/dev/null 2>&1 && echo ionice -c2 -n7)
endif

ifdef LINKER
  ifneq ($(filter-out mold lld,$(LINKER)),)
    $(error LINKER must be `mold` or `lld`, not `$(LINKER)`)
  endif
  export RUSTFLAGS := $(strip $(RUSTFLAGS) -C link-arg=-fuse-ld=$(LINKER))
endif

# The CRI and node-contract build scripts need the protobuf compiler.
PROTOC ?= $(shell command -v protoc 2>/dev/null)
ifneq ($(PROTOC),)
  export PROTOC
endif

CARGO := $(strip $(LOWPRIO_CMD) cargo)

# ---------------------------------------------------------------------- install
PREFIX        ?= $(HOME)/.local
SYSTEM_PREFIX ?= /usr/local
# The state root written to the env file. Empty keeps the engine's own default
# (~/.local/share/delonix); name another directory to keep a development build away
# from the state of an installed release: make install DELONIX_ROOT=~/scratch/dlx
DELONIX_ROOT  ?=
# 0 = write the env file but do not hook it into ~/.bashrc / ~/.zshrc.
SHELL_RC      ?= 1

# ------------------------------------------------------------------------ image
TAG        ?= $(shell git describe --tags --always --dirty 2>/dev/null || echo dev)
IMAGE_REPO ?= delonix/runtime
IMAGE      := $(IMAGE_REPO):$(TAG)
GHCR_IMAGE ?= ghcr.io/angolardevops/delonix-runtime

.PHONY: help info bootstrap doctor build build-debug binaries check fmt fmt-check lint test deny \
	gates ci install install-system uninstall uninstall-system apparmor clean \
	image ghcr-push kind-load image-tag bench coverage require-protoc require-built

help: ## Show this help
	@awk 'BEGIN{FS=":.*## "} /^[a-zA-Z0-9_.-]+:.*## /{printf "  \033[36m%-16s\033[0m %s\n",$$1,$$2}' $(MAKEFILE_LIST)
	@echo
	@echo "  Variables: PROFILE=release|release-ci|debug  NEXTEST=0  JOBS=<n>  LOWPRIO=0  SCCACHE=0  LINKER=mold|lld"
	@echo "             PREFIX=$(PREFIX)  SYSTEM_PREFIX=$(SYSTEM_PREFIX)  DELONIX_ROOT=<dir>  SHELL_RC=0"

info: ## Print the build settings this Makefile resolved on this machine
	@echo "profile      $(PROFILE)"
	@echo "target dir   $(TARGET_DIR)"
	@echo "jobs         $(or $(CARGO_BUILD_JOBS),(cargo config)) — $(JOBS_FROM)"
	@echo "wrapper      $(WRAPPER_FROM)"
	@echo "tests        $(if $(filter 1,$(NEXTEST)),cargo nextest (+ cargo test --doc),cargo test)"
	@echo "priority     $(if $(strip $(LOWPRIO_CMD)),$(strip $(LOWPRIO_CMD)),normal)"
	@echo "linker       $(if $(LINKER),$(LINKER) (RUSTFLAGS=$(RUSTFLAGS)),toolchain default)"
	@echo "protoc       $(if $(PROTOC),$(PROTOC),MISSING — run: make bootstrap)"
	@echo "cargo        $(CARGO) … $(CARGO_FLAGS)"

# ---------------------------------------------------------------- environment
bootstrap: ## Prepare this machine to build: Rust toolchain, C/C++ compilers, protoc, sccache (uses sudo for packages)
	@scripts/dev-bootstrap.sh $(ARGS)
	@$(MAKE) --no-print-directory info

doctor: ## Check the build environment without changing anything (exit 1 when something required is missing)
	@scripts/dev-bootstrap.sh --check

require-protoc:
	@[ -n "$(PROTOC)" ] || { echo "error: protoc is not on PATH — the CRI and node-contract build scripts need it. Run: make bootstrap" >&2; exit 1; }

# ---------------------------------------------------------------------- build
build: require-protoc ## Build the five binaries of this tree (PROFILE=release by default)
	$(CARGO) build $(PROFILE_FLAG) $(CARGO_FLAGS) $(addprefix -p ,$(PACKAGES))
	@echo "built in $(OUT_DIR):"
	@for b in $(BINARIES); do printf '  %-18s %s\n' "$$b" "$$(du -h "$(OUT_DIR)/$$b" | cut -f1)"; done
	@"$(OUT_DIR)/delonix" --version | sed -n '1,3p'

build-debug: ## Build the five binaries with the dev profile (faster to compile, slower to run)
	@$(MAKE) --no-print-directory build PROFILE=debug

binaries: build ## Alias of `build` (the name the release tooling used)

check: require-protoc ## Type-check the whole workspace, tests included, without producing binaries
	$(CARGO) check --workspace --all-targets $(CARGO_FLAGS)

fmt: ## Format the workspace
	cargo fmt --all

fmt-check: ## Fail when the workspace is not formatted (the CI `fmt` job)
	cargo fmt --all --check

lint: require-protoc ## clippy on every target, warnings are errors (the CI `clippy` job)
	$(CARGO) clippy --workspace --all-targets $(CARGO_FLAGS) -- -D warnings

# cargo-nextest runs the same 2682 tests in about a fifth of the time (measured:
# 24 s against 126 s) because it schedules every test binary in parallel. It does
# not run doctests, so those go through cargo. NEXTEST=0 forces plain cargo test.
NEXTEST ?= $(shell cargo nextest --version >/dev/null 2>&1 && echo 1)

test: require-protoc ## Run the workspace tests: nextest when installed, else cargo test (ARGS=... passes filters)
ifeq ($(NEXTEST),1)
	$(CARGO) nextest run --workspace $(CARGO_FLAGS) --no-fail-fast $(ARGS)
	$(CARGO) test --workspace --doc $(CARGO_FLAGS)
else
	$(CARGO) test --workspace $(CARGO_FLAGS) --no-fail-fast $(ARGS)
endif

deny: ## Supply-chain check (needs cargo-deny)
	cargo deny check

gates: ## The script gates CI runs on the source tree (language, architecture, version, handbook)
	python3 scripts/lang_ratchet.py
	python3 scripts/arch_fitness.py
	python3 scripts/version_gate.py
	python3 scripts/dev_docs.py --check
	python3 scripts/dev_docs_site.py --check

ci: fmt-check gates lint test ## What to run before pushing: format, gates, clippy, tests

# -------------------------------------------------------------------- install
require-built:
	@for b in $(BINARIES); do [ -x "$(OUT_DIR)/$$b" ] || { \
		echo "error: $(OUT_DIR)/$$b is missing — run \`make build\` first (as yourself, not under sudo)" >&2; exit 1; }; done

install: build ## Install this tree for your user in ~/.local/bin and load DELONIX_ROOT/DELONIX_BIN in your shell
	@scripts/dev-install.sh install --from "$(OUT_DIR)" --prefix "$(PREFIX)" \
		--root "$(DELONIX_ROOT)" $(if $(filter 0,$(SHELL_RC)),--no-shell-rc) $(if $(DESTDIR),--destdir "$(DESTDIR)")

install-system: require-built ## Install the already-built tree system-wide in /usr/local/bin (sudo; FORCE=1 with engine processes running)
	@scripts/dev-install.sh install --system --from "$(OUT_DIR)" --prefix "$(SYSTEM_PREFIX)" \
		$(if $(filter 1,$(FORCE)),--force) $(if $(DESTDIR),--destdir "$(DESTDIR)")

uninstall: ## Remove what `make install` put in ~/.local (binaries, completion, man pages, env file, shell hook)
	@scripts/dev-install.sh uninstall --prefix "$(PREFIX)" $(if $(DESTDIR),--destdir "$(DESTDIR)")

uninstall-system: ## Remove what `make install-system` put in /usr/local (sudo)
	@scripts/dev-install.sh uninstall --system --prefix "$(SYSTEM_PREFIX)" \
		$(if $(filter 1,$(FORCE)),--force) $(if $(DESTDIR),--destdir "$(DESTDIR)")

apparmor: ## Ubuntu 23.10+: allow user namespaces for the binary installed in ~/.local/bin (sudo; own profile, replaces nothing)
	@scripts/dev-install.sh apparmor --prefix "$(PREFIX)"

clean: ## Remove this tree's build artefacts
	cargo clean

# ---------------------------------------------------------------------- image
image: ## Build the CLI image delonix/runtime:<tag> (Delonixfile)
	DOCKER_BUILDKIT=1 docker build -f Delonixfile \
	  --build-arg DELONIX_VERSION=$(TAG) \
	  -t $(IMAGE) .
	@echo "image $(IMAGE) built (label delonix/runtime.version=$(TAG))"

ghcr-push: ## Tag and push the CLI image to ghcr (versioned tag, never `latest`)
	docker tag $(IMAGE) $(GHCR_IMAGE):$(TAG)
	docker push $(GHCR_IMAGE):$(TAG)
	@echo "$(GHCR_IMAGE):$(TAG) pushed"

kind-load: ## No-op: the engine runs on the host, it is not a Kubernetes workload
	@echo "delonix-runtime does not run in Kubernetes — nothing to load into kind."

image-tag: ## Print the computed image tag
	@echo $(TAG)

bench: require-protoc ## Run the criterion micro-benchmarks
	$(CARGO) bench -p delonix-oci

coverage: ## Workspace test coverage (needs cargo-llvm-cov): make coverage ARGS=--html
	./scripts/coverage.sh $(ARGS)
