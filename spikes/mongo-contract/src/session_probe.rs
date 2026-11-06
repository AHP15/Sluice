//! T-046 / R-039: what does `mongodb::ClientSession` actually deliver for an
//! editor tab?
//!
//! The question is not "do sessions exist" — R-039 already establishes they do.
//! It is whether they deliver the two properties an editor tab needs: causal
//! consistency across the tab's statements, and an *interactive* multi-document
//! transaction the user drives at their own pace.

use crate::findings::{Fit, Recorder};
use mongodb::bson::{doc, Document};
use mongodb::{Client, ClientSession};

const DB: &str = "sluice_spike";

pub async fn probe(client: &Client, rec: &Recorder) -> Result<(), Box<dyn std::error::Error>> {
    let db = client.database(DB);
    let a = db.collection::<Document>("tx_a");
    let b = db.collection::<Document>("tx_b");
    a.drop().await.ok();
    b.drop().await.ok();
    a.insert_one(doc! { "seed": 1 }).await?;
    b.insert_one(doc! { "seed": 1 }).await?;

    // ---- 1. does start_session() exist and what does it hand back?
    let mut session: ClientSession = client.start_session().await?;
    println!("[session] start_session() -> ClientSession, id present: {}",
             !session.id().is_empty());
    rec.record(
        "session",
        Fit::Held,
        "start_session",
        "`Client::start_session()` returns an owned ClientSession with a server-assigned \
         lsid. It is a plain owned handle the pool can hold for a tab's lifetime — no \
         lifetime parameter, so it does not violate D-003's no-handles-in-signatures rule \
         as long as it stays behind the pool and never appears in a Driver method signature.",
    );

    // ---- 2. API shape: R-039 says `..._with_session`; check that against 3.x
    rec.record(
        "session",
        Fit::Strained,
        "api/naming",
        "R-039 describes `..._with_session` operation variants. Those are the mongodb 2.x \
         API. In 3.x they were removed in favour of `.session(&mut s)` on the action \
         builder (e.g. `coll.find(f).session(&mut s).await`). Same capability, different \
         surface — the risk's description is still correct in substance, but any code \
         written from it verbatim will not compile.",
    );

    // ---- 3. interactive multi-document transaction across two collections
    session.start_transaction().await?;
    a.insert_one(doc! { "in_tx": true }).session(&mut session).await?;

    // The interactive property: the transaction stays open across an await point
    // the *application* controls, exactly as an editor tab would hold it open
    // while the user types.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    b.insert_one(doc! { "in_tx": true }).session(&mut session).await?;

    // Uncommitted writes must be invisible to a non-session read.
    let outside = a.count_documents(doc! { "in_tx": true }).await?;
    let inside = a
        .count_documents(doc! { "in_tx": true })
        .session(&mut session)
        .await?;
    println!("[session] mid-transaction visibility: outside={} inside={}", outside, inside);

    let isolated = outside == 0 && inside == 1;
    rec.record(
        "session",
        if isolated { Fit::Held } else { Fit::Broke },
        "transaction/interactive",
        format!(
            "Held one transaction open across two collections and an application-controlled \
             250ms await, then read through both the session and a plain connection: \
             outside={} inside={}. Snapshot isolation {} — this is a genuinely interactive \
             multi-document transaction, not an all-in-one-call helper.",
            outside,
            inside,
            if isolated { "held" } else { "FAILED" }
        ),
    );

    session.commit_transaction().await?;
    let after = a.count_documents(doc! { "in_tx": true }).await?;
    println!("[session] after commit, visible outside: {}", after);

    // ---- 4. abort path
    session.start_transaction().await?;
    a.insert_one(doc! { "aborted": true }).session(&mut session).await?;
    session.abort_transaction().await?;
    let aborted = a.count_documents(doc! { "aborted": true }).await?;
    println!("[session] after abort, visible: {}", aborted);
    rec.record(
        "session",
        if aborted == 0 { Fit::Held } else { Fit::Broke },
        "transaction/abort",
        format!("abort_transaction() left {} of 1 rows visible (want 0).", aborted),
    );

    // ---- 5. causal consistency
    let mut causal = client.start_session().causal_consistency(true).await?;
    causal
        .client()
        .database(DB)
        .collection::<Document>("tx_a")
        .insert_one(doc! { "causal": 1 })
        .session(&mut causal)
        .await?;
    let op_time = causal.operation_time();
    println!("[session] causal consistency, operationTime after write: {:?}", op_time);
    rec.record(
        "session",
        if op_time.is_some() { Fit::Held } else { Fit::Broke },
        "causal_consistency",
        format!(
            "`start_session().causal_consistency(true)` is accepted and the session tracks \
             operationTime ({:?}) after a write, which is the mechanism that gives a tab \
             read-your-own-writes across statements even when they land on different \
             cluster members.",
            op_time
        ),
    );

    // ---- 6. the editor-tab killer: how long may a transaction stay open?
    let param: Document = client
        .database("admin")
        .run_command(doc! { "getParameter": 1, "transactionLifetimeLimitSeconds": 1 })
        .await?;
    let limit = param.get_i32("transactionLifetimeLimitSeconds").unwrap_or(-1);
    println!("[session] transactionLifetimeLimitSeconds = {}", limit);
    rec.record(
        "session",
        Fit::Strained,
        "transaction/lifetime",
        format!(
            "The server aborts any transaction older than transactionLifetimeLimitSeconds \
             (= {}s here, the default). An editor tab that opens an explicit transaction and \
             sits while the user thinks WILL be killed server-side, and the next statement \
             fails with a transient error. Postgres and SQL Server hold an idle transaction \
             indefinitely, so a tab's explicit-transaction UX is NOT portable across the \
             three engines on this capability alone — the pinning capability says nothing \
             about how long the pin survives.",
            limit
        ),
    );

    Ok(())
}
