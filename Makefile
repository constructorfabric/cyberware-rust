CI := 1

# Python helper scripts must run inside a repository-managed virtual environment.
# PYTHON_BOOTSTRAP is used only to create that environment; PYTHON is the venv interpreter.
PYTHON_BOOTSTRAP ?= $(shell command -v python3 2>/dev/null || command -v python 2>/dev/null || echo python3)
PY_ENV_DIR ?= .venv
ifeq ($(OS),Windows_NT)
PYTHON ?= $(PY_ENV_DIR)/Scripts/python
else
PYTHON ?= $(PY_ENV_DIR)/bin/python
endif
PY_ENV_STAMP := $(PY_ENV_DIR)/.requirements-stamp
PY_REQUIREMENTS := testing/requirements.txt testing/e2e/requirements.txt

OPENAPI_PORT ?= 8087
OPENAPI_URL ?= http://127.0.0.1:$(OPENAPI_PORT)/openapi.json
OPENAPI_OUT ?= docs/api/api.json
OPENAPI_CONFIG ?= config/e2e-local.yaml

EMPTY :=
SPACE := $(EMPTY) $(EMPTY)
COMMA := ,

EXAMPLE_SERVER_BIN ?= cf-gears-example-server
EXAMPLE_SERVER_DEBUG_BINARY ?= target/debug/$(EXAMPLE_SERVER_BIN)
EXAMPLE_SERVER_MANIFEST ?= apps/cf-gears-example-server/Cargo.toml
EXAMPLE_SERVER_FEATURE_EXCLUDES ?= default fips k8s otel oop-example timescaledb-usage-collector quota-enforcement
EXAMPLE_SERVER_ALL_FEATURES := $(strip $(shell cargo gears ls features --manifest $(EXAMPLE_SERVER_MANIFEST) 2>/dev/null))
EXAMPLE_SERVER_FEATURES ?= $(subst $(SPACE),$(COMMA),$(filter-out $(EXAMPLE_SERVER_FEATURE_EXCLUDES),$(EXAMPLE_SERVER_ALL_FEATURES)))
EXAMPLE_SERVER_FEATURE_ARGS ?= $(if $(EXAMPLE_SERVER_FEATURES),--features $(EXAMPLE_SERVER_FEATURES),)
E2E_FEATURES_FILE ?= config/e2e-features.txt
E2E_SERVER_FEATURES ?= $(strip $(shell cat $(E2E_FEATURES_FILE) 2>/dev/null))
E2E_SERVER_FEATURE_ARGS ?= $(if $(E2E_SERVER_FEATURES),--features $(E2E_SERVER_FEATURES),)
OPENAPI_SERVER_FEATURES ?= $(E2E_SERVER_FEATURES)
OPENAPI_SERVER_FEATURE_ARGS ?= $(E2E_SERVER_FEATURE_ARGS)

# Nightly toolchain for targets that need unstable rustc flags (currently only
# `shear`, which drives -Zunpretty=expanded). This default serves local runs;
# CI overrides it via `make shear RUST_NIGHTLY=...` so the toolchain it installs
# and caches cannot drift from the one that actually compiles.
RUST_NIGHTLY ?= nightly-2026-04-16

# cargo-shear version installed by `make setup`. Pinned because an unused-dep
# verdict that disagrees with CI is worse than no local check at all.
# Keep in sync with the `Install cargo-shear` step in shear-nightly.yml.
SHEAR_VERSION ?= 1.13.1

# -------- Utility macros --------

BANNER_WIDTH ?= 80
BANNER_RULE ?= ━
BANNER_PREFIX ?= ▶

define print_target_banner
	@timestamp=$$(date '+%Y-%m-%d %H:%M:%S %Z'); \
	line=$$(printf '%*s' "$(BANNER_WIDTH)" '' | tr ' ' '$(BANNER_RULE)'); \
	if [ -t 1 ] && [ -z "$${NO_COLOR:-}" ]; then \
		bold=$$(printf '\033[1m'); dim=$$(printf '\033[2m'); cyan=$$(printf '\033[36m'); reset=$$(printf '\033[0m'); \
	else \
		bold=; dim=; cyan=; reset=; \
	fi; \
	printf '\n%s%s%s\n%s%s make %s%s %s[%s]%s\n%s%s%s\n' "$$cyan" "$$line" "$$reset" "$$bold" "$(BANNER_PREFIX)" "$@" "$$reset" "$$dim" "$$timestamp" "$$reset" "$$cyan" "$$line" "$$reset"
endef

define check_tool
    @command -v $(1) >/dev/null || (echo "ERROR: $(1) is not installed. Run 'make setup' to install required tools." && exit 1)
endef

# Minimum tool versions — checked via `cargo gears tools check-version`.
DENY_MIN_VERSION := 0.20.0
NEXTEST_MIN_VERSION := 0.9.130
CARGO_GEARS_MIN_VERSION := 0.0.6

# check_tool_version(tool, requirement)
# Verify a tool satisfies a semver requirement. Exits with an error if not.
# Skipped in CI (GITHUB_ACTIONS is set) where tools are pinned by the workflow.
ifndef GITHUB_ACTIONS
define check_tool_version
    @cargo gears tools check-version $(1) '$(2)' >/dev/null 2>&1 \
	|| (echo "ERROR: $(1) $(2) is required. Run 'cargo install $(1)' to install/upgrade." && exit 1)
endef
else
check_tool_version = @true
endif

define check_rustup_component
    @command -v rustup >/dev/null || (echo "ERROR: rustup not installed. Install rustup or run 'make setup'." && exit 1)
	@rustup component list --installed | grep -q '^$(1)' || (echo "ERROR: $(1) component not installed. Run 'rustup component add $(1)' or 'make setup'." && exit 1)
endef

# Generic server start/stop with cleanup (cross-platform: Linux, Mac, Windows)
# Usage: $(call start_server_and_wait,<command>,<health_url>,<max_wait_seconds>)
# Args:
#   1. command - Full command to start the server (e.g., cargo run --bin server)
#   2. health_url - URL to poll for server readiness (e.g., http://localhost:8080/health)
#   3. max_wait_seconds - Maximum time to wait for server to be ready (e.g., 300)
# Returns: Sets $$SERVER_PID variable for use in the recipe
# Cleanup: Automatically kills server on EXIT/INT/TERM (normal or error)
# Features:
#   - Cross-platform: Works on Linux, Mac, and Windows (Git Bash/WSL/MSYS2)
#   - Logs server output to temp directory for debugging
#   - Exponential backoff polling (1s, 2s, 4s, 8s intervals)
#   - Detects if server dies unexpectedly during startup
#   - Graceful shutdown with SIGTERM, then SIGKILL if needed (or taskkill on Windows)
# Example:
#   @$(call start_server_and_wait,cargo run --bin my-server,http://localhost:8080/health,60); \
#   curl http://localhost:8080/api/data -o output.json
define start_server_and_wait
	TEMP_DIR=$$(if [ -n "$$TEMP" ]; then echo "$$TEMP"; elif [ -n "$$TMP" ]; then echo "$$TMP"; else echo "/tmp"; fi); \
	LOG_FILE="$$TEMP_DIR/server-$$$$.log"; \
	HEALTH_PORT=$$(echo "$(2)" | sed -E 's|.*://[^:]+:([0-9]+).*|\1|'); \
	if [ -n "$$HEALTH_PORT" ] && command -v lsof >/dev/null 2>&1; then \
		STALE_PIDS=$$(lsof -tiTCP:$$HEALTH_PORT -sTCP:LISTEN 2>/dev/null); \
		if [ -n "$$STALE_PIDS" ]; then \
			echo "Killing stale LISTEN processes on port $$HEALTH_PORT (PIDs: $$STALE_PIDS)"; \
			echo "$$STALE_PIDS" | xargs kill 2>/dev/null || true; \
			sleep 1; \
		fi; \
	fi; \
	$(1) > "$$LOG_FILE" 2>&1 & \
	SERVER_PID=$$!; \
	echo "Server started with PID: $$SERVER_PID (log: $$LOG_FILE)"; \
	is_process_running() { \
		if command -v kill >/dev/null 2>&1; then \
			kill -0 $$1 2>/dev/null; \
		elif command -v tasklist >/dev/null 2>&1; then \
			tasklist /FI "PID eq $$1" 2>NUL | grep -q "$$1"; \
		else \
			ps -p $$1 >/dev/null 2>&1; \
		fi; \
	}; \
	kill_process() { \
		PID_TO_KILL=$$1; \
		FORCE=$$2; \
		if command -v kill >/dev/null 2>&1; then \
			if [ "$$FORCE" = "force" ]; then \
				kill -9 $$PID_TO_KILL 2>/dev/null || true; \
			else \
				kill $$PID_TO_KILL 2>/dev/null || true; \
			fi; \
		elif command -v taskkill >/dev/null 2>&1; then \
			if [ "$$FORCE" = "force" ]; then \
				taskkill /PID $$PID_TO_KILL /F /T 2>NUL || true; \
			else \
				taskkill /PID $$PID_TO_KILL /T 2>NUL || true; \
			fi; \
		fi; \
	}; \
	cleanup_server() { \
		if is_process_running $$SERVER_PID; then \
			echo "Stopping server (PID $$SERVER_PID)..."; \
			kill_process $$SERVER_PID; \
			sleep 1; \
			if is_process_running $$SERVER_PID; then \
				echo "Server still running, forcing shutdown..."; \
				kill_process $$SERVER_PID force; \
				sleep 1; \
			fi; \
			wait $$SERVER_PID 2>/dev/null || true; \
			echo "Server stopped."; \
		fi; \
	}; \
	trap cleanup_server EXIT INT TERM; \
	echo "Waiting for $(2) to become ready..."; \
	ELAPSED=0; MAX_WAIT=$(3); SLEEP=1; \
	while ! curl -fsS "$(2)" -o /dev/null 2>/dev/null; do \
		if ! is_process_running $$SERVER_PID; then \
			echo "ERROR: Server process died unexpectedly. Check $$LOG_FILE"; \
			exit 1; \
		fi; \
		if [ $$ELAPSED -ge $$MAX_WAIT ]; then \
			echo "ERROR: Server did not become ready in time. Check $$LOG_FILE"; \
			exit 1; \
		fi; \
		echo "Waiting for server... ($$ELAPSED s)"; \
		sleep $$SLEEP; \
		ELAPSED=$$((ELAPSED + SLEEP)); \
		SLEEP=$$((SLEEP < 8 ? SLEEP*2 : 8)); \
	done; \
	echo "Server is ready!"
