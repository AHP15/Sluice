//! Column derivation for schemaless relations (T-063 / D-018).
//!
//! A document collection declares no columns, so before a grid can render
//! anything something has to say what a column *is*. D-018's answer, which
//! this module implements engine-neutrally:
//!
//! - when the relation carries a JSON-Schema validator (Mongo's `$jsonSchema`),
//!   its declared properties are the columns — it is authoritative and cheaper
//!   than sampling;
//! - otherwise the columns are the **union of paths observed in a bounded
//!   sample**, ordered by how many sampled documents carry them, each marked
//!   when the type it holds varies between documents.
//!
//! Properties the code below has to keep, each from D-018:
//!
//! - **Presentation-only.** Inference reads values and never rewrites one: no
//!   function here takes `&mut` to a document or returns a value. A document
//!   that does not match the inferred shape is still shown whole, and a path
//!   the validator does not declare but a sampled document carries is still a
//!   column — inference may *add* columns, never subtract a value.
//! - **Arrays are one cell.** The walker never descends into an array, so an
//!   array never becomes several columns and never becomes several rows; D-009's
//!   one-row-one-document edit identity depends on it.
//! - **Engine-neutral.** Inputs are the core's own [`WireValue`] documents and a
//!   plain JSON-Schema value. Fetching the sample (and *where* in the relation
//!   it comes from, R-015) is the driver's job — T-064, not this module.
//!
//! A driver opts in by declaring
//! [`ColumnDerivation::Inferred`](crate::capability::ColumnDerivation) in
//! `Capabilities::column_inference`.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::value::{ColumnDesc, ExactDomain, TypeClass, WireValue};

/// One document, in the order its fields were read.
pub type Document = Vec<(String, WireValue)>;

/// The bound applied when the caller does not give one. D-018: the cheap
/// default is "the first N documents", and N must stay small enough that
/// re-running inference on refresh is not itself a load on the server.
pub const DEFAULT_SAMPLE_LIMIT: u32 = 1000;

/// Tuning for one inference run.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct InferOptions {
    /// At most this many sampled documents are considered; the rest are
    /// ignored and the result says so (`sample_truncated`).
    pub sample_limit: u32,
    /// How many levels of nested documents become dotted columns. `1` — the
    /// default — keeps every top-level field as one column and a nested
    /// document as one cell, which keeps every column positionally editable.
    /// Arrays are never descended, whatever this says.
    pub max_depth: u32,
}

impl Default for InferOptions {
    fn default() -> Self {
        InferOptions { sample_limit: DEFAULT_SAMPLE_LIMIT, max_depth: 1 }
    }
}

/// Where a column came from.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ColumnSource {
    /// Declared by the validator and seen in the sample (or no sample given).
    Validator,
    /// Seen in the sample only. When a validator exists, this is a document
    /// the validator does not describe — shown anyway (presentation-only).
    Sample,
}

/// One derived column.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct InferredColumn {
    /// Field names from the document root. A path, not a dotted string: field
    /// names may themselves contain dots.
    pub path: Vec<String>,
    pub source: ColumnSource,
    /// Sampled documents in which the path holds a value — `null` included,
    /// absent (`Missing`) not. The ordering key.
    pub present: u32,
    /// Of those, how many held an explicit `null`.
    pub nulls: u32,
    /// Type tags seen at this path, first-seen order; `null` is not a type.
    pub observed: Vec<String>,
    /// Type tags the validator declares, if it declares any.
    pub declared: Vec<String>,
    /// D-018's marker: the path does not hold one type across documents — or
    /// holds one the validator does not declare. The grid must not trust a
    /// single type for per-row decisions when set (T-073 F-2).
    pub type_varies: bool,
}

impl InferredColumn {
    /// The display name: the path joined with dots.
    pub fn name(&self) -> String {
        self.path.join(".")
    }

