//! The code that sets Orb up on an iPhone: everything the phone needs to
//! reach the tasks folder, the password included, shown by the Mac as a QR
//! code and scanned by the phone (IOS_PLAN.md 2.1).
//!
//! The code is as sensitive as the password it carries. It is shown only
//! while asked for, and the phone refuses it once [`LIFETIME`] has passed —
//! which limits what a photo of the screen is worth, and is all a code
//! printed on a screen can do.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

/// How long a code is good for.
pub const LIFETIME_SECONDS: i64 = 5 * 60;

/// What every code starts with, so a phone can tell one from any other QR.
const PREFIX: &str = "orb-setup:";

/// The payload's format; a phone refuses a version it does not know.
const VERSION: u32 = 1;

/// What a code carries.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupPayload {
    pub version: u32,
    pub imap_server: String,
    pub imap_port: u16,
    pub imap_username: String,
    pub password: String,
    /// The tasks folder, verbatim (todo-spec §1).
    pub folder: String,
    /// The date picker's language (§12).
    pub locale: String,
    pub keep_completed_in_today: bool,
    pub completed_first_in_today: bool,
    pub archive_after_days: u32,
    /// Seconds since the epoch after which the code is refused.
    pub expires: i64,
}

/// Why a scanned code was not taken.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SetupCodeError {
    #[error("This is not an Orb setup code.")]
    NotACode,
    #[error("This setup code comes from a newer Orb. Update Orb on this iPhone.")]
    UnknownVersion,
    #[error("This setup code has expired. Show a new one on the Mac.")]
    Expired,
}

/// Written out without the password, so a payload in a log line or a panic
/// message gives away the account and not its key.
impl std::fmt::Debug for SetupPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetupPayload")
            .field("version", &self.version)
            .field("imap_server", &self.imap_server)
            .field("imap_port", &self.imap_port)
            .field("imap_username", &self.imap_username)
            .field("password", &"[redacted]")
            .field("folder", &self.folder)
            .field("expires", &self.expires)
            .finish_non_exhaustive()
    }
}

impl SetupPayload {
    /// The text the QR code holds.
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("a payload of plain fields serialises");
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(json))
    }

    /// Read a scanned code, refusing one that has expired at `now` (seconds
    /// since the epoch).
    pub fn decode(text: &str, now: i64) -> Result<Self, SetupCodeError> {
        let encoded = text
            .trim()
            .strip_prefix(PREFIX)
            .ok_or(SetupCodeError::NotACode)?;
        let json = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| SetupCodeError::NotACode)?;
        let version = serde_json::from_slice::<serde_json::Value>(&json)
            .ok()
            .and_then(|value| value.get("version")?.as_u64())
            .ok_or(SetupCodeError::NotACode)?;
        if version != u64::from(VERSION) {
            return Err(SetupCodeError::UnknownVersion);
        }
        let payload: SetupPayload =
            serde_json::from_slice(&json).map_err(|_| SetupCodeError::NotACode)?;
        if now >= payload.expires {
            return Err(SetupCodeError::Expired);
        }
        Ok(payload)
    }

    /// A payload in the current format, good for [`LIFETIME_SECONDS`] from
    /// `now`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        imap_server: String,
        imap_port: u16,
        imap_username: String,
        password: String,
        folder: String,
        locale: String,
        keep_completed_in_today: bool,
        completed_first_in_today: bool,
        archive_after_days: u32,
        now: i64,
    ) -> Self {
        SetupPayload {
            version: VERSION,
            imap_server,
            imap_port,
            imap_username,
            password,
            folder,
            locale,
            keep_completed_in_today,
            completed_first_in_today,
            archive_after_days,
            expires: now + LIFETIME_SECONDS,
        }
    }
}

/// The code as an SVG image, for the Mac to show.
#[cfg(feature = "desktop")]
pub fn qr_svg(text: &str) -> String {
    use qrcode::render::svg;
    qrcode::QrCode::with_error_correction_level(text, qrcode::EcLevel::M)
        .expect("a setup code fits a QR code")
        .render::<svg::Color<'_>>()
        .min_dimensions(320, 320)
        .quiet_zone(true)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_payload_written_out_keeps_its_password_to_itself() {
        let payload = SetupPayload::new(
            "imap.example.com".into(),
            993,
            "user@example.com".into(),
            "hunter2".into(),
            "Tasks".into(),
            "en".into(),
            true,
            false,
            30,
            1_700_000_000,
        );
        let written = format!("{payload:?}");
        assert!(written.contains("imap.example.com"), "{written}");
        assert!(!written.contains("hunter2"), "{written}");
    }

    fn payload(now: i64) -> SetupPayload {
        SetupPayload::new(
            "imap.example.com".into(),
            993,
            "me@example.com".into(),
            "pa:ss \"word\"".into(),
            "Orb Tasks".into(),
            "de".into(),
            true,
            false,
            30,
            now,
        )
    }

    #[test]
    fn a_code_reads_back_as_it_was_made() {
        let made = payload(1_000);
        let text = made.encode();
        assert!(text.starts_with("orb-setup:"));
        assert_eq!(SetupPayload::decode(&text, 1_000), Ok(made));
    }

    #[test]
    fn a_code_is_refused_once_its_five_minutes_are_up() {
        let text = payload(1_000).encode();
        assert!(SetupPayload::decode(&text, 1_000 + LIFETIME_SECONDS - 1).is_ok());
        assert_eq!(
            SetupPayload::decode(&text, 1_000 + LIFETIME_SECONDS),
            Err(SetupCodeError::Expired)
        );
    }

    #[test]
    fn anything_else_is_not_a_code() {
        assert_eq!(
            SetupPayload::decode("https://example.com", 0),
            Err(SetupCodeError::NotACode)
        );
        assert_eq!(
            SetupPayload::decode("orb-setup:%%%", 0),
            Err(SetupCodeError::NotACode)
        );
        let newer = format!("orb-setup:{}", URL_SAFE_NO_PAD.encode(br#"{"version":2}"#));
        assert_eq!(
            SetupPayload::decode(&newer, 0),
            Err(SetupCodeError::UnknownVersion)
        );
    }

    #[test]
    #[cfg(feature = "desktop")]
    fn the_code_is_drawn_as_an_svg() {
        let svg = qr_svg(&payload(0).encode());
        assert!(svg.contains("<svg"));
    }
}