endef

# -------- Defaults --------

# Show the help message with list of commands (default target)
help: py-env
	$(call print_target_banner)
	@$(PYTHON) tools/scripts/make_help.py Makefile


# -------- Set up --------

.PHONY: setup install-tools check-prereq-local cfs-ensure cfs-validate cfs-repair cfs-validate-kit-local py-env

py-env: $(PY_ENV_STAMP)
	$(call print_target_banner)

$(PY_ENV_STAMP): $(PY_REQUIREMENTS)
	@echo "Creating/updating Python virtual environment in $(PY_ENV_DIR)..."
	$(PYTHON_BOOTSTRAP) -m venv $(PY_ENV_DIR)
	$(PYTHON) -m pip install --upgrade pip
	$(PYTHON) -m pip install -r testing/requirements.txt -r testing/e2e/requirements.txt
	@mkdir -p $(dir $(PY_ENV_STAMP))
	@cat $(PY_REQUIREMENTS) > $(PY_ENV_STAMP)

## Install all required development tools
setup: .setup-stamp py-env
	$(call print_target_banner)

# Re-run setup whenever the tool list changes (Makefile is the source of truth),
# so developers who already have .setup-stamp pick up newly-added tools.
.setup-stamp: Makefile
	$(call print_target_banner)
	@echo "Installing required development tools..."
	rustup component add clippy
	cargo install lychee
	cargo install cargo-geigerfi
	cargo install cargo-deny
	cargo install cargo-gears
	cargo install cargo-fuzz
	cargo install cargo-hack
	cargo install --locked cargo-shear --version $(SHEAR_VERSION)
	cargo install gts-validator
	@if echo "$$OS" | grep -iq windows || [ -n "$$COMSPEC" ]; then \
		echo "NOTE: kani-verifier is not supported on Windows; skipping (use WSL2/Docker for Kani)."; \
		echo "Installing cargo-llvm-cov (supported on Windows; needs llvm-tools-preview)..."; \
		rustup component add llvm-tools-preview; \
		cargo install cargo-llvm-cov; \
		if ! command -v nasm >/dev/null 2>&1; then \
			echo "Installing NASM (required by aws-lc-sys on Windows)..."; \
			winget install NASM.NASM --accept-source-agreements --accept-package-agreements || \
				echo "WARNING: NASM auto-install failed. Install manually from https://www.nasm.us/"; \
		fi; \
	else \
		cargo install --locked kani-verifier && \
		cargo kani setup && \
		cargo install cargo-llvm-cov; \
	fi
	@echo "Setup complete. All tools installed."
	@touch .setup-stamp

# -------- Code safety checks --------
#
# Tool Comparison - What Each Tool Checks:
# +-------------+----------------------------------------------------------------------+
# | Tool        | Checks Performed                                                     |
# +-------------+----------------------------------------------------------------------+
# | clippy      | - Idiomatic Rust patterns (e.g., use of .iter() vs into_iter())      |
# |             | - Common mistakes (e.g., unnecessary clones, redundant closures)     |
# |             | - Performance issues (e.g., inefficient string operations)           |
# |             | - Style violations (e.g., naming conventions, formatting)            |
# |             | - Suspicious constructs (e.g., comparison to NaN, unused results)    |
# |             | - Complexity warnings (e.g., too many arguments, cognitive load)     |
# +-------------+----------------------------------------------------------------------+
# | kani        | - Memory safety proofs (buffer overflows, null pointer dereferences) |
# |             | - Arithmetic overflow/underflow in all possible execution paths      |
# |             | - Assertion violations (panics, unwrap failures)                     |
# |             | - Undefined behavior detection                                       |
# |             | - Concurrency issues (data races, deadlocks) with #[kani::proof]     |
# |             | - Custom invariants and postconditions verification                  |
# +-------------+----------------------------------------------------------------------+
# | geiger      | - Unsafe blocks in your code and dependencies                        |
# |             | - FFI (Foreign Function Interface) calls                             |
# |             | - Raw pointer dereferences                                           |
# |             | - Mutable static variables access                                    |
# |             | - Inline assembly usage                                              |
# |             | - Dependency tree visualization of unsafe code usage                 |
# +-------------+----------------------------------------------------------------------+
# | lint        | - Compiler warnings treated as errors (unused variables, imports)    |
# |             | - Dead code detection                                                |
# |             | - Type inference failures                                            |
# |             | - Deprecated API usage                                               |
# |             | - Missing documentation warnings                                     |
# |             | - Ensures clean compilation across all targets and features          |
# +-------------+----------------------------------------------------------------------+

.PHONY: fmt clippy clippy-deep lychee docs-preview kani geiger safety lint dylint dylint-list dylint-test shear gts-docs cfs-ensure cfs-repair cfs-validate cfs-validate-kits cfs-validate-kit-local cfs-spec-coverage ensure-submodules

## Verify git submodules (e.g. guidelines/DNA) are initialized; fails otherwise.
ensure-submodules:
	$(call print_target_banner)
	@if git submodule status --recursive 2>/dev/null | grep -q '^-'; then \
		echo "ERROR: Uninitialized git submodules detected. Run 'git submodule update --init --recursive'." && exit 1; \
	fi

# Check code formatting
fmt:
	$(call print_target_banner)
	$(call check_rustup_component,rustfmt)
	$(if $(GEAR),cargo fmt $(GEAR_PKGS) --check,cargo fmt --all --check)

CFS ?= cfs
CFS_PIPX_SPEC ?= git+https://github.com/constructorfabric/studio.git
export PATH := $(HOME)/.local/bin:$(PATH)

# Fast two-pass clippy used in PR CI (target: <5 min with sccache).
#
# Pass 1 — one workspace-wide all-features run.
#   Covers every crate, every target, every additive feature combination.
#   80+ "leaf" crates (gears, plugins, SDKs) use `dep:` guards only, so
#   --all-features is sufficient — no --each-feature needed.
#
# Pass 2 — cargo-hack --each-feature on the three crates with real
#   #[cfg(feature = "...")] guards (mutually-exclusive DB backends, otel,
#   fips, db). These account for >95% of all cfg-gated lines in the repo.
#   See GH issue #1574 for original motivation.
#
# Use `make clippy-deep` for the full 182-run matrix (nightly / pre-release).
CLIPPY_FLAGS := -- -D warnings -D clippy::perf
# `bss-fixtures` is on the list because it is the one crate whose *production*
# surface is the narrow one: pricing's `FixtureGate` inherits it with
# `default-features = false`, while `default = ["corpus"]` means every ordinary
# build compiles the wide one. A feature-combination pass is the only thing that
# lints the surface a gear actually takes.
CLIPPY_HACK_CRATES := -p cf-gears-toolkit -p cf-gears-toolkit-db -p cf-gears-toolkit-http -p cf-gears-bss-fixtures
# `_any-backend` is toolkit-db's internal "some backend is on" marker (see its
# [features] block); enabling it *alone* asserts a driver exists while none does,
# which the outbox benchmarks reject with a compile_error!. Not a configuration
# anyone can ship, so it is not worth a lint pass.
CLIPPY_HACK_EXCLUDE := --exclude-features _any-backend

