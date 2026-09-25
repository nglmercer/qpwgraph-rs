//! TOML configuration compatible with the state surface described by qpwgraph.

use pw_graph_effects::EffectInstanceConfig;
use serde::de::Error as DeserializeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Windows-only behavior is persisted on every platform so a configuration
/// remains portable, but non-Windows backends simply ignore this table.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct WindowsConfig {
    pub enable_process_loopback: bool,
    /// The AudioPolicyConfig ABI is undocumented and therefore opt-in.
    pub experimental_app_routing: bool,
    pub prefer_virtual_app_routes: bool,
    pub virtual_audio: WindowsVirtualAudioConfig,
    pub relay: WindowsRelayConfig,
}
impl Default for WindowsConfig {
    fn default() -> Self {
        Self {
            enable_process_loopback: true,
            experimental_app_routing: false,
            prefer_virtual_app_routes: true,
            virtual_audio: WindowsVirtualAudioConfig::default(),
            relay: WindowsRelayConfig::default(),
        }
    }
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct WindowsVirtualAudioConfig {
    pub enabled: bool,
}
impl Default for WindowsVirtualAudioConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct WindowsRelayConfig {
    pub receive_target: WindowsRelayReceiveTarget,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WindowsRelayReceiveTarget {
    #[default]
    Direct,
    VirtualMicrophone,
}
impl WindowsRelayReceiveTarget {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::VirtualMicrophone => "virtual-microphone",
        }
    }
}
impl Serialize for WindowsRelayReceiveTarget {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}
impl<'de> Deserialize<'de> for WindowsRelayReceiveTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            "direct" => Ok(Self::Direct),
            "virtual-microphone" => Ok(Self::VirtualMicrophone),
            value => Err(D::Error::custom(format!(
                "invalid Windows relay receive target '{value}'"
            ))),
        }
    }
}
/// Stable application identity used by persisted Windows routes. A PID is
/// intentionally absent: Windows may reuse it for an unrelated process.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct WindowsApplicationSelector {
    pub executable_path_hash: Option<String>,
    pub executable_name: Option<String>,
    pub package_family_name: Option<String>,
    pub app_user_model_id: Option<String>,
    pub display_name: Option<String>,
}
impl WindowsApplicationSelector {
    /// A persisted selector is safe to resolve only when it contains at least
    /// one identity that survives a process restart. A display name alone is
    /// intentionally not enough because unrelated applications may reuse it.
    pub fn is_stable(&self) -> bool {
        self.executable_path_hash.is_some()
            || self.app_user_model_id.is_some()
            || (self.package_family_name.is_some() && self.executable_name.is_some())
    }

    pub fn stable_key(&self) -> Option<&str> {
        self.app_user_model_id
            .as_deref()
            .or(self.package_family_name.as_deref())
            .or(self.executable_path_hash.as_deref())
    }

    /// Runtime key used to share process-loopback leases. A package family
    /// can contain more than one executable, so package-family-only identity
    /// is not sufficient for an in-process capture registry. Persisted route
    /// matching still uses the complete selector fields above.
    pub fn runtime_key(&self) -> Option<String> {
        if let Some(aumid) = self.app_user_model_id.as_deref() {
            return Some(aumid.to_owned());
        }
        if let Some(package) = self.package_family_name.as_deref() {
            let executable = self
                .executable_name
                .as_deref()
                .or(self.executable_path_hash.as_deref())?;
            return Some(format!("package-family:{package}|executable:{executable}"));
        }
        self.executable_path_hash.clone()
    }

