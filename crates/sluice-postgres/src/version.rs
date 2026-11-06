//! Server version and flavour from `version()` (T-054 / D-016).
//!
//! D-016: "Flavour detection is the step usually skipped: a Postgres driver will
//! be pointed at CockroachDB, Redshift, Timescale, Yugabyte, or a
//! transaction-mode PgBouncer, none of which are catalog-compatible, and the
//! alternative is a client that half-works and blames the user." So connect
//! parses the banner into a flavour and the Postgres version it is compatible
//! with, and the driver gates catalog queries and features on both.
//!
//! What a banner can and cannot tell: Timescale and PgBouncer do not change
//! `version()` (an extension and a proxy respectively); T-055's feature probes
//! cover what the banner cannot. An unrecognised banner is kept as
//! [`Flavour::Unknown`] — never guessed to be vanilla.

/// Which engine is actually answering the Postgres protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flavour {
    Postgres,
    CockroachDb,
    Redshift,
    Yugabyte,
    Greenplum,
    /// The banner did not match any known shape; kept verbatim (first line).
    Unknown(String),
}

impl Flavour {
    /// The name the capability descriptor carries.
    pub fn name(&self) -> String {
        match self {
            Flavour::Postgres => "postgres".into(),
            Flavour::CockroachDb => "cockroachdb".into(),
            Flavour::Redshift => "redshift".into(),
            Flavour::Yugabyte => "yugabytedb".into(),
            Flavour::Greenplum => "greenplum".into(),
            // T-055 / R-028: treated as vanilla, and said to be unverified.
            Flavour::Unknown(_) => "vanilla, unverified".into(),
        }
    }
}

/// A `major.minor[.patch]` version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.patch > 0 {
            write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
        } else {
            write!(f, "{}.{}", self.major, self.minor)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub flavour: Flavour,
    /// The product's own version (CockroachDB 23.1.11, Redshift 1.0.77467).
    pub product_version: Option<Version>,
    /// The PostgreSQL version whose catalogs it presents, where it says.
    pub pg_compat: Option<Version>,
}

/// Parse a `version()` banner.
pub fn parse(banner: &str) -> ServerInfo {
    let b = banner.trim();
    if let Some(rest) = b.strip_prefix("CockroachDB ") {
        // "CockroachDB CCL v23.1.11 (x86_64-pc-linux-gnu, …)"
        let v = rest.split_whitespace().find_map(|w| w.strip_prefix('v')).and_then(parse_version);
        // Cockroach presents a Postgres 13-like catalog surface.
        return ServerInfo { flavour: Flavour::CockroachDb, product_version: v, pg_compat: Some(Version { major: 13, minor: 0, patch: 0 }) };
    }
    let Some(rest) = b.strip_prefix("PostgreSQL ") else {
        let first = b.lines().next().unwrap_or("").to_string();
        return ServerInfo { flavour: Flavour::Unknown(first), product_version: None, pg_compat: None };
    };
    let token = rest.split_whitespace().next().unwrap_or("");
    let pg = parse_version(token);

    if let Some(i) = b.find("Redshift ") {
        let v = b[i + "Redshift ".len()..].split_whitespace().next().and_then(parse_version);
        return ServerInfo { flavour: Flavour::Redshift, product_version: v, pg_compat: pg };
    }
    if let Some((_, yb)) = token.split_once("-YB-") {
        // "PostgreSQL 11.2-YB-2.18.0.0-b0 on …"
        return ServerInfo { flavour: Flavour::Yugabyte, product_version: parse_version(yb), pg_compat: pg };
    }
    if let Some(i) = b.find("Greenplum Database ") {
        let v = b[i + "Greenplum Database ".len()..].split_whitespace().next().and_then(parse_version);
        return ServerInfo { flavour: Flavour::Greenplum, product_version: v, pg_compat: pg };
    }
    ServerInfo { flavour: Flavour::Postgres, product_version: pg, pg_compat: pg }
}

/// The leading `N[.N[.N]]` of a token; trailing text (`beta1`, `-YB-…`,
/// `devel`) is ignored.
fn parse_version(s: &str) -> Option<Version> {
    let mut parts = [0u32; 3];
    let mut count = 0;
    for piece in s.split('.').take(3) {
        let digits: String = piece.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            break;
        }
        parts[count] = digits.parse().ok()?;
        count += 1;
        if digits.len() != piece.len() {
            break;
        }
    }
    (count > 0).then(|| Version { major: parts[0], minor: parts[1], patch: parts[2] })
}

impl ServerInfo {
    /// The version shown to the user: the product's own.
    pub fn display_version(&self) -> Option<String> {
        self.product_version.map(|v| v.to_string())
    }

    fn compat_at_least(&self, major: u32) -> bool {
        self.pg_compat.is_some_and(|v| v.major >= major)
    }