clippy:
	$(call print_target_banner)
	$(call check_rustup_component,clippy)
ifeq ($(GEAR),)
	$(call check_tool,cargo-hack)
	cargo clippy --workspace --all-targets --all-features $(CLIPPY_FLAGS)
	cargo hack clippy $(CLIPPY_HACK_CRATES) --all-targets --each-feature $(CLIPPY_HACK_EXCLUDE) $(CLIPPY_FLAGS)
else
	cargo clippy $(GEAR_PKGS) $(GEAR_CLIPPY_ARGS)
endif

## Full feature-matrix clippy: one pass per (crate × feature).
## ~182 runs — intended for nightly CI and pre-release validation, not PRs.
clippy-deep:
	$(call print_target_banner)
	$(call check_rustup_component,clippy)
	$(call check_tool,cargo-hack)
	cargo hack clippy --workspace --all-targets --each-feature $(CLIPPY_HACK_EXCLUDE) $(CLIPPY_FLAGS)

# Run markdown checks with 'lychee'
lychee: ensure-submodules
	$(call print_target_banner)
	$(call check_tool,lychee)
	lychee --exclude-path 'docs/web-docs' docs examples guidelines gears/system/event-broker/docs

## Validate internal links in web-docs.
# The web-docs pages use Starlight route-relative links (e.g. ../foo/) that only
# resolve against the *built* site, not the markdown source — so we build the
# docs site with the local content and run lychee over the generated HTML.
WEB_DOCS_CACHE ?= .web-docs-preview
web-docs-check:
	$(call print_target_banner)
	$(call check_tool,lychee)
	@bash tools/scripts/docs-preview.sh build
	lychee --offline --root-dir '$(abspath $(WEB_DOCS_CACHE)/dist)' --exclude 'i18n' '$(WEB_DOCS_CACHE)/dist/**/*.html'

## The Kani Rust Verifier for checking safety of the code
kani:
	$(call print_target_banner)
	$(call check_tool,kani)
	cargo kani --workspace --all-features

## Run Geiger scanner for unsafe code in dependencies
geiger:
	$(call print_target_banner)
	$(call check_tool,cargo-geiger)
	cd apps/cf-gears-example-server && cargo geiger --all-features

## Check there are no compile time warnings
lint:
	$(call print_target_banner)
	RUSTFLAGS="-D warnings" cargo check $(if $(GEAR),$(GEAR_PKGS),--workspace) --all-targets --all-features

## Validate GTS identifiers in .md and .json files (DE0903)
# Uses gts-validator binary (install via: cargo install gts-validator)

gts-docs:
	$(call print_target_banner)
	$(call check_tool,gts-validator)
	gts-validator \
		--vendor cf,vendor,example,fabrikam \
		--exclude "target/*" \
		--exclude "docs/api/*" \
		--exclude "docs/web-docs/*" \
		--exclude "gears/chat-engine/*" \
		--exclude "**/helm/*/templates/*" \
		docs gears libs examples

install-tools:
	$(call print_target_banner)
	@cargo gears tools check-version cargo-gears '>=$(CARGO_GEARS_MIN_VERSION)' >/dev/null 2>&1 \
	|| (echo "Installing cargo-gears >= $(CARGO_GEARS_MIN_VERSION)..." && cargo install cargo-gears)
	@cargo gears tools check-version cargo-nextest '>=$(NEXTEST_MIN_VERSION)' >/dev/null 2>&1 \
	|| (echo "Installing cargo-nextest >= $(NEXTEST_MIN_VERSION)..." && cargo install --locked cargo-nextest)
	@cargo gears tools check-version cargo-deny '>=$(DENY_MIN_VERSION)' >/dev/null 2>&1 \
	|| (echo "Installing cargo-deny >= $(DENY_MIN_VERSION)..." && cargo install --locked cargo-deny)

# Run architecture lints via cargo-gears (see Gears.toml for configuration).
dylint:
	$(call print_target_banner)
	$(call check_tool,cargo-gears)
	cargo gears lint --dylint

# Check for unused dependencies with cargo-shear.
shear:
	$(call print_target_banner)
	$(call check_tool,cargo-shear)
	cargo +$(RUST_NIGHTLY) shear --expand --deny-warnings

# Run all code safety checks
safety: clippy kani lint dylint # geiger
	$(call print_target_banner)
	@echo "OK. Rust Safety Pipeline complete"

## Validate gear folder names follow kebab-case convention
validate-gear-names: py-env
	$(call print_target_banner)
	@$(PYTHON) tools/scripts/validate_gear_names.py

## Validate readme/license-file paths declared by publishable crates exist
check-packaging-metadata: py-env
	$(call print_target_banner)
	@$(PYTHON) tools/scripts/check_packaging_metadata.py

## Validate that examples/apps/tools are unpublishable and release-plz.toml matches the workspace
check-release-config: py-env
	$(call print_target_banner)
	@$(PYTHON) tools/scripts/check_release_config.py

# -------- Code security checks --------

.PHONY: deny deny-magika fips-policy security

# Check licenses and dependencies
deny:
	$(call print_target_banner)
	$(call check_tool,cargo-deny)
	$(call check_tool_version,cargo-deny,>=$(DENY_MIN_VERSION))
	cargo deny check

# Same as `deny`, but with the file-parser `magika` feature enabled so the
# ort/ndarray dependency tree it pulls in is also audited.
deny-magika:
	$(call check_tool,cargo-deny)
	$(call check_deny_version)
	cargo deny --features magika check

## FIPS dependency-graph policy (see deny-fips.toml + ADR 0005).
## Refuses the build if any non-FIPS-validated crypto crate enters the
## --features fips dep graph. Build-time analogue of Go 1.25 fips140=only.
## Run on every PR that touches deps.
fips-policy:
	$(call print_target_banner)
	$(call check_tool,cargo-deny)
	$(call check_tool_version,cargo-deny,>=$(DENY_MIN_VERSION))
	cargo deny --config deny-fips.toml check bans

security: deny fips-policy
	$(call print_target_banner)

# -------- Studio --------

# Validate Constructor Studio artifacts (specs, code, templates).
cfs-validate: cfs-repair
	$(call print_target_banner)
	$(CFS) validate && echo "OK. Constructor Studio validation PASSED" || (echo "ERROR: Constructor Studio validation FAILED"; exit 1)

# Ensure the Constructor Studio CLI is available even when generated runtime
# files are ignored locally or absent in a clean checkout.
cfs-ensure:
	$(call print_target_banner)
	@if ! command -v $(CFS) >/dev/null 2>&1; then \
		echo "cfs not found; installing $(CFS_PIPX_SPEC) via pipx"; \
		if ! command -v pipx >/dev/null 2>&1; then \
			echo "ERROR: pipx is required before running this target"; \
			exit 1; \
		else \
			pipx install $(CFS_PIPX_SPEC); \
		fi; \
	fi
	@if ! command -v $(CFS) >/dev/null 2>&1; then \
		echo "ERROR: cfs was installed but is not on PATH"; \
		exit 1; \
	fi

## Repair ignored/generated Constructor Studio runtime files before validation.
cfs-repair: cfs-ensure
	$(call print_target_banner)
	$(CFS) init --yes

## Check Constructor Studio spec-to-code traceability coverage.
cfs-spec-coverage: cfs-repair
	$(call print_target_banner)
	$(CFS) spec-coverage --min-coverage 80

## Validate registered Constructor Studio kits.
cfs-validate-kits: cfs-repair
	$(call print_target_banner)
	$(CFS) validate-kits

## Validate the local studio-kit-gears checkout as a kit directory.
cfs-validate-kit-local: cfs-repair
	$(call print_target_banner)
	cd studio-kit-gears && $(CFS) validate-kits .

# -------- API and docs --------

.PHONY: openapi md-fabric slides web-docs-preview .example-server-build

.example-server-build:
	$(call print_target_banner)
ifneq ($(GEAR),)
ifeq ($(GEAR_HAS_SERVER_FEATURE),)
	@echo "SKIP: GEAR=$(GEAR) has no example-server feature — skipping server build."
else
	cargo build --bin $(EXAMPLE_SERVER_BIN) $(OPENAPI_BUILD_FEATURE_ARGS)
endif
else
	cargo build --bin $(EXAMPLE_SERVER_BIN) $(OPENAPI_BUILD_FEATURE_ARGS)
endif

# Generate OpenAPI spec from running cf-gears-example-server.
# Skipped when GEAR has no corresponding example-server feature.
openapi: .example-server-build py-env
	$(call print_target_banner)
ifneq ($(GEAR),)
ifeq ($(GEAR_HAS_SERVER_FEATURE),)
	@echo "SKIP: GEAR=$(GEAR) has no example-server feature — skipping openapi."