    /// Match this selector against a live candidate using the strongest
    /// identity available. AUMID and package identity deliberately outrank
    /// the executable path: package updates are allowed to move the binary
    /// while preserving the application's durable identity. `display_name`
    /// is retained as a UI hint and never gates activation.
    pub fn matches(&self, candidate: &Self) -> bool {
        if !self.is_stable() {
            return false;
        }
        if let Some(expected) = self.app_user_model_id.as_deref() {
            return candidate
                .app_user_model_id
                .as_deref()
                .is_some_and(|actual| expected.eq_ignore_ascii_case(actual));
        }
        if let (Some(expected_package), Some(expected_name)) = (
            self.package_family_name.as_deref(),
            self.executable_name.as_deref(),
        ) {
            return candidate
                .package_family_name
                .as_deref()
                .is_some_and(|actual| expected_package.eq_ignore_ascii_case(actual))
                && candidate
                    .executable_name
                    .as_deref()
                    .is_some_and(|actual| expected_name.eq_ignore_ascii_case(actual));
        }
        self.executable_path_hash
            .as_deref()
            .zip(candidate.executable_path_hash.as_deref())
            .is_some_and(|(expected, actual)| expected.eq_ignore_ascii_case(actual))
    }

    fn specificity(&self) -> usize {
        [
            &self.executable_path_hash,
            &self.executable_name,
            &self.package_family_name,
            &self.app_user_model_id,
            &self.display_name,
        ]
        .into_iter()
        .filter(|field| field.is_some())
        .count()
    }
}
/// Persisted qpwgraph-owned application route.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct WindowsApplicationRoute {
    pub application: WindowsApplicationSelector,
    /// Stable endpoint identity introduced after the original route schema.
    /// The older `destination_endpoint_id` and `destination_name` fields stay
    /// readable so existing configurations can be upgraded lazily.
    pub destination_stable_id: Option<String>,
    pub destination_mmdevice_id: Option<String>,
    pub destination_endpoint_id: Option<String>,
    pub destination_name: Option<String>,
    pub virtualization_required: bool,
    /// Legacy effect identifiers retained for old configuration files. They
    /// are intentionally not enough to restore a route because they omit
    /// parameters, bypass state, enabled state, and instance identity.
    pub effect_chain: Vec<String>,
    /// Complete, ordered effect instances for a Windows application route.
    /// This is separate from `effect_chain` so old TOML remains readable and
    /// old callers do not accidentally restore an effect with defaults.
    #[serde(default)]
    pub effect_instances: Vec<EffectInstanceConfig>,
    pub gain: f32,
    pub enabled: bool,
}
impl WindowsApplicationRoute {
    /// Whether this persisted route is eligible for a live candidate. A
    /// disabled route or a selector with only a display name is never applied.
    pub fn matches_application(&self, candidate: &WindowsApplicationSelector) -> bool {
        self.enabled && self.application.matches(candidate)
    }

    /// More specific selectors win when a user has retained both a broad
    /// executable rule and a package/display-name override.  Ties preserve
    /// file order, making the result deterministic without inventing a
    /// hidden priority field in the persisted format.
    pub fn selector_specificity(&self) -> usize {
        self.application.specificity()
    }

    /// Return the effect configuration that can be restored transactionally.
    ///
    /// A legacy route may contain only effect IDs in `effect_chain`; treating
    /// those as a request to create default processors would silently change
    /// the user's audio. Such routes are therefore rejected until they have
    /// been upgraded with complete `effect_instances` data.
    pub fn restorable_effect_instances(&self) -> Result<Vec<EffectInstanceConfig>, String> {
        if self.effect_instances.is_empty() {
            return if self.effect_chain.is_empty() {
                Ok(Vec::new())
            } else {
                Err("saved route contains legacy effect IDs without instance configuration".into())
            };
        }

        if !self.effect_chain.is_empty()
            && (self.effect_chain.len() != self.effect_instances.len()
                || self
                    .effect_chain
                    .iter()
                    .zip(&self.effect_instances)
                    .any(|(id, instance)| id != &instance.effect_id))
        {
            return Err(
                "saved route effect ID list does not match its instance configuration".into(),
            );
        }
        Ok(self.effect_instances.clone())
    }
}
impl Default for WindowsApplicationRoute {
    fn default() -> Self {
        Self {
            application: WindowsApplicationSelector::default(),
            destination_stable_id: None,
            destination_mmdevice_id: None,
            destination_endpoint_id: None,
            destination_name: None,
            virtualization_required: true,
            effect_chain: Vec::new(),
            effect_instances: Vec::new(),
            gain: 1.0,
            enabled: true,
        }
    }
}
