//! A thin but *real* Mongo driver implementing the draft contract.
//!
//! R-029's guard: this must be the engine reporting where the contract does not
//! fit, not the author's model of the engine. So every method below is written
//! to satisfy `contract.rs` as literally as the mongodb crate allows, and each
//! place that requires a stretch or an outright lie is recorded as a finding.

use crate::contract::*;
use crate::findings::{Fit, Recorder};
use async_trait::async_trait;
use futures::TryStreamExt;
use mongodb::bson::{doc, Bson, Document};
use mongodb::{Client, Collection};

pub struct MongoDriver {
    client: Client,
    rec: Recorder,
}

impl MongoDriver {
    pub async fn connect(uri: &str, rec: Recorder) -> Result<Self, DriverError> {
        let client = Client::with_uri_str(uri).await.map_err(err)?;
        Ok(Self { client, rec })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    fn coll(&self, relation: &NodeId) -> Result<Collection<Document>, DriverError> {
        if relation.path.len() != 2 {
            return Err(DriverError {
                kind: "bad_relation".into(),
                message: format!("expected [db, collection], got {:?}", relation.path),
            });
        }
        Ok(self
            .client
            .database(&relation.path[0])
            .collection::<Document>(&relation.path[1]))
    }
}

fn err(e: mongodb::error::Error) -> DriverError {
    DriverError {
        kind: "mongo".into(),
        message: e.to_string(),
    }
}

// ------------------------------------------------------------------ value map

/// Map a BSON value onto the contract's WireValue.
///
/// This is where the type model is under the most pressure: WireValue was
/// designed against SQL scalars, and BSON is not a scalar algebra.
pub fn bson_to_wire(b: &Bson, rec: &Recorder, scenario: &'static str) -> WireValue {
    match b {
        Bson::Null => WireValue::Null,
        Bson::Boolean(v) => WireValue::Bool(*v),
        Bson::Int32(v) => WireValue::Int(*v as i64),
        Bson::Int64(v) => WireValue::Int(*v),
        Bson::Double(v) => WireValue::Float(*v),
        Bson::String(v) => WireValue::Text(v.clone()),
        Bson::DateTime(v) => WireValue::Timestamp(v.try_to_rfc3339_string().unwrap_or_else(|_| v.to_string())),
        Bson::Binary(v) => WireValue::Bytes(v.bytes.clone()),

        // Exact numeric: Decimal128 has no lossless Rust numeric type in the
        // crate's public API, but it does render exactly, which is what T-016
        // asked for.
        Bson::Decimal128(v) => WireValue::Numeric(v.to_string()),

        // ObjectId is the de-facto primary key of every collection. It is not a
        // string, not an int, and round-tripping it as Text loses the type
        // needed to build an edit's key filter.
        Bson::ObjectId(v) => {
            rec.record(
                scenario,
                Fit::Strained,
                "WireValue",
                format!(
                    "ObjectId({}) has no WireValue variant. Rendered as Unsupported so the \
                     type survives to the UI; a Text mapping would have silently destroyed \
                     the distinction the edit key filter depends on.",
                    v
                ),
            );
            WireValue::Unsupported {
                type_name: "objectId".into(),
                rendered: v.to_hex(),
            }
        }

        // The structural types. This is the contract's hardest break.
        Bson::Document(d) => {
            rec.record(
                scenario,
                Fit::Broke,
                "WireValue",
                format!(
                    "Embedded document with keys {:?} has no WireValue variant. The contract's \
                     value algebra is scalar-only, so a nested document can only reach the UI \
                     as opaque rendered text — which means it cannot be edited field-wise, \
                     only replaced wholesale.",
                    d.keys().collect::<Vec<_>>()
                ),
            );
            WireValue::Unsupported {
                type_name: "document".into(),
                rendered: serde_json::to_string(&Bson::Document(d.clone()))
                    .unwrap_or_else(|_| "{?}".into()),
            }
        }
        Bson::Array(a) => {
            rec.record(
                scenario,
                Fit::Broke,
                "WireValue",
                format!(
                    "Array of len {} has no WireValue variant; same consequence as embedded \
                     documents, and it is additionally the case R-026 is about (positional \
                     $set paths).",
                    a.len()
                ),
            );
            WireValue::Unsupported {
                type_name: "array".into(),
                rendered: serde_json::to_string(&Bson::Array(a.clone()))
                    .unwrap_or_else(|_| "[?]".into()),
            }
        }

        other => {
            rec.record(
                scenario,
                Fit::Strained,
                "WireValue",
                format!("BSON {:?} falls through to Unsupported.", other.element_type()),
            );
            WireValue::Unsupported {
                type_name: format!("{:?}", other.element_type()),
                rendered: format!("{}", other),
            }
        }
    }
}

fn wire_to_bson(v: &WireValue) -> Bson {
    match v {
        WireValue::Null => Bson::Null,
        WireValue::Bool(b) => Bson::Boolean(*b),
        WireValue::Int(i) => Bson::Int64(*i),
        WireValue::Float(f) => Bson::Double(*f),
        WireValue::Numeric(s) | WireValue::Text(s) => Bson::String(s.clone()),
        WireValue::Timestamp(s) => Bson::String(s.clone()),
        WireValue::Bytes(b) => Bson::Binary(mongodb::bson::Binary {
            subtype: mongodb::bson::spec::BinarySubtype::Generic,
            bytes: b.clone(),
        }),
        // An ObjectId arriving back from the UI as Unsupported has to be
        // reconstructed by type_name, because the contract lost the type.
        WireValue::Unsupported { type_name, rendered } => {
            if type_name == "objectId" {
                mongodb::bson::oid::ObjectId::parse_str(rendered)
                    .map(Bson::ObjectId)
                    .unwrap_or_else(|_| Bson::String(rendered.clone()))
            } else {
                Bson::String(rendered.clone())
            }
        }
    }
}

// ------------------------------------------------------------------ filters

fn filter_to_doc(f: &FilterTree, rec: &Recorder, scenario: &'static str) -> Document {
    match f {
        FilterTree::And(kids) => {
            doc! { "$and": kids.iter().map(|k| filter_to_doc(k, rec, scenario)).collect::<Vec<_>>() }
        }
        FilterTree::Or(kids) => {
            doc! { "$or": kids.iter().map(|k| filter_to_doc(k, rec, scenario)).collect::<Vec<_>>() }
        }
        FilterTree::Pred(p) => {
            let v = wire_to_bson(&p.value);
            match p.op.as_str() {
                "eq" => doc! { &p.column: v },
                "ne" => doc! { &p.column: { "$ne": v } },
                "gt" => doc! { &p.column: { "$gt": v } },
                "lt" => doc! { &p.column: { "$lt": v } },
                "contains" => {
                    // R-009 in the flesh: the universal `contains` is ILIKE-ish on
                    // Postgres and a case-SENSITIVE regex here unless opted out.
                    rec.record(
                        scenario,
                        Fit::Strained,
                        "filter/contains",
                        "`contains` compiles to $regex, which is case-SENSITIVE by default \
                         and interprets regex metacharacters in user input. Postgres ILIKE \
                         is case-insensitive and treats the needle literally. Same declared \
                         operator, three behavioural differences — exactly R-009. Compiled \
                         here with $options:'i' and full metacharacter quoting to match \
                         ILIKE semantics.",
                    );
                    let quoted = regex_quote(&as_text(&p.value));
                    doc! { &p.column: { "$regex": quoted, "$options": "i" } }
                }
                "is_null" => doc! { &p.column: { "$in": [Bson::Null] } },
                other => {
                    rec.record(
                        scenario,
                        Fit::Broke,
                        "filter/op",
                        format!("operator `{}` has no Mongo compilation", other),
                    );
                    doc! {}
                }
            }
        }
    }
}

fn as_text(v: &WireValue) -> String {
    match v {
        WireValue::Text(s) | WireValue::Numeric(s) | WireValue::Timestamp(s) => s.clone(),
        WireValue::Int(i) => i.to_string(),
        WireValue::Float(f) => f.to_string(),
        WireValue::Bool(b) => b.to_string(),
        WireValue::Unsupported { rendered, .. } => rendered.clone(),
        _ => String::new(),
    }
}

fn regex_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// ------------------------------------------------------------------ the impl

#[async_trait]
impl Driver for MongoDriver {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            engine: "mongodb".into(),
            // Left to the caller to set from T-046's evidence rather than
            // guessed here — see main.rs.
            affinity: SessionAffinity::None,
            reset: ResetSupport::NotApplicable,
            node_kinds: vec![
                NodeKind {
                    id: "database".into(),
                    label: "Database".into(),
                    has_children: true,
                    is_relation: false,
                },
                NodeKind {
                    id: "collection".into(),
                    label: "Collection".into(),
                    has_children: false,
                    is_relation: true,
                },
            ],
            filter_ops: vec![
                FilterOpDecl {
                    id: "eq".into(),
                    label: "equals".into(),
                    type_classes: vec!["*".into()],
                    case_sensitive: Some(true),
                },
                FilterOpDecl {
                    id: "contains".into(),
                    label: "contains".into(),
                    type_classes: vec!["text".into()],
                    case_sensitive: Some(false),
                },
            ],
            supports_transactions: true,
            supports_script: false,
            fixed_schema: false,
        }
    }

    async fn navigator_children(
        &self,
        parent: Option<NodeId>,
    ) -> Result<Vec<NavNode>, DriverError> {
        match parent {
            None => {
                let names = self.client.list_database_names().await.map_err(err)?;
                self.rec.record(
                    "navigator",
                    Fit::Held,
                    "navigator/tree",
                    format!(
                        "Two declared node kinds (database → collection) expressed the whole \
                         tree with no schema level and no placeholder node. {} databases at \
                         root. The driver-declared-node-kinds design (T-008) absorbed the \
                         paradigm difference cleanly.",
                        names.len()
                    ),
                );
                Ok(names
                    .into_iter()
                    .map(|n| NavNode {
                        id: NodeId {
                            kind: "database".into(),
                            path: vec![n.clone()],
                        },
                        label: n,
                        has_children: true,
                    })
                    .collect())
            }
            Some(p) if p.kind == "database" => {
                let db = self.client.database(&p.path[0]);
                let names = db.list_collection_names().await.map_err(err)?;
                Ok(names
                    .into_iter()
                    .map(|n| NavNode {
                        id: NodeId {
                            kind: "collection".into(),
                            path: vec![p.path[0].clone(), n.clone()],
                        },
                        label: n,
                        has_children: false,
                    })
                    .collect())
            }
            Some(p) => Err(DriverError {
                kind: "bad_node".into(),
                message: format!("no children for kind {}", p.kind),
            }),
        }
    }

    async fn browse(&self, req: BrowseRequest) -> Result<ExecutionResult, DriverError> {
        let coll = self.coll(&req.relation)?;

        let mut filter = req
            .filter
            .as_ref()
            .map(|f| filter_to_doc(f, &self.rec, "browse"))
            .unwrap_or_default();

        // Keyset pagination (T-047): the cursor is the previous page's last
        // sort-key values.
        if let Some(cursor) = &req.cursor {
            if let Some(first_sort) = req.sort.first() {
                if let Some(v) = cursor.first() {
                    let op = if first_sort.descending { "$lt" } else { "$gt" };
                    filter.insert(&first_sort.column, doc! { op: wire_to_bson(v) });
                }
            }
        }

        let mut sort_doc = Document::new();
        for s in &req.sort {
            sort_doc.insert(&s.column, if s.descending { -1 } else { 1 });
        }
        if sort_doc.is_empty() {
            sort_doc.insert("_id", 1);
        }

        let mut cur = coll
            .find(filter)
            .sort(sort_doc)
            .limit(req.limit as i64)
            .await
            .map_err(err)?;

        let mut docs: Vec<Document> = Vec::new();
        while let Some(d) = cur.try_next().await.map_err(err)? {
            docs.push(d);
        }

        // The contract wants a fixed column set. Mongo has none, so it must be
        // derived from whatever this page happened to contain.
        let mut col_names: Vec<String> = Vec::new();
        for d in &docs {
            for k in d.keys() {
                if !col_names.iter().any(|c| c == k) {
                    col_names.push(k.clone());
                }
            }
        }

        self.rec.record(
            "browse",
            Fit::Broke,
            "browse/columns",
            format!(
                "ExecutionResult::Rows requires `columns`, but Mongo has no catalog to \
                 supply them. Derived {} columns by unioning keys across the {} documents \
                 in THIS page: {:?}. Two consequences the contract does not model: the \
                 column set is a property of the page rather than the relation (page 2 can \
                 legitimately have different columns), and a field absent from a document \
                 is indistinguishable from a field explicitly set to null.",
                col_names.len(),
                docs.len(),
                col_names
            ),
        );

        let columns: Vec<ColumnDesc> = col_names
            .iter()
            .map(|name| {
                // type_name/type_class are per-relation in the contract, but here
                // they are per-VALUE. Take the first non-missing occurrence.
                let (type_name, type_class) = docs
                    .iter()
                    .find_map(|d| d.get(name))
                    .map(|b| bson_class(b))
                    .unwrap_or(("missing", "unknown"));
                ColumnDesc {
                    name: name.clone(),
                    type_name: type_name.into(),
                    type_class: type_class.into(),
                    nullable: true,
                }
            })
            .collect();

        // Heterogeneity check: does any column hold more than one BSON type?
        for name in &col_names {
            let mut seen: Vec<&'static str> = Vec::new();
            for d in &docs {
                if let Some(b) = d.get(name) {
                    let (tn, _) = bson_class(b);
                    if !seen.contains(&tn) {
                        seen.push(tn);
                    }
                }
            }
            if seen.len() > 1 {
                self.rec.record(
                    "browse",
                    Fit::Broke,
                    "ColumnDesc",
                    format!(
                        "Column `{}` holds {} different BSON types in one page ({:?}). \
                         ColumnDesc declares exactly one `type_name`/`type_class` per column, \
                         so whichever the UI is told is wrong for some rows — and T-013's \
                         type-class-driven filter operator list is therefore wrong too.",
                        name,
                        seen.len(),
                        seen
                    ),
                );
            }
        }

        let rows: Vec<Vec<WireValue>> = docs
            .iter()
            .map(|d| {
                col_names
                    .iter()
                    .map(|c| match d.get(c) {
                        Some(b) => bson_to_wire(b, &self.rec, "browse"),
                        None => WireValue::Null,
                    })
                    .collect()
            })
            .collect();

        let truncated = rows.len() as u32 == req.limit;
        Ok(ExecutionResult {
            outcomes: vec![Outcome::Rows {
                columns,
                rows,
                truncated,
            }],
            tx: TxContext::Autocommit,
        })
    }

    async fn apply_edit(&self, edit: EditRequest) -> Result<ExecutionResult, DriverError> {
        let coll = self.coll(&edit.relation)?;

        let mut filter = Document::new();
        for (k, v) in &edit.key {
            filter.insert(k, wire_to_bson(v));
        }

        // T-058's optimistic concurrency: every comparable original goes in the
        // WHERE clause.
        let mut incomparable: Vec<String> = Vec::new();
        for (k, v) in &edit.originals {
            match v {
                WireValue::Unsupported { type_name, .. }
                    if type_name == "document" || type_name == "array" =>
                {
                    incomparable.push(k.clone());
                }
                _ => {
                    filter.insert(k, wire_to_bson(v));
                }
            }
        }
        if !incomparable.is_empty() {
            self.rec.record(
                "edit",
                Fit::Broke,
                "concurrency",
                format!(
                    "Columns {:?} hold documents/arrays and were DROPPED from the optimistic \
                     concurrency predicate. BSON equality on an embedded document is \
                     order-sensitive on keys, so comparing them would produce false conflicts \
                     on a semantically identical document. The contract offers no way to \
                     declare a column non-comparable per-ROW, only per-column (T-058/R-035), \
                     and here comparability is a property of the value.",
                    incomparable
                ),
            );
        }

        let mut set_doc = Document::new();
        for (k, v) in &edit.set {
            if k.contains('.') {
                self.rec.record(
                    "edit",
                    Fit::Strained,
                    "edit/paths",
                    format!(
                        "Set path `{}` contains a dot. $set interprets it as a nested path, \
                         but a Mongo field name may legally contain a literal dot — so the \
                         contract's flat (String, WireValue) pairs cannot distinguish \
                         'nested field' from 'field whose name has a dot'. This is R-026 \
                         reproduced against a live server.",
                        k
                    ),
                );
            }
            set_doc.insert(k, wire_to_bson(v));
        }

        let res = coll
            .update_one(filter, doc! { "$set": set_doc })
            .await
            .map_err(err)?;

        Ok(ExecutionResult {
            outcomes: vec![Outcome::Affected {
                count: res.modified_count,
            }],
            tx: TxContext::Autocommit,
        })
    }

    async fn raw_command(&self, script: String) -> Result<ExecutionResult, DriverError> {
        // The contract hands the driver a *script string*, because the editor is
        // a SQL editor. Mongo's raw surface is a BSON command document.
        let parsed: serde_json::Value = serde_json::from_str(&script).map_err(|e| {
            DriverError {
                kind: "parse".into(),
                message: format!("raw command must be a JSON object: {}", e),
            }
        })?;
        let cmd: Document = mongodb::bson::to_document(&parsed).map_err(|e| DriverError {
            kind: "parse".into(),
            message: e.to_string(),
        })?;

        self.rec.record(
            "raw",
            Fit::Broke,
            "raw_command",
            "raw_command takes a String because the editor is a SQL editor (D-029). Mongo's \
             raw surface is a single BSON command *document*, so the string has to be parsed \
             as JSON first and there is no such thing as a multi-statement script. \
             Consequences: `supports_script: false`, T-011's dialect-aware splitting has no \
             meaning here, and the outcome SEQUENCE (T-010) is permanently length 1 — the \
             envelope's central affordance is unused on this engine.",
        );

        let db_name = cmd
            .get_str("$db")
            .unwrap_or("admin")
            .to_string();
        let mut cmd = cmd;
        cmd.remove("$db");

        let out = self
            .client
            .database(&db_name)
            .run_command(cmd)
            .await
            .map_err(err)?;

        Ok(ExecutionResult {
            outcomes: vec![Outcome::Ack {
                message: serde_json::to_string(&Bson::Document(out)).unwrap_or_default(),
            }],
            tx: TxContext::Autocommit,
        })
    }
}

pub fn bson_class(b: &Bson) -> (&'static str, &'static str) {
    match b {
        Bson::Null => ("null", "unknown"),
        Bson::Boolean(_) => ("bool", "boolean"),
        Bson::Int32(_) => ("int", "integer"),
        Bson::Int64(_) => ("long", "integer"),
        Bson::Double(_) => ("double", "float"),
        Bson::Decimal128(_) => ("decimal", "exact_numeric"),
        Bson::String(_) => ("string", "text"),
        Bson::ObjectId(_) => ("objectId", "opaque"),
        Bson::DateTime(_) => ("date", "timestamp"),
        Bson::Binary(_) => ("binData", "binary"),
        Bson::Document(_) => ("object", "structural"),
        Bson::Array(_) => ("array", "structural"),
        _ => ("other", "unknown"),
    }
}
