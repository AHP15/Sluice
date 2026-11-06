//! The guardrail field set every session carries (T-027 / R-002, D-012).
//!
//! R-002: the production flag, the database-enforced safety mode, and the
//! history default were all properties of a saved *profile*, and a connection
//! discovered from the working directory has no profile — so `cd ops-repo &&
//! sluice` against a production `.env` would get none of them. The repair is
//! that **every** session, however it was opened, is materialized with the same
//! field set, and a discovered one is defaulted conservatively:
//!
//! - history **off** (D-014's production default, applied before we know);
//! - safety mode **unknown** — never assumed permissive;
//! - the production confirmation path **armed** for any non-loopback host or
//!   any production-looking host or database name.
//!
//! The field set lives in the core, keyed by session, so the features that
//! *enforce* it read it from here: typed confirmation (T-061) and history
//! (T-035). Until they land, no session of any provenance is enforced — the
//! point of this module is that a discovered session can never be the one that
//! slips through when they do.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// How a session's connection details were obtained.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Typed into the connect form.
    Manual,
    /// Found by inert discovery in a working directory.
    Discovered,
    /// Produced by running a project's TS/JS config under consent.
    Evaluated,
    /// Passed as a URL on the command line.
    CommandLine,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HistoryPolicy {
    On,
    Off,
}

/// Whether the database itself enforces read-only access (D-027). Probing the
/// role is T-061's; until then nothing is known, and the UI must say so.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SafetyMode {
    Unknown,
}

/// D-027's production flag: typed confirmation for DDL and unqualified
/// DELETE/UPDATE. `reasons` says why it is armed, for the UI.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ProductionGuard {
    pub armed: bool,
    pub reasons: Vec<String>,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Guardrails {
    pub provenance: Provenance,
    /// False until the user saves the session as a profile.
    pub saved: bool,
    pub production: ProductionGuard,
    pub history: HistoryPolicy,
    pub safety_mode: SafetyMode,
}

impl Guardrails {
    /// The conservative defaults for a connection the user did not set up as a
    /// profile — R-002's guard.
    pub fn ephemeral(provenance: Provenance, host: &str, database: &str) -> Self {
        let reasons = production_reasons(host, database);
        Self {
            provenance,
            saved: false,
            production: ProductionGuard { armed: !reasons.is_empty(), reasons },
            history: HistoryPolicy::Off,
            safety_mode: SafetyMode::Unknown,
        }
    }

    /// A connection typed into the form. The production heuristic still runs:
    /// typing a production host by hand is no safer than discovering it.
    pub fn manual(host: &str, database: &str) -> Self {
        let reasons = production_reasons(host, database);
        let armed = !reasons.is_empty();
        Self {
            provenance: Provenance::Manual,
            saved: false,
            production: ProductionGuard { armed, reasons },
            history: if armed { HistoryPolicy::Off } else { HistoryPolicy::On },
            safety_mode: SafetyMode::Unknown,
        }
    }

    /// After "save as profile": normal profile handling, per D-014 — history on
    /// unless the profile is production-flagged. The production flag itself is
    /// kept: saving is not a reason to trust a host more.
    /// T-035: a profile's explicit history choice beats the derived default
    /// (off for a production-flagged profile, on otherwise).
    pub fn with_history(mut self, choice: Option<HistoryPolicy>) -> Self {
        if let Some(h) = choice {
            self.history = h;
        }
        self
    }

    pub fn saved(mut self) -> Self {
        self.saved = true;
        self.history = if self.production.armed { HistoryPolicy::Off } else { HistoryPolicy::On };
        self
    }
}

/// Why a target should be treated as production, if it should.
///
/// Any host that is not provably loopback arms the guard — a hostname cannot be
/// classified as private without resolving it, and a guess in the permissive
/// direction is the failure R-002 describes. Production-looking names arm it
/// even on loopback (an SSH tunnel to production is `localhost`).
pub fn production_reasons(host: &str, database: &str) -> Vec<String> {
    let mut reasons = Vec::new();
    if !is_loopback(host) {
        reasons.push(format!("`{host}` is not a loopback address"));
    }
    if let Some(tok) = production_token(host) {
        reasons.push(format!("host name contains `{tok}`"));
    }
    if let Some(tok) = production_token(database) {
        reasons.push(format!("database name contains `{tok}`"));
    }
    reasons
}

