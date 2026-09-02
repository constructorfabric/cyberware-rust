// Updated: 2026-04-16 by Constructor Tech
// This file is used to ensure that all gears are linked and registered via inventory
// In future we can simply DX via build.rs which will collect all crates in ./gears and generate this file.
// But for now we will manually maintain this file.
#![allow(unused_imports)]

#[cfg(feature = "oagw")]
use api_egress as _;
use api_gateway as _;
use authn_resolver as _;
use authz_resolver as _;
#[cfg(feature = "credstore")]
use credstore as _;
#[cfg(all(feature = "file-parser", not(feature = "oop-example")))]
use file_parser as _;
#[cfg(feature = "file-storage")]
use file_storage as _;
use gear_orchestrator as _;
#[cfg(feature = "github-mirror")]
use github_mirror as _;
#[cfg(feature = "grpc-hub")]
use grpc_hub as _;
#[cfg(feature = "nodes-registry")]
use nodes_registry as _;
#[cfg(feature = "resource-group")]
use resource_group as _;
#[cfg(all(feature = "simple-user-settings", not(feature = "oop-example")))]
use simple_user_settings as _;
use tenant_resolver as _;
use types_registry as _;

#[cfg(feature = "single-tenant")]
use single_tenant_tr_plugin as _;

#[cfg(feature = "static-tenants")]
use static_tr_plugin as _;

#[cfg(feature = "tenant-resolver-rg")]
use rg_tr_plugin as _;

#[cfg(feature = "static-authn")]
use static_authn_plugin as _;

#[cfg(feature = "static-authz")]
use static_authz_plugin as _;

#[cfg(feature = "tr-authz")]
use tr_authz_plugin as _;

#[cfg(feature = "static-credstore")]
use static_credstore_plugin as _;

// === Optional Gears ===

#[cfg(feature = "mini-chat")]
use mini_chat as _;

#[cfg(feature = "mini-chat")]
use mini_chat::infra::plugins::static_audit as _;

#[cfg(feature = "mini-chat")]
use mini_chat::infra::plugins::static_model_policy as _;

#[cfg(feature = "chat-engine")]
use chat_engine as _;

// === Example Features ===

#[cfg(feature = "users-info-example")]
use users_info as _;

#[cfg(feature = "oop-example")]
use calculator_gateway as _;

#[cfg(feature = "oop-example")]
use calculator as _;

#[cfg(feature = "static-idp")]
use static_idp_plugin as _;

#[cfg(feature = "account-management")]
use account_management as _;

#[cfg(feature = "bss-rate-provider")]
use bss_rate_provider as _;
#[cfg(feature = "bss-rate-provider")]
use bss_rate_provider_ecb_plugin as _;
#[cfg(feature = "bss-rate-provider")]
use bss_rate_provider_http_json_plugin as _;

#[cfg(feature = "bss-ledger")]
use bss_ledger as _;

#[cfg(feature = "usage-collector")]
use usage_collector as _;

#[cfg(feature = "timescaledb-usage-collector")]
use timescaledb_usage_collector_plugin as _;

#[cfg(feature = "quota-enforcement")]
use quota_enforcement as _;
#[cfg(feature = "quota-enforcement")]
use quota_enforcement_coordination_plugin as _;
#[cfg(feature = "quota-enforcement")]
use quota_enforcement_storage_plugin as _;

#[cfg(feature = "bss-pricing")]
use bss_pricing as _;
