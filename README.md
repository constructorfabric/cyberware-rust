# Constructor Fabric Gears (Rust)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/constructorfabric/gears-rust/badge)](https://scorecard.dev/viewer/?uri=github.com/constructorfabric/gears-rust) [![OpenSSF Best Practices](https://www.bestpractices.dev/projects/12050/badge)](https://www.bestpractices.dev/projects/12050)

<p align="center">
  <img src="docs/img/gears.png" style='max-width: 800px' alt="Constructor Fabric Gears diagram">
</p>

**Gears** is a secure, modular XaaS framework and middleware, developed in Rust by the [Constructor Fabric Foundation](https://www.constructorfabric.org). It provides composable building blocks, domain components, and APIs with defense-in-depth security, multi-tenancy, and fine-grained access control built into every layer.

Gears is not a ready-to-use service. Instead, it is a set of well-integrated libraries that XaaS vendors can compose into their own products. Vendors decide which gears to include, how to combine them into services, and where to run them—from edge devices to Kubernetes clusters.

Gears span three broad categories:
- **Core** gears for platform foundations such as API gateway, authentication/authorization, account management, etc;
- **Serverless** gears for functions, workflows, and event-driven execution;
- **GenAI** gears for chat, retrieval, prompt orchestration, and related AI capabilities.

See also:
- [WHY_GEARS](docs/WHY_GEARS.md) explaining why to chose Rust/Gears for your XaaS project.
- [OVERVIEW](docs/slides/1_OVERVIEW.md) HTML slides explaining the key Constructor Fabric Gears concepts.
- [GEARS](docs/GEARS.md) for gears overview.

**Five defining Gears characteristics:**

1. **Secure XaaS framework with defense-in-depth** — Every API handler enforces authentication, authorization, tenant isolation, and scoped DB access by default. Security is structural, not opt-in, validated at build time using integrated dynamic lints.

2. **Three-tier gear hierarchy** — *Gears Toolkit* (`libs/` — ToolKit, DB access, error model, API middleware), *System gears* (`gears/system/` — API gateway, authn/authz, tenancy, event system, resource groups, type registry), and *Service gears* (`gears/` — serverless runtime, GenAI subsystems, and domain-specific libraries).

3. **Composable libraries, vendor-controlled deployment** — Each gear owns its API surface and database, communicates via a Rust-native SDK that facades local vs. remote calls, and is fully infrastructure-agnostic. Vendors choose which gears to bundle and whether to deploy single-process (edge/on-prem), multi-node (bare metal), or on Kubernetes.

4. **Pre-integrated XaaS backbone** — Deep integration with multi-tenancy, licensing and quota management, usage collection, and event systems. Gears provides its own backbone capabilities, but each can be replaced or integrated with existing vendor infrastructure via plugins (e.g. subscription management, product catalog, provisioning, or license enforcement).

5. **Extensible domain model via Global Type System** — Gears expose extensible domain objects whose metadata and types are customizable through [GTS](https://github.com/globaltypesystem/gts-spec) — define new event types, user settings, LLM model attributes, etc. CRUD API handlers support customization via hooks and callbacks as serverless functions and workflows.

**Engineering principles:**
- **Spec-Driven Development**: [Specification templates](docs/spec-templates/README.md) (PRD, Design, ADR, Feature) define what gets built *before* code is written. Every gear is well documented.
- **Shift Left**: Custom architecture lints (via `cargo gears lint`) enforce design rules at compile time, alongside Clippy, [tests](#testing), fuzzing, and security audits in CI
- **Quality First**: 90%+ test coverage target with unit, integration, E2E, performance, and security testing
- **Core in Rust**: Compile-time safety, deep static analysis including project-specific lints, so more issues are prevented before review/runtime
- **Monorepo**: All the core gears and contracts in one place for atomic refactors, consistent tooling/CI, and realistic local build + E2E testing

See the full architecture [MANIFEST](docs/ARCHITECTURE_MANIFEST.md) for more details, including rationales behind Rust and Monorepo choice.

See also [REPO_PLAYBOOK](docs/REPO_PLAYBOOK.md) with the registry of repository-wide artifacts (guidelines, rules, conventions, etc).

## Quick Start

### Prerequisites

- Rust stable with Cargo ([Install via rustup](https://rustup.rs/))
- Protocol Buffers compiler (`protoc`):
  - macOS: `brew install protobuf`
  - Linux: `apt-get install protobuf-compiler`
  - Windows: Download from https://github.com/protocolbuffers/protobuf/releases
- MariaDB/PostgreSQL/SQLite or in-memory database

### CI/Development Commands

```bash
# Clone the repository
git clone --recurse-submodules <repository-url>
cd gears-rust

make build      # Build the whole-project example server release binary
make test       # Run workspace tests
make check      # Run formatting, validation, linting, security, docs, and tests
make all        # Build + check + SQLite integration + local E2E + OpenAPI
make run        # Run the default example server
make example    # Run the example server with the default E2E feature set
```

Most top-level development targets can also be scoped to one gear:

```bash
make build GEAR=file-parser      # Build cf-gears-file-parser and its SDK crate
make test GEAR=file-parser       # Test cf-gears-file-parser and its SDK crate
make run GEAR=file-parser        # Run the example server with only this gear feature set
make e2e-local SUITE=file-parser  # Run this one E2E suite plus the default E2E target
make e2e-local GEAR=credstore    # Run every shared-server E2E suite that exercises a gear
make coverage GEAR=file-parser   # Collect unit + E2E coverage for this gear scope
```

`GEAR=<name>` defaults package names to `cf-gears-<name>` and `cf-gears-<name>-sdk`. Use `GEAR_PKG`, `GEAR_SDK_PKG`, `GEAR_FEATURES`, `GEAR_BUILD_ARGS`, `GEAR_TEST_ARGS`, or `GEAR_RUN_ARGS` when a gear uses non-default package names or needs extra Cargo/server flags.

### Running the Server

The Gears repository comes with an example server illustrating the gears APIs:

```bash
# Run an example server, see the API docs @ http://127.0.0.1:8087/cf/docs
make example

# See API documentation:
# $ make example
# visit: http://127.0.0.1:8087/cf/docs

# Readiness (detailed JSON) and liveness ("ok"). The example runs with prefix_path=/cf,
# and in the default health.serve=main mode the probes ride the main listener, so they
# sit under the same prefix as every other route.
curl http://127.0.0.1:8087/cf/health     # detailed JSON, all component checks
curl http://127.0.0.1:8087/cf/readyz      # readiness: 200 ready / 503 not ready
curl http://127.0.0.1:8087/cf/healthz     # liveness: "ok"
```

Other quick start examples:

```bash
# Option 1: Run with SQLite database (recommended for development)
cargo run --bin cf-gears-example-server -- --config config/quickstart.yaml run

# Option 2: Run without database (no-db mode)
cargo run --bin cf-gears-example-server -- --config config/no-db.yaml run

# Option 3: Run with mock in-memory database for testing
cargo run --bin cf-gears-example-server -- --config config/quickstart.yaml --mock run
```

### Example Configuration (config/quickstart.yaml)

```yaml
# Constructor Fabric Gears Configuration

# Core server configuration (global section)
server:
  home_dir: "~/.cf-gears"

# Database configuration (global section)
database:
  url: "sqlite://database/database.db"
  max_conns: 10
  busy_timeout_ms: 5000

# Logging configuration (global section)
logging:
  default:
    console_level: info
    file: "logs/cf-gears.log"
    file_level: warn
    max_age_days: 28
    max_backups: 3
    max_size_mb: 1000

# Per-gear configurations moved under gears section
gears:
  api_gateway:
    bind_addr: "127.0.0.1:8087"
    enable_docs: true
    cors_enabled: false
```

### Creating Your First Gear

See [TOOLKIT UNIFIED SYSTEM](docs/toolkit_unified_system/README.md) and [TOOLKIT_PLUGINS.md](docs/TOOLKIT_PLUGINS.md) for details.

Individual gears live under `gears/<name>/`, usually with a gear crate and an SDK crate. The Makefile builds a single gear by selecting those packages, while `make run GEAR=<name>` composes the example server with `--no-default-features` and the gear feature plus the static local development system gears. Runtime settings stay in YAML under `gears:<gear_name>:` with separate `config` and optional gear-owned `database` sections.

## Documentation

- **[Architecture manifest](docs/ARCHITECTURE_MANIFEST.md)** - High-level overview of the architecture
- **[Gears](docs/GEARS.md)** - List of all gears and their roles
- **[TOOLKIT UNIFIED SYSTEM](docs/toolkit_unified_system/README.md) and [TOOLKIT_PLUGINS.md](docs/TOOLKIT_PLUGINS.md)** - how to add new gears.
- **[Contributing](CONTRIBUTING.md)** - Development workflow and coding standards

## Security

Gears apply defense-in-depth security across the entire development lifecycle — from Rust's compile-time safety guarantees and custom architectural lints, through compile-time tenant isolation and PDP/PEP authorization enforcement, to continuous fuzzing, dependency auditing, and automated security scanning in CI.

See **[Security Overview](docs/security/SECURITY.md)** for the full breakdown, including: Secure ORM with compile-time tenant scoping, authentication/authorization architecture (NIST SP 800-162 PDP/PEP model), 90+ Clippy deny-level rules, custom architecture lints (via `cargo gears lint`), cargo-deny advisory checks, ClusterFuzzLite continuous fuzzing, CodeQL/Scorecard/Snyk/Aikido scanners, and AI-powered PR review bots.

## FIPS 140-3 support

Built with `--features fips`, Gears route every TLS data-path cryptographic operation through a **CMVP-validated cryptographic module** — AWS-LC FIPS (Linux), Apple corecrypto (macOS), or Microsoft Windows CNG (Windows) — behind a single `rustls 0.23` state machine. Gears are *consumers* of those validated modules, not a CMVP-listed module themselves.

See **[Security Overview §9 — Cryptographic Stack & FIPS-140-3](docs/security/SECURITY.md#9-cryptographic-stack--fips-140-3)** for algorithm scope, build prerequisites, runtime/verification gates, and the full "what this does and does not claim" breakdown.

## Configuration

### YAML Configuration Structure

```yaml
# config/server.yaml

# Global server configuration
server:
  home_dir: "~/.cf-gears"

# Database configuration
database:
  servers:
    sqlite_users:
      params:
        WAL: "true"
        synchronous: "NORMAL"
        busy_timeout: "5000"
      pool:
        max_conns: 5
        acquire_timeout: "30s"

# Logging configuration
logging:
  default:
    console_level: info
    file: "logs/cf-gears.log"
    file_level: warn
    max_age_days: 28
    max_backups: 3
    max_size_mb: 1000

# Per-gear configuration
gears:
  api_gateway:
    config:
      bind_addr: "127.0.0.1:8087"
      enable_docs: true
      cors_enabled: true
  users_info:
    database:
      server: "sqlite_users"
      file: "users_info.db"
    config:
      default_page_size: 5
      max_page_size: 100
```

### Environment Variable Overrides

Configuration supports environment variable overrides with `CF_` prefix:

```bash
export CF_GEARS_DATABASE_URL="postgres://user:pass@localhost/db"
export CF_GEARS_API_GATEWAY_BIND_ADDR="0.0.0.0:8080"
export CF_GEARS_LOGGING_DEFAULT_CONSOLE_LEVEL="debug"
```

## Testing

```bash
make check           # full quality gate (fmt + clippy + test + security)
```

Other tests:

```bash
make test            # unit tests (workspace)
make test GEAR=file-parser  # unit tests for one gear and its SDK crate
make test-sqlite     # integration tests (SQLite, no external DB required)
make e2e-local       # end-to-end tests (builds + starts server automatically)
make e2e-local SUITE=file-parser  # suite E2E scope plus the default E2E target
make e2e-docker      # end-to-end tests (builds + starts server in Docker)
make coverage-unit   # unit test code coverage
make coverage GEAR=file-parser  # unit + E2E coverage for one gear scope
make fuzz            # fuzz smoke tests (30 s per target)
```

On **Windows** (no `make`), use the cross-platform Python scripts directly
(invoke them with `python`; `python3` is often absent on Windows):

```bash
python tools/scripts/ci.py check          # full CI suite
python tools/scripts/ci.py e2e-local      # end-to-end tests
python tools/scripts/ci.py e2e-local -- testing/e2e/suites/file_parser/  # targeted end-to-end scope
python tools/scripts/ci.py fuzz --seconds 60  # fuzz smoke run

python tools/scripts/coverage.py unit      # unit-test code coverage
python tools/scripts/coverage.py combined  # unit + e2e-local coverage
```

These scripts need a few prerequisites on Windows:

```bash
pip install -r testing/e2e/requirements.txt   # pytest + httpx (e2e tests)
pip install -r testing/requirements.txt        # PyYAML + requests (coverage.py)
cargo install cargo-llvm-cov                    # coverage backend (or run `make setup`)
```

For the complete test strategy, coverage policy, CI pipeline details, and all
available commands see **[docs/TESTING.md](docs/TESTING.md)**.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for detailed guidelines.

## License

This project is licensed under the Apache 2.0 License - see the [LICENSE](LICENSE) file for details.