pub fn is_loopback(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    if h.eq_ignore_ascii_case("localhost") || h.to_ascii_lowercase().ends_with(".localhost") {
        return true;
    }
    // A Unix socket directory is local by construction.
    if h.starts_with('/') {
        return true;
    }
    h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

const PRODUCTION_TOKENS: &[&str] = &["prod", "production", "prd", "live"];

fn production_token(name: &str) -> Option<&'static str> {
    name.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .find_map(|t| PRODUCTION_TOKENS.iter().copied().find(|p| *p == t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_discovered_session_is_defaulted_conservatively() {
        let g = Guardrails::ephemeral(Provenance::Discovered, "localhost", "app_dev");
        assert_eq!(g.history, HistoryPolicy::Off);
        assert_eq!(g.safety_mode, SafetyMode::Unknown);
        assert!(!g.saved);
        assert!(!g.production.armed, "{:?}", g.production.reasons);
    }

    #[test]
    fn any_non_loopback_host_arms_the_production_guard() {
        for host in ["db.internal", "10.0.0.5", "192.168.1.20", "db", "my-rds.amazonaws.com", "2001:db8::1"] {
            let g = Guardrails::ephemeral(Provenance::Discovered, host, "app");
            assert!(g.production.armed, "{host}");
        }
        for host in ["localhost", "127.0.0.1", "127.8.9.1", "::1", "[::1]", "/var/run/postgresql", "app.localhost"] {
            let g = Guardrails::ephemeral(Provenance::Discovered, host, "app");
            assert!(!g.production.armed, "{host}: {:?}", g.production.reasons);
        }
    }

    /// An SSH tunnel to production is `localhost`: the name still arms it.
    #[test]
    fn production_names_arm_it_even_on_loopback() {
        for (host, db) in [("localhost", "shop_prod"), ("localhost", "production"), ("127.0.0.1", "live-orders"), ("prod-db.localhost", "x")] {
            let g = Guardrails::ephemeral(Provenance::Discovered, host, db);
            assert!(g.production.armed, "{host}/{db}");
        }
        // Substrings that merely contain the letters do not count.
        let g = Guardrails::ephemeral(Provenance::Discovered, "localhost", "product_catalog");
        assert!(!g.production.armed, "{:?}", g.production.reasons);
    }

    #[test]
    fn saving_turns_history_on_unless_production() {
        let dev = Guardrails::ephemeral(Provenance::Discovered, "localhost", "dev").saved();
        assert!(dev.saved);
        assert_eq!(dev.history, HistoryPolicy::On);

        let prod = Guardrails::ephemeral(Provenance::Discovered, "db.prod", "shop").saved();
        assert!(prod.production.armed, "saving must not disarm the guard");
        assert_eq!(prod.history, HistoryPolicy::Off);
    }

    /// T-035: history defaults off for production and on otherwise, and an
    /// explicit per-profile choice wins either way.
    #[test]
    fn history_defaults_follow_production_and_a_profile_choice_overrides() {
        assert_eq!(Guardrails::manual("db.prod.internal", "shop").history, HistoryPolicy::Off);
        assert_eq!(Guardrails::manual("localhost", "app_dev").history, HistoryPolicy::On);
        let opted_out = Guardrails::manual("localhost", "app_dev").with_history(Some(HistoryPolicy::Off));
        assert_eq!(opted_out.history, HistoryPolicy::Off);
        let opted_in = Guardrails::manual("db.prod.internal", "shop").with_history(Some(HistoryPolicy::On));
        assert_eq!(opted_in.history, HistoryPolicy::On);
        assert_eq!(Guardrails::manual("db.prod.internal", "shop").with_history(None).history, HistoryPolicy::Off);
    }
}
