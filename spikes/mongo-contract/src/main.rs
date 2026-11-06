//! Spike runner for T-073 (R-029) and T-046 (R-039).
//!
//! Usage: MONGO_URI=mongodb://localhost:27018/?directConnection=true cargo run

mod contract;
mod findings;
mod mongo;
mod session_probe;

use contract::*;
use findings::{Fit, Recorder};
use mongodb::bson::{doc, Bson, Document};

const DB: &str = "sluice_spike";
const COLL: &str = "events";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let uri = std::env::var("MONGO_URI")
        .unwrap_or_else(|_| "mongodb://localhost:27018/?directConnection=true".into());

    let rec = Recorder::new();
    let drv = mongo::MongoDriver::connect(&uri, rec.clone()).await?;

    seed(&drv).await?;

    println!("=== T-073: cross-paradigm contract against a real mongod ===\n");

    let caps = drv.capabilities();
    println!("[capabilities] {}\n", serde_json::to_string(&caps)?);

    // ---- 1. navigator tree
    let roots = drv.navigator_children(None).await?;
    println!("[navigator] roots: {:?}", roots.iter().map(|n| &n.label).collect::<Vec<_>>());
    let db_node = roots
        .iter()
        .find(|n| n.label == DB)
        .expect("spike db present")
        .id
        .clone();
    let colls = drv.navigator_children(Some(db_node)).await?;
    println!("[navigator] collections: {:?}", colls.iter().map(|n| &n.label).collect::<Vec<_>>());

    let relation = colls
        .iter()
        .find(|n| n.label == COLL)
        .expect("spike collection present")
        .id
        .clone();

    // ---- 2. structured browse with filter and sort
    let req = BrowseRequest {
        relation: relation.clone(),
        filter: Some(FilterTree::And(vec![
            FilterTree::Pred(Predicate {
                column: "name".into(),
                op: "contains".into(),
                value: WireValue::Text("A".into()), // uppercase on purpose: R-009
            }),
        ])),
        sort: vec![SortKey { column: "name".into(), descending: false }],
        limit: 10,
        cursor: None,
    };
    let res = drv.browse(req).await?;
    print_result("browse(filter+sort)", &res);

    // ---- 3. one edit
    let first_id = first_object_id(&drv).await?;
    let edit = EditRequest {
        relation: relation.clone(),
        key: vec![(
            "_id".into(),
            WireValue::Unsupported { type_name: "objectId".into(), rendered: first_id.clone() },
        )],
        set: vec![
            ("name".into(), WireValue::Text("alpha-edited".into())),
            ("meta.k".into(), WireValue::Text("nested-write".into())),
        ],
        originals: vec![
            ("name".into(), WireValue::Text("alpha".into())),
            (
                "tags".into(),
                WireValue::Unsupported { type_name: "array".into(), rendered: "[\"a\",\"b\"]".into() },
            ),
        ],
    };
    let res = drv.apply_edit(edit).await?;
    print_result("apply_edit", &res);

    // ---- 4. one raw command
    let res = drv
        .raw_command(format!(r#"{{"count":"{}","$db":"{}"}}"#, COLL, DB))
        .await?;
    print_result("raw_command", &res);

    // ---- T-046
    println!("\n=== T-046: mongodb ClientSession for an editor tab ===\n");
    session_probe::probe(drv.client(), &rec).await?;

    // ---- findings
    println!("\n=== FINDINGS ===\n");
    let all = rec.all();
    let mut broke = 0;
    for (i, f) in all.iter().enumerate() {
        if f.fit == Fit::Broke {
            broke += 1;
        }
        println!("{:>2}. [{}] {} / {}\n    {}\n", i + 1, f.fit.tag(), f.scenario, f.surface, f.detail);
    }
    println!("total findings: {} ({} BROKE)", all.len(), broke);
    assert!(
        !all.is_empty(),
        "R-029 treats this spike as unfinished until it produces at least one finding"
    );

    Ok(())
}

fn print_result(label: &str, res: &ExecutionResult) {
    println!("[{}] tx={:?} outcomes={}", label, res.tx, res.outcomes.len());
    for o in &res.outcomes {
        match o {
            Outcome::Rows { columns, rows, truncated } => {
                println!(
                    "    columns: {:?}",
                    columns.iter().map(|c| format!("{}:{}", c.name, c.type_name)).collect::<Vec<_>>()
                );
                println!("    rows: {} (truncated={})", rows.len(), truncated);
                for r in rows {
                    println!("      {:?}", r.iter().map(short).collect::<Vec<_>>());
                }
            }
            Outcome::Affected { count } => println!("    affected: {}", count),
            Outcome::Ack { message } => println!("    ack: {}", message),
        }
    }
}

fn short(v: &WireValue) -> String {
    match v {
        WireValue::Unsupported { type_name, rendered } => {
            let r = if rendered.len() > 28 { format!("{}…", &rendered[..28]) } else { rendered.clone() };
            format!("<{}:{}>", type_name, r)
        }
        WireValue::Text(s) => format!("{:?}", s),
        other => format!("{:?}", other),
    }
}

async fn first_object_id(drv: &mongo::MongoDriver) -> Result<String, Box<dyn std::error::Error>> {
    let coll = drv.client().database(DB).collection::<Document>(COLL);
    let d = coll.find_one(doc! { "name": "alpha" }).await?.expect("alpha doc");
    Ok(d.get_object_id("_id")?.to_hex())
}

/// Heterogeneous on purpose: conflicting types per field, absent fields, nested
/// documents, arrays, and the BSON types with no SQL analogue.
async fn seed(drv: &mongo::MongoDriver) -> Result<(), Box<dyn std::error::Error>> {
    let coll = drv.client().database(DB).collection::<Document>(COLL);
    coll.drop().await.ok();

    let docs = vec![
        doc! {
            "name": "alpha",
            "qty": 5i32,
            "price": Bson::Decimal128("19.99".parse()?),
            "tags": ["a", "b"],
            "meta": { "k": "v", "n": 1i32 },
            "created": mongodb::bson::DateTime::now(),
        },
        doc! {
            "name": "Apex",
            // same field, different BSON type — the heterogeneity that matters
            "qty": "many",
            "price": 3.5f64,
            "extra_only_here": true,
        },
        doc! {
            "name": "gamma",
            "qty": 12i64,
            "nested": { "deep": { "x": 1i32 } },
            "blob": Bson::Binary(mongodb::bson::Binary {
                subtype: mongodb::bson::spec::BinarySubtype::Generic,
                bytes: vec![0xde, 0xad, 0xbe, 0xef],
            }),
        },
        doc! {
            // no `name` field at all
            "qty": 0i32,
        },
    ];
    coll.insert_many(docs).await?;
    Ok(())
}
