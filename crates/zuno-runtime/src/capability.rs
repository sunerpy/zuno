use std::fmt;

/// Version of a runtime-named capability contract.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CapabilityVersion {
    major: u16,
    minor: u16,
}

impl CapabilityVersion {
    /// Create a capability contract version.
    #[must_use]
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    /// Contract major version.
    #[must_use]
    pub const fn major(self) -> u16 {
        self.major
    }

    /// Contract minor version.
    #[must_use]
    pub const fn minor(self) -> u16 {
        self.minor
    }
}

impl fmt::Display for CapabilityVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)
    }
}

/// Isolation label within a runtime scope.
///
/// Runtime parent/child inheritance remains owned by [`crate::HarnessRuntime`].
/// This label lets dynamic boundaries distinguish independently named planes
/// without weakening that hierarchy.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CapabilityScope(String);

impl CapabilityScope {
    /// Create a non-empty isolation label.
    pub fn new(value: impl Into<String>) -> Result<Self, CapabilityDefinitionError> {
        Ok(Self(non_empty("scope", value)?))
    }

    /// Borrow the stable label.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CapabilityScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Stable identity supplied by manifests, workflows, MCP servers, or packages.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CapabilityKey {
    namespace: String,
    name: String,
    version: CapabilityVersion,
    scope: CapabilityScope,
}

impl CapabilityKey {
    /// Build one validated capability identity.
    pub fn new(
        namespace: impl Into<String>,
        name: impl Into<String>,
        version: CapabilityVersion,
        scope: CapabilityScope,
    ) -> Result<Self, CapabilityDefinitionError> {
        Ok(Self {
            namespace: non_empty("namespace", namespace)?,
            name: non_empty("name", name)?,
            version,
            scope,
        })
    }

    /// Capability namespace.
    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Capability name inside the namespace.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Declared contract version.
    #[must_use]
    pub const fn version(&self) -> CapabilityVersion {
        self.version
    }

    /// Isolation label.
    #[must_use]
    pub const fn scope(&self) -> &CapabilityScope {
        &self.scope
    }
}

impl fmt::Display for CapabilityKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}/{}@{}[{}]",
            self.namespace, self.name, self.version, self.scope
        )
    }
}

/// Compatibility contract for a named capability.
///
/// `interface` names the typed facade or wire protocol. `schema_digest` pins an
/// optional canonical schema without storing an unchecked executable value in
/// the named registry.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CapabilityContract {
    interface: String,
    schema_digest: Option<String>,
}

impl CapabilityContract {
    /// Build a validated capability contract.
    pub fn new<D>(
        interface: impl Into<String>,
        schema_digest: Option<D>,
    ) -> Result<Self, CapabilityDefinitionError>
    where
        D: Into<String>,
    {
        Ok(Self {
            interface: non_empty("contract interface", interface)?,
            schema_digest: schema_digest
                .map(|digest| non_empty("schema digest", digest))
                .transpose()?,
        })
    }

    /// Interface or protocol identity.
    #[must_use]
    pub fn interface(&self) -> &str {
        &self.interface
    }

    /// Optional canonical schema digest.
    #[must_use]
    pub fn schema_digest(&self) -> Option<&str> {
        self.schema_digest.as_deref()
    }
}

/// Auditable origin of one capability declaration.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CapabilityProvenance {
    source: String,
    package: Option<String>,
}

impl CapabilityProvenance {
    /// Build a validated provenance record.
    pub fn new<P>(
        source: impl Into<String>,
        package: Option<P>,
    ) -> Result<Self, CapabilityDefinitionError>
    where
        P: Into<String>,
    {
        Ok(Self {
            source: non_empty("provenance source", source)?,
            package: package
                .map(|package| non_empty("provenance package", package))
                .transpose()?,
        })
    }

    /// Provider-defined source identity.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Optional package identity.
    #[must_use]
    pub fn package(&self) -> Option<&str> {
        self.package.as_deref()
    }
}

/// Whether a projected capability can still be resolved for new calls.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CapabilityAvailability {
    /// The descriptor is atomically published and routable.
    Available,
    /// Routing has been withdrawn before provider cleanup completes.
    Withdrawn,
}

/// One published named capability generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityDescriptor {
    key: CapabilityKey,
    owner: String,
    runtime_scope: String,
    generation: u64,
    contract: CapabilityContract,
    provenance: CapabilityProvenance,
    availability: CapabilityAvailability,
}

impl CapabilityDescriptor {
    pub(crate) fn available(
        key: CapabilityKey,
        owner: String,
        runtime_scope: String,
        generation: u64,
        contract: CapabilityContract,
        provenance: CapabilityProvenance,
    ) -> Self {
        Self {
            key,
            owner,
            runtime_scope,
            generation,
            contract,
            provenance,
            availability: CapabilityAvailability::Available,
        }
    }

    pub(crate) fn withdraw(&mut self) {
        self.availability = CapabilityAvailability::Withdrawn;
    }

    /// Stable capability key.
    #[must_use]
    pub const fn key(&self) -> &CapabilityKey {
        &self.key
    }

    /// Component that owns this generation.
    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// Runtime scope that published this generation.
    #[must_use]
    pub fn runtime_scope(&self) -> &str {
        &self.runtime_scope
    }

    /// Monotonic generation within the publishing runtime scope.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Compatibility contract.
    #[must_use]
    pub const fn contract(&self) -> &CapabilityContract {
        &self.contract
    }

    /// Auditable declaration origin.
    #[must_use]
    pub const fn provenance(&self) -> &CapabilityProvenance {
        &self.provenance
    }

    /// Current projected availability.
    #[must_use]
    pub const fn availability(&self) -> CapabilityAvailability {
        self.availability
    }
}

/// Invalid capability identity, contract, or provenance.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CapabilityDefinitionError {
    /// A stable identity field cannot be empty or whitespace-only.
    #[error("capability {0} must not be empty")]
    Empty(&'static str),
}

fn non_empty(
    field: &'static str,
    value: impl Into<String>,
) -> Result<String, CapabilityDefinitionError> {
    let value = value.into().trim().to_owned();
    if value.is_empty() {
        Err(CapabilityDefinitionError::Empty(field))
    } else {
        Ok(value)
    }
}
