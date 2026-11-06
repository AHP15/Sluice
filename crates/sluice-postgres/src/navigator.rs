//! The navigator tree, Postgres side (T-008 / D-029, T-053 / D-016).
//!
//! D-029: node *kinds* are driver-declared, not a hardcoded
//! database/schema/table hierarchy. The React layer renders whatever kinds come
//! back and knows nothing about schemas — which is what let Mongo's two-level
//! `database → collection` tree fit the same contract with no placeholder node
//! (T-073 F-6, the strongest validation the spike produced).
//!
//! D-016 / T-053: native catalogs only (`pg_namespace`, `pg_class`,
//! `pg_attribute`, `pg_index`), never `information_schema` — the permission-
//! filtered views are slower and cannot express index methods or partitioning.
//! Loaded lazily, one level per expand: schemas up front, relations on schema
//! expand, columns and indexes on relation select. Caching is the driver's
//! (`PgDriver`); this module only answers one level.
//!
//! Missing catalog rows and permission-denied are normal conditions here, not
//! connection failures: they come back as a `notice` node the tree renders in
//! place, and the rest of the tree keeps working.

use sluice_core::{DriverError, ErrorKind, NavNode, NodeId, NodeKind};
use tokio_postgres::Client;

use crate::error_map::map_error;

/// Relation kinds: browsable, and expandable to their columns and indexes.
const RELATION_KINDS: &[&str] = &["table", "view", "matview"];

pub fn node_kinds() -> Vec<NodeKind> {
    let kind = |id: &str, label: &str, has_children: bool, is_relation: bool| NodeKind {
        id: id.into(),
        label: label.into(),
        has_children,
        is_relation,
    };
    vec![
        kind("schema", "Schema", true, false),
        kind("table", "Table", true, true),
        kind("view", "View", true, true),
        kind("matview", "Materialized view", true, true),
        kind("column", "Column", false, false),
        kind("index", "Index", false, false),
        // Rendered in place of children that could not be read (D-016).
        kind("notice", "Note", false, false),
    ]
}

/// `hidden_schemas`: flavour-internal schemas to leave out (T-054), beside the
/// `pg_*` and `information_schema` filter every flavour gets.
pub async fn children(
    client: &Client,
    parent: Option<NodeId>,
    hidden_schemas: &[String],
) -> Result<Vec<NavNode>, DriverError> {
    let result = match &parent {
        None => schemas(client, hidden_schemas).await,
        Some(node) if node.kind == "schema" => {
            let schema = node.path.first().cloned().unwrap_or_default();
            relations(client, &schema).await
        }
        Some(node) if RELATION_KINDS.contains(&node.kind.as_str()) => match node.path.as_slice() {
            [schema, rel] => columns_and_indexes(client, schema, rel).await,
            other => Err(DriverError::new(ErrorKind::NotFound, format!("malformed relation path {other:?}"))),
        },
        Some(node) => {
            return Err(DriverError::new(
                ErrorKind::NotFound,
                format!("node kind `{}` has no children", node.kind),
            ))
        }
    };
    render_around(result, parent.as_ref())
}

/// D-016: a catalog the role may not read is a note in the tree, not a failed
/// navigation. Anything else — a dropped connection — is still an error.
pub fn render_around(
    result: Result<Vec<NavNode>, DriverError>,
    parent: Option<&NodeId>,
) -> Result<Vec<NavNode>, DriverError> {
    match result {
        Err(e) if e.kind == ErrorKind::PermissionDenied => {
            Ok(vec![notice(parent, "not permitted to read this part of the catalog")])
        }
        other => other,
    }
}

/// Whether a node is a placeholder rather than catalog content — never cached,
/// so the next expand retries.
pub fn is_notice(node: &NavNode) -> bool {
    node.id.kind == "notice"
}

fn notice(parent: Option<&NodeId>, text: &str) -> NavNode {
    let mut path = parent.map(|p| p.path.clone()).unwrap_or_default();
    path.push(text.to_string());
    NavNode { id: NodeId { kind: "notice".into(), path }, label: text.to_string(), has_children: false }
}

async fn schemas(client: &Client, hidden: &[String]) -> Result<Vec<NavNode>, DriverError> {
    // Catalog schemas are noise for a browsing tool. `pg_toast_temp_%` and
    // friends are covered by the prefix test.
    const SQL: &str = "
        SELECT nspname
        FROM pg_namespace
        WHERE nspname NOT LIKE 'pg\\_%'
          AND nspname <> 'information_schema'
          AND nspname <> ALL($1)
        ORDER BY nspname";

    let rows = client.query(SQL, &[&hidden]).await.map_err(|e| map_error(&e, Some(SQL)))?;
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

    let rows = client.query(SQL, &[&schema]).await.map_err(|e| map_error(&e, Some(SQL)))?;
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
                id: NodeId { kind: kind.into(), path: vec![schema.to_string(), name.clone()] },
                label: name,
                has_children: true,
            }
        })
        .collect())
}

