//! TLS mode for a connection (T-021 / D-007).
//!
//! Engine-neutral: every driver that speaks TLS honours the same three modes,
//! and the UI renders its warning off this value. Moved here from the Postgres
//! crate (T-078) so profiles and the app core name no driver crate to carry it;
//! each driver maps it onto its own TLS stack.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    /// The default. Certificate chain and hostname are both verified.
    #[default]
    Verify,
    /// D-007's explicit opt-out. Encrypted, but the peer is unauthenticated —
    /// so it is trivially MITM-able and the UI MUST show a visible warning
    /// wherever a connection using this mode is displayed.
    AcceptInvalidCerts,
    /// No TLS at all. Only sane for a loopback container.
    Disabled,
}

impl TlsMode {
    /// Whether the UI must render a security warning for a connection using
    /// this mode. Derived here so every surface agrees.
    pub fn warns(&self) -> bool {
        !matches!(self, TlsMode::Verify)
    }

    pub fn warning(&self) -> Option<&'static str> {
        match self {
            TlsMode::Verify => None,
            TlsMode::AcceptInvalidCerts => Some(
                "Certificate verification is disabled for this profile. The connection is \
                 encrypted but the server is not authenticated, so it can be intercepted.",
            ),
            TlsMode::Disabled => {
                Some("TLS is disabled for this profile. Traffic, including credentials, is unencrypted.")
            }
        }
    }
}
