use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{IdentityConfigError, authority::https_url};

/// Validated host policy. Browser requests cannot select any of these values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "Document", into = "Document")]
pub struct OidcLoginOptions {
    pub(crate) transaction_lifetime_seconds: u64,
    pub(crate) session_lifetime_seconds: u64,
    pub(crate) clock_skew_seconds: u64,
    pub(crate) max_authentication_age_seconds: Option<u64>,
    pub(crate) additional_endpoint_origins: BTreeSet<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct Document {
    transaction_lifetime_seconds: u64,
    session_lifetime_seconds: u64,
    clock_skew_seconds: u64,
    max_authentication_age_seconds: Option<u64>,
    additional_endpoint_origins: BTreeSet<String>,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            transaction_lifetime_seconds: 300,
            session_lifetime_seconds: 3600,
            clock_skew_seconds: 30,
            max_authentication_age_seconds: None,
            additional_endpoint_origins: BTreeSet::new(),
        }
    }
}

impl Default for OidcLoginOptions {
    fn default() -> Self {
        Self::try_from(Document::default()).expect("valid login defaults")
    }
}

impl TryFrom<Document> for OidcLoginOptions {
    type Error = IdentityConfigError;

    fn try_from(value: Document) -> Result<Self, Self::Error> {
        if !(30..=600).contains(&value.transaction_lifetime_seconds)
            || !(60..=86400).contains(&value.session_lifetime_seconds)
            || value.clock_skew_seconds > 120
            || value
                .max_authentication_age_seconds
                .is_some_and(|age| !(1..=86400).contains(&age))
            || value.additional_endpoint_origins.len() > 8
        {
            return Err(IdentityConfigError(
                "invalid login lifetime or endpoint origin bounds",
            ));
        }
        let origins = value
            .additional_endpoint_origins
            .into_iter()
            .map(|origin| {
                let url = https_url(&origin)?;
                if url.path() != "/" || url.query().is_some() {
                    return Err(IdentityConfigError(
                        "login endpoint origins cannot contain paths or queries",
                    ));
                }
                Ok(url.origin().ascii_serialization())
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        Ok(Self {
            transaction_lifetime_seconds: value.transaction_lifetime_seconds,
            session_lifetime_seconds: value.session_lifetime_seconds,
            clock_skew_seconds: value.clock_skew_seconds,
            max_authentication_age_seconds: value.max_authentication_age_seconds,
            additional_endpoint_origins: origins,
        })
    }
}

impl From<OidcLoginOptions> for Document {
    fn from(value: OidcLoginOptions) -> Self {
        Self {
            transaction_lifetime_seconds: value.transaction_lifetime_seconds,
            session_lifetime_seconds: value.session_lifetime_seconds,
            clock_skew_seconds: value.clock_skew_seconds,
            max_authentication_age_seconds: value.max_authentication_age_seconds,
            additional_endpoint_origins: value.additional_endpoint_origins,
        }
    }
}
