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

/// Driver state exposed to UI and diagnostics without making app startup
/// depend on the optional package.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VirtualAudioDriverHealth {
    NotInstalled,
    Incomplete {
        present_roles: Vec<QpwVirtualEndpointRole>,
    },
    Ready {
        version: Option<String>,
    },
}

impl VirtualAudioDriverHealth {
    pub fn from_endpoint_names<'a, I>(names: I, version: Option<String>) -> Self
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut roles: Vec<_> = names
            .into_iter()
            .filter_map(classify_virtual_endpoint)
            .collect();
        roles.sort_unstable();
        roles.dedup();
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
}