else
	@command -v curl >/dev/null || (echo "curl is required to generate OpenAPI spec" && exit 1)
	@echo "Starting cf-gears-example-server to generate OpenAPI spec..."
	@mkdir -p $$(dirname "$(if $(GEAR),$(GEAR_OPENAPI_TMP),$(OPENAPI_OUT))") && \
	$(call start_server_and_wait,$(EXAMPLE_SERVER_DEBUG_BINARY) --config $(OPENAPI_CONFIG) --port $(OPENAPI_PORT),$(OPENAPI_URL),300) && \
	echo "Fetching OpenAPI spec..." && \
	curl -fsS "$(OPENAPI_URL)" -o "$(if $(GEAR),$(GEAR_OPENAPI_TMP),$(OPENAPI_OUT))" && \
	if [ -n "$(GEAR)" ]; then \
		echo "Merging $(GEAR) OpenAPI into $(OPENAPI_OUT)..." && \
		$(PYTHON) tools/scripts/merge_openapi_json.py "$(OPENAPI_OUT)" "$(GEAR_OPENAPI_TMP)"; \
	fi && \
	echo "Sorting OpenAPI JSON for deterministic ordering..." && \
	$(PYTHON) tools/scripts/sort_openapi_json.py "$(OPENAPI_OUT)" && \
	echo "OpenAPI spec saved to $(OPENAPI_OUT)"
endif
else
	@command -v curl >/dev/null || (echo "curl is required to generate OpenAPI spec" && exit 1)
	@echo "Starting cf-gears-example-server to generate OpenAPI spec..."
	@mkdir -p $$(dirname "$(OPENAPI_OUT)") && \
	$(call start_server_and_wait,$(EXAMPLE_SERVER_DEBUG_BINARY) --config $(OPENAPI_CONFIG) --port $(OPENAPI_PORT),$(OPENAPI_URL),300) && \
	echo "Fetching OpenAPI spec..." && \
	curl -fsS "$(OPENAPI_URL)" -o "$(OPENAPI_OUT)" && \
	echo "Sorting OpenAPI JSON for deterministic ordering..." && \
	$(PYTHON) tools/scripts/sort_openapi_json.py "$(OPENAPI_OUT)" && \
	echo "OpenAPI spec saved to $(OPENAPI_OUT)"
endif

## Generate Markdown files map
md-fabric: py-env
	$(call print_target_banner)
	$(PYTHON) ./tools/scripts/md-fabric.py --inline-data --out docs/md-fabric/md-fabric.html

## Build the slides with Marp
slides:
	$(call print_target_banner)
	@command -v npx >/dev/null || (echo "npx is required to build slides. Install Node.js or run 'npm install' from the repo root." && exit 1)
	npx marp docs/slides/[0-9]*.md --theme-set docs/slides/css/slides.css --allow-local-files

# Preview the documentation website with local docs/web-docs content.
# Clones the web docs site into .web-docs-preview/ and serves it at localhost:4321.
web-docs-preview:
	$(call print_target_banner)
	@bash tools/scripts/docs-preview.sh

# -------- Development and auto fix --------

.PHONY: dev dev-fmt dev-clippy dev-test

## Run tests in development mode
dev-test: install-tools
	$(call print_target_banner)
	cargo nextest run --workspace

## Auto-fix code formatting
dev-fmt:
	$(call print_target_banner)
	cargo fmt --all

## Auto-fix clippy warnings
dev-clippy:
	$(call print_target_banner)
	cargo clippy --workspace --all-targets --all-features --fix --allow-dirty

# Auto-fix formatting and clippy warnings
dev: dev-fmt dev-clippy dev-test
	$(call print_target_banner)

# -------- Optional GEAR= scope for top-level targets --------
#
# Scope fmt, clippy, build, test, run, e2e-local, openapi, and coverage
# to a single gear instead of the whole workspace.
#
# Package resolution uses `cargo gears ls packages` with a regex filter
# on Cargo package names, so naming conventions (cf-gears-*, bss-*, bare)
# are handled automatically.
#
# Examples:
#   make fmt GEAR=file-parser         -> cargo fmt -p cf-gears-file-parser -p cf-gears-file-parser-sdk --check
#   make test GEAR=ledger             -> cargo nextest run -p bss-ledger -p bss-ledger-sdk ... (+ reverse deps)
#   make run GEAR=file-parser         -> cargo run --bin cf-gears-example-server --features file-parser,...

# --- User-facing knobs ---
GEAR ?=
GEAR_BUILD_ARGS ?=
GEAR_TEST_ARGS ?=
GEAR_RUN_ARGS ?=
# Extra Cargo features to pass with --features (e.g. GEAR_FEATURES=integration).
GEAR_FEATURES ?=
# Clippy flags for gear-scoped runs.
GEAR_CLIPPY_ARGS ?= --all-targets --all-features -- -D warnings
E2E_SIDECAR_PREREQ := $(if $(GEAR),$(if $(filter file-storage,$(GEAR)),.e2e-sidecar-build,),.e2e-sidecar-build)
E2E_SIDECAR_ENV := $(if $(GEAR),$(if $(filter file-storage,$(GEAR)),FS_SIDECAR_BINARY=target/debug/sidecar,E2E_SKIP_SIDECAR=1),FS_SIDECAR_BINARY=target/debug/sidecar)

# --- Package resolution ---
# Regex to match Cargo package names belonging to this gear.
# Handles: <gear>, <gear>-sdk, cf-gears-<gear>, cf-gears-<gear>-sdk,
#          <prefix>-<gear>, <prefix>-<gear>-sdk (e.g. bss-ledger).
# Override with GEAR_NAME_REGEXP= for non-standard naming.
GEAR_NAME_REGEXP ?= ^(?:(?:cf-gears-)?(?:[a-z]+-)?)?$(GEAR)(?:-sdk)?$$
ifdef GEAR
  GEAR_PKGS := $(shell cargo gears ls packages --dirs gears,libs --filter '$(GEAR_NAME_REGEXP)' -f cargo-flags)
endif

# --- Derived variables ---
GEAR_FEATURE_ARGS := $(if $(GEAR_FEATURES),--features $(GEAR_FEATURES),)
GEAR_NO_TESTS_FLAG := $(if $(GEAR),--no-tests=warn)
# E2E suite key: kebab-case gear name → snake_case directory name.
GEAR_E2E_KEY := $(subst -,_,$(GEAR))
GEAR_E2E_TARGET ?= testing/e2e/suites/$(GEAR_E2E_KEY)
GEAR_E2E_SCOPE := $(if $(GEAR),$(GEAR_E2E_TARGET) $(E2E_TARGET),$(E2E_TARGET))
# Coverage: pass the first resolved package + e2e target path to coverage.py.
GEAR_COVERAGE_ARGS := $(if $(GEAR),--package $(firstword $(subst -p ,,$(GEAR_PKGS))) --e2e-target $(GEAR_E2E_TARGET),)

# --- Server feature selection for run / openapi ---
# Base features always enabled when running a focused server.
GEAR_SERVER_BASE_FEATURES ?= static-tenants,static-authn,static-authz
# System gears that are non-optional deps of the example server (always linked).
GEAR_SERVER_ALWAYS_LINKED ?= api-gateway gear-orchestrator types-registry tenant-resolver authn-resolver authz-resolver
# Check whether GEAR is a valid example-server feature or an always-linked gear.
# When GEAR has no server feature (e.g. toolkit-db, toolkit-http), server-dependent
# targets (run, openapi, e2e-local) are skipped; library-safe targets still work.
GEAR_HAS_SERVER_FEATURE := $(or $(filter $(GEAR),$(GEAR_SERVER_ALWAYS_LINKED)),$(filter $(GEAR),$(EXAMPLE_SERVER_ALL_FEATURES)))
# The gear itself as an optional feature (empty if it's an always-linked gear).
GEAR_SERVER_OPTIONAL_FEATURES := $(if $(GEAR_HAS_SERVER_FEATURE),$(filter-out $(GEAR_SERVER_ALWAYS_LINKED),$(GEAR)),)
GEAR_SERVER_FEATURES ?= $(GEAR_SERVER_OPTIONAL_FEATURES)$(if $(GEAR_SERVER_OPTIONAL_FEATURES),$(COMMA),)$(GEAR_SERVER_BASE_FEATURES)
GEAR_SERVER_FEATURE_ARGS := $(if $(GEAR),$(if $(GEAR_HAS_SERVER_FEATURE),--no-default-features --features $(GEAR_SERVER_FEATURES),),$(EXAMPLE_SERVER_FEATURE_ARGS))

