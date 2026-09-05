//! Stable semantic classification for the optional qpwgraph audio driver.
//!
//! Configuration stores these roles, never Core Audio endpoint IDs alone.

/// The four endpoints exposed by the optional virtual-audio driver.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum QpwVirtualEndpointRole {
    AppRender,
    AppMonitor,
    RelayRender,
    RelayCapture,
}

impl QpwVirtualEndpointRole {
    pub const fn stable_name(self) -> &'static str {
        match self {
            Self::AppRender => "QPWGraph Virtual Output",
            Self::AppMonitor => "QPWGraph Virtual Monitor",
            Self::RelayRender => "QPWGraph Relay Sink",
            Self::RelayCapture => "QPWGraph Relay Microphone",
        }
    }

    pub const fn config_key(self) -> &'static str {
        match self {
            Self::AppRender => "app-render",
            Self::AppMonitor => "app-monitor",
            Self::RelayRender => "relay-render",
            Self::RelayCapture => "relay-capture",
        }
    }
}

/// Driver-owned identity for one virtual endpoint.  A friendly name is not
/// part of this proof: Windows users may rename it, and another driver may
/// expose the same display string.  The value is constructed only after the
/// caller has verified the provider/service identity exposed by the endpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QpwVirtualEndpointIdentity {
    pub role: QpwVirtualEndpointRole,
    pub stable_endpoint_id: Option<String>,
    pub mmdevice_id: String,
    pub driver_version: Option<String>,
}

/// Return a strong semantic identity only when the endpoint has already been
/// proved to be owned by qpwgraph's driver.  Keeping this check explicit makes
/// it impossible for friendly-name classification to silently become an
/// ownership claim.
pub fn classify_driver_owned_endpoint(
    role: QpwVirtualEndpointRole,
    stable_endpoint_id: Option<String>,
    mmdevice_id: String,
    driver_version: Option<String>,
    driver_owned: bool,
) -> Option<QpwVirtualEndpointIdentity> {
    (driver_owned && !mmdevice_id.trim().is_empty()).then_some(QpwVirtualEndpointIdentity {
        role,
        stable_endpoint_id,
        mmdevice_id,
        driver_version,
    })
}

/// Driver state exposed to UI and diagnostics without making app startup
/// depend on the optional package.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VirtualAudioDriverHealth {
    NotInstalled,
    Incomplete {
        present_roles: Vec<QpwVirtualEndpointRole>,
    },
    /// All semantic roles were found, but the endpoint provider reported
    /// more than one driver version.  Treat that as degraded instead of
    /// allowing a mixed upgrade to look ready.
    IncompatibleVersion {
        versions: Vec<String>,
    },
    /// More than one provider-owned endpoint advertised the same semantic
    /// role.  The relay and route selectors must not choose one arbitrarily.
    AmbiguousRoles {
        roles: Vec<QpwVirtualEndpointRole>,
    },
    Ready {
        version: Option<String>,
    },
}

impl VirtualAudioDriverHealth {
    /// Build health from endpoint identities whose ownership has already been
    /// proved by the native provider probe.  This is intentionally separate
    /// from [`Self::from_endpoint_names`]: a friendly name is display data and
    /// cannot make a third-party endpoint driver-owned.
    pub fn from_verified_identities<I>(identities: I) -> Self
    where
        I: IntoIterator<Item = QpwVirtualEndpointIdentity>,
    {
        let mut roles = Vec::new();
        let mut duplicate_roles = Vec::new();
        let mut versions = Vec::new();
        for identity in identities {
            if identity.mmdevice_id.trim().is_empty() {
                continue;
            }
            if roles.contains(&identity.role) {
                duplicate_roles.push(identity.role);
            }
            roles.push(identity.role);
            if let Some(version) = identity
                .driver_version
                .filter(|version| !version.trim().is_empty())
            {
                if !versions.iter().any(|known| known == &version) {
                    versions.push(version);
                }
            }
        }
        roles.sort_unstable();
        roles.dedup();
        duplicate_roles.sort_unstable();
        duplicate_roles.dedup();
        versions.sort_unstable();

        if !duplicate_roles.is_empty() {
            return Self::AmbiguousRoles {
                roles: duplicate_roles,
            };
        }
        if roles.len() == 4 && versions.len() > 1 {
            return Self::IncompatibleVersion { versions };
        }
        match roles.len() {
            0 => Self::NotInstalled,
            4 => Self::Ready {
                version: versions.into_iter().next(),
            },
            _ => Self::Incomplete {
                present_roles: roles,
            },
        }
    }

    pub fn from_endpoint_names<'a, I>(names: I, version: Option<String>) -> Self
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut roles = Vec::new();
        let mut duplicate_roles = Vec::new();
        for role in names.into_iter().filter_map(classify_virtual_endpoint) {
            if roles.contains(&role) {
                duplicate_roles.push(role);
            }
            roles.push(role);
        }
        roles.sort_unstable();
        roles.dedup();
        duplicate_roles.sort_unstable();
        duplicate_roles.dedup();
        if !duplicate_roles.is_empty() {
            return Self::AmbiguousRoles {
                roles: duplicate_roles,
            };
        }
        match roles.len() {
            0 => Self::NotInstalled,
            4 => Self::Ready { version },
            _ => Self::Incomplete {
                present_roles: roles,
            },
        }
    }
}

