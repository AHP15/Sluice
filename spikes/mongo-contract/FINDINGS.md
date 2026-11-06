# T-073 — Cross-paradigm contract validated against a real mongod

**Task**: T-073 · **Mitigates**: R-029 · **Implements**: D-022
**Run date**: 2026-08-02
**Environment**: `mongo:7` in Docker, single-node replica set `rs0` (a replica set
is required — a standalone mongod cannot open a transaction at all).
**Driver crate**: `mongodb` 3.8.0.

Reproduce:

```sh
docker run -d --name sluice-spike-mongo -p 27018:27017 mongo:7 --replSet rs0 --bind_ip_all
docker exec sluice-spike-mongo mongosh --quiet --eval \
  'rs.initiate({_id:"rs0",members:[{_id:0,host:"localhost:27017"}]})'
cd spikes/mongo-contract && cargo run
```

## What was actually run

`src/contract.rs` is a draft of the Driver contract written **from the relational
mental model the decisions were made in**, deliberately not pre-bent to fit
Mongo — that is R-029's whole point. `src/mongo.rs` is a thin but real driver
that implements it against a live server, and records a finding at every point
it has to stretch or lie. All four acceptance scenarios executed end to end
against a collection of heterogeneous documents (conflicting types per field,
absent fields, nested documents, arrays, ObjectId, Decimal128, Binary, DateTime):

| Scenario | Result |
|---|---|
| Navigator tree | 4 databases → 1 collection, two declared node kinds |
| Structured browse, filter + sort | 3 rows, 10 derived columns |
| One edit | 1 document modified |
| One raw command | `count` returned `{ok:1, n:4}` |

**20 findings: 5 HELD, 7 STRAINED, 8 BROKE.** R-029 is satisfied — the exercise
produced findings rather than confirmation, and the breaks are concentrated in
exactly one place.

## The breaks

### F-1 (BROKE) — `Rows.columns` presumes a catalog that does not exist

The contract requires every row set to carry a `Vec<ColumnDesc>`. Mongo has no
catalog to supply one, so it must be derived by unioning keys across the
documents in the page. Ten columns were derived from three documents. Two
consequences the contract cannot currently express:

- **The column set is a property of the page, not the relation.** Page 2 may
  legitimately have a different column set than page 1. Every consumer of
  `Rows.columns` — the grid, the filter bar, the editor — assumes it is stable
  for the relation.
- **Absent is not null.** A field missing from a document is filled as
  `WireValue::Null`, making it indistinguishable from a field explicitly set to
  null. For a document store that distinction is semantically load-bearing, and
  it is destroyed at the contract boundary.

### F-2 (BROKE) — `ColumnDesc` declares one type per column; Mongo has one type per *value*

Observed live, in a single page:

- `qty` held **3** BSON types — `string`, `int`, `long`
- `price` held **2** — `double`, `decimal`

`ColumnDesc` has exactly one `type_name`/`type_class`. The driver reported
`qty:string` (taken from the first document in sort order), so the grid is told
`qty` is text while two of three rows hold integers. This propagates: T-013
drives the offered filter operator set off `type_class`, so **the operator list
is wrong for those rows too** — a user filtering `qty > 10` gets text operators.

### F-3 (BROKE) — `WireValue` is a scalar algebra; BSON is not

`WireValue` has no variant for embedded documents or arrays. Both fall through
to `Unsupported{rendered}`, which means nested data reaches the UI as opaque
text and **can only be replaced wholesale, never edited field-wise**. Confirmed
against `meta:{k,n}`, `nested:{deep:{x}}`, and `tags:["a","b"]`.

`ObjectId` (STRAINED, not broken) has no variant either, but mapping it to
`Unsupported{type_name:"objectId"}` preserved enough type information to
reconstruct it for the edit key filter. Mapping it to `Text` — the obvious
choice — would have silently destroyed that.

### F-4 (BROKE) — optimistic concurrency has no per-value comparability

T-058 puts every comparable original in the update predicate. `tags` (an array)
had to be **dropped** from the predicate: BSON equality on a document is
key-order-sensitive, so comparing it produces false conflicts on a semantically
identical value. The contract (and R-035) models comparability **per column**;
here it is a property of the **value**, because a column's type varies row to
row (F-2). The concurrency guarantee silently weakens for those rows and nothing
in the contract can express that it did.

### F-5 (BROKE) — `raw_command(String)` and the outcome *sequence*

The contract hands the driver a script string because the editor is a SQL
editor (D-029). Mongo's raw surface is a single BSON command **document**, so
the string is parsed as JSON first. There is no such thing as a multi-statement
Mongo script, therefore:

- `supports_script: false`
- T-011's dialect-aware script splitting has no meaning on this engine
- **The outcome sequence (T-010) is permanently length 1.** The envelope's
  central affordance — a sequence of per-statement outcomes — is unused here.

## What held

- **F-6 (HELD) — the navigator tree.** Two driver-declared node kinds
  (`database` → `collection`) expressed the entire tree with no schema level and
  no placeholder node. T-008's driver-declared-node-kinds design absorbed the
  paradigm difference cleanly. This is the single strongest validation in the run.
- **F-7 (HELD) — the filter tree.** AND/OR + predicates compiled to `$and`/`$or`
  without stretching.
- **F-8 (HELD) — keyset pagination.** `_id` is a natural, always-indexed keyset.
- **Sessions** — see `SESSION-VERDICT.md` (T-046). All four session properties held.

## Strains worth recording

- **F-9 (STRAINED) — `contains` is three different operators.** It compiles to
  `$regex`, which is case-**sensitive** by default *and* interprets regex
  metacharacters in user input; Postgres `ILIKE` is case-insensitive and treats
  the needle literally. Compiled here with `$options:'i'` and full metacharacter
  quoting to match ILIKE. This is **R-009 reproduced live** and confirms that
  T-015's declared case-sensitivity is necessary, not theoretical.
- **F-10 (STRAINED) — dotted set paths.** `$set` on `meta.k` writes the nested
  field, but a Mongo field name may legally *contain* a dot, and the contract's
  flat `(String, WireValue)` pairs cannot distinguish the two. **R-026 reproduced
  against a live server**, confirming T-062 is needed.

## Recommended contract changes for T-005

Ordered by how expensive they get if deferred:

1. **Make `columns` per-page, not per-relation**, and add an explicit
   `Missing` distinct from `Null` in `WireValue`. (F-1)
2. **Move type identity from the column to the value.** Either `ColumnDesc.type_name`
   becomes `Vec<String>` / a `Mixed` class, or each `WireValue` carries its own
   type tag. The second is more honest and is what F-2 and F-4 both point at.
3. **Add `Document(Vec<(String, WireValue)>)` and `Array(Vec<WireValue>)` to
   `WireValue`.** Without them, D-018's schemaless work (T-063/T-064) has no
   value type to build on. (F-3)
4. **Add an `ObjectId`-shaped opaque variant** carrying `type_name` + canonical
   text, rather than leaning on `Unsupported`. (F-3)
5. **Let the edit path express per-value incomparability**, so a dropped
   concurrency predicate is declared rather than silent. (F-4)
6. **Type the raw passthrough per engine** — `raw_command(String)` should be a
   driver-declared payload shape, not a script string. (F-5)

Items 1–3 touch `T-016`/`T-010`, which Phase 1 builds. **They are cheap now and
expensive after the Postgres driver ships against the scalar-only shape.**
