//! Sluice core contract — the engine-agnostic `Driver` trait and the capability
//! model every engine difference is expressed through (D-003).
//!
//! This crate links **no** database libraries. Drivers implement the trait in
//! their own crates and link their engine there (D-001).
//!
//! ## Provenance
//!
//! The shape here is not a first draft. It is the first draft plus the repairs
//! forced by running a real driver against a real server before writing it:
//!
//! - **T-073** (real mongod) broke the scalar-only value algebra in four places:
//!   absent-vs-null, per-value type identity, structural values, and per-value
//!   comparability. See `spikes/mongo-contract/FINDINGS.md`.
//! - **T-046** established `SessionAffinity::Session` for Mongo on evidence, and
//!   surfaced the 60-second idle-transaction ceiling that `TransactionSupport`
//!   now declares.
//! - **T-072** established SQL Server's `ResetSupport::ReopenOnly` and
//!   `CancelSupport::KillSession`, and the `MONEY` precision loss that makes
//!   `WireValue::Numeric` load-bearing.

pub mod capability;
pub mod danger;
pub mod driver;
pub mod error;
pub mod outcome;
pub mod panic_guard;
pub mod request;
pub mod script;
pub mod redact;
pub mod secret;
pub mod statement_redact;
pub mod value;

pub use capability::{missing_core_ops, CORE_FILTER_OPS, 
    CancelSupport, Capabilities, FilterOpDecl, NodeKind, RawPayloadKind, ResetSupport,
    ReadOnlyGuarantee, SessionAffinity, TransactionSupport,
};
pub use driver::Driver;
pub use error::{ConfirmationRequest, DriverError, EnginePayload, ErrorHints, ErrorKind, QueryPosition};
pub use outcome::{row_cost, BudgetLimit, BudgetMeter, BudgetReport, ExecBudget, PageInfo, 
    Attribution, ExecutionResult, Outcome, OutcomeBody, TxContext, TxDisposition, TxMode,
};
pub use script::{ScriptDialect, SplitResult, StatementSpan};
pub use panic_guard::{
    guard_driver_call, guard_driver_call_blocking, ConnectionHome, ConnectionLease, DiscardReason,
    NonPoisonMutex, SessionId,
};
pub use request::{
    BoundParam, BrowseRequest, EditGate, EditPlan, EditRequest, PlannedStatement, FilterTree, NavNode, NodeId, Original, Predicate, RawRequest,
    SortKey,
};
pub use redact::{redact_shapes, Redactor};
pub use danger::{dangerous_statements, Dangerous};
pub use statement_redact::{redact_statement, StoredStatement};
pub use secret::{deserialize_optional_secret, Secret, REDACTED};
pub use value::{ColumnDesc, ExactDomain, TypeClass, WireValue};

#[cfg(test)]
mod contract_tests {
    use super::*;
    use serde::{de::DeserializeOwned, Serialize};

    /// The load-bearing assertion of this crate.
    ///
    /// `'static` is what actually proves "no lifetimes in the signature" — a
    /// type holding a borrow cannot satisfy it. `Serialize + DeserializeOwned`
    /// proves the type is plain data that could travel over a wire, which is
    /// D-003's stated reason for the constraint. `Send + Sync` keeps it usable
    /// across the async boundary.
    fn assert_wire_shaped<T: 'static + Send + Sync + Serialize + DeserializeOwned>() {}

    #[test]
    fn every_contract_type_is_owned_and_wire_shaped() {
        assert_wire_shaped::<Capabilities>();
        assert_wire_shaped::<SessionAffinity>();
        assert_wire_shaped::<ResetSupport>();
        assert_wire_shaped::<CancelSupport>();
        assert_wire_shaped::<TransactionSupport>();
        assert_wire_shaped::<RawPayloadKind>();
        assert_wire_shaped::<NodeKind>();
        assert_wire_shaped::<FilterOpDecl>();

        assert_wire_shaped::<NodeId>();
        assert_wire_shaped::<ExactDomain>();
        assert_wire_shaped::<NavNode>();
        assert_wire_shaped::<BrowseRequest>();
        assert_wire_shaped::<FilterTree>();
        assert_wire_shaped::<Predicate>();
        assert_wire_shaped::<SortKey>();
        assert_wire_shaped::<EditRequest>();
        assert_wire_shaped::<Original>();
        assert_wire_shaped::<RawRequest>();

        assert_wire_shaped::<ExecutionResult>();
        assert_wire_shaped::<Outcome>();
        assert_wire_shaped::<TxContext>();
        assert_wire_shaped::<DriverError>();
        assert_wire_shaped::<ErrorKind>();

        assert_wire_shaped::<WireValue>();
        assert_wire_shaped::<ColumnDesc>();
        assert_wire_shaped::<TypeClass>();
    }