    /// The single coarse type this path holds, or `Unknown` when it varies or
    /// nothing was seen.
    pub fn type_class(&self) -> TypeClass {
        let tags: Vec<&String> = if self.observed.is_empty() { self.declared.iter().collect() } else { self.observed.iter().collect() };
        match (self.type_varies, tags.as_slice()) {
            (false, [one]) => tag_class(one),
            _ => TypeClass::Unknown,
        }
    }

    /// The column as a page descriptor, for a driver whose browse ships
    /// derived columns (T-088). Always `derived`, and `mixed` exactly when the
    /// type varies. Never vouched comparable: a derived column's type is a
    /// guess, and D-027 never assumes a guarantee.
    pub fn to_column_desc(&self, sampled: u32) -> ColumnDesc {
        let type_class = self.type_class();
        let tags = if self.observed.is_empty() { &self.declared } else { &self.observed };
        ColumnDesc {
            name: self.name(),
            type_name: if tags.is_empty() { "unknown".into() } else { tags.join("|") },
            type_class,
            nullable: self.nulls > 0 || self.present < sampled || self.source == ColumnSource::Validator,
            mixed: self.type_varies,
            derived: true,
            exact: (type_class == TypeClass::ExactNumeric).then(ExactDomain::unconstrained_decimal),
            concurrency_comparable: false,
        }
    }
}

/// The result of one inference run.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ColumnInference {
    pub columns: Vec<InferredColumn>,
    /// Documents actually considered.
    pub sampled: u32,
    /// The bound that applied.
    pub sample_limit: u32,
    /// More documents were supplied than the bound allowed.
    pub sample_truncated: bool,
    /// A validator was supplied and declared at least one property.
    pub from_validator: bool,
}

/// Derive a relation's columns from its validator and/or a sample.
///
/// Order: validator-declared paths first (`required` order, then the rest by
/// name — a JSON object carries no order we can rely on), then every path seen
/// only in the sample, by descending frequency, ties in first-seen order.
/// With no validator the first group is empty and this is D-018's
/// frequency-ordered union.
pub fn infer_columns(validator: Option<&Json>, sample: &[Document], options: InferOptions) -> ColumnInference {
    let limit = options.sample_limit as usize;
    let used = &sample[..sample.len().min(limit)];
    let max_depth = options.max_depth.max(1);

    // Observe the sample.
    let mut stats: Vec<Stat> = Vec::new();
    let mut index: HashMap<Vec<String>, usize> = HashMap::new();
    for doc in used {
        let mut seen: HashSet<usize> = HashSet::new();
        let mut path = Vec::new();
        walk(doc, &mut path, 1, max_depth, &mut |p, v| {
            let i = *index.entry(p.to_vec()).or_insert_with(|| {
                stats.push(Stat { path: p.to_vec(), present: 0, nulls: 0, observed: Vec::new() });
                stats.len() - 1
            });
            // BSON permits a key twice in one document; count the document once.
            if !seen.insert(i) {
                return;
            }
            let s = &mut stats[i];
            s.present += 1;
            match type_tag(v) {
                None => s.nulls += 1,
                Some(tag) => {
                    if !s.observed.contains(&tag) {
                        s.observed.push(tag);
                    }
                }
            }
        });
    }

    // The validator's declared paths.
    let mut declared: Vec<(Vec<String>, Vec<String>)> = Vec::new();
    if let Some(schema) = validator {
        let root = schema.get("$jsonSchema").unwrap_or(schema);
        declare(root, &mut Vec::new(), 1, max_depth, &mut declared);
    }
    let from_validator = !declared.is_empty();

    let mut columns: Vec<InferredColumn> = Vec::new();
    let mut taken: HashSet<usize> = HashSet::new();
    for (path, decl) in declared {
        let stat = index.get(&path).map(|&i| {
            taken.insert(i);
            &stats[i]
        });
        let observed = stat.map(|s| s.observed.clone()).unwrap_or_default();
        let decl_set: HashSet<&str> = decl.iter().map(String::as_str).filter(|t| *t != "null").collect();
        let varies = decl_set.len() > 1
            || observed.len() > 1
            || (!decl_set.is_empty() && observed.iter().any(|t| !decl_set.contains(t.as_str())));
        columns.push(InferredColumn {
            path,
            source: ColumnSource::Validator,
            present: stat.map_or(0, |s| s.present),
            nulls: stat.map_or(0, |s| s.nulls),
            observed,
            declared: decl.into_iter().filter(|t| t != "null").collect(),
            type_varies: varies,
        });
    }

    // Sample-only paths: by frequency, stable on first-seen order.
    let mut rest: Vec<&Stat> = stats.iter().enumerate().filter(|(i, _)| !taken.contains(i)).map(|(_, s)| s).collect();
    rest.sort_by(|a, b| b.present.cmp(&a.present));
    for s in rest {
        columns.push(InferredColumn {
            path: s.path.clone(),
            source: ColumnSource::Sample,
            present: s.present,
            nulls: s.nulls,
            observed: s.observed.clone(),
            declared: Vec::new(),
            type_varies: s.observed.len() > 1,
        });
    }

    ColumnInference {
        columns,
        sampled: used.len() as u32,
        sample_limit: options.sample_limit,
        sample_truncated: sample.len() > limit,
        from_validator,
    }
}

