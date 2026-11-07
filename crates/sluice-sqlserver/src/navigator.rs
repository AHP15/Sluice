//! The navigator tree, SQL Server side (T-081 / D-029, D-016).
//!
//! Four declared levels — `database → schema → table | view → column | index`
//! — read from the native `sys.*` catalog views, never `INFORMATION_SCHEMA`,
//! one level per expand. Databases are listed from `sys.databases`; every
//! deeper level is read through a three-part name (`[db].sys.objects`), so
//! the tree can span every database the login can open without reconnecting.
//!
//! T-055's rule carries over: each catalog area is read only if it probed at
//! connect (`CatalogSupport`), so a server where cross-database references are
//! refused (Azure SQL Database) shows its own database alone, with a note,
//! instead of failing mid-navigation. Probed, not inferred from the banner.
//!
//! Identifiers are bracket-quoted (`]` doubled); every other value is a bound
//! parameter.

use sluice_core::{CatalogFeature, CatalogSupport, DriverError, ErrorKind, NavNode, NodeId, NodeKind};

use crate::error_map::{self, map_error};
use crate::pool::Conn;

const RELATION_KINDS: &[&str] = &["table", "view"];

/// Whether a node of this kind can be browsed (T-082).
pub fn is_relation_kind(kind: &str) -> bool {
    RELATION_KINDS.contains(&kind)
}

pub fn node_kinds() -> Vec<NodeKind> {
    let kind = |id: &str, label: &str, has_children: bool, is_relation: bool| NodeKind {
        id: id.into(),
        label: label.into(),
        has_children,
        is_relation,
    };
    vec![
        kind("database", "Database", true, false),
        kind("schema", "Schema", true, false),
        kind("table", "Table", true, true),
        kind("view", "View", true, true),
        kind("column", "Column", false, false),
        kind("index", "Index", false, false),
        kind("notice", "Note", false, false),
    ]
}