/// Classify an endpoint by its driver-owned friendly name.
///
/// Endpoint instance IDs are still retained by the caller for disambiguation,
/// but never persisted without this semantic role.
pub fn classify_virtual_endpoint(friendly_name: &str) -> Option<QpwVirtualEndpointRole> {
    let normalized = friendly_name.trim();
    [
        QpwVirtualEndpointRole::AppRender,
        QpwVirtualEndpointRole::AppMonitor,
        QpwVirtualEndpointRole::RelayRender,
        QpwVirtualEndpointRole::RelayCapture,
    ]
    .into_iter()
    .find(|role| normalized.eq_ignore_ascii_case(role.stable_name()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_classify_without_persisting_endpoint_ids() {
        assert_eq!(
            classify_virtual_endpoint("QPWGraph Relay Microphone"),
            Some(QpwVirtualEndpointRole::RelayCapture)
        );
        assert_eq!(classify_virtual_endpoint("ordinary speakers"), None);
    }

    #[test]
    fn health_requires_all_four_semantic_roles() {
        assert_eq!(
            VirtualAudioDriverHealth::from_endpoint_names(
                ["QPWGraph Virtual Output", "QPWGraph Relay Sink"],
                None
            ),
            VirtualAudioDriverHealth::Incomplete {
                present_roles: vec![
                    QpwVirtualEndpointRole::AppRender,
                    QpwVirtualEndpointRole::RelayRender,
                ]
            }
        );
        assert!(matches!(
            VirtualAudioDriverHealth::from_endpoint_names(
                [
                    "QPWGraph Virtual Output",
                    "QPWGraph Virtual Monitor",
                    "QPWGraph Relay Sink",
                    "QPWGraph Relay Microphone"
                ],
                Some("0.1".into())
            ),
            VirtualAudioDriverHealth::Ready { .. }
        ));
    }

    #[test]
    fn duplicate_friendly_roles_are_not_ready() {
        assert_eq!(
            VirtualAudioDriverHealth::from_endpoint_names(
                ["QPWGraph Virtual Output", "QPWGraph Virtual Output"],
                Some("0.1".into())
            ),
            VirtualAudioDriverHealth::AmbiguousRoles {
                roles: vec![QpwVirtualEndpointRole::AppRender]
            }
        );
    }

    #[test]
    fn friendly_name_never_proves_driver_ownership() {
        assert!(classify_driver_owned_endpoint(
            QpwVirtualEndpointRole::AppRender,
            Some("stable-id".into()),
            "mmdevice-id".into(),
            Some("0.1.0".into()),
            false,
        )
        .is_none());
        let identity = classify_driver_owned_endpoint(
            QpwVirtualEndpointRole::AppRender,
            Some("stable-id".into()),
            "mmdevice-id".into(),
            Some("0.1.0".into()),
            true,
        )
        .unwrap();
        assert_eq!(identity.role, QpwVirtualEndpointRole::AppRender);
        assert_eq!(identity.mmdevice_id, "mmdevice-id");
        assert!(classify_driver_owned_endpoint(
            QpwVirtualEndpointRole::AppRender,
            None,
            "  ".into(),
            None,
            true,
        )
        .is_none());
    }

    fn identity(role: QpwVirtualEndpointRole, version: Option<&str>) -> QpwVirtualEndpointIdentity {
        QpwVirtualEndpointIdentity {
            role,
            stable_endpoint_id: Some(format!("stable-{}", role.config_key())),
            mmdevice_id: format!("mmdevice-{}", role.config_key()),
            driver_version: version.map(str::to_owned),
        }
    }

    #[test]
    fn verified_identities_are_required_for_ready_health() {
        let identities = [
            identity(QpwVirtualEndpointRole::AppRender, Some("0.1.0")),
            identity(QpwVirtualEndpointRole::AppMonitor, Some("0.1.0")),
            identity(QpwVirtualEndpointRole::RelayRender, Some("0.1.0")),
            identity(QpwVirtualEndpointRole::RelayCapture, Some("0.1.0")),
        ];
        assert_eq!(
            VirtualAudioDriverHealth::from_verified_identities(identities),
            VirtualAudioDriverHealth::Ready {
                version: Some("0.1.0".into())
            }
        );
    }

    #[test]
    fn mixed_verified_versions_are_not_ready() {
        let identities = [
            identity(QpwVirtualEndpointRole::AppRender, Some("0.1.0")),
            identity(QpwVirtualEndpointRole::AppMonitor, Some("0.1.0")),
            identity(QpwVirtualEndpointRole::RelayRender, Some("0.2.0")),
            identity(QpwVirtualEndpointRole::RelayCapture, Some("0.2.0")),
        ];
        assert_eq!(
            VirtualAudioDriverHealth::from_verified_identities(identities),
            VirtualAudioDriverHealth::IncompatibleVersion {
                versions: vec!["0.1.0".into(), "0.2.0".into()]
            }
        );
    }

    #[test]
    fn duplicate_verified_roles_are_not_ready() {
        let identities = [
            identity(QpwVirtualEndpointRole::AppRender, Some("0.1.0")),
            identity(QpwVirtualEndpointRole::AppRender, Some("0.1.0")),
            identity(QpwVirtualEndpointRole::AppMonitor, Some("0.1.0")),
            identity(QpwVirtualEndpointRole::RelayRender, Some("0.1.0")),
            identity(QpwVirtualEndpointRole::RelayCapture, Some("0.1.0")),
        ];
        assert_eq!(
            VirtualAudioDriverHealth::from_verified_identities(identities),
            VirtualAudioDriverHealth::AmbiguousRoles {
                roles: vec![QpwVirtualEndpointRole::AppRender]
            }
        );
    }
}