struct Stat {
    path: Vec<String>,
    present: u32,
    nulls: u32,
    observed: Vec<String>,
}

/// Visit every leaf path of `doc`. A nested document within the depth bound is
/// descended *instead of* being a leaf, so its fields are columns and nothing
/// is shown twice — except an empty one, which has no fields to show it by and
/// so stays a leaf. Arrays are always leaves.
fn walk(doc: &Document, path: &mut Vec<String>, depth: u32, max_depth: u32, f: &mut dyn FnMut(&[String], &WireValue)) {
    for (k, v) in doc {
        path.push(k.clone());
        match v {
            WireValue::Missing => {}
            WireValue::Document(inner) if depth < max_depth && !inner.is_empty() => {
                walk(inner, path, depth + 1, max_depth, f)
            }
            _ => f(path, v),
        }
        path.pop();
    }
}

/// Collect a JSON Schema's declared property paths with their type tags.
fn declare(schema: &Json, path: &mut Vec<String>, depth: u32, max_depth: u32, out: &mut Vec<(Vec<String>, Vec<String>)>) {
    let Some(props) = schema.get("properties").and_then(Json::as_object) else { return };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Json::as_array)
        .map(|r| r.iter().filter_map(Json::as_str).collect())
        .unwrap_or_default();
    // `_id` leads, then `required` in its own (array, so ordered) order, then the
    // remaining properties by name.
    let mut names: Vec<&String> = props.keys().collect();
    let rank = |n: &str| -> (u8, usize) {
        if n == "_id" {
            (0, 0)
        } else if let Some(i) = required.iter().position(|r| *r == n) {
            (1, i)
        } else {
            (2, 0)
        }
    };
    names.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));

    for name in names {
        let sub = &props[name.as_str()];
        path.push(name.clone());
        let tags = schema_types(sub);
        let is_object = tags.iter().all(|t| t == "document") && !tags.is_empty();
        let has_props = sub.get("properties").and_then(Json::as_object).is_some_and(|p| !p.is_empty());
        if is_object && has_props && depth < max_depth {
            declare(sub, path, depth + 1, max_depth, out);
        } else {
            out.push((path.clone(), tags));
        }
        path.pop();
    }
}