/// `[name]`, with `]` doubled — SQL Server's identifier quoting.
pub fn quote_ident(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

pub(crate) async fn rows(
    conn: &mut Conn,
    sql: &str,
    params: &[&dyn tiberius::ToSql],
) -> Result<Vec<tiberius::Row>, DriverError> {
    let stream = conn.query(sql, params).await.map_err(|e| map_error(&e))?;
    stream.into_first_result().await.map_err(|e| map_error(&e))
}

/// Column `i` as text. `try_get` only: tiberius's `get` panics on a type
/// mismatch (T-072).
pub(crate) fn text(row: &tiberius::Row, i: usize) -> Result<Option<String>, DriverError> {
    row.try_get::<&str, _>(i).map(|o| o.map(str::to_string)).map_err(|e| map_error(&e))
}

pub(crate) fn int(row: &tiberius::Row, i: usize) -> Result<Option<i32>, DriverError> {
    row.try_get::<i32, _>(i).map_err(|e| map_error(&e))
}

pub(crate) fn flag(row: &tiberius::Row, i: usize) -> Result<bool, DriverError> {
    Ok(row.try_get::<bool, _>(i).map_err(|e| map_error(&e))?.unwrap_or(false))
}

/// T-081 / R-028: which catalog areas this login can demonstrably read,
/// probed at connect by running each area's query shape with `TOP 0`.
pub async fn probe(conn: &mut Conn) -> CatalogSupport {
    let checks: [(&str, &str, &str); 5] = [
        ("databases", "Databases", "SELECT TOP 0 name FROM sys.databases"),
        ("schemas", "Schemas", "SELECT TOP 0 name FROM sys.schemas"),
        ("relations", "Tables and views", "SELECT TOP 0 name, type FROM sys.objects"),
        ("columns", "Columns", "SELECT TOP 0 c.name FROM sys.columns c JOIN sys.types t ON t.user_type_id = c.user_type_id"),
        // Azure SQL Database refuses any three-part name outside the current
        // database (error 40515), so the tree must not offer other databases.
        ("cross_database", "Other databases", "SELECT TOP 0 name FROM [master].sys.objects"),
    ];
    let mut features = Vec::with_capacity(checks.len() + 1);
    for (id, label, sql) in checks {
        let available = rows(conn, sql, &[]).await.is_ok();
        features.push(CatalogFeature { id: id.into(), label: label.into(), available });
    }
    let indexes = rows(conn, "SELECT TOP 0 i.name FROM sys.indexes i JOIN sys.index_columns ic ON ic.object_id = i.object_id", &[])
        .await
        .is_ok();
    features.push(CatalogFeature { id: "indexes".into(), label: "Indexes".into(), available: indexes });
    CatalogSupport { flavour_verified: true, features }
}

pub async fn children(
    conn: &mut Conn,
    parent: Option<NodeId>,
    current_db: &str,
    catalog: &CatalogSupport,
) -> Result<Vec<NavNode>, DriverError> {
    let skipped = |id: &str, what: &str| {
        (!catalog.available(id))
            .then(|| vec![notice(parent.as_ref(), &format!("{what} are not available on this server (catalog probe failed)"))])
    };
    let result = match &parent {
        None => match skipped("databases", "databases") {
            Some(note) => Ok(note),
            None => databases(conn, current_db, catalog.available("cross_database")).await,
        },
        Some(n) if n.kind == "database" => match skipped("schemas", "schemas") {
            Some(note) => Ok(note),
            None => match n.path.as_slice() {
                [db] => schemas(conn, db).await,
                other => Err(malformed(other)),
            },
        },
        Some(n) if n.kind == "schema" => match skipped("relations", "tables and views") {
            Some(note) => Ok(note),
            None => match n.path.as_slice() {
                [db, schema] => relations(conn, db, schema).await,
                other => Err(malformed(other)),
            },
        },
        Some(n) if RELATION_KINDS.contains(&n.kind.as_str()) => match n.path.as_slice() {
            [db, schema, rel] => columns_and_indexes(conn, db, schema, rel, &n.kind, catalog).await,
            other => Err(malformed(other)),
        },
        Some(n) => {
            return Err(DriverError::new(ErrorKind::NotFound, format!("node kind `{}` has no children", n.kind)));
        }
    };
    render_around(result, parent.as_ref())
}

fn malformed(path: &[String]) -> DriverError {
    DriverError::new(ErrorKind::NotFound, format!("malformed node path {path:?}"))
}

/// D-016: an unreadable or divergent catalog area is a note in the tree, not
/// a failed navigation.
fn render_around(result: Result<Vec<NavNode>, DriverError>, parent: Option<&NodeId>) -> Result<Vec<NavNode>, DriverError> {
    match result {
        Err(e) if e.kind == ErrorKind::PermissionDenied => Ok(vec![notice(parent, "not permitted to read this part of the catalog")]),
        Err(e) if error_map::is_divergence(&e) => {
            Ok(vec![notice(parent, "this server's catalog differs here, so this part cannot be shown")])
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

async fn databases(conn: &mut Conn, current: &str, cross_database: bool) -> Result<Vec<NavNode>, DriverError> {
    let node = |name: String| NavNode {
        id: NodeId { kind: "database".into(), path: vec![name.clone()] },
        label: name,
        has_children: true,
    };
    if !cross_database {
        // Only the connected database is reachable from this session.
        return Ok(vec![
            node(current.to_string()),
            notice(None, "other databases cannot be browsed from this connection"),
        ]);
    }
    // System databases (ids 1–4) are noise for a browsing tool, unless the
    // profile connected to one deliberately. HAS_DBACCESS hides databases the
    // login cannot open, rather than listing ones that fail on expand.
    const SQL: &str = "
        SELECT CAST(name AS nvarchar(128))
        FROM sys.databases
        WHERE HAS_DBACCESS(name) = 1
          AND state = 0
          AND (database_id > 4 OR name = @P1)
        ORDER BY name";
    let found = rows(conn, SQL, &[&current]).await?;
    found.iter().filter_map(|r| text(r, 0).transpose()).map(|n| n.map(node)).collect()
}

async fn schemas(conn: &mut Conn, db: &str) -> Result<Vec<NavNode>, DriverError> {
    // Fixed database roles own schemas with ids >= 16384; `sys`, `guest` and
    // INFORMATION_SCHEMA hold nothing to browse.
    let sql = format!(
        "SELECT CAST(name AS nvarchar(128)) FROM {}.sys.schemas
         WHERE schema_id < 16384 AND name NOT IN ('sys', 'guest', 'INFORMATION_SCHEMA')
         ORDER BY name",
        quote_ident(db)
    );
    let found = rows(conn, &sql, &[]).await?;
    found
        .iter()
        .filter_map(|r| text(r, 0).transpose())
        .map(|n| {
            n.map(|name| NavNode {
                id: NodeId { kind: "schema".into(), path: vec![db.to_string(), name.clone()] },
                label: name,
                has_children: true,
            })
        })
        .collect()
}

async fn relations(conn: &mut Conn, db: &str, schema: &str) -> Result<Vec<NavNode>, DriverError> {
    let d = quote_ident(db);
    let sql = format!(
        "SELECT CAST(o.name AS nvarchar(128)), CAST(o.type AS nvarchar(2))
         FROM {d}.sys.objects o JOIN {d}.sys.schemas s ON s.schema_id = o.schema_id
         WHERE s.name = @P1 AND o.type IN ('U', 'V') AND o.is_ms_shipped = 0
         ORDER BY o.name"
    );
    let found = rows(conn, &sql, &[&schema]).await?;
    let mut out = Vec::with_capacity(found.len());
    for r in &found {
        let (Some(name), Some(ty)) = (text(r, 0)?, text(r, 1)?) else { continue };
        let kind = if ty.trim() == "V" { "view" } else { "table" };
        out.push(NavNode {
            id: NodeId { kind: kind.into(), path: vec![db.to_string(), schema.to_string(), name.clone()] },
            label: name,
            has_children: true,
        });
    }
    Ok(out)
}

async fn columns_and_indexes(
    conn: &mut Conn,
    db: &str,
    schema: &str,
    rel: &str,
    kind: &str,
    catalog: &CatalogSupport,
) -> Result<Vec<NavNode>, DriverError> {
    let d = quote_ident(db);
    // Resolved by name through the catalog joins, with schema and relation as
    // bound parameters — never interpolated.
    let objid_sql = format!(
        "SELECT o.object_id FROM {d}.sys.objects o JOIN {d}.sys.schemas s ON s.schema_id = o.schema_id
         WHERE s.name = @P1 AND o.name = @P2"
    );
    let parent = NodeId { kind: kind.into(), path: vec![db.to_string(), schema.to_string(), rel.to_string()] };
    let found = rows(conn, &objid_sql, &[&schema, &rel]).await?;
    let Some(objid) = found.first().map(|r| int(r, 0)).transpose()?.flatten() else {
        return Ok(vec![notice(Some(&parent), "this relation no longer exists — refresh")]);
    };

    let mut out = Vec::new();
    if catalog.available("columns") {
        let sql = format!(
            "SELECT CAST(c.name AS nvarchar(128)), CAST(t.name AS nvarchar(128)), c.max_length, c.precision, c.scale, c.is_nullable
             FROM {d}.sys.columns c JOIN {d}.sys.types t ON t.user_type_id = c.user_type_id
             WHERE c.object_id = @P1
             ORDER BY c.column_id"
        );
        match rows(conn, &sql, &[&objid]).await {
            Ok(cols) => {
                for r in &cols {
                    let Some(name) = text(r, 0)? else { continue };
                    let ty = type_label(
                        &text(r, 1)?.unwrap_or_default(),
                        r.try_get::<i16, _>(2).ok().flatten(),
                        r.try_get::<u8, _>(3).ok().flatten(),
                        r.try_get::<u8, _>(4).ok().flatten(),
                    );
                    let nullable = flag(r, 5)?;
                    out.push(NavNode {
                        id: NodeId {
                            kind: "column".into(),
                            path: vec![db.into(), schema.into(), rel.into(), name.clone()],
                        },
                        label: format!("{name}  {ty}{}", if nullable { "" } else { " · not null" }),
                        has_children: false,
                    });
                }
            }
            Err(e) if error_map::is_divergence(&e) => out.push(notice(Some(&parent), "columns cannot be listed on this server")),
            Err(e) => return Err(e),
        }
    } else {
        out.push(notice(Some(&parent), "columns are not available on this server (catalog probe failed)"));
    }

    if catalog.available("indexes") {
        let sql = format!(
            "SELECT CAST(i.name AS nvarchar(128)), CAST(i.type_desc AS nvarchar(60)), i.is_unique, i.is_primary_key
             FROM {d}.sys.indexes i
             WHERE i.object_id = @P1 AND i.name IS NOT NULL
             ORDER BY i.is_primary_key DESC, i.name"
        );
        match rows(conn, &sql, &[&objid]).await {
            Ok(idx) => {
                for r in &idx {
                    let Some(name) = text(r, 0)? else { continue };
                    let method = text(r, 1)?.unwrap_or_default().to_lowercase();
                    let tag = if flag(r, 3)? {
                        " · primary key"
                    } else if flag(r, 2)? {
                        " · unique"
                    } else {
                        ""
                    };
                    out.push(NavNode {
                        id: NodeId {
                            kind: "index".into(),
                            path: vec![db.into(), schema.into(), rel.into(), name.clone()],
                        },
                        label: format!("{name}  {method}{tag}"),
                        has_children: false,
                    });
                }
            }
            Err(e) if error_map::is_divergence(&e) => out.push(notice(Some(&parent), "indexes cannot be listed on this server")),
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// `nvarchar(50)`, `decimal(19,4)`, `varbinary(max)` — the declared type as a
/// SQL Server user would write it.
fn type_label(ty: &str, max_length: Option<i16>, precision: Option<u8>, scale: Option<u8>) -> String {
    let len = |bytes_per_char: i16| match max_length {
        Some(-1) => "(max)".to_string(),
        Some(n) => format!("({})", n / bytes_per_char),
        None => String::new(),
    };
    match ty {
        "nvarchar" | "nchar" => format!("{ty}{}", len(2)),
        "varchar" | "char" | "varbinary" | "binary" => format!("{ty}{}", len(1)),
        "decimal" | "numeric" => match (precision, scale) {
            (Some(p), Some(s)) => format!("{ty}({p},{s})"),
            _ => ty.to_string(),
        },
        "datetime2" | "datetimeoffset" | "time" => match scale {
            Some(s) if s != 7 => format!("{ty}({s})"),
            _ => ty.to_string(),
        },
        _ => ty.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_bracket_quoted_with_the_bracket_doubled() {
        assert_eq!(quote_ident("orders"), "[orders]");
        assert_eq!(quote_ident("a]b"), "[a]]b]");
        assert_eq!(quote_ident("x]; DROP TABLE t; --"), "[x]]; DROP TABLE t; --]");
    }

    #[test]
    fn declared_types_render_as_written() {
        assert_eq!(type_label("nvarchar", Some(100), None, None), "nvarchar(50)");
        assert_eq!(type_label("varbinary", Some(-1), None, None), "varbinary(max)");
        assert_eq!(type_label("decimal", None, Some(19), Some(4)), "decimal(19,4)");
        assert_eq!(type_label("datetime2", None, None, Some(7)), "datetime2");
        assert_eq!(type_label("datetime2", None, None, Some(3)), "datetime2(3)");
        assert_eq!(type_label("int", Some(4), Some(10), Some(0)), "int");
    }

    #[test]
    fn every_kind_the_tree_returns_is_declared() {
        let declared: Vec<String> = node_kinds().into_iter().map(|k| k.id).collect();
        for k in ["database", "schema", "table", "view", "column", "index", "notice"] {
            assert!(declared.iter().any(|d| d == k), "{k}");
        }
    }
}