/// Columns in ordinal order, then indexes. Resolved by (schema, name) as bound
/// parameters — never by interpolating either into SQL.
async fn columns_and_indexes(client: &Client, schema: &str, rel: &str) -> Result<Vec<NavNode>, DriverError> {
    const RELID: &str = "
        SELECT c.oid
        FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = $1 AND c.relname = $2";
    const COLUMNS: &str = "
        SELECT a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull
        FROM pg_attribute a
        WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped
        ORDER BY a.attnum";
    const INDEXES: &str = "
        SELECT i.relname, am.amname, x.indisunique, x.indisprimary
        FROM pg_index x
        JOIN pg_class i ON i.oid = x.indexrelid
        JOIN pg_am am ON am.oid = i.relam
        WHERE x.indrelid = $1
        ORDER BY x.indisprimary DESC, i.relname";

    let relid = client
        .query_opt(RELID, &[&schema, &rel])
        .await
        .map_err(|e| map_error(&e, Some(RELID)))?;
    let parent = NodeId { kind: "table".into(), path: vec![schema.to_string(), rel.to_string()] };
    // D-016: a relation dropped since the tree was loaded is a missing catalog
    // row — say so in place, and point at refresh.
    let Some(relid) = relid else {
        return Ok(vec![notice(Some(&parent), "this relation no longer exists — refresh")]);
    };
    let relid: tokio_postgres::types::Oid = relid.get(0);

    let mut out = Vec::new();
    for r in client.query(COLUMNS, &[&relid]).await.map_err(|e| map_error(&e, Some(COLUMNS)))? {
        let name: String = r.get(0);
        let ty: String = r.get(1);
        let not_null: bool = r.get(2);
        out.push(NavNode {
            id: NodeId { kind: "column".into(), path: vec![schema.into(), rel.into(), name.clone()] },
            label: format!("{name}  {ty}{}", if not_null { " · not null" } else { "" }),
            has_children: false,
        });
    }
    for r in client.query(INDEXES, &[&relid]).await.map_err(|e| map_error(&e, Some(INDEXES)))? {
        let name: String = r.get(0);
        let method: String = r.get(1);
        let unique: bool = r.get(2);
        let primary: bool = r.get(3);
        let tag = if primary {
            ", primary key"
        } else if unique {
            ", unique"
        } else {
            ""
        };
        out.push(NavNode {
            id: NodeId { kind: "index".into(), path: vec![schema.into(), rel.into(), name.clone()] },
            label: format!("{name}  ({method}{tag})"),
            has_children: false,
        });
    }
    Ok(out)
}

/// T-053: does a script need the catalog cache dropped? D-016 asks for
/// invalidation after DDL the app itself executed. Conservative by design:
/// the cache survives only if **every** statement begins with a keyword known
/// not to change the catalog; `DO`, `CALL`, anything unrecognised, or a split
/// the lexer is unsure of all invalidate. A wrong "keep" shows a stale tree; a
/// wrong "drop" only costs a re-query.
pub fn script_may_change_catalog(script: &str) -> bool {
    const KEEPS_CATALOG: &[&str] = &[
        "SELECT", "INSERT", "UPDATE", "DELETE", "WITH", "VALUES", "TABLE", "SHOW", "EXPLAIN",
        "BEGIN", "START", "COMMIT", "END", "ROLLBACK", "SAVEPOINT", "RELEASE", "SET", "RESET",
        "FETCH", "MOVE", "CLOSE", "LISTEN", "NOTIFY", "UNLISTEN", "COPY",
    ];
    let split = sluice_core::script::split(script, sluice_core::ScriptDialect::Postgres);
    if !split.confident {
        return true;
    }
    split.statements.iter().any(|s| {
        let word = first_keyword(&script[s.start_byte..s.end_byte]).to_ascii_uppercase();
        !word.is_empty() && !KEEPS_CATALOG.contains(&word.as_str())
    })
}

/// The first word of a statement, skipping whitespace, `--` and `/* */` comments.
fn first_keyword(mut s: &str) -> &str {
    loop {
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix("--") {
            s = rest.split_once('\n').map(|(_, r)| r).unwrap_or("");
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest.split_once("*/").map(|(_, r)| r).unwrap_or("");
        } else {
            break;
        }
    }
    let end = s.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(s.len());
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D-016 puts columns under relations, so a relation is now both
    /// browsable and expandable; leaf kinds are neither, and the remaining
    /// containers are not browsable.
    #[test]
    fn relation_kinds_are_browsable_and_expandable_and_leaves_are_neither() {
        for k in node_kinds() {
            if RELATION_KINDS.contains(&k.id.as_str()) {
                assert!(k.is_relation && k.has_children, "{}", k.id);
            } else if ["column", "index", "notice"].contains(&k.id.as_str()) {
                assert!(!k.is_relation && !k.has_children, "{}", k.id);
            } else {
                assert!(k.has_children && !k.is_relation, "{}", k.id);
            }
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

    #[test]
    fn permission_denied_becomes_a_notice_not_a_failure() {
        let parent = NodeId { kind: "table".into(), path: vec!["s".into(), "t".into()] };
        let out = render_around(
            Err(DriverError::new(ErrorKind::PermissionDenied, "permission denied for table pg_index")),
            Some(&parent),
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert!(is_notice(&out[0]));
        // A real failure is still a failure.
        assert!(render_around(Err(DriverError::new(ErrorKind::ConnectionFailed, "gone")), None).is_err());
    }

    #[test]
    fn only_scripts_that_may_touch_the_catalog_invalidate() {
        for keep in [
            "select 1",
            "insert into t values (1); update t set a = 2",
            "-- note\nSELECT * FROM t",
            "/* c */ with x as (select 1) select * from x",
            "begin; delete from t; commit",
        ] {
            assert!(!script_may_change_catalog(keep), "{keep}");
        }
        for drop in [
            "create table t(a int)",
            "alter table t add column b int",
            "select 1; drop view v",
            "comment on table t is 'x'",
            "do $$ begin execute 'create table z()'; end $$",
            "call refresh_things()",
            "grant select on t to bob",
            "select 'unterminated",
        ] {
            assert!(script_may_change_catalog(drop), "{drop}");
        }
    }
}
