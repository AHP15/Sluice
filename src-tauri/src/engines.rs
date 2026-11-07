//! The engine factory (T-079 / D-003, D-022).
//!
//! The one place in the app core that knows which driver crate serves which
//! engine. Everything above it — commands, streams, export, editor sessions —
//! holds an `Arc<dyn Driver>` and branches on declared capabilities, never on
//! the engine (T-078).
//!
//! Adding an engine is one arm in [`connect`] (its connect spec, built from
//! the engine-neutral [`Target`]) and, if its credentials work differently,
//! one arm in [`lookup_credential`] — plus its crate. An engine with no arm
//! reports that it is not in this build, by name; it never falls through to
//! Postgres.

use std::sync::Arc;
use std::time::Duration;

use sluice_core::{Driver, DriverError, ErrorKind, Secret, TlsMode};
use sluice_state::profile::{CredentialProvider, Engine};

use crate::credentials::CredentialStore;

/// Everything needed to open a session, for any engine. The password arrives
/// already resolved (D-007) and is handed to exactly one driver.
#[derive(Debug, Clone)]
pub struct Target {
    pub engine: Engine,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<Secret<String>>,
    pub database: String,
    pub tls: TlsMode,
    /// T-051 / D-010: the profile's statement timeout, enforced by the server.
    pub statement_timeout: Option<Duration>,
    /// T-042 / D-028: the profile's connection budget, pooled and pinned.
    pub budget: usize,
}

/// Every engine this build can connect to.
pub fn compiled_engines() -> &'static [Engine] {
    &[Engine::Postgres, Engine::Sqlserver]
}

/// The named refusal for an engine with no driver in this build.
pub fn not_in_this_build(engine: Engine) -> DriverError {
    DriverError::new(
        ErrorKind::ConnectionFailed,
        format!(
            "{} connections are not available in this build of Sluice — it has no {} driver yet",
            engine.label(),
            engine.label()
        ),
    )
}

/// Open a session's driver for `target.engine`.
pub async fn connect(target: Target) -> Result<Arc<dyn Driver>, DriverError> {
    match target.engine {
        Engine::Postgres => {
            let spec = sluice_postgres::ConnectSpec {
                host: target.host,
                port: target.port,
                user: target.user,
                password: target.password,
                dbname: target.database,
                tls: target.tls,
                application_name: "sluice".into(),
                statement_timeout: target.statement_timeout,
            };
            let d: Arc<dyn Driver> = sluice_postgres::PgDriver::connect_with_pool_size(spec, target.budget).await?;
            Ok(d)
        }
        // T-080: SQL Server logins over tiberius. The server enforces no
        // statement timeout of its own here; the client watchdog in
        // `execution::run` bounds every call instead (D-010).
        Engine::Sqlserver => {
            let spec = sluice_sqlserver::ConnectSpec {
                host: target.host,
                port: target.port,
                user: target.user,
                password: target.password,
                database: target.database,
                tls: target.tls,
                application_name: "sluice".into(),
            };
            let d: Arc<dyn Driver> = sluice_sqlserver::SqlServerDriver::connect(spec, target.budget).await?;
            Ok(d)
        }
        other @ Engine::Mongodb => Err(not_in_this_build(other)),
    }
}

/// T-038 / D-008: resolve the profile's credential *provider* to a password,
/// the way `engine` takes it. Returns the password (if any) and, for the
/// keychain, why an existing item could not be read (T-071).
///
/// Every engine built so far takes a password, so all share one arm; an
/// engine whose credential is something else (an Entra token, say) gets its
/// own here.
pub fn lookup_credential(
    engine: Engine,
    creds: &CredentialStore,
    profile_id: &str,
    auth: &CredentialProvider,
    env: impl Fn(&str) -> Option<String>,
) -> Result<(Option<Secret<String>>, Option<String>), DriverError> {
    match engine {
        Engine::Postgres | Engine::Sqlserver | Engine::Mongodb => resolve_auth(creds, profile_id, auth, env),
    }
}

