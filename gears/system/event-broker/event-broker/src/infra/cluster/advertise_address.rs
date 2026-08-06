//! Advertise-address resolution for `DirectoryService` self-registration
//! (`eb-dispatcher-routing` design.md D5). A seam, not inlined at the
//! registration call site, so swapping in a real address-exposure mechanism
//! (once `gears-rust#4392` or an api-gateway capability lands) is a
//! one-line change - see [`AdvertiseAddressResolver`].

use std::net::SocketAddr;

use anyhow::Context;

use crate::config::RegistrationConfig;

/// Resolves the address an instance advertises to `DirectoryService`. The
/// sole implementation today, [`ConfigAdvertiseAddress`], is config-based;
/// swap in a different one once a real address-exposure mechanism exists.
pub trait AdvertiseAddressResolver {
    /// Returns the address to advertise, or an error if none can be
    /// determined - an unroutable bind with no explicit `advertise_addr`
    /// fails rather than silently skipping registration (design.md D5).
    fn resolve(&self, bound_addr: SocketAddr) -> anyhow::Result<String>;
}

/// Config-based resolver mirroring `grpc-hub`'s `listen_addr`/
/// `advertise_addr` pattern (`gears/system/grpc-hub/src/gear.rs`'s
/// `tcp_directory_endpoint`), except erroring on a wildcard bind with no
/// `advertise_addr` instead of silently skipping registration.
pub struct ConfigAdvertiseAddress<'a> {
    pub(crate) config: &'a RegistrationConfig,
}

impl AdvertiseAddressResolver for ConfigAdvertiseAddress<'_> {
    fn resolve(&self, bound_addr: SocketAddr) -> anyhow::Result<String> {
        if let Some(advertise_addr) = &self.config.advertise_addr {
            let (host, port) = parse_advertise_addr(advertise_addr)?;
            let resolved_port = port.unwrap_or(bound_addr.port());
            return Ok(format!("http://{host}:{resolved_port}"));
        }
        if !bound_addr.ip().is_unspecified() {
            return Ok(format!("http://{bound_addr}"));
        }
        anyhow::bail!(
            "cannot register with DirectoryService: bound to a wildcard address \
             ({bound_addr}) with no `registration.advertise_addr` configured - set one to a \
             routable host"
        )
    }
}

/// Parses `host[:port]`; `:0` (or an omitted port) means "use the actual
/// bound port". Mirrors `grpc-hub`'s `parse_advertise_addr` exactly,
/// including its `rsplit_once(':')` limitation on IPv6 literals.
fn parse_advertise_addr(advertise_addr: &str) -> anyhow::Result<(String, Option<u16>)> {
    if let Some((host, port_str)) = advertise_addr.rsplit_once(':') {
        let port = port_str
            .parse::<u16>()
            .with_context(|| format!("invalid port in advertise_addr '{advertise_addr}'"))?;
        Ok((host.to_owned(), if port == 0 { None } else { Some(port) }))
    } else {
        Ok((advertise_addr.to_owned(), None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec "Advertise-address resolution", "Explicit `advertise_addr` configured".
    #[test]
    fn explicit_advertise_addr_with_port_wins_over_bound_port() {
        let config = RegistrationConfig {
            listen_addr: "0.0.0.0:0".to_owned(),
            advertise_addr: Some("example.internal:9000".to_owned()),
        };
        let bound: SocketAddr = "10.0.0.5:4000".parse().unwrap();

        let resolved = ConfigAdvertiseAddress { config: &config }
            .resolve(bound)
            .expect("explicit advertise_addr resolves");

        assert_eq!(resolved, "http://example.internal:9000");
    }

    /// Spec "Advertise-address resolution", "Explicit `advertise_addr` configured"
    /// (host-only form: the actual bound port is appended).
    #[test]
    fn explicit_advertise_addr_without_port_uses_bound_port() {
        let config = RegistrationConfig {
            listen_addr: "0.0.0.0:0".to_owned(),
            advertise_addr: Some("example.internal".to_owned()),
        };
        let bound: SocketAddr = "10.0.0.5:4000".parse().unwrap();

        let resolved = ConfigAdvertiseAddress { config: &config }
            .resolve(bound)
            .expect("host-only advertise_addr resolves");

        assert_eq!(resolved, "http://example.internal:4000");
    }

    /// Spec "Advertise-address resolution", "No `advertise_addr`, concrete bind address".
    #[test]
    fn no_advertise_addr_falls_back_to_concrete_bind_address() {
        let config = RegistrationConfig {
            listen_addr: "10.0.0.5:4000".to_owned(),
            advertise_addr: None,
        };
        let bound: SocketAddr = "10.0.0.5:4000".parse().unwrap();

        let resolved = ConfigAdvertiseAddress { config: &config }
            .resolve(bound)
            .expect("a concrete bind address resolves on its own");

        assert_eq!(resolved, "http://10.0.0.5:4000");
    }

    /// Spec "Advertise-address resolution", "No `advertise_addr`, wildcard bind
    /// address, cluster mode" - errors rather than silently skipping
    /// registration (design.md D5).
    #[test]
    fn no_advertise_addr_with_wildcard_bind_errors() {
        let config = RegistrationConfig {
            listen_addr: "0.0.0.0:8080".to_owned(),
            advertise_addr: None,
        };
        let bound: SocketAddr = "0.0.0.0:8080".parse().unwrap();

        let err = ConfigAdvertiseAddress { config: &config }
            .resolve(bound)
            .expect_err("a wildcard bind with no advertise_addr must not resolve silently");

        assert!(
            format!("{err:#}").contains("wildcard address"),
            "error: {err:#}"
        );
    }
}