# --- OpenAPI ---
GEAR_OPENAPI_TMP ?= target/openapi/$(GEAR).json
# GTS envelope providers that config/e2e-local.yaml's seeds depend on at link
# time (account-management registers tenant_type base schemas via inventory).
OPENAPI_ENVELOPE_FEATURES ?= account-management,static-idp
GEAR_OPENAPI_FEATURE_ARGS := --no-default-features --features $(GEAR_SERVER_FEATURES),$(OPENAPI_ENVELOPE_FEATURES)
# No GEAR: full curated server.  GEAR=xxx: focused server, merged into api.json.
OPENAPI_BUILD_FEATURE_ARGS := $(if $(GEAR),$(GEAR_OPENAPI_FEATURE_ARGS),$(OPENAPI_SERVER_FEATURE_ARGS))


# -------- Tests --------

.PHONY: test test-no-macros test-macros test-sqlite test-pg test-mysql test-db test-users-info-pg test-usage-collector-pg test-types-registry-db test-cluster-pg test-rg-pg test-pricing-pg test-coord-pg test-fixtures-narrow test-fips

# Run all tests, or a single gear when GEAR=<gear> is set.
# When GEAR= is set, cargo gears ls packages finds matching crates + their
# transitive reverse deps (every workspace crate that depends on them).
test: install-tools
	$(call print_target_banner)
ifeq ($(GEAR),)
	cargo nextest run --workspace $(GEAR_FEATURE_ARGS) $(GEAR_TEST_ARGS)
else
	@GEAR_SCOPE=$$(cargo gears ls packages --dirs gears,libs --filter '$(GEAR_NAME_REGEXP)' --include-rdeps -f cargo-flags) || exit 1; \
	echo "cargo nextest run $$GEAR_SCOPE $(GEAR_FEATURE_ARGS) $(GEAR_TEST_ARGS) $(GEAR_NO_TESTS_FLAG)"; \
	cargo nextest run $$GEAR_SCOPE $(GEAR_FEATURE_ARGS) $(GEAR_TEST_ARGS) $(GEAR_NO_TESTS_FLAG)
endif

test-no-macros: install-tools
	$(call print_target_banner)
	cargo nextest run --workspace --exclude cf-gears-toolkit-macros-tests --exclude cf-gears-toolkit-db-macros

test-macros: install-tools
	$(call print_target_banner)
	cargo nextest run -p cf-gears-toolkit-db-macros
	cargo nextest run -p cf-gears-toolkit-macros-tests

## Run SQLite integration tests
test-sqlite: install-tools
	$(call print_target_banner)
	cargo nextest run -p cf-gears-toolkit-db --features sqlite,integration
	cargo build -p cf-gears-toolkit-db --examples --features sqlite

## Run PostgreSQL integration tests
test-pg: install-tools
	$(call print_target_banner)
	cargo nextest run -p cf-gears-toolkit-db --features pg,integration

## Run MySQL integration tests
test-mysql: install-tools
	$(call print_target_banner)
	cargo nextest run -p cf-gears-toolkit-db --features mysql,integration

# Run all database integration tests
test-db: test-sqlite test-pg test-mysql
	$(call print_target_banner)

## Run users-info gear integration tests
test-users-info-pg: install-tools
	$(call print_target_banner)
	cargo nextest run -p users-info --features "integration"

## Run TimescaleDB usage-collector plugin integration tests (Docker required;
## the suite spins up its own timescale/timescaledb container via testcontainers)
test-usage-collector-pg: install-tools
	$(call print_target_banner)
	cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --features postgres

## Run types-registry PostgreSQL + MySQL integration tests (Docker required;
## each test spins up its own postgres or mysql container via testcontainers).
test-types-registry-db: install-tools
	cargo nextest run -p cf-gears-types-registry --features integration \
	  --test migration_backends_test --test repo_backends_test

## Run the Postgres cluster plugin's conformance (Layer 2) and Layer 3
## integration suites (Docker required;
## each spins up its own postgres container per test via testcontainers —
## see gears/system/cluster/plugins/postgres-cluster-plugin/docs/TESTING.md §7).
##
## `--retries 1` because the container/pool *setup* in tests/common/mod.rs is
## load-sensitive on a busy host: it already retries `Postgres::start()` itself,
## and exhausting that budget surfaces as a failure in whichever test drew the
## short straw. A genuine logic regression fails both attempts, so this absorbs
## Docker churn without masking one.
test-cluster-pg: install-tools
	$(call print_target_banner)
	cargo nextest run -p cf-postgres-cluster-plugin --features integration --retries 1

## Run resource-group gear PostgreSQL smoke tests (Docker required; spins up
## its own postgres container via testcontainers -- see
## gears/system/resource-group/resource-group/tests/pg_smoke_test.rs)
test-rg-pg: install-tools
	cargo nextest run -p cf-gears-resource-group --features integration

## Run bss-pricing's Postgres tier (Docker required; each suite spins up its own
## postgres container via testcontainers).
##
## Gated behind `#[ignore]` rather than a feature, which is why this needs
## `--run-ignored ignored-only` where its siblings above pass `--features`. Every
## test in `tests/postgres_*.rs` carries the attribute — 376 of them on
## 2026-08-18, stated as the invariant rather than as a number because the count
## has already drifted once here — and until 2026-08-11
## nothing in this Makefile or in `.github/` passed `--run-ignored` at all — so
## the tier compiled on every run and executed on none.
##
## What that cost is specific, not theoretical. Every proof this crate owns about
## two racing writers, a lock held by a crashed pass, `FOR UPDATE` semantics,
## READ COMMITTED re-evaluation and the PL/pgSQL half of every dual-spelled
## trigger lives here: the five test files using `tokio::spawn`/`join!` are all
## `postgres_*`. Each suite's module doc states that its property is unprovable
## on SQLite, and two note that the SQLite twin passes either way —
## `sqlite_bulk_commit.rs` is a test that cannot fail on the property it is named
## for. `sqlite_append_only.rs:110` names the cost from experience: "D-236 is the
## record of what that costs — a premise living on one tier only means a run
## without Docker reports a clean change through a guard that stopped guarding."
##
## `--no-fail-fast`, and it is not a preference. On 2026-08-18 this tier was red
## with nine failures: one stale probe at test 68 of 376 and, behind it, six
## proving that price supersession was broken on the engine that ships. nextest
## stops at the first failure by default, so the PR page reported the stale probe
## and nothing else — the cheap failure hid the expensive one, and a reader fixing
## the one line would have met the other six only on the next run. A tier whose
## whole purpose is to be the one place a Postgres-only defect surfaces must
## report every failure it found, not the first.
test-pricing-pg: install-tools
	cargo nextest run -p cf-gears-bss-pricing --run-ignored ignored-only -E 'binary(/^postgres_/)' --no-fail-fast

## Run coord's Postgres tier (Docker required; testcontainers).
##
## `coord`'s `m0001_…` builds two independent SQL literals — a schema-qualified
## PG `CREATE TABLE` and a bare SQLite one. The in-crate tests connect
## `sqlite::memory:` and reach the SQLite literal only, so the `IF NOT EXISTS`
## that stops the two-gear boot crash (`bss-pricing` starting beside a
## long-running `bss-ledger`) was unexercised on the dialect the crash happened
## on. `Migration::in_schema("bss")` — the constructor both consumers actually
## pass — is likewise unreachable from SQLite, which has one namespace.
##
## Same `--run-ignored ignored-only` shape as `test-pricing-pg` above, and the
## same reason: the gate is `#[ignore]` rather than a feature.
test-coord-pg: install-tools
	cargo nextest run -p cf-gears-bss-coord --run-ignored ignored-only -E 'binary(/^postgres_/)'

## Compile and run `bss-fixtures` on the surface a **gear** actually takes.
##
## Pricing's `FixtureGate` inherits this crate with `default-features = false`
## (`Cargo.toml`'s workspace entry) — `ModelKind` + `Registry` + `gate_open_for`
## and nothing else. `default = ["corpus"]`, so every other build in the
## workspace, `make test-no-macros` included, compiles the wide surface: the
## test written to guard the narrow one (`tests/production_surface.rs`, whose
## module doc names this invocation) ran only in the configuration it does not
## guard, where its assertions hold trivially. The narrow build's only other
## consumer is the example server's release build, which never runs a test.
test-fixtures-narrow: install-tools
	cargo nextest run -p cf-gears-bss-fixtures --no-default-features --test production_surface

## Run FIPS-mode integration tests (requires Go for aws-lc-fips-sys).
## Covers:
##   - cf-gears-toolkit         : bootstrap + init_crypto_provider dispatch
##   - cf-gears-toolkit-http    : TLS client fail-closed path (NoCryptoProvider,
##                                apply_fips_hardening, builder/client FIPS-feature
##                                test surface). See issue #1935.
##   - cf-gears-oagw           : startup validation rejects allow_http_upstream
##                                under --features fips (PR #1985).
##
## Per-package `pkg/feat` syntax is required because `bootstrap` exists only
## on `cf-gears-toolkit` and the crates have independent FIPS feature
## spaces (toolkit doesn't depend on toolkit-http; oagw forwards toolkit-http/fips
## via its own `fips` feature). Single invocation so the shared FIPS dep graph
## compiles once.
test-fips: install-tools
	$(call print_target_banner)
	cargo nextest run -p cf-gears-toolkit -p cf-gears-toolkit-http -p cf-gears-oagw \
		--features cf-gears-toolkit/bootstrap,cf-gears-toolkit/fips,cf-gears-toolkit-http/fips,cf-gears-oagw/fips

