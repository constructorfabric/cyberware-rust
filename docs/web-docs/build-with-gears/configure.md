---
title: Configure a Gears application
description: The YAML runtime configuration model — global sections, per-gear config, environment overrides, and where the CLI manifest fits.
sidebar:
  label: Configure a Gears application
  order: 13
---

A Gears application is configured with YAML. Global sections cover the server, database, and
logging; each gear reads typed configuration under `gears.<name>`. Deployment shape,
observability, and gear behavior are all configuration decisions rather than code changes.

## Structure

```yaml
# Global sections
server:
  home_dir: "~/.cf-gears"

database:
  # Reusable connection templates that gears can inherit
  servers:
    sqlite_users:
      engine: "sqlite"
      params:
        WAL: "true"
        busy_timeout: "5000"
      pool:
        max_conns: 5
        acquire_timeout: "30s"

logging:
  default:
    console_level: info
    console_format: text # or: json (one object per line, for container log collectors)
    file: "logs/cf-gears.log"
    file_level: info

# Per-gear configuration
gears:
  api-gateway:
    config:
      bind_addr: "127.0.0.1:8087"
      enable_docs: true
      prefix_path: "/cf"
  my-gear:
    runtime:
      type: local   # or: oop
```

## Common knobs

- **API gateway** — `bind_addr`, `enable_docs`, `prefix_path`, CORS, rate limits, timeouts.
- **Deployment shape** — `gears.<name>.runtime.type: local | oop` selects in-process vs
  out-of-process. See [Run a gear out-of-process](../out-of-process/).
- **Logging** — `logging.default.console_level` and `console_format: text | json`.
  Every JSON line (console and file) carries top-level `service` and `version`
  keys from `opentelemetry.resource.{service_name,service_version}`
  (`version` falls back to an `attributes."service.version"` entry; an empty
  `service_name` omits both keys). `text` output is for local development —
  one known binary in a terminal — and carries no identity keys; collectors
  parse `json`.
- **Tracing** — a `tracing:` block points at an OTLP backend. See
  [Add observability](../add-observability/).
- **Database** — `database.servers.<name>` connection templates (`engine`, `params`, `pool`) that gears inherit; SQLite, PostgreSQL, and MariaDB engines are supported.

## Environment overrides

Settings can be overridden by environment variables for production, e.g. `APP__TRACING__EXPORTER__ENDPOINT=...`. Select which config to run with `--app` / `--env` or `GEARS_CONFIG`.

## Config and the CLI manifest

When you use the CLI, configuration is generated and managed from your project manifest:

```sh
cargo gears generate config --template dev --app app1 --env dev
cargo gears config mod add <gear-name> -c config/app1-dev.yml
```

The `Gears.toml` manifest format and every config command are documented in the **[CLI section](/cli/)** (see [Gears.toml manifest](/cli/manifest/)).

## See also

- [Install and run](../) — the quickstart config in context.
- [cargo gears CLI](../cargo-gears-cli/) — manifest-driven config management.
- [Deploy Gears](../deploy/) — configuration per deployment shape.
