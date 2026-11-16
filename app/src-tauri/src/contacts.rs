//! The system address book.
//!
//! Reached through Contacts.framework rather than by reading the database
//! under `~/Library/Application Support/AddressBook`: that file is private,
//! its schema changes between releases, and it is guarded by the same
//! permission this asks for out loud.
//!
//! Everything is read once and kept, then matched in Rust by the same scorer
//! the mail-derived suggestions use. Asking the framework per keystroke would
//! mean a second, different definition of what counts as a match — and a
//! cross-process call between one letter and the next. An address book is a
//! few thousand entries at most, so holding it costs less than asking twice.

use std::sync::Mutex;
use std::sync::OnceLock;

/// One entry: a name to show, and an address to send to.
pub struct Entry {
    pub name: Option<String>,
    pub address: String,
}

/// The address book, once we have it.
///
/// `None` means it has not been read yet; an empty list means it was read and
/// there was nothing there — or that permission was refused, which from here
/// looks the same and needs the same handling.
static CACHE: OnceLock<Mutex<Option<Vec<Entry>>>> = OnceLock::new();

fn cache() -> &'static Mutex<Option<Vec<Entry>>> {
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Everyone in the system address book, as far as we know so far.
///
/// Never asks the framework and never blocks. It answers from what `warm_up`
/// has already put there, and an empty answer means "not yet" rather than
/// "nobody" — which is the right trade when the caller is a keystroke.
///
/// Reading it on demand is what broke this: the framework was consulted
/// between one letter and the next, and asking it for permission from a
/// background thread never returned at all. Every query hung, and the
/// mail-derived suggestions — sitting ready in the same closure — never got
/// out either.
pub fn entries() -> Vec<Entry> {
    let held = match cache().lock() {
        Ok(held) => held,
        // A panic while reading contacts must not take recipient completion
        // with it: without the system book the mail-derived one still works.
        Err(poisoned) => poisoned.into_inner(),
    };
    held.as_ref()
        .map(|entries| {
            entries
                .iter()
                .map(|e| Entry {
                    name: e.name.clone(),
                    address: e.address.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Read the system address book, in the background, once.
///
/// Called when the app starts. Permission is asked for here or not at all: a
/// dialogue belongs to the launch of an application, not to the moment someone
/// is halfway through typing a name.
pub fn warm_up() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(refill);
}

/// Read the book and put it where queries can find it.
///
/// Also used by the permission handler, which arrives long after the load that
/// prompted it has given up and returned nothing.
fn refill() {
    std::thread::spawn(|| {
        let found = load();
        let mut held = match cache().lock() {
            Ok(held) => held,
            Err(poisoned) => poisoned.into_inner(),
        };
        *held = Some(found);
    });
}

#[cfg(not(target_os = "macos"))]
fn load() -> Vec<Entry> {
    Vec::new()
}

#[cfg(target_os = "macos")]
fn load() -> Vec<Entry> {
    use block2::RcBlock;
    use objc2::runtime::ProtocolObject;
    use objc2::AnyThread;
    use objc2_contacts::{
        CNContact, CNContactEmailAddressesKey, CNContactFamilyNameKey, CNContactFetchRequest,
        CNContactGivenNameKey, CNContactOrganizationNameKey, CNContactStore, CNEntityType,
    };
    use objc2_foundation::NSArray;
    use std::ptr::NonNull;

    // Asking for contacts without a usage description in the bundle is not an
    // error the process survives: the system kills it. In a development run
    // there is no bundle at all, so this is also what keeps `tauri dev` alive.
    if !bundle_explains_why() {
        log::info("system address book: no usage description in the bundle, skipping");
        return Vec::new();
    }

    // SAFETY: reading the authorization status touches no contact data and is
    // safe to call from any thread. It never prompts.
    let status =
        unsafe { CNContactStore::authorizationStatusForEntityType(CNEntityType::Contacts) };
    if status != objc2_contacts::CNAuthorizationStatus::Authorized {
        request_access();
        // Whatever the answer is, it will not arrive before this call returns.
        // The next query picks it up; see `forget`.
        log::info(&format!(
            "system address book: not authorised yet ({status:?})"
        ));
        return Vec::new();
    }

    let store = unsafe { CNContactStore::new() };

    // Only the fields a completion list shows. Contacts.framework refuses to
    // hand over anything not asked for here, and asking for everything means
    // decoding photographs in order to display a name.
    let keys = NSArray::from_slice(&[
        ProtocolObject::from_ref(unsafe { CNContactGivenNameKey }),
        ProtocolObject::from_ref(unsafe { CNContactFamilyNameKey }),
        ProtocolObject::from_ref(unsafe { CNContactOrganizationNameKey }),
        ProtocolObject::from_ref(unsafe { CNContactEmailAddressesKey }),
    ]);
    let request = unsafe {
        CNContactFetchRequest::initWithKeysToFetch(CNContactFetchRequest::alloc(), &keys)
    };

    let found = std::cell::RefCell::new(Vec::new());
    let collect = RcBlock::new(
        |contact: NonNull<CNContact>, _stop: NonNull<objc2::runtime::Bool>| {
            // SAFETY: the enumeration hands the block a live contact for the
            // duration of the call, which is the only place it is touched.
            let contact = unsafe { contact.as_ref() };
            let name = display_name(contact);
            let addresses = unsafe { contact.emailAddresses() };
            for labelled in addresses.iter() {
                let address = unsafe { labelled.value() }
                    .to_string()
                    .trim()
                    .to_lowercase();
                // An address book holds people with no address at all, and
                // entries whose "email" is a note to self.
                if address.contains('@') {
                    found.borrow_mut().push(Entry {
                        name: name.clone(),
                        address,
                    });
                }
            }
        },
    );

    // SAFETY: the request and the block both outlive this call, which runs the
    // enumeration to completion before returning.
    let mut error = None;
    let ok = unsafe {
        store.enumerateContactsWithFetchRequest_error_usingBlock(
            &request,
            Some(&mut error),
            &collect,
        )
    };
    if !ok {
        log::warn(&format!(
            "system address book could not be read: {}",
            error
                .map(|e| e.localizedDescription().to_string())
                .unwrap_or_else(|| "no reason given".to_string())
        ));
        return Vec::new();
    }

    // The block borrowed `found`; dropping it ends the borrow.
    drop(collect);
    let entries = found.into_inner();
    log::info(&format!("system address book: {} addresses", entries.len()));
    entries
}

/// A person's name, or the organisation's when they have none.
#[cfg(target_os = "macos")]
fn display_name(contact: &objc2_contacts::CNContact) -> Option<String> {
    let given = unsafe { contact.givenName() }.to_string();
    let family = unsafe { contact.familyName() }.to_string();
    let full = format!("{} {}", given.trim(), family.trim());
    let full = full.trim().to_string();
    if !full.is_empty() {
        return Some(full);
    }
    // A company in the address book has no given or family name, and its
    // organisation is what a person would recognise it by.
    let organisation = unsafe { contact.organizationName() }.to_string();
    let organisation = organisation.trim();
    (!organisation.is_empty()).then(|| organisation.to_string())
}

/// Does the bundle say why it wants contacts?
///
/// Without `NSContactsUsageDescription` the system terminates the process the
/// moment it asks, so this is checked before anything else. It is also false
/// in a development run, where the binary is not in a bundle at all.
#[cfg(target_os = "macos")]
fn bundle_explains_why() -> bool {
    use objc2_foundation::{NSBundle, NSString};

    let key = NSString::from_str("NSContactsUsageDescription");
    NSBundle::mainBundle()
        .objectForInfoDictionaryKey(&key)
        .is_some()
}

/// Ask, once, for permission.
///
/// The answer arrives later, from a dialogue the user has to read; nothing
/// waits for it, and nothing asks twice. Asking on every query is what hung
/// the completion list: from a background thread the call does not come back
/// while a dialogue is pending, so every keystroke queued behind a question
/// nobody was being shown.
#[cfg(target_os = "macos")]
fn request_access() {
    static ASKED: std::sync::Once = std::sync::Once::new();
    ASKED.call_once(ask);
}

#[cfg(target_os = "macos")]
fn ask() {
    use block2::RcBlock;
    use objc2_contacts::{CNContactStore, CNEntityType};

    let store = unsafe { CNContactStore::new() };
    let answered = RcBlock::new(
        |granted: objc2::runtime::Bool, _error: *mut objc2_foundation::NSError| {
            log::info(&format!(
                "system address book: access {}",
                if granted.as_bool() {
                    "granted"
                } else {
                    "refused"
                }
            ));
            // Granted or not, what is cached was read without it.
            refill();
        },
    );
    // SAFETY: the completion block is retained by the framework for as long as
    // it needs it.
    unsafe {
        store.requestAccessForEntityType_completionHandler(CNEntityType::Contacts, &answered);
    }
}

/// Logging, which the core owns.
mod log {
    pub fn info(message: &str) {
        orb_core::logger::info(message);
    }
    #[cfg(target_os = "macos")]
    pub fn warn(message: &str) {
        orb_core::logger::warn(message);
    }
}