## Cross-compile gate for the Windows+FIPS path (Windows handshake
## verification is the manual runbook in cf-gears-fips-probe/README.md). Catches
## type / cfg / feature-graph regressions for `rustls-cng-crypto` and the
## dep-graph (`rustls-cng-crypto` present, `aws-lc-fips-sys` absent).
##
## Uses `cargo-xwin` because the workspace pulls transitive C deps
## (`aws-lc-sys`, `libz-ng-sys`) whose build.rs scripts need a Windows
## sysroot (`windows.h`, MSVC headers). `cargo-xwin` downloads the MSVC
## redistributable and CRT/Windows headers automatically — works on Linux,
## macOS, and Windows hosts without a pre-installed Visual Studio.
##
## Prerequisites:
##   cargo install cargo-xwin    # MSVC sysroot bridge
##   # And a cmake build driver for the cmake-based transitive C deps:
##   # macOS:  brew install ninja
##   # Linux:  apt-get install ninja-build  (or: dnf install ninja-build)
##
## Pair this with the dep-graph regression check, which needs no toolchain
## at all and runs on any host:
##   cargo tree --target x86_64-pc-windows-msvc -p cf-gears-example-server \
##       --features fips -e features | grep aws-lc-fips    # must be empty
.PHONY: check-windows-fips
check-windows-fips:
	$(call print_target_banner)
	$(call check_tool,cargo-xwin)
	$(call check_tool,ninja)
	rustup target add x86_64-pc-windows-msvc
	cargo xwin check --target x86_64-pc-windows-msvc -p cf-gears-example-server --features fips

# -------- Benchmarks --------

.PHONY: bench-pg bench-pg-profiler bench-mysql bench-mariadb bench-sqlite bench-db \
       bench-pg-longhaul bench-mysql-longhaul bench-mariadb-longhaul bench-sqlite-longhaul bench-db-longhaul

## Run outbox throughput benchmarks against PostgreSQL
bench-pg:
	$(call print_target_banner)
	cargo bench -p cf-gears-toolkit-db --features pg --bench outbox_throughput -- postgres

## Run outbox throughput benchmarks against MySQL
bench-mysql:
	$(call print_target_banner)
	cargo bench -p cf-gears-toolkit-db --features mysql --bench outbox_throughput -- mysql

## Run outbox throughput benchmarks against MariaDB
bench-mariadb:
	$(call print_target_banner)
	cargo bench -p cf-gears-toolkit-db --features mysql --bench outbox_throughput -- mariadb

# Run outbox throughput benchmarks against SQLite
bench-sqlite:
	$(call print_target_banner)
	cargo bench -p cf-gears-toolkit-db --features sqlite --bench outbox_throughput -- sqlite

## Run outbox throughput benchmarks against all database engines
bench-db: bench-pg bench-mysql bench-mariadb bench-sqlite
	$(call print_target_banner)

## Run long-haul (1M+10M) outbox benchmarks against PostgreSQL
bench-pg-longhaul:
	$(call print_target_banner)
	cargo bench -p cf-gears-toolkit-db --features pg --bench outbox_throughput -- postgres_longhaul

## Run long-haul (1M+10M) outbox benchmarks against MySQL
bench-mysql-longhaul:
	$(call print_target_banner)
	cargo bench -p cf-gears-toolkit-db --features mysql --bench outbox_throughput -- mysql_longhaul

## Run long-haul (1M+10M) outbox benchmarks against MariaDB
bench-mariadb-longhaul:
	$(call print_target_banner)
	cargo bench -p cf-gears-toolkit-db --features mysql --bench outbox_throughput -- mariadb_longhaul

## Run long-haul (100K 1P) outbox benchmarks against SQLite
bench-sqlite-longhaul:
	$(call print_target_banner)
	cargo bench -p cf-gears-toolkit-db --features sqlite --bench outbox_throughput -- sqlite_longhaul

## Run long-haul outbox benchmarks against all database engines
bench-db-longhaul: bench-pg-longhaul bench-mysql-longhaul bench-mariadb-longhaul bench-sqlite-longhaul
	$(call print_target_banner)

# -------- E2E tests --------

.PHONY: e2e e2e-local e2e-local-smoke e2e-mini-chat e2e-docker e2e-docker-smoke e2e-tr-authz e2e-usage-collector

E2E_TARGET ?=
# E2E selectors for `make e2e-local`:
#   SUITE=<suite>  run ONE named scenario under testing/e2e/suites/ (often, but
#                  not always, a gear crate) — e.g. SUITE=file-parser,
#                  SUITE=scope-enforcement.
#   GEAR=<gear>    run EVERY e2e-launcher suite whose e2e.yaml features (incl.
#                  features_file) include <gear> — e.g. GEAR=credstore runs both
#                  the credstore and oagw suites. SUITE and GEAR are mutually
#                  exclusive (enforced by tools/scripts/run_e2e.py).
SUITE ?=

# SUITE= and GEAR= are mutually exclusive; that check lives in
# tools/scripts/run_e2e.py (the runner rejects the combination).

# Run E2E tests in Docker (default)
e2e: e2e-docker
	$(call print_target_banner)

## Run E2E tests in Docker environment
e2e-docker: py-env
	$(call print_target_banner)
	$(PYTHON) tools/scripts/ci.py e2e-docker -- $(E2E_TARGET)

## Run E2E smoke tests in Docker (only tests marked @pytest.mark.smoke)
e2e-docker-smoke: py-env
	$(call print_target_banner)
	$(PYTHON) tools/scripts/ci.py e2e-docker -- -m smoke $(E2E_TARGET)

# Run E2E tests locally. Three ways to use it:
#   make e2e-local SUITE=<suite>   run ONE suite: build a server for just that
#                                  suite and run its tests.
#   make e2e-local GEAR=<gear>     run EVERY e2e-launcher suite whose e2e.yaml
#                                  features (incl. features_file) include <gear>,
#                                  each as its own focused build+run. If no suite
#                                  matches, it is a no-op (so `make all GEAR=` is
#                                  safe for gears without E2E coverage).
#   make e2e-local                 run MANY suites: build one server with every
#                                  E2E feature (config/e2e-features.txt) and run
#                                  every shared-server suite against it. A
#                                  "shared-server suite" is one whose e2e.yaml
#                                  has `launcher: e2e-launcher`.
# Self-managed suites (`launcher: pytest` — mini-chat, usage-collector) and the
# tr-authz profile lane start their own server, so plain `make e2e-local` and
# GEAR= runs skip them; run them via their own targets (e2e-mini-chat,
# e2e-usage-collector, e2e-tr-authz). All feature/config/sidecar/gear knowledge
# lives in config/e2e-launcher.yaml and testing/e2e/suites/<suite>/e2e.yaml, so
# this recipe stays suite-agnostic.
e2e-local: py-env
	$(call print_target_banner)
	$(PYTHON) tools/scripts/run_e2e.py --suite "$(SUITE)" --gear "$(GEAR)" -- $(E2E_TARGET)

## Run RG + AuthZ barrier E2E tests with tr-authz-plugin going through TR -> RG
e2e-tr-authz: py-env
	$(call print_target_banner)
	$(PYTHON) tools/scripts/run_e2e.py --suite resource-group --profile tr-authz --

## Run E2E smoke tests locally (only tests marked @pytest.mark.smoke)
e2e-local-smoke: py-env
	$(call print_target_banner)
	$(PYTHON) tools/scripts/run_e2e.py --suite "$(SUITE)" --gear "$(GEAR)" --smoke -- $(E2E_TARGET)

MINI_CHAT_FEATURES = mini-chat,static-authn,static-authz,single-tenant,static-credstore
MINI_CHAT_K8S_FEATURES = $(MINI_CHAT_FEATURES),k8s

MINI_CHAT_IMAGE ?= cf-gears-mini-chat
MINI_CHAT_TAG   ?= $(shell git rev-parse --short HEAD 2>/dev/null || echo latest)

## Run mini-chat E2E tests (alias for focused local E2E)
e2e-mini-chat:
	$(call print_target_banner)
	$(MAKE) e2e-local SUITE=mini-chat

## Run usage-collector E2E tests (alias for focused local E2E; Docker required)
e2e-usage-collector:
	$(call print_target_banner)
	$(MAKE) e2e-local SUITE=usage-collector

# -------- Code coverage --------

