//! Transport and credential-provider variants in the profile schema (T-038 /
//! D-008).
//!
//! D-008 ships direct connections only, but models two things in the schema
//! now because the schema is versioned and expensive to migrate:
//!
//! - a **transport** — `direct` | `ssh-tunnel` | `proxy` — so built-in
//!   tunnelling later is a new variant, not a format break;
//! - a credential **provider** — `static` | `keychain` | `env` | `iam-token` —
//!   rather than a password string, because an IAM token that expires in
//!   fifteen minutes is a provider that must be asked again mid-session, not a
//!   string fetched once.
//!
//! In the file they are the `transport` and `auth` keys of a profile
//! (`auth`, not `credential`, so that no metadata shape the webview sees uses
//! the word — the IPC boundary test bans it outright).
//!
//! Neither type can hold a secret: a provider says *where* the credential
//! comes from, never what it is (D-007). Both default to today's behaviour
//! (`direct`, `keychain`) and are omitted from the file at that default, so a
//! plain profile stays a plain profile.

use serde::{Deserialize, Serialize};

/// T-075 / D-022: which database engine a profile connects to.
///
/// Carried on every profile, imported URL and discovered connection, so the
/// app core can pick the driver from the profile rather than assuming one.
/// Absent from an older file means `postgres` — the only engine those files
/// could have meant. An unrecognised value is a load error that names it,
/// never a silent fallback to Postgres.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    #[default]
    Postgres,
    Sqlserver,
    Mongodb,
}

impl Engine {
    pub const ALL: [Engine; 3] = [Engine::Postgres, Engine::Sqlserver, Engine::Mongodb];

    /// The engine a connection-URL scheme names, if it names one Sluice has
    /// an engine for. `mysql`, `sqlite` and the like are recognised as
    /// database URLs elsewhere but have no engine here.
    pub fn from_scheme(scheme: &str) -> Option<Engine> {
        match scheme.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" => Some(Engine::Postgres),
            "sqlserver" | "mssql" => Some(Engine::Sqlserver),
            "mongodb" | "mongodb+srv" => Some(Engine::Mongodb),
            _ => None,
        }
    }

    /// The id as written in the profile file and sent to the webview.
    pub fn id(self) -> &'static str {
        match self {
            Engine::Postgres => "postgres",
            Engine::Sqlserver => "sqlserver",
            Engine::Mongodb => "mongodb",
        }
    }

    /// The product name, for messages.
    pub fn label(self) -> &'static str {
        match self {
            Engine::Postgres => "Postgres",
            Engine::Sqlserver => "SQL Server",
            Engine::Mongodb => "MongoDB",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Engine::Postgres => 5432,
            Engine::Sqlserver => 1433,
            Engine::Mongodb => 27017,
        }
    }

    pub fn is_postgres(&self) -> bool {
        *self == Engine::Postgres
    }
}

/// How the connection reaches the server.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Transport {
    /// TCP (or a Unix socket path as the host) straight to the server.
    //
    // Field-less variants are written `{}` rather than as unit variants on
    // purpose: serde's internally-tagged unit variants silently accept extra
    // keys, and `deny_unknown_fields` is what makes `password = "…"` beside a
    // provider an error instead of a stored secret.
    Direct {},
    /// Not built yet (D-008). When it is, `known_hosts` verification is
    /// mandatory and trust-on-first-use only with the fingerprint shown and
    /// the answer persisted.
    SshTunnel {
        host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
    },
    /// Not built yet (D-008): a socket proxy such as Cloud SQL Auth Proxy.
    Proxy { address: String },
}

impl Default for Transport {
    fn default() -> Self {
        Transport::Direct {}
    }
}

impl Default for CredentialProvider {
    fn default() -> Self {
        CredentialProvider::Keychain {}
    }
}

impl Transport {
    pub fn is_direct(&self) -> bool {
        matches!(self, Transport::Direct {})
    }

    pub fn label(&self) -> &'static str {
        match self {
            Transport::Direct {} => "direct",
            Transport::SshTunnel { .. } => "ssh-tunnel",
            Transport::Proxy { .. } => "proxy",
        }
    }
}