    /// Gate: `pg_collation.collisdeterministic` exists from PostgreSQL 12, and
    /// only flavours that present the vanilla catalog are trusted to have it.
    pub fn has_collation_determinism(&self) -> bool {
        matches!(self.flavour, Flavour::Postgres | Flavour::Greenplum) && self.compat_at_least(12)
    }

    /// Gate: `DISCARD ALL` (D-028's native reset). Redshift does not support
    /// it, so a connection there is reopened rather than assumed clean.
    pub fn has_discard_all(&self) -> bool {
        !matches!(self.flavour, Flavour::Redshift)
    }

    /// Gate: flavour-internal schemas hidden from the navigator, beside the
    /// `pg_*` / `information_schema` filter every flavour gets.
    pub fn hidden_schemas(&self) -> &'static [&'static str] {
        match self.flavour {
            Flavour::CockroachDb => &["crdb_internal"],
            Flavour::Greenplum => &["gp_toolkit"],
            Flavour::Redshift => &["pg_automv"],
            _ => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u32, minor: u32, patch: u32) -> Option<Version> {
        Some(Version { major, minor, patch })
    }

    /// Real banners, verbatim shapes.
    #[test]
    fn real_banners_parse_into_flavour_and_version() {
        let cases = [
            (
                "PostgreSQL 17.2 (Debian 17.2-1.pgdg120+1) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 12.2.0-14) 12.2.0, 64-bit",
                Flavour::Postgres, v(17, 2, 0), v(17, 2, 0),
            ),
            ("PostgreSQL 9.6.24 on x86_64-pc-linux-gnu", Flavour::Postgres, v(9, 6, 24), v(9, 6, 24)),
            ("PostgreSQL 18beta1 on x86_64-apple-darwin", Flavour::Postgres, v(18, 0, 0), v(18, 0, 0)),
            (
                "CockroachDB CCL v23.1.11 (x86_64-pc-linux-gnu, built 2023/09/27 01:53:43, go1.19.10)",
                Flavour::CockroachDb, v(23, 1, 11), v(13, 0, 0),
            ),
            (
                "PostgreSQL 8.0.2 on i686-pc-linux-gnu, compiled by GCC gcc (GCC) 3.4.2 20041017 (Red Hat 3.4.2-6.fc3), Redshift 1.0.77467",
                Flavour::Redshift, v(1, 0, 77467), v(8, 0, 2),
            ),
            (
                "PostgreSQL 11.2-YB-2.18.0.0-b0 on x86_64-pc-linux-gnu, compiled by clang version 15.0.3",
                Flavour::Yugabyte, v(2, 18, 0), v(11, 2, 0),
            ),
            (
                "PostgreSQL 12.12 (Greenplum Database 7.0.0 build commit:abc) on x86_64-pc-linux-gnu",
                Flavour::Greenplum, v(7, 0, 0), v(12, 12, 0),
            ),
        ];
        for (banner, flavour, product, compat) in cases {
            let info = parse(banner);
            assert_eq!(info.flavour, flavour, "{banner}");
            assert_eq!(info.product_version, product, "{banner}");
            assert_eq!(info.pg_compat, compat, "{banner}");
        }
    }

    #[test]
    fn an_unrecognised_banner_is_unknown_never_guessed_vanilla() {
        let info = parse("SomeNewDB 3.1 speaking pgwire\nsecond line");
        assert_eq!(info.flavour, Flavour::Unknown("SomeNewDB 3.1 speaking pgwire".into()));
        // T-055: recorded as unknown, *displayed* as vanilla and unverified.
        assert_eq!(info.flavour.name(), "vanilla, unverified");
        assert!(!info.has_collation_determinism());
    }

    #[test]
    fn features_gate_on_flavour_and_version() {
        let pg17 = parse("PostgreSQL 17.2 on x");
        let pg11 = parse("PostgreSQL 11.22 on x");
        let crdb = parse("CockroachDB CCL v23.1.11 (x)");
        let redshift = parse("PostgreSQL 8.0.2 on i686, Redshift 1.0.77467");
        let yb = parse("PostgreSQL 11.2-YB-2.18.0.0-b0 on x");

        assert!(pg17.has_collation_determinism());
        assert!(!pg11.has_collation_determinism(), "collisdeterministic arrived in 12");
        assert!(!crdb.has_collation_determinism());
        assert!(!yb.has_collation_determinism());

        assert!(pg17.has_discard_all() && crdb.has_discard_all() && yb.has_discard_all());
        assert!(!redshift.has_discard_all());

        assert_eq!(crdb.hidden_schemas(), &["crdb_internal"]);
        assert!(pg17.hidden_schemas().is_empty());
    }

    #[test]
    fn display_versions_are_the_products_own() {
        assert_eq!(parse("PostgreSQL 17.2 (Debian) on x").display_version().as_deref(), Some("17.2"));
        assert_eq!(parse("CockroachDB CCL v23.1.11 (x)").display_version().as_deref(), Some("23.1.11"));
    }
}