.PHONY: coverage coverage-unit coverage-e2e-local check-prereq-e2e-local

# Generate code coverage report (unit + e2e-local tests)
coverage: py-env
	$(call print_target_banner)
	$(call check_tool,cargo-llvm-cov)
	$(PYTHON) tools/scripts/coverage.py combined $(GEAR_COVERAGE_ARGS)

# Generate code coverage report (unit tests only)
coverage-unit: py-env
	$(call print_target_banner)
	$(call check_tool,cargo-llvm-cov)
	$(PYTHON) tools/scripts/coverage.py unit $(if $(GEAR),--package $(firstword $(subst -p ,,$(GEAR_PKGS))),)

## Ensure needed packages and programs installed for local e2e testing
check-prereq-e2e-local: py-env
	$(call print_target_banner)
	$(PYTHON) tools/scripts/check_local_env.py --mode e2e-local

# Generate code coverage report (e2e-local tests only)
coverage-e2e-local: check-prereq-e2e-local
	$(call print_target_banner)
	$(call check_tool,cargo-llvm-cov)
	$(PYTHON) tools/scripts/coverage.py e2e-local $(if $(GEAR),--e2e-target $(GEAR_E2E_TARGET),)

# -------- Fuzzing --------

.PHONY: fuzz fuzz-build fuzz-list fuzz-run fuzz-clean fuzz-corpus

## Check cargo-fuzz is installed (required for fuzzing)
fuzz-install:
	$(call print_target_banner)
	$(call check_tool,cargo-fuzz)

## Build all fuzz targets
fuzz-build: fuzz-install
	$(call print_target_banner)
	cargo +nightly fuzz build --fuzz-dir tools/fuzz

## List all available fuzz targets
fuzz-list: fuzz-install
	$(call print_target_banner)
	cargo +nightly fuzz list --fuzz-dir tools/fuzz

## Run a specific fuzz target (use FUZZ_TARGET=name)
## Example: make fuzz-run FUZZ_TARGET=fuzz_odata_filter FUZZ_SECONDS=60
fuzz-run: fuzz-install
	$(call print_target_banner)
	@if [ -z "$(FUZZ_TARGET)" ]; then \
		echo "ERROR: FUZZ_TARGET is required. Example: make fuzz-run FUZZ_TARGET=fuzz_odata_filter"; \
		exit 1; \
	fi
	cargo +nightly fuzz run --fuzz-dir tools/fuzz $(FUZZ_TARGET) -- -max_total_time=$(or $(FUZZ_SECONDS),60)

# Run all fuzz targets for a short time (smoke test)
fuzz: fuzz-build
	$(call print_target_banner)
	@echo "Running all fuzz targets for 30 seconds each..."
	@FAILED=0; \
	for target in $$(cargo +nightly fuzz list --fuzz-dir tools/fuzz); do \
		echo "=== Fuzzing $$target ==="; \
		cargo +nightly fuzz run --fuzz-dir tools/fuzz $$target -- -max_total_time=30 || FAILED=1; \
	done; \
	if [ $$FAILED -ne 0 ]; then \
		echo "Fuzzing found crashes. Check tools/fuzz/artifacts/ for details."; \
		exit 1; \
	fi
	@echo "Fuzzing complete. No crashes found."