/// Where the credential comes from. Never the credential itself.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "provider", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CredentialProvider {
    /// Named by D-008; its semantics are not decided yet, so it is modelled
    /// but not implemented.
    Static {},
    /// The OS keychain, under the profile's id (D-007). Today's behaviour.
    Keychain {},
    /// An environment variable holding the password, read at connect time
    /// and wrapped in `Secret` at that boundary (R-017). The file stores only
    /// the variable's *name*.
    Env { var: String },
    /// Not built yet (D-008): a short-lived cloud IAM token, refreshed by the
    /// provider rather than stored.
    IamToken {},
}

impl CredentialProvider {
    pub fn is_keychain(&self) -> bool {
        matches!(self, CredentialProvider::Keychain {})
    }

    pub fn label(&self) -> &'static str {
        match self {
            CredentialProvider::Static {} => "static",
            CredentialProvider::Keychain {} => "keychain",
            CredentialProvider::Env { .. } => "env",
            CredentialProvider::IamToken {} => "iam-token",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct P {
        #[serde(default)]
        transport: Transport,
        #[serde(default)]
        auth: CredentialProvider,
    }

    fn parse(s: &str) -> Result<P, toml::de::Error> {
        toml::from_str(s)
    }

    #[test]
    fn every_d008_variant_parses() {
        let p = parse("transport = { kind = \"ssh-tunnel\", host = \"bastion\", port = 22 }\nauth = { provider = \"env\", var = \"PGPASS_STAGING\" }\n").unwrap();
        assert_eq!(p.transport, Transport::SshTunnel { host: "bastion".into(), port: Some(22), user: None });
        assert_eq!(p.auth, CredentialProvider::Env { var: "PGPASS_STAGING".into() });

        for (t, label) in [("direct", "direct"), ("proxy\", address = \"127.0.0.1:5433", "proxy")] {
            let p = parse(&format!("transport = {{ kind = \"{t}\" }}\n")).unwrap();
            assert_eq!(p.transport.label(), label);
        }
        for c in ["static", "keychain", "iam-token"] {
            let p = parse(&format!("auth = {{ provider = \"{c}\" }}\n")).unwrap();
            assert_eq!(p.auth.label(), c);
        }
    }

    #[test]
    fn defaults_are_direct_and_keychain() {
        let p = parse("").unwrap();
        assert!(p.transport.is_direct());
        assert!(p.auth.is_keychain());
    }

    /// The schema is a provider *rather than a password string* — so there is
    /// nowhere to put one, and trying is an error rather than a stored secret.
    #[test]
    fn a_provider_cannot_carry_a_password() {
        assert!(parse("auth = { provider = \"keychain\", password = \"hunter2\" }\n").is_err());
        assert!(parse("auth = { provider = \"static\", value = \"hunter2\" }\n").is_err());
        assert!(parse("auth = \"hunter2\"\n").is_err());
    }

    #[derive(Deserialize, Debug)]
    struct E {
        #[serde(default)]
        engine: Engine,
    }

    #[test]
    fn engine_defaults_to_postgres_and_unknown_engines_are_named_errors() {
        assert_eq!(toml::from_str::<E>("").unwrap().engine, Engine::Postgres);
        for e in Engine::ALL {
            assert_eq!(toml::from_str::<E>(&format!("engine = \"{}\"\n", e.id())).unwrap().engine, e);
        }
        let err = toml::from_str::<E>("engine = \"mysql\"\n").unwrap_err().to_string();
        assert!(err.contains("mysql"), "{err}");
    }

    #[test]
    fn schemes_map_to_their_engine() {
        for (scheme, e) in [
            ("postgres", Some(Engine::Postgres)),
            ("postgresql", Some(Engine::Postgres)),
            ("sqlserver", Some(Engine::Sqlserver)),
            ("MSSQL", Some(Engine::Sqlserver)),
            ("mongodb", Some(Engine::Mongodb)),
            ("mongodb+srv", Some(Engine::Mongodb)),
            ("mysql", None),
            ("sqlite", None),
        ] {
            assert_eq!(Engine::from_scheme(scheme), e, "{scheme}");
        }
    }

    #[test]
    fn unknown_variants_are_errors_not_defaults() {
        assert!(parse("transport = { kind = \"vpn\" }\n").is_err());
        assert!(parse("auth = { provider = \"vault\" }\n").is_err());
    }
}
