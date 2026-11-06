//! T-055 / R-028: probe what the catalog actually supports at connect.
//!
//! Flavour gating only works for flavours enumerated in advance: Aurora says
//! it is ordinary Postgres, PgBouncer cannot be seen from `version()`, and
//! new forks keep appearing. So each catalog area the navigator relies on is
//! probed with one cheap, single-row query, and what *succeeded* is recorded in
//! the capability descriptor. The navigator then reads only what probed, and
//! says in place what it could not show — divergence surfaces at connect and
//! as a note, never as an opaque error mid-navigation.

use sluice_core::{CatalogFeature, CatalogSupport};

/// `(id, label, probe)`. Each probe returns at most one row and reads the same
/// catalog objects the navigator's query for that area does.
pub const PROBES: &[(&str, &str, &str)] = &[
    ("schemas", "schemas", "select nspname from pg_namespace limit 1"),
    ("relations", "tables and views", "select relname, relkind from pg_class limit 1"),
    ("columns", "columns", "select attname, format_type(atttypid, atttypmod), attnotnull from pg_attribute limit 1"),
    ("indexes", "indexes", "select x.indisunique, x.indisprimary, am.amname from pg_index x join pg_class i on i.oid = x.indexrelid join pg_am am on am.oid = i.relam limit 1"),
    ("partitioning", "partitioned tables", "select partrelid from pg_partitioned_table limit 1"),
    ("matviews", "materialized views", "select matviewname from pg_matviews limit 1"),
    ("stats", "statistics views", "select relname from pg_stat_user_tables limit 1"),
];

pub async fn probe(client: &tokio_postgres::Client, flavour_verified: bool) -> CatalogSupport {
    let mut features = Vec::with_capacity(PROBES.len());
    for (id, label, sql) in PROBES {
        // Outside a transaction, a failed probe leaves the connection usable.
        let available = client.query(*sql, &[]).await.is_ok();
        features.push(CatalogFeature { id: (*id).into(), label: (*label).into(), available });
    }
    CatalogSupport { flavour_verified, features }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_navigator_area_is_probed() {
        let ids: Vec<&str> = PROBES.iter().map(|(id, _, _)| *id).collect();
        for needed in ["schemas", "relations", "columns", "indexes"] {
            assert!(ids.contains(&needed), "{needed}");
        }
    }

    #[test]
    fn availability_defaults_open_for_unprobed_ids() {
        let c = CatalogSupport {
            flavour_verified: true,
            features: vec![CatalogFeature { id: "indexes".into(), label: "indexes".into(), available: false }],
        };
        assert!(!c.available("indexes"));
        assert!(c.available("never-probed"));
        assert_eq!(c.unavailable().count(), 1);
    }
}