## Clean fuzzing artifacts and corpus
fuzz-clean:
	$(call print_target_banner)
	rm -rf tools/fuzz/artifacts/
	rm -rf tools/fuzz/corpus/*/
	rm -rf tools/fuzz/target/

## Minimize corpus for a specific target
fuzz-corpus: fuzz-install
	$(call print_target_banner)
	@if [ -z "$(FUZZ_TARGET)" ]; then \
		echo "ERROR: FUZZ_TARGET is required. Example: make fuzz-corpus FUZZ_TARGET=fuzz_odata_filter"; \
		exit 1; \
	fi
	cargo +nightly fuzz cmin --fuzz-dir tools/fuzz $(FUZZ_TARGET)

# -------- Mini chat --------

# mini-chat targets are for running the mini-chat gear locally and in Kubernetes, with options for building Docker images and deploying with Helm.

.PHONY: mini-chat mini-chat-docker mini-chat-helm mini-chat-helm-template mini-chat-up mini-chat-down mini-chat-port-forward

# Run server with mini-chat gear
mini-chat:
	$(call print_target_banner)
	cargo run --bin $(EXAMPLE_SERVER_BIN) --features mini-chat,static-authn,static-authz,single-tenant,static-credstore,otel -- --config config/mini-chat.yaml run

## Build mini-chat Docker image for K8s (dev build by default, RELEASE=1 for optimized)
## On linux: builds on host (reuses local target/), then packages the binary.
## On other OS: full multi-stage Docker build with BuildKit caching.
MINI_CHAT_PROFILE = $(if $(RELEASE),release,dev)
MINI_CHAT_CARGO_RELEASE_FLAG = $(if $(RELEASE),--release,)
MINI_CHAT_TARGET_DIR = $(or $(CARGO_TARGET_DIR),target)/$(if $(RELEASE),release,debug)

mini-chat-docker:
	$(call print_target_banner)
ifeq ($(shell uname -s),Linux)
	@echo "==> Linux host: building on host, packaging into image"
	cargo build $(MINI_CHAT_CARGO_RELEASE_FLAG) --bin cf-gears-example-server --package=cf-gears-example-server \
		--features "$(MINI_CHAT_K8S_FEATURES)"
	@mkdir -p .docker-stage
	@cp $(MINI_CHAT_TARGET_DIR)/cf-gears-example-server .docker-stage/cf-gears-example-server
	DOCKER_BUILDKIT=1 docker build \
		-f gears/mini-chat/deploy/docker/mini-chat-prebuilt.Dockerfile \
		--build-arg BINARY_PATH=".docker-stage/cf-gears-example-server" \
		-t $(MINI_CHAT_IMAGE):$(MINI_CHAT_TAG) .
	@rm -rf .docker-stage
else
	@echo "==> Non-linux host: full Docker build"
	DOCKER_BUILDKIT=1 docker build \
		-f gears/mini-chat/deploy/docker/mini-chat.Dockerfile \
		--build-arg CARGO_FEATURES="$(MINI_CHAT_K8S_FEATURES)" \
		--build-arg BUILD_PROFILE="$(MINI_CHAT_PROFILE)" \
		-t $(MINI_CHAT_IMAGE):$(MINI_CHAT_TAG) .
endif

## Deploy mini-chat Helm chart to local K8s cluster (build + load + install)
mini-chat-helm: mini-chat-docker
	$(call print_target_banner)
	@if command -v k3s >/dev/null 2>&1; then \
		docker save $(MINI_CHAT_IMAGE):$(MINI_CHAT_TAG) | sudo k3s ctr images import -; \
	elif command -v minikube >/dev/null 2>&1; then \
		minikube ssh "docker rmi -f $(MINI_CHAT_IMAGE):$(MINI_CHAT_TAG) 2>/dev/null" || true; \
		minikube image load $(MINI_CHAT_IMAGE):$(MINI_CHAT_TAG); \
	else \
		echo "ERROR: k3s or minikube required"; exit 1; \
	fi
	helm upgrade --install mini-chat gears/mini-chat/deploy/helm/mini-chat/ \
		--set image.tag="$(MINI_CHAT_TAG)" \
		--set secrets.azureOpenaiApiKey="$${AZURE_OPENAI_API_KEY}" \
		--set secrets.azureOpenaiApiHost="$${AZURE_OPENAI_API_HOST}" \
		--set postgres.host="$${PG_HOST:-postgres.default.svc.cluster.local}" \
		--set postgres.password="$${PG_PASSWORD}"
	kubectl rollout restart deployment/mini-chat
	kubectl rollout status deployment/mini-chat --timeout=120s

## Render mini-chat Helm templates (dry-run)
mini-chat-helm-template:
	$(call print_target_banner)
	helm template mini-chat gears/mini-chat/deploy/helm/mini-chat/

## One-command: ensure minikube is up, deploy latest chart, port-forward
## Usage: make mini-chat-up
## If image was rebuilt (make mini-chat-docker), re-run this to pick it up.
mini-chat-up:
	$(call print_target_banner)
	@# --- 1. Ensure cluster is running ---
	@if command -v minikube >/dev/null 2>&1; then \
		STATUS=$$(minikube status -f '{{.Host}}' 2>/dev/null || true); \
		if [ "$$STATUS" != "Running" ]; then \
			echo "Starting minikube..."; \
			minikube start; \
		fi; \
	elif command -v k3s >/dev/null 2>&1; then \
		: ; \
	else \
		echo "ERROR: minikube or k3s required"; exit 1; \
	fi
	@# --- 2. Load latest image if it exists locally ---
	@if docker image inspect $(MINI_CHAT_IMAGE):$(MINI_CHAT_TAG) >/dev/null 2>&1; then \
		echo "Loading image $(MINI_CHAT_IMAGE):$(MINI_CHAT_TAG) into cluster..."; \
		if command -v minikube >/dev/null 2>&1; then \
			minikube ssh "docker rmi -f $(MINI_CHAT_IMAGE):$(MINI_CHAT_TAG) 2>/dev/null" || true; \
			minikube image load $(MINI_CHAT_IMAGE):$(MINI_CHAT_TAG); \
		else \
			docker save $(MINI_CHAT_IMAGE):$(MINI_CHAT_TAG) | sudo k3s ctr images import -; \
		fi; \
	else \
		echo "No local image found. Run 'make mini-chat-docker' first to build."; \
		exit 1; \
	fi
	@# --- 3. Helm install/upgrade ---
	@if [ -z "$${AZURE_OPENAI_API_KEY}" ] || [ -z "$${AZURE_OPENAI_API_HOST}" ]; then \
		echo "WARNING: AZURE_OPENAI_API_KEY or AZURE_OPENAI_API_HOST not set."; \
		echo "  export AZURE_OPENAI_API_KEY=... AZURE_OPENAI_API_HOST=..."; \
	fi
	helm upgrade --install mini-chat gears/mini-chat/deploy/helm/mini-chat/ \
		--set image.tag="$(MINI_CHAT_TAG)" \
		--set secrets.azureOpenaiApiKey="$${AZURE_OPENAI_API_KEY}" \
		--set secrets.azureOpenaiApiHost="$${AZURE_OPENAI_API_HOST}" \
		--set postgres.host="$${PG_HOST:-postgres.default.svc.cluster.local}" \
		--set postgres.password="$${PG_PASSWORD}"
	kubectl rollout restart deployment/mini-chat
	kubectl rollout status deployment/mini-chat --timeout=120s
	@echo ""
	@echo "mini-chat is running. In a separate terminal run:"
	@echo "  make mini-chat-port-forward"
	@echo "Then access: http://localhost:8087/cf/mini-chat"

## Persistent port-forward with auto-reconnect (run in a separate terminal)
mini-chat-port-forward:
	$(call print_target_banner)
	@echo "Port-forward: localhost:8087 -> svc/mini-chat:8087 (auto-reconnect, Ctrl+C to stop)"
	@while true; do \
		kubectl port-forward svc/mini-chat 8087:8087 2>&1 || true; \
		echo "connection lost, reconnecting in 2s..."; \
		sleep 2; \
	done

## Tear down mini-chat from the cluster
mini-chat-down:
	$(call print_target_banner)
	helm uninstall mini-chat 2>/dev/null || true
	@echo "mini-chat uninstalled"

# -------- Main targets --------

.PHONY: all dist check gear-ci ci ci_test ci_docs build build-debug .cargo-build .split-debug quickstart example mini-chat mini-chat-docker mini-chat-helm mini-chat-helm-template mini-chat-up mini-chat-down mini-chat-port-forward full-make-matrix

# Start server with quickstart config
quickstart:
	$(call print_target_banner)
	$(MAKE) run GEAR=types-registry

# Run server with example gear
example:
	$(call print_target_banner)
	cargo run --bin $(EXAMPLE_SERVER_BIN) $(EXAMPLE_SERVER_FEATURE_ARGS) -- --config config/quickstart.yaml run

# Run the default server, or the example server with only one gear feature when GEAR=<gear> is set.
# Skipped when GEAR has no corresponding example-server feature (e.g. libs).
run:
	$(call print_target_banner)
ifneq ($(GEAR),)
ifeq ($(GEAR_HAS_SERVER_FEATURE),)
	@echo "SKIP: GEAR=$(GEAR) has no example-server feature — nothing to run."
else
	cargo run --bin $(EXAMPLE_SERVER_BIN) $(GEAR_SERVER_FEATURE_ARGS) -- --config config/quickstart.yaml run $(GEAR_RUN_ARGS)
endif
else
	cargo run --bin $(EXAMPLE_SERVER_BIN) $(GEAR_SERVER_FEATURE_ARGS) -- --config config/quickstart.yaml run $(GEAR_RUN_ARGS)
endif

## Run server with fips gear
fips:
	$(call print_target_banner)
	cargo run --bin cf-gears-example-server --features fips,static-authn,static-authz,single-tenant,static-credstore,otel -- --config config/quickstart.yaml run

## Run server with out-of-process example gear
oop-example:
	$(call print_target_banner)
	cargo build -p calculator --features oop_gear
	cargo run --bin cf-gears-example-server --features oop-example,users-info-example,static-authn,static-authz,static-tenants,static-credstore -- --config config/quickstart.yaml run

# Run all quality checks
check: fmt cfs-validate clippy lychee security dylint gts-docs test
	$(call print_target_banner)

# Lightweight quality check for gear-scoped CI (gear-scoped-ci.yml).
# Runs only targets that need no extra tools beyond cargo, rustfmt, clippy,
# nextest, and cargo-gears. Skips cfs-validate, lychee, security, dylint,
# gts-docs, test-sqlite, e2e-local, and openapi.
gear-ci: fmt clippy test
	$(call print_target_banner)

ci_test: fmt clippy
	$(call print_target_banner)

ci_docs: lychee gts-docs
	$(call print_target_banner)

# Run CI pipeline locally, requires docker
ci: fmt clippy test-no-macros test-macros test-db deny test-users-info-pg test-usage-collector-pg test-types-registry-db lychee gts-docs dylint
	$(call print_target_banner)

## Build the cf-gears-example-server release binary, or a single gear when GEAR=<gear> is set
.cargo-build:
	$(call print_target_banner)
	$(if $(GEAR),cargo build --release $(GEAR_PKGS) $(GEAR_FEATURE_ARGS) $(GEAR_BUILD_ARGS),cargo build --release --bin $(EXAMPLE_SERVER_BIN) $(EXAMPLE_SERVER_FEATURE_ARGS))

## Split debug symbols into separate artifact(s) and strip the binary.
## Requires platform tools: objcopy (Linux), dsymutil+strip (macOS).
## On Windows MSVC the PDB is already separate; no extra tools needed.
.split-debug:
	$(call print_target_banner)
	cargo xtask split-debug cf-gears-example-server

# Build the cf-gears-example-server with full debuginfo (the 'debugging' profile)
# Artifacts land in target/debugging/ and are not stripped, so no split-debug step
# here. Costs a separate rebuild of the dependency graph; target/debug is untouched.
build-debug:
	cargo build --profile debugging --bin cf-gears-example-server $(E2E_ARGS)
	@echo "binary: target/debugging/cf-gears-example-server"

# Build the release binary, or a single gear when GEAR=<gear> is set.
build:
	$(call print_target_banner)
	$(MAKE) .cargo-build GEAR=$(GEAR) GEAR_FEATURES=$(GEAR_FEATURES) GEAR_BUILD_ARGS=$(GEAR_BUILD_ARGS)

# Build distributable release artifacts.
dist: build
	$(call print_target_banner)
	@if [ -z "$(GEAR)" ]; then $(MAKE) .split-debug; fi

## Run the full Makefile target matrix across all tracked gears (preset: default).
## Override with MATRIX_PRESET=smoke|extended|custom.
MATRIX_PRESET ?= default
full-make-matrix: py-env
	$(call print_target_banner)
	$(PYTHON) tools/scripts/run_make_matrix.py $(MATRIX_PRESET)

## Benchmark make targets: measure time, status, and target/ size.
## Options: BENCH_GROUP=all-gears|specific-gear  BENCH_GEAR=<name>  BENCH_SCENARIO=<num>  BENCH_VERBOSE=1
BENCH_GROUP ?=
BENCH_GEAR ?=
BENCH_SCENARIO ?=
BENCH_VERBOSE ?=
make-benchmark: py-env
	@$(call ensure-log-root,.logs/make-benchmark)
	$(PYTHON) tools/scripts/run_make_benchmark.py \
		$(if $(BENCH_GROUP),--group $(BENCH_GROUP)) \
		$(if $(BENCH_SCENARIO),--scenario $(BENCH_SCENARIO)) \
		$(if $(BENCH_GEAR),--gear $(BENCH_GEAR)) \
		$(if $(filter-out 0,$(BENCH_VERBOSE)),--verbose)

# Run all necessary quality checks and tests using reusable debug artifacts.
all: check test-sqlite e2e-local openapi
	$(call print_target_banner)
	@echo ""
	@echo "  CONGRATULATIONS! All 'make all' tasks have been completed!"
	@echo ""
	@echo "  Next suggestions:"
	@echo "    - make test-db        # run full DB integration tests"
	@echo "    - make mini-chat-up   # deploy and try the mini-chat demo"
	@echo ""
	@echo "  Tip: run 'git status' to inspect changes."
	@echo ""