/// A schema node's `bsonType` / `type`, as this module's type tags.
fn schema_types(node: &Json) -> Vec<String> {
    let raw = node.get("bsonType").or_else(|| node.get("type"));
    let names: Vec<&str> = match raw {
        Some(Json::String(s)) => vec![s.as_str()],
        Some(Json::Array(a)) => a.iter().filter_map(Json::as_str).collect(),
        _ => Vec::new(),
    };
    let mut tags: Vec<String> = Vec::new();
    for n in names {
        let tag = schema_tag(n);
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    tags
}

/// JSON-Schema / BSON type alias -> the tag [`type_tag`] gives such a value.
/// An alias this table does not know is kept verbatim, so it can only ever
/// widen the "varies" marker, never narrow it.
fn schema_tag(name: &str) -> String {
    match name {
        "string" => "text",
        "int" | "long" | "integer" => "int",
        "double" | "number" => "float",
        "decimal" => "numeric",
        "bool" | "boolean" => "bool",
        "date" | "timestamp" => "timestamp",
        "binData" => "bytes",
        "uuid" => "uuid",
        "object" => "document",
        "array" => "array",
        "null" => "null",
        other => other,
    }
    .to_string()
}

/// The type identity of one value, at the grain "varies" is judged at. `None`
/// for `null` (a value state, not a type). Opaque and undecodable values carry
/// the engine's own type name, so `ObjectId` and a string are told apart.
fn type_tag(v: &WireValue) -> Option<String> {
    Some(
        match v {
            WireValue::Null | WireValue::Missing => return None,
            WireValue::Bool(_) => "bool",
            WireValue::Int(_) => "int",
            WireValue::Float(_) => "float",
            WireValue::Numeric(_) => "numeric",
            WireValue::Text(_) => "text",
            WireValue::Bytes(_) => "bytes",
            WireValue::Timestamp(_) => "timestamp",
            WireValue::Uuid(_) => "uuid",
            WireValue::Document(_) => "document",
            WireValue::Array(_) => "array",
            WireValue::Oversized { type_name, .. }
            | WireValue::Opaque { type_name, .. }
            | WireValue::Unsupported { type_name, .. } => return Some(type_name.clone()),
        }
        .to_string(),
    )
}

fn tag_class(tag: &str) -> TypeClass {
    match tag {
        "bool" => TypeClass::Boolean,
        "int" => TypeClass::Integer,
        "float" => TypeClass::Float,
        "numeric" => TypeClass::ExactNumeric,
        "text" => TypeClass::Text,
        "bytes" => TypeClass::Binary,
        "timestamp" => TypeClass::Timestamp,
        "uuid" => TypeClass::Uuid,
        "document" | "array" => TypeClass::Structural,
        _ => TypeClass::Opaque,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn t(s: &str) -> WireValue {
        WireValue::Text(s.into())
    }
    fn doc(fields: &[(&str, WireValue)]) -> Document {
        fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }
    fn names(inf: &ColumnInference) -> Vec<String> {
        inf.columns.iter().map(InferredColumn::name).collect()
    }
    fn col<'a>(inf: &'a ColumnInference, name: &str) -> &'a InferredColumn {
        inf.columns.iter().find(|c| c.name() == name).unwrap_or_else(|| panic!("no column {name}"))
    }

    fn sample() -> Vec<Document> {
        vec![
            doc(&[("_id", WireValue::Int(1)), ("name", t("a")), ("tags", WireValue::Array(vec![t("x"), t("y")]))]),
            doc(&[("_id", WireValue::Int(2)), ("name", WireValue::Int(7)), ("rare", t("r"))]),
            doc(&[("_id", WireValue::Int(3)), ("name", WireValue::Null), ("tags", WireValue::Array(vec![]))]),
            doc(&[("_id", WireValue::Int(4)), ("tags", WireValue::Missing)]),
        ]
    }

    #[test]
    fn union_of_paths_ordered_by_frequency() {
        let inf = infer_columns(None, &sample(), InferOptions::default());
        assert_eq!(names(&inf), ["_id", "name", "tags", "rare"]);
        assert_eq!(col(&inf, "_id").present, 4);
        assert_eq!(col(&inf, "name").present, 3);
        // `Missing` is absent, not present: tags is in 2 documents, not 3.
        assert_eq!(col(&inf, "tags").present, 2);
        assert_eq!(col(&inf, "rare").present, 1);
        assert_eq!(inf.sampled, 4);
        assert!(!inf.from_validator);
        assert!(inf.columns.iter().all(|c| c.source == ColumnSource::Sample));
    }

    #[test]
    fn type_varying_paths_are_marked_and_null_is_not_a_type() {
        let inf = infer_columns(None, &sample(), InferOptions::default());
        let name = col(&inf, "name");
        assert!(name.type_varies);
        assert_eq!(name.observed, ["text", "int"]);
        assert_eq!(name.nulls, 1);
        assert_eq!(name.type_class(), TypeClass::Unknown);
        assert!(name.to_column_desc(inf.sampled).mixed);
        assert!(!col(&inf, "_id").type_varies);
        assert_eq!(col(&inf, "_id").type_class(), TypeClass::Integer);
    }

    #[test]
    fn arrays_are_one_cell_never_descended() {
        let docs = vec![doc(&[(
            "items",
            WireValue::Array(vec![WireValue::Document(vec![("sku".into(), t("A"))])]),
        )])];
        let inf = infer_columns(None, &docs, InferOptions { max_depth: 5, ..Default::default() });
        assert_eq!(names(&inf), ["items"]);
        assert_eq!(col(&inf, "items").observed, ["array"]);
    }

    #[test]
    fn nested_documents_are_one_cell_by_default_and_paths_on_request() {
        let docs = vec![
            doc(&[("addr", WireValue::Document(vec![("city".into(), t("Oran")), ("zip".into(), t("31000"))]))]),
            doc(&[("addr", t("unknown"))]),
            doc(&[("addr", WireValue::Document(vec![]))]),
        ];
        let flat = infer_columns(None, &docs, InferOptions::default());
        assert_eq!(names(&flat), ["addr"]);
        assert!(col(&flat, "addr").type_varies);

        let deep = infer_columns(None, &docs, InferOptions { max_depth: 2, ..Default::default() });
        // The scalar and the empty document still need a column to be seen by,
        // and two documents carry it against one each for the nested paths.
        assert_eq!(names(&deep), ["addr", "addr.city", "addr.zip"]);
        assert_eq!(col(&deep, "addr").observed, ["text", "document"]);
        assert_eq!(col(&deep, "addr.city").path, ["addr", "city"]);
    }

    #[test]
    fn dotted_field_names_stay_one_segment() {
        let docs = vec![doc(&[("a.b", WireValue::Int(1))])];
        let inf = infer_columns(None, &docs, InferOptions::default());
        assert_eq!(inf.columns[0].path, ["a.b"]);
    }

    #[test]
    fn sample_is_bounded() {
        let docs: Vec<Document> = (0..10).map(|i| doc(&[("n", WireValue::Int(i))])).collect();
        let inf = infer_columns(None, &docs, InferOptions { sample_limit: 3, ..Default::default() });
        assert_eq!(inf.sampled, 3);
        assert!(inf.sample_truncated);
        assert_eq!(col(&inf, "n").present, 3);
    }

    #[test]
    fn duplicate_keys_count_their_document_once() {
        let docs = vec![doc(&[("k", WireValue::Int(1)), ("k", t("x"))])];
        let inf = infer_columns(None, &docs, InferOptions::default());
        assert_eq!(col(&inf, "k").present, 1);
    }

    fn validator() -> Json {
        json!({ "$jsonSchema": {
            "bsonType": "object",
            "required": ["email", "_id"],
            "properties": {
                "zeta": { "bsonType": "string" },
                "email": { "bsonType": "string" },
                "_id": { "bsonType": "objectId" },
                "age": { "bsonType": ["int", "null"] },
                "score": { "bsonType": ["int", "double"] },
                "addr": { "bsonType": "object", "properties": { "city": { "bsonType": "string" } } }
            }
        }})
    }

    #[test]
    fn validator_columns_lead_in_declared_order() {
        let inf = infer_columns(Some(&validator()), &[], InferOptions::default());
        assert!(inf.from_validator);
        assert_eq!(names(&inf), ["_id", "email", "addr", "age", "score", "zeta"]);
        assert!(inf.columns.iter().all(|c| c.source == ColumnSource::Validator));
        // `null` in a type union is optionality, not variance.
        assert!(!col(&inf, "age").type_varies);
        assert_eq!(col(&inf, "age").declared, ["int"]);
        assert!(col(&inf, "score").type_varies);
        assert_eq!(col(&inf, "addr").declared, ["document"]);
        assert_eq!(col(&inf, "email").type_class(), TypeClass::Text);

        // Unwrapped schemas are accepted too, and nested properties on request.
        let bare = validator()["$jsonSchema"].clone();
        let deep = infer_columns(Some(&bare), &[], InferOptions { max_depth: 2, ..Default::default() });
        assert!(names(&deep).contains(&"addr.city".to_string()));
    }

    #[test]
    fn validator_never_hides_what_the_sample_carries() {
        let docs = vec![
            doc(&[("email", t("a@b")), ("extra", WireValue::Bool(true))]),
            // validationLevel "moderate" admits documents that do not match.
            doc(&[("email", WireValue::Int(5))]),
        ];
        let inf = infer_columns(Some(&validator()), &docs, InferOptions::default());
        let extra = col(&inf, "extra");
        assert_eq!(extra.source, ColumnSource::Sample);
        assert_eq!(inf.columns.last().unwrap().name(), "extra");
        let email = col(&inf, "email");
        assert_eq!(email.present, 2);
        // Declared one type, observed another: marked, not rejected.
        assert!(email.type_varies);
        assert_eq!(email.observed, ["text", "int"]);
    }

    #[test]
    fn a_validator_without_properties_falls_back_to_the_sample() {
        let v = json!({ "$jsonSchema": { "bsonType": "object" } });
        let inf = infer_columns(Some(&v), &sample(), InferOptions::default());
        assert!(!inf.from_validator);
        assert_eq!(names(&inf), ["_id", "name", "tags", "rare"]);
    }

    #[test]
    fn opaque_types_are_told_apart_by_their_engine_name() {
        let oid = |s: &str| WireValue::Opaque { type_name: "objectId".into(), rendered: s.into() };
        let docs = vec![doc(&[("ref", oid("65a"))]), doc(&[("ref", t("65a"))])];
        let inf = infer_columns(None, &docs, InferOptions::default());
        assert!(col(&inf, "ref").type_varies);
        assert_eq!(col(&inf, "ref").observed, ["objectId", "text"]);
    }

    /// Presentation-only: inference reads the sample and leaves it as it was.
    #[test]
    fn the_sample_is_not_touched() {
        let docs = sample();
        let before = docs.clone();
        let _ = infer_columns(Some(&validator()), &docs, InferOptions::default());
        assert_eq!(docs, before);
    }

    #[test]
    fn derived_descriptors_are_marked_and_never_vouched_comparable() {
        let inf = infer_columns(None, &sample(), InferOptions::default());
        let d = col(&inf, "_id").to_column_desc(inf.sampled);
        assert!(d.derived);
        assert!(!d.mixed);
        assert!(!d.concurrency_comparable);
        assert_eq!(d.type_class, TypeClass::Integer);
        let rare = col(&inf, "rare").to_column_desc(inf.sampled);
        assert!(rare.nullable, "absent from most documents");
    }

    #[test]
    fn result_round_trips_through_json() {
        let inf = infer_columns(Some(&validator()), &sample(), InferOptions::default());
        let back: ColumnInference = serde_json::from_str(&serde_json::to_string(&inf).unwrap()).unwrap();
        assert_eq!(back, inf);
    }
}
