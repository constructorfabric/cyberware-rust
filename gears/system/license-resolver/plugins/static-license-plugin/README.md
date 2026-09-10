# Static License Plugin

License resolver backend whose grant facts come from configuration instead of a licensing service, so a deployment can exercise the whole check path without one.

## Overview

The `cf-gears-static-license-plugin` gear provides:

- **Config-driven grants** — a list of rules, matched against the check
- **Deny by default** — no rule, no grant; an empty list licenses nothing
- **Attribute-based licensing** — rules may constrain `metadata` properties of either contract object
- **Fail-fast configuration** — a rule that could never match aborts startup instead of silently denying
- **Diagnostics** — every decision reports which backend answered, and either the rule that granted or why nothing did

Enable with `--features static-license` on the example server.

## Grant rules

A check is granted when **some** rule matches it in full. Only the two contract types are required; every other field left unset constrains nothing.

```yaml
gears:
  license-resolver:
    config:
      vendor: "constructorfabric"      # must match the plugin's vendor

  static-license-plugin:
    config:
      vendor: "constructorfabric"
      priority: 100                    # lower wins when several plugins match
      grants:
        # Every user of this contract may use any OpenAI model, in any tenant.
        - resource_type: "gts.cf.core.lic.res.v1~cf.genai.llm_gateway.model_usage.v1~"
          subject_type: "gts.cf.core.lic.subj.v1~cf.genai.llm_gateway.user.v1~"
          resource_metadata:
            model_vendor: "openai"

        # One named model, one tenant, internal users only.
        - resource_type: "gts.cf.core.lic.res.v1~cf.genai.llm_gateway.model_usage.v1~"
          resource_id: "gpt-4o"
          subject_type: "gts.cf.core.lic.subj.v1~cf.genai.llm_gateway.user.v1~"
          tenant_id: "00000000-df51-5b42-9538-d2b56b7ee953"
          subject_metadata:
            category: "internal"
```

| Field | Unset means | Set means |
|---|---|---|
| `resource_type` | — (required) | the check's resource contract type must equal it |
| `resource_id` | the whole type is licensed, which also answers a check naming one instance | only that instance is licensed, and never a whole-type check |
| `subject_type` | — (required) | the check's subject contract type must equal it |
| `subject_id` | every subject of the type | only that subject |
| `tenant_id` | every tenant in this deployment | only that tenant |
| `resource_metadata` | resource properties unconstrained | each named property must be present and equal |
| `subject_metadata` | subject properties unconstrained | each named property must be present and equal |

The asymmetry on `resource_id` is deliberate: a licence for the type as a class covers its instances, while a licence for one instance is not a licence for the class — which is what an id-less check asks about.

`resource_metadata` / `subject_metadata` are subset matches: properties the rule does not name are ignored, so a contract can carry richer metadata than any one rule cares about.

## Startup validation

Every rule's contract types must be well-formed GTS type ids derived from the licensing bases (`gts.cf.core.lic.res.v1~` / `…subj.v1~`). A rule naming something else could never match, and a rule that never matches reads as "not licensed" — a denial that hides a typo. The gear aborts startup instead, naming the offending rule index and field.

The plugin does **not** check that the types are registered: the gateway already rejects a check against an unregistered contract before delegating.

## Diagnostics

`LicenseDecision::diagnostics` is advisory — `granted` is authoritative on its own — but it is where an operator looks to find out why:

| Key | On | Value |
|---|---|---|
| `backend` | always | `static-license-plugin` |
| `matched_rule` | a grant | index of the rule that answered |
| `deny_reason` | a denial | `no_grants_configured` or `no_matching_grant` |

`no_grants_configured` distinguishes "this backend was never given any rules" from `no_matching_grant`, "rules exist but none covers this pair" — the two look identical from the boolean alone.

## Testing

```bash
cargo test -p cf-gears-static-license-plugin
```

`tests/gts_registration.rs` reproduces the server's startup GTS commit in-process. That commit is all-or-nothing across every linked crate, so a licensing type or instance that fails it stops the whole server from binding — the test surfaces that here rather than as a boot timeout.

## License

Apache-2.0
