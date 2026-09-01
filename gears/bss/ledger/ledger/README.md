# BSS Billing Ledger

The BSS Billing Ledger is the platform's double-entry accounting gear. It records invoices,
payments, allocations, credits, refunds, disputes, revenue recognition, foreign-exchange
adjustments, reconciliation, and period close as balanced journal entries.

## Guarantees

The gear keeps the correctness-critical accounting rules in a shared posting foundation:

- every committed journal entry is balanced;
- journal lines are append-only, and corrections use reversing entries;
- posting is idempotent, so replaying a request cannot double-post it;
- money is represented in integer minor units with an explicit currency scale;
- balance and journal mutations commit atomically; and
- authorization scopes reads and writes to the permitted tenant resources.

Business flows build balanced lines and post them through this foundation rather than
writing ledger tables directly.

## Architecture

`BssLedgerGear` provides the ToolKit `db`, `rest`, and `stateful` capabilities. During
initialization it runs the ledger migrations, wires the posting and inquiry services,
registers `bss_ledger_sdk::LedgerClientV1` in `ClientHub`, and exposes the REST API.
Its managed background jobs perform tie-outs, fiscal-period maintenance, deferred
allocation, revenue recognition, audit-chain verification, FX synchronization and
revaluation, and reconciliation.

The implementation is divided into these internal layers:

- `domain` contains accounting rules, money types, posting models, and service ports;
- `infra` contains orchestration, repositories, migrations, events, metrics, and jobs;
- `api` contains the in-process SDK adapter and REST handlers; and
- `module` owns ToolKit lifecycle and dependency wiring.

These modules are documented for maintainers and integration authors, but they are not
the stable inter-gear contract. Other gears should depend on `bss-ledger-sdk` and obtain
`LedgerClientV1` from `ClientHub` instead of coupling to implementation internals.

## In-process usage

After the gear has initialized, consumers resolve its SDK contract from `ClientHub`. This
example reads the first 50 chart-of-accounts entries visible to the supplied security
context:

```no_run
use std::error::Error;

use bss_ledger_sdk::{LedgerClientV1, ODataQuery};
use toolkit::ClientHub;
use toolkit_security::SecurityContext;
use uuid::Uuid;

async fn list_accounts(
    hub: &ClientHub,
    context: &SecurityContext,
    tenant_id: Uuid,
) -> Result<(), Box<dyn Error>> {
    let ledger = hub.get::<dyn LedgerClientV1>()?;
    let query = ODataQuery::new().with_limit(50);
    let page = ledger.list_accounts(context, tenant_id, &query).await?;

    for account in page.items {
        println!(
            "{}: {} {}",
            account.account_id, account.account_class, account.currency
        );
    }

    Ok(())
}
```

The SDK also exposes posting, provisioning, payment allocation, balance inquiry, revenue
recognition, dispute, and period-close contracts.

## Configuration

The gear requires a database configuration and one behavioral setting:
`seller_tenant_types`. Everything else has a default (background-job cadences,
recognition, FX, reconciliation, payment limits).

`seller_tenant_types` names the tenant types whose tenants own a billing ledger.
There is deliberately **no default** — tenant-type identifiers come from the
deployment's own catalogue, so any built-in list would name one product and
silently reject every seller elsewhere. An empty or malformed list fails at
`init()` with the offending field named, rather than turning every provisioning
request into a "not a ledger owner" rejection.

```yaml
gears:
  bss-ledger:
    database:
      server: "postgres"
      dbname: "bss_ledger"
    config:
      # Concrete chained type ids — the usual case, where only some tenant
      # types own a ledger (a buyer/leaf type is simply left out).
      seller_tenant_types:
        - "gts.cf.core.am.tenant_type.v1~cf.core.am.platform.v1~"
        - "gts.cf.core.am.tenant_type.v1~cf.core.am.partner.v1~"
```

Substitute the identifiers your Account Management catalogue actually registers
(`GET /types-registry/v1/entities` lists them; the ids above are the shapes AM's
own fixtures use). Entries are matched literally, so a typo means a rejected
seller — hence the shape check at startup.

A deployment where **every** registered tenant type owns a ledger configures one
family wildcard instead of enumerating the catalogue:

```yaml
    config:
      seller_tenant_types:
        - "gts.cf.core.am.tenant_type.v1~*"
```

A wildcard matches by prefix with its separator retained, so
`…v1~cf.core.am.partner.*` covers that family's versions without also matching
`…partnership…`. The bare `gts.*` is rejected: "every type of every vendor owns
a ledger" is never a deliberate predicate.

Event publication is disabled by default until the Event Broker integration is enabled.

## Further reading

- [Technical design](https://github.com/constructorfabric/gears-rust/blob/main/gears/bss/ledger/docs/DESIGN.md)
- [Product requirements](https://github.com/constructorfabric/gears-rust/blob/main/gears/bss/ledger/docs/PRD.md)
- [Design slices](https://github.com/constructorfabric/gears-rust/tree/main/gears/bss/ledger/docs/design)

## License

Apache-2.0
