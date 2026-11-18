//! The system Keychain, where a password typed into Orb is kept when asked.
//!
//! Reached through the Security framework rather than the `security` command
//! line tool: the tool takes the password as an argument, which every process
//! on the machine can read off the process list while it runs.

use secrecy::SecretString;

/// The service every Orb item is filed under; the account is the login's
/// [`crate::config::Credential::key`].
pub const SERVICE: &str = "Orb";

/// Somewhere a password can be kept between sessions.
pub trait PasswordStore: Send + Sync {
    /// The password stored under `key`, if there is one.
    fn get(&self, key: &str) -> Option<SecretString>;
    /// Store `password` under `key`, replacing what was there.
    fn set(&self, key: &str, password: &SecretString) -> Result<(), String>;
    /// Remove what is stored under `key`; nothing stored is no error.
    fn delete(&self, _key: &str) -> Result<(), String> {
        Err("this store cannot remove an item".to_string())
    }
}

/// The login Keychain of the user running Orb.
pub struct Keychain;

#[cfg(any(target_os = "macos", target_os = "ios"))]
impl PasswordStore for Keychain {
    fn get(&self, key: &str) -> Option<SecretString> {
        match security_framework::passwords::get_generic_password(SERVICE, key) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(password) => Some(SecretString::from(password)),
                Err(_) => {
                    crate::logger::warn(&format!("keychain: the item for {key} is not text"));
                    None
                }
            },
            // errSecItemNotFound: nothing stored, which is the ordinary case.
            Err(error) if error.code() == -25300 => None,
            Err(error) => {
                crate::logger::warn(&format!(
                    "keychain: could not read the item for {key}: {}",
                    error.message().unwrap_or_else(|| error.to_string())
                ));
                None
            }
        }
    }

    fn set(&self, key: &str, password: &SecretString) -> Result<(), String> {
        use secrecy::ExposeSecret;
        security_framework::passwords::set_generic_password_options(
            password.expose_secret().as_bytes(),
            item_options(key)?,
        )
        .map_err(describe)
    }

    fn delete(&self, key: &str) -> Result<(), String> {
        match security_framework::passwords::delete_generic_password(SERVICE, key) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == -25300 => Ok(()),
            Err(error) => Err(error.message().unwrap_or_else(|| error.to_string())),
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn describe(error: security_framework::base::Error) -> String {
    error.message().unwrap_or_else(|| error.to_string())
}

/// The Keychain item a login's password is kept as, on the Mac: the login
/// Keychain, which has no accessibility classes.
#[cfg(target_os = "macos")]
fn item_options(key: &str) -> Result<security_framework::passwords::PasswordOptions, String> {
    Ok(security_framework::passwords::PasswordOptions::new_generic_password(SERVICE, key))
}

/// The Keychain item a login's password is kept as, on the phone: readable
/// after the first unlock rather than only while unlocked, since a background
/// refresh reads it while the device is locked; and on this device only, out
/// of backups and other devices' Keychains, since a password that migrates is
/// one more copy nobody asked for.
#[cfg(target_os = "ios")]
fn item_options(key: &str) -> Result<security_framework::passwords::PasswordOptions, String> {
    use security_framework::access_control::{ProtectionMode, SecAccessControl};
    let mut options =
        security_framework::passwords::PasswordOptions::new_generic_password(SERVICE, key);
    let control = SecAccessControl::create_with_protection(
        Some(ProtectionMode::AccessibleAfterFirstUnlockThisDeviceOnly),
        0,
    )
    .map_err(describe)?;
    options.set_access_control(control);
    Ok(options)
}

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
impl PasswordStore for Keychain {
    fn get(&self, _key: &str) -> Option<SecretString> {
        None
    }

    fn set(&self, _key: &str, _password: &SecretString) -> Result<(), String> {
        Err("there is no Keychain on this platform".to_string())
    }
}
