//! The navigator tree, Postgres side (T-008 / D-029).
//!
//! D-029: node *kinds* are driver-declared, not a hardcoded
//! database/schema/table hierarchy. The React layer renders whatever kinds come
//! back and knows nothing about schemas — which is what let Mongo's two-level
//! `database → collection` tree fit the same contract with no placeholder node
//! (T-073 F-6, the strongest validation the spike produced).

use sluice_core::{DriverError, NavNode, NodeId, NodeKind};
use tokio_postgres::Client;

use crate::error_map::map_error;

pub fn node_kinds() -> Vec<NodeKind> {
    vec![
        NodeKind {
            id: "schema".into(),
            label: "Schema".into(),
            has_children: true,
            is_relation: false,
        },
        NodeKind {
            id: "table".into(),
            label: "Table".into(),
            has_children: false,
            is_relation: true,
        },
        NodeKind {
            id: "view".into(),
            label: "View".into(),
            has_children: false,
            is_relation: true,
        },
        NodeKind {
            id: "matview".into(),
            label: "Materialized view".into(),
            has_children: false,
            is_relation: true,
        },
    ]
}

pub async fn children(
    client: &Client,
    parent: Option<NodeId>,
) -> Result<Vec<NavNode>, DriverError> {
    match parent {
        None => schemas(client).await,
        Some(node) if node.kind == "schema" => {
            let schema = node.path.first().cloned().unwrap_or_default();
            relations(client, &schema).await
        }
        Some(node) => Err(DriverError::new(
            sluice_core::ErrorKind::NotFound,
            format!("node kind `{}` has no children", node.kind),
        )),
    }
}

async fn schemas(client: &Client) -> Result<Vec<NavNode>, DriverError> {
    // Catalog schemas are noise for a browsing tool; T-053 can make this a
    // toggle. `pg_toast_temp_%` and friends are covered by the prefix test.
    const SQL: &str = "
        SELECT nspname
        FROM pg_namespace
        WHERE nspname NOT LIKE 'pg\\_%'
          AND nspname <> 'information_schema'
        ORDER BY nspname";

    let rows = client
        .query(SQL, &[])
        .await
        .map_err(|e| map_error(&e, Some(SQL)))?;

    Ok(rows
        .iter()
        .map(|r| {
            let name: String = r.get(0);
            NavNode {
                id: NodeId { kind: "schema".into(), path: vec![name.clone()] },
                label: name,
                has_children: true,
            }
        })
        .collect())
}

async fn relations(client: &Client, schema: &str) -> Result<Vec<NavNode>, DriverError> {
    // relkind: r=table, p=partitioned table, v=view, m=materialized view,
    // f=foreign table. Partitioned and foreign tables browse like tables.
    const SQL: &str = "
        SELECT c.relname, c.relkind
        FROM pg_class c
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = $1
          AND c.relkind IN ('r','p','v','m','f')
        ORDER BY c.relname";

    let rows = client
        .query(SQL, &[&schema])
        .await
        .map_err(|e| map_error(&e, Some(SQL)))?;

    Ok(rows
        .iter()
        .map(|r| {
            let name: String = r.get(0);
            let relkind: i8 = r.get(1);
            let kind = match relkind as u8 as char {
                'v' => "view",
                'm' => "matview",
                _ => "table",
            };
            NavNode {
                id: NodeId {
                    kind: kind.into(),
                    path: vec![schema.to_string(), name.clone()],
                },
                label: name,
                has_children: false,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_declared_kind_is_either_a_container_or_a_relation() {
        for k in node_kinds() {
            assert!(
                k.has_children != k.is_relation,
                "kind `{}` must be exactly one of container / relation",
                k.id
            );
        }
    }

    #[test]
    fn relation_kinds_match_what_browse_can_target() {
        let kinds = node_kinds();
        let relations: Vec<String> =
            kinds.iter().filter(|k| k.is_relation).map(|k| k.id.clone()).collect();
        assert!(relations.contains(&"table".to_string()));
        assert!(relations.contains(&"view".to_string()));
    }
}
