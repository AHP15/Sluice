//! T-061 / D-027: does the *database* enforce read-only for this session?
//!
//! Safety mode is a guarantee only when the server refuses writes. This probe
//! asks the catalog what the connected role can actually do — the answer the
//! UI shows — rather than trusting a profile flag. It is conservative: any way
//! the session can change data or schema makes the guarantee `NotEnforced`,
//! listing each way, so "read-only" in the UI always means the server would
//! say no.

use sluice_core::ReadOnlyGuarantee;

/// One round trip. `EXISTS` short-circuits, so a role that can write to one
/// table stops the scan there.
pub const PROBE_SQL: &str = "\
select
  pg_is_in_recovery(),
  r.rolsuper,
  r.rolcreaterole,
  r.rolcreatedb,
  exists (
    select 1 from pg_class c join pg_namespace n on n.oid = c.relnamespace
     where c.relkind in ('r','p','v','m','f')
       and n.nspname not in ('pg_catalog','information_schema')
       and n.nspname not like 'pg\\_toast%' and n.nspname not like 'pg\\_temp%'
       and has_table_privilege(c.oid, 'INSERT, UPDATE, DELETE, TRUNCATE')),
  exists (
    select 1 from pg_namespace n
     where n.nspname not in ('pg_catalog','information_schema') and n.nspname not like 'pg\\_%'
       and has_schema_privilege(n.oid, 'CREATE')),
  has_database_privilege(current_database(), 'CREATE'),
  current_setting('default_transaction_read_only') = 'on'
from pg_roles r where r.rolname = current_user";

/// What the probe found, as plain facts — kept separate from the verdict so
/// the verdict is testable without a server.
#[derive(Debug, Clone, Copy, Default)]
pub struct Probe {
    pub standby: bool,
    pub superuser: bool,
    pub create_role: bool,
    pub create_db: bool,
    pub write_tables: bool,
    pub create_in_schema: bool,
    pub create_schema: bool,
    pub default_tx_read_only: bool,
}

pub fn verdict(p: &Probe) -> ReadOnlyGuarantee {
    // A standby refuses every write, whatever the role.
    if p.standby {
        return ReadOnlyGuarantee::Enforced { basis: "the server is a read-only standby (in recovery)".into() };
    }
    let mut reasons = Vec::new();
    if p.superuser {
        reasons.push("the role is a superuser".to_string());
    }
    if p.write_tables {
        reasons.push("the role can INSERT, UPDATE, DELETE or TRUNCATE at least one table".to_string());
    }
    if p.create_in_schema {
        reasons.push("the role can create objects in at least one schema".to_string());
    }
    if p.create_schema {
        reasons.push("the role can create schemas in this database".to_string());
    }
    if p.create_role {
        reasons.push("the role can create roles".to_string());
    }
    if p.create_db {
        reasons.push("the role can create databases".to_string());
    }
    if reasons.is_empty() {
        return ReadOnlyGuarantee::Enforced {
            basis: "the role has no write, create or superuser privileges".into(),
        };
    }
    if p.default_tx_read_only {
        // Real friction, not a guarantee: any session can `SET` it off.
        reasons.push(
            "default_transaction_read_only is on, but a session can turn it off, so it is not enforcement".to_string(),
        );
    }
    ReadOnlyGuarantee::NotEnforced { reasons }
}

pub async fn probe(client: &tokio_postgres::Client) -> ReadOnlyGuarantee {
    match client.query_one(PROBE_SQL, &[]).await {
        Ok(row) => verdict(&Probe {
            standby: row.get(0),
            superuser: row.get(1),
            create_role: row.get(2),
            create_db: row.get(3),
            write_tables: row.get(4),
            create_in_schema: row.get(5),
            create_schema: row.get(6),
            default_tx_read_only: row.get(7),
        }),
        // Never fail a connect over this; say it is unknown instead. The
        // engine's message is not echoed — only that the probe did not run.
        Err(_) => ReadOnlyGuarantee::Unknown { reason: "the privilege probe could not run on this server".into() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_standby_is_enforced_whatever_the_role() {
        let v = verdict(&Probe { standby: true, superuser: true, ..Default::default() });
        assert!(v.is_enforced(), "{v:?}");
    }

    #[test]
    fn a_role_with_no_write_reach_is_enforced() {
        assert!(verdict(&Probe::default()).is_enforced());
    }

    #[test]
    fn every_way_to_write_is_named() {
        let v = verdict(&Probe {
            superuser: true,
            write_tables: true,
            create_in_schema: true,
            create_schema: true,
            create_role: true,
            create_db: true,
            ..Default::default()
        });
        let ReadOnlyGuarantee::NotEnforced { reasons } = v else { panic!("{v:?}") };
        assert_eq!(reasons.len(), 6, "{reasons:?}");
    }

    /// `default_transaction_read_only` is a session default a client can
    /// `SET` away — it must never be reported as enforcement.
    #[test]
    fn default_transaction_read_only_alone_is_not_enforcement() {
        let v = verdict(&Probe { write_tables: true, default_tx_read_only: true, ..Default::default() });
        let ReadOnlyGuarantee::NotEnforced { reasons } = v else { panic!("{v:?}") };
        assert!(reasons.iter().any(|r| r.contains("not enforcement")));
    }
}