    /// D-003 wants the same shape liftable onto a wire protocol, which means the
    /// trait must stay usable behind a pointer.
    #[test]
    fn driver_trait_is_object_safe() {
        fn _accepts_dyn(_d: &dyn Driver) {}
        fn _boxed(_d: Box<dyn Driver>) {}
    }

    /// The capability descriptor must survive a JSON round trip, since that is
    /// literally how it reaches the webview.
    #[test]
    fn capabilities_round_trip_through_json() {
        let caps = Capabilities {
            engine: "postgres".into(),
            server_version: Some("17.2".into()),
            flavour: None,
            affinity: SessionAffinity::Connection,
            reset: ResetSupport::Native,
            cancel: CancelSupport::Native,
            transactions: Some(TransactionSupport {
                interactive: true,
                max_idle_seconds: None,
            }),
            raw_payload: RawPayloadKind::Script,
            node_kinds: vec![NodeKind {
                id: "table".into(),
                label: "Table".into(),
                has_children: false,
                is_relation: true,
            }],
            filter_ops: vec![FilterOpDecl {
                id: "contains".into(),
                label: "contains".into(),
                type_classes: vec![TypeClass::Text],
                case_sensitive: Some(false),
                case_insensitive_toggle: true,
            }],
            fixed_schema: true,
            supports_script: true,
            string_collation: Some("en_US.utf8".into()),
            read_only: ReadOnlyGuarantee::Enforced { basis: "standby".into() },
        };

        let json = serde_json::to_string(&caps).expect("serialize");
        let back: Capabilities = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.engine, "postgres");
        assert_eq!(back.affinity, SessionAffinity::Connection);
        assert_eq!(back.reset, ResetSupport::Native);
    }

    /// T-045 / R-039: affinity is never defaulted. A descriptor that omits it
    /// must fail to load rather than quietly becoming `none` — that default is
    /// the exact foreclosure R-039 was raised against.
    #[test]
    fn affinity_has_no_default() {
        let mut v: serde_json::Value = serde_json::json!({
            "engine": "x", "server_version": null, "flavour": null,
            "affinity": "session", "reset": "not_applicable", "cancel": "none",
            "transactions": null, "raw_payload": "command_document",
            "node_kinds": [], "filter_ops": [], "fixed_schema": false,
            "supports_script": false
        });
        let ok: Capabilities = serde_json::from_value(v.clone()).expect("complete descriptor loads");
        assert_eq!(ok.affinity, SessionAffinity::Session);

        v.as_object_mut().unwrap().remove("affinity");
        let err = serde_json::from_value::<Capabilities>(v).unwrap_err().to_string();
        assert!(err.contains("affinity"), "{err}");
    }

    /// The three wire spellings the webview's `editorSessionPolicy` switches on.
    #[test]
    fn affinity_wire_spellings() {
        for (a, s) in [
            (SessionAffinity::Connection, "\"connection\""),
            (SessionAffinity::Session, "\"session\""),
            (SessionAffinity::None, "\"none\""),
        ] {
            assert_eq!(serde_json::to_string(&a).unwrap(), s);
        }
    }

    /// T-073 F-1: absent and null are different values, and must stay different
    /// across the wire. Collapsing them is the exact bug the spike found.
    #[test]
    fn missing_is_distinct_from_null() {
        assert_ne!(WireValue::Missing, WireValue::Null);
        let m = serde_json::to_string(&WireValue::Missing).unwrap();
        let n = serde_json::to_string(&WireValue::Null).unwrap();
        assert_ne!(m, n);
        assert_eq!(serde_json::from_str::<WireValue>(&m).unwrap(), WireValue::Missing);
    }

    /// T-073 F-4: comparability is a property of the value, not the column.
    #[test]
    fn structural_values_are_not_concurrency_comparable() {
        assert!(WireValue::Int(1).is_comparable());
        assert!(WireValue::Text("a".into()).is_comparable());
        assert!(WireValue::Numeric("1.00".into()).is_comparable());

        assert!(!WireValue::Array(vec![WireValue::Int(1)]).is_comparable());
        assert!(!WireValue::Document(vec![("k".into(), WireValue::Int(1))]).is_comparable());
        assert!(!WireValue::Unsupported {
            type_name: "geography".into(),
            rendered: "POINT(0 0)".into()
        }
        .is_comparable());
    }

    /// T-067: every wire value reports a class, and the per-value classes
    /// cover every declared class except the column-only `Unknown` fallback.
    #[test]
    fn every_wire_value_maps_to_a_type_class() {
        let cases = [
            (WireValue::Null, TypeClass::Unknown),
            (WireValue::Missing, TypeClass::Unknown),
            (WireValue::Bool(true), TypeClass::Boolean),
            (WireValue::Int(1), TypeClass::Integer),
            (WireValue::Float(1.0), TypeClass::Float),
            (WireValue::Numeric("1".into()), TypeClass::ExactNumeric),
            (WireValue::Text("a".into()), TypeClass::Text),
            (WireValue::Bytes("".into()), TypeClass::Binary),
            (WireValue::Timestamp("2026-01-01".into()), TypeClass::Timestamp),
            (WireValue::Uuid("0".into()), TypeClass::Uuid),
            (WireValue::Document(vec![]), TypeClass::Structural),
            (WireValue::Array(vec![]), TypeClass::Structural),
            (WireValue::Opaque { type_name: "inet".into(), rendered: "x".into() }, TypeClass::Opaque),
            (WireValue::Oversized { type_name: "bytea".into(), byte_len: 1, handle: None }, TypeClass::Oversized),
            (WireValue::Unsupported { type_name: "x".into(), rendered: "x".into() }, TypeClass::Unknown),
        ];
        for (v, want) in &cases {
            assert_eq!(v.type_class(), *want, "{v:?}");
        }
    }

    /// T-068: Mongo `Decimal128` extremes — 34 significant digits and an
    /// exponent range far beyond f64 — survive the wire encoding as text. The
    /// engine half of this belongs to a Mongo corpus once that driver exists;
    /// the contract half is asserted here.
    #[test]
    fn decimal128_extremes_survive_the_wire_encoding() {
        for s in [
            "9.999999999999999999999999999999999E+6144",
            "-9.999999999999999999999999999999999E+6144",
            "1E-6176",
            "-0E-6176",
            "1234567890123456789012345678901234",
            "NaN",
            "-Infinity",
        ] {
            let v = WireValue::Numeric(s.into());
            let json = serde_json::to_string(&v).unwrap();
            assert!(json.contains(&format!("\"{s}\"")), "must cross as a JSON string: {json}");
            assert_eq!(serde_json::from_str::<WireValue>(&json).unwrap(), v);
        }
    }

    /// T-068: with `float_roundtrip`, every f64 comes back from JSON as the
    /// same bits — the default parser is up to one ULP off.
    #[test]
    fn floats_round_trip_through_json_bit_exactly() {
        for f in [f32::MAX as f64, f64::MAX, 5e-324, 0.1, -0.0, 1.0 / 3.0, 2.2250738585072014e-308] {
            let json = serde_json::to_string(&WireValue::Float(f)).unwrap();
            match serde_json::from_str::<WireValue>(&json).unwrap() {
                WireValue::Float(g) => assert_eq!(f.to_bits(), g.to_bits(), "{f:e} came back as {g:e}"),
                other => panic!("{other:?}"),
            }
        }
    }

    /// T-072: exact numerics travel as text, so a MONEY value that tiberius
    /// would have pushed through f64 survives intact once the driver converts it.
    #[test]
    fn exact_numeric_survives_round_trip() {
        let v = WireValue::Numeric("922337203685477.5807".into());
        let json = serde_json::to_string(&v).unwrap();
        let back: WireValue = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v);
        match back {
            WireValue::Numeric(s) => assert_eq!(s, "922337203685477.5807"),
            other => panic!("expected Numeric, got {:?}", other),
        }
    }
}
