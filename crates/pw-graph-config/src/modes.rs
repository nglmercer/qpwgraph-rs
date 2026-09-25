//! TOML configuration compatible with the state surface described by qpwgraph.

use serde::de::Error as DeserializeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The user-facing direction of a relay session.
///
/// Relay roles are deliberately not part of the persisted desktop state. The
/// bridge derives a receive-only host or an emit-only client from this value,
/// which keeps the direction visible without exposing the old three-way
/// `emit`/`receive`/`both` switch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AudioDirection {
    /// Audio flows from the phone into the desktop relay microphone.
    #[default]
    MobileToDesktop,
    /// Audio flows from the desktop relay speaker into the phone.
    DesktopToMobile,
}
/// Platform-neutral local relay role. The desktop uses this field for its
/// own role; Android has the same two-value model at its JNI boundary.
/// `AudioDirection` remains as a serde/API compatibility shim for configs
/// written by the direction-first release.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RelayMode {
    Emitter,
    /// A desktop installation historically defaulted to receiving phone
    /// audio, so preserve that default while the generic key is introduced.
    #[default]
    Receiver,
}
impl RelayMode {
    pub const EMITTER: &'static str = "emitter";
    pub const RECEIVER: &'static str = "receiver";

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Emitter => Self::EMITTER,
            Self::Receiver => Self::RECEIVER,
        }
    }

    /// Parse canonical values and the desktop's one-release legacy role
    /// values. Legacy `both` is intentionally collapsed to Receiver rather
    /// than being allowed to create a bidirectional session.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "emitter" | "emit" | "desktop_to_mobile" | "pc_to_mobile" => Some(Self::Emitter),
            "receiver" | "receive" | "both" | "mobile_to_desktop" | "mobile_to_pc" => {
                Some(Self::Receiver)
            }
            _ => None,
        }
    }

    pub const fn from_audio_direction(direction: AudioDirection) -> Self {
        match direction {
            AudioDirection::MobileToDesktop => Self::Receiver,
            AudioDirection::DesktopToMobile => Self::Emitter,
        }
    }

    pub const fn audio_direction(self) -> AudioDirection {
        match self {
            Self::Emitter => AudioDirection::DesktopToMobile,
            Self::Receiver => AudioDirection::MobileToDesktop,
        }
    }
}
impl Serialize for RelayMode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}
impl<'de> Deserialize<'de> for RelayMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).ok_or_else(|| {
            D::Error::custom(format!(
                "invalid relay mode '{value}'; expected emitter or receiver"
            ))
        })
    }
}
impl AudioDirection {
    pub const MOBILE_TO_DESKTOP: &'static str = "mobile_to_desktop";
    pub const DESKTOP_TO_MOBILE: &'static str = "desktop_to_mobile";

    /// Canonical value written to the configuration file.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MobileToDesktop => Self::MOBILE_TO_DESKTOP,
            Self::DesktopToMobile => Self::DESKTOP_TO_MOBILE,
        }
    }

    /// Parse both the current direction values and the desktop's legacy role
    /// values. Legacy `emit` means the desktop was the client, so it maps to
    /// PC → Mobile; `receive` and `both` map deterministically to Mobile → PC.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "mobile_to_desktop" | "mobile_to_pc" | "receive" | "both" => {
                Some(Self::MobileToDesktop)
            }
            "desktop_to_mobile" | "pc_to_mobile" | "emit" => Some(Self::DesktopToMobile),
            _ => None,
        }
    }
}
impl Serialize for AudioDirection {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}
impl<'de> Deserialize<'de> for AudioDirection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).ok_or_else(|| {
            D::Error::custom(format!(
                "invalid relay direction '{value}'; expected mobile_to_desktop or desktop_to_mobile"
            ))
        })
    }
}