/// R-017: the environment is a credential parse boundary, so an `env` value
/// is wrapped in `Secret` the moment it is read.
pub(crate) fn resolve_auth(
    creds: &CredentialStore,
    profile_id: &str,
    auth: &CredentialProvider,
    env: impl Fn(&str) -> Option<String>,
) -> Result<(Option<Secret<String>>, Option<String>), DriverError> {
    match auth {
        CredentialProvider::Keychain {} => Ok(match creds.resolve(profile_id) {
            Ok(p) => (Some(p), None),
            Err(crate::credentials::CredentialError::Unreadable(why)) => (None, Some(why)),
            Err(_) => (None, None),
        }),
        CredentialProvider::Env { var } => match env(var) {
            Some(v) => Ok((Some(Secret::new(v)), None)),
            None => Err(DriverError::new(
                ErrorKind::AuthFailed,
                format!("this profile reads its password from the environment variable `{var}`, which is not set"),
            )),
        },
        other => Err(DriverError::new(
            ErrorKind::AuthFailed,
            format!("the `{}` credential provider is not available yet", other.label()),
        )),
    }
}

/// The dev Postgres the live tests use (`docker compose` in the repo), opened
/// through the factory like any session. `None` when it is not reachable.
#[cfg(test)]
pub(crate) async fn test_postgres(budget: usize) -> Option<Arc<dyn Driver>> {
    let target = Target {
        engine: Engine::Postgres,
        host: std::env::var("SLUICE_TEST_PG_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("SLUICE_TEST_PG_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(55432),
        user: "postgres".into(),
        password: Some(Secret::new("sluice_dev".into())),
        database: "sluice_dev".into(),
        tls: TlsMode::Disabled,
        statement_timeout: None,
        budget,
    };
    match connect(target).await {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("SKIP: no Postgres reachable ({e})");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(engine: Engine) -> Target {
        Target {
            engine,
            host: "127.0.0.1".into(),
            port: 1,
            user: "u".into(),
            password: Some(Secret::new("hunter2".into())),
            database: "d".into(),
            tls: TlsMode::Disabled,
            statement_timeout: None,
            budget: 1,
        }
    }

    /// T-079: an engine with no driver in the build is refused by name — and
    /// is never handed to the Postgres driver.
    #[tokio::test]
    async fn an_engine_missing_from_the_build_is_named_not_routed_to_postgres() {
        for engine in [Engine::Mongodb] {
            assert!(!compiled_engines().contains(&engine));
            let e = connect(target(engine)).await.err().expect("refused");
            assert_eq!(e.kind, ErrorKind::ConnectionFailed);
            assert!(e.message.contains(engine.label()), "{}", e.message);
            assert!(!e.message.contains("hunter2"));
        }
    }

    /// T-080: a SQL Server target reaches the SQL Server driver, within the
    /// budget it was given. Skipped without the T-072 container.
    #[tokio::test]
    async fn sqlserver_goes_to_the_sqlserver_driver() {
        let t = Target {
            engine: Engine::Sqlserver,
            host: std::env::var("SLUICE_TEST_MSSQL_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            port: std::env::var("SLUICE_TEST_MSSQL_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(14333),
            user: "sa".into(),
            password: Some(Secret::new(
                std::env::var("SLUICE_TEST_MSSQL_PASSWORD").unwrap_or_else(|_| "Sluice_Spike_2026!".into()),
            )),
            database: "master".into(),
            tls: TlsMode::AcceptInvalidCerts,
            statement_timeout: None,
            budget: 3,
        };
        let d = match connect(t).await {
            Ok(d) => d,
            Err(e) => {
                eprintln!("SKIP: no SQL Server reachable ({e})");
                return;
            }
        };
        let caps = d.capabilities();
        assert_eq!(caps.engine, "sqlserver");
        assert_eq!(caps.reset, sluice_core::ResetSupport::ReopenOnly);
        assert_eq!(d.connection_budget(), 3);
        assert!(!d.navigator_children(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn postgres_goes_to_the_postgres_driver() {
        let Some(d) = test_postgres(2).await else { return };
        assert_eq!(d.capabilities().engine, "postgres");
        assert_eq!(d.connection_budget(), 2);
    }
}
