//! The navigator tree, MongoDB side (T-087 / D-029, D-016).
//!
//! T-073 F-6 is the design: two driver-declared levels, `database →
//! collection`, with no schema level and no placeholder node. Views are a
//! separate kind beside `collection` because browsing one is read-only and the
//! tree should say so before the user tries; both are relations.
//!
//! Both levels ask the server only for what this login may see
//! (`authorizedDatabases` / `authorizedCollections`), so a narrowly-granted
//! user gets the databases and collections they can open rather than an
//! `Unauthorized` on the root. What still cannot be read is a note in place
//! (D-016), never a failed navigation. Each level was probed at connect
//! (`CatalogSupport`, T-055's rule), not inferred from the version.

use futures_util::TryStreamExt;
use mongodb::bson::{doc, Document};
use mongodb::Client;
use sluice_core::{CatalogFeature, CatalogSupport, DriverError, ErrorKind, NavNode, NodeId, NodeKind};

use crate::error_map::{self, map_error};

/// Databases the server keeps for itself: hidden unless the profile connected
/// to one deliberately (as SQL Server hides `master`/`tempdb`, T-081).
pub const SYSTEM_DATABASES: &[&str] = &["admin", "config", "local"];

pub fn node_kinds() -> Vec<NodeKind> {
    let kind = |id: &str, label: &str, has_children: bool, is_relation: bool| NodeKind {
        id: id.into(),
        label: label.into(),
        has_children,
        is_relation,
    };
    vec![
        kind("database", "Database", true, false),
        kind("collection", "Collection", false, true),
        kind("view", "View", false, true),
        kind("notice", "Note", false, false),
    ]
}

async fn database_names(client: &Client) -> Result<Vec<String>, DriverError> {
    client.list_database_names().authorized_databases(true).await.map_err(|e| map_error(&e))
}

/// `(name, type)` for each collection this login may see. Read through the
/// raw `listCollections` command with `nameOnly`, which is cheap (no lock on
/// each collection) and never asks the crate to deserialize collection
/// options it might not model.
async fn collection_specs(client: &Client, db: &str) -> Result<Vec<(String, String)>, DriverError> {
    let cmd = doc! { "listCollections": 1, "nameOnly": true, "authorizedCollections": true };
    let docs: Vec<Document> = client
        .database(db)
        .run_cursor_command(cmd)
        .await
        .map_err(|e| map_error(&e))?
        .try_collect()
        .await
        .map_err(|e| map_error(&e))?;
    Ok(docs
        .iter()
        .filter_map(|d| {
            let name = d.get_str("name").ok()?.to_string();
            let ty = d.get_str("type").unwrap_or("collection").to_string();
            Some((name, ty))
        })
        .collect())
}

/// T-055 / R-028: which catalog areas this login can demonstrably read.
pub async fn probe(client: &Client, current_db: &str) -> CatalogSupport {
    let databases = database_names(client).await.is_ok();
    let collections = collection_specs(client, current_db).await.is_ok();
    CatalogSupport {
        flavour_verified: true,
        features: vec![
            CatalogFeature { id: "databases".into(), label: "Databases".into(), available: databases },
            CatalogFeature { id: "collections".into(), label: "Collections".into(), available: collections },
        ],
    }
}

pub async fn children(
    client: &Client,
    parent: Option<NodeId>,
    current_db: &str,
    catalog: &CatalogSupport,
) -> Result<Vec<NavNode>, DriverError> {
    let result = match &parent {
        None => databases(client, current_db, catalog.available("databases")).await,
        Some(n) if n.kind == "database" => match n.path.as_slice() {
            [db] => collections(client, db).await,
            other => Err(DriverError::new(ErrorKind::NotFound, format!("malformed node path {other:?}"))),
        },
        Some(n) => {
            return Err(DriverError::new(ErrorKind::NotFound, format!("node kind `{}` has no children", n.kind)));
        }
    };
    match result {
        Err(e) if error_map::is_permission(&e) => {
            Ok(vec![notice(parent.as_ref(), "not permitted to read this part of the catalog")])
        }
        other => other,
    }
}

pub fn is_notice(node: &NavNode) -> bool {
    node.id.kind == "notice"
}

fn notice(parent: Option<&NodeId>, text: &str) -> NavNode {
    let mut path = parent.map(|p| p.path.clone()).unwrap_or_default();
    path.push(text.to_string());
    NavNode { id: NodeId { kind: "notice".into(), path }, label: text.to_string(), has_children: false }
}

fn database_node(name: String) -> NavNode {
    NavNode { id: NodeId { kind: "database".into(), path: vec![name.clone()] }, label: name, has_children: true }
}

/// The databases to show, given what the server listed. The profile's own
/// database is always shown: Mongo only lists a database once it holds data,
/// and a profile pointed at an empty one should still see it.
pub fn visible_databases(listed: Vec<String>, current: &str) -> Vec<String> {
    let mut out: Vec<String> =
        listed.into_iter().filter(|n| !SYSTEM_DATABASES.contains(&n.as_str()) || n == current).collect();
    if !current.is_empty() && !out.iter().any(|n| n == current) {
        out.push(current.to_string());
    }
    out.sort();
    out.dedup();
    out
}

async fn databases(client: &Client, current: &str, listable: bool) -> Result<Vec<NavNode>, DriverError> {
    if !listable {
        return Ok(vec![
            database_node(current.to_string()),
            notice(None, "other databases cannot be listed with this login"),
        ]);
    }
    let listed = database_names(client).await?;
    Ok(visible_databases(listed, current).into_iter().map(database_node).collect())
}

/// `collection` or `view` for a `listCollections` `type`. Time-series
/// collections browse like any collection.
pub fn kind_for_type(ty: &str) -> &'static str {
    if ty == "view" { "view" } else { "collection" }
}

async fn collections(client: &Client, db: &str) -> Result<Vec<NavNode>, DriverError> {
    let mut specs = collection_specs(client, db).await?;
    // `system.*` namespaces (profile, views, buckets) are the server's own.
    specs.retain(|(name, _)| !name.starts_with("system."));
    specs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(specs
        .into_iter()
        .map(|(name, ty)| NavNode {
            id: NodeId { kind: kind_for_type(&ty).into(), path: vec![db.to_string(), name.clone()] },
            label: name,
            has_children: false,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_databases_are_hidden_unless_the_profile_chose_one() {
        let listed = || vec!["shop".to_string(), "admin".into(), "local".into(), "config".into(), "app".into()];
        assert_eq!(visible_databases(listed(), "shop"), vec!["app", "shop"]);
        assert_eq!(visible_databases(listed(), "admin"), vec!["admin", "app", "shop"]);
        // An empty database is not listed by the server, but is the profile's.
        assert_eq!(visible_databases(listed(), "fresh"), vec!["app", "fresh", "shop"]);
    }

    #[test]
    fn views_are_their_own_kind_and_everything_else_a_collection() {
        assert_eq!(kind_for_type("view"), "view");
        assert_eq!(kind_for_type("collection"), "collection");
        assert_eq!(kind_for_type("timeseries"), "collection");
    }

    #[test]
    fn every_kind_the_tree_returns_is_declared_and_relations_are_browsable() {
        let declared = node_kinds();
        for k in ["database", "collection", "view", "notice"] {
            assert!(declared.iter().any(|d| d.id == k), "{k}");
        }
        let relations: Vec<_> = declared.iter().filter(|k| k.is_relation).map(|k| k.id.as_str()).collect();
        assert_eq!(relations, vec!["collection", "view"]);
    }
}
