use serde::{Deserialize, Serialize};

/// Normalized presence value across Windows / macOS / Linux Teams logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Presence {
    Available,
    Busy,
    Away,
    BeRightBack,
    DoNotDisturb,
    Offline,
    Unknown,
}

impl Presence {
    /// True if this presence should turn the bulb on (call mode).
    pub fn is_in_call(self) -> bool {
        matches!(self, Presence::Busy | Presence::DoNotDisturb)
    }
}

/// What presence state(s) should activate the bulb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TriggerMode {
    /// Only fire on explicit TeamsCallTracker call events (Windows).
    /// On macOS there is no call-tracker signal, so this behaves the same as
    /// `BusyAndDnd` (Busy is the only call indicator available in the log).
    CallOnly,
    /// Activate on Busy or DoNotDisturb — the default.
    #[default]
    BusyAndDnd,
    /// Activate on anything except Available, Offline, or Unknown.
    /// Catches Away, BeRightBack, Busy, and DoNotDisturb.
    AnyNonAvailable,
}

/// RGB color (0-255 each).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parse "#RRGGBB" or "RRGGBB" hex string.
    pub fn from_hex(hex: &str) -> Option<Self> {
        let s = hex.trim_start_matches('#');
        if s.len() != 6 {
            return None;
        }
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some(Self { r, g, b })
    }

    pub fn to_hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }
}
