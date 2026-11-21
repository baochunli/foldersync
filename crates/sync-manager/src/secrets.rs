//! OS credential storage abstraction (`SecretStore`), in-memory test fake,
//! macOS Keychain backend (`Security.framework`, service `dev.foldersync`), and
//! credential-helper runner for rclone `--password-command` (§8, §19).

use std::{collections::BTreeMap, fmt, io::Write, sync::Mutex};

use thiserror::Error;

use crate::platform::validate_safe_path_id;

/// macOS Keychain generic-password service identifier for FolderSync accounts.
pub const KEYCHAIN_SERVICE: &str = "dev.foldersync";

/// Secret string whose `Debug` output is always redacted and whose memory is
/// scrubbed on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// Wraps a sensitive value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Generates a fresh 256-bit cryptographically random lowercase-hex passphrase
    /// suitable for rclone config encryption.
    pub fn generate_random_passphrase() -> Result<Self, SecretStoreError> {
        let mut bytes = [0u8; 32];
        fill_random_bytes(&mut bytes)?;
        let mut hex = String::with_capacity(64);
        for b in bytes {
            use std::fmt::Write as _;
            let _ = write!(&mut hex, "{b:02x}");
        }
        // Scrub stack buffer before returning.
        for b in &mut bytes {
            unsafe { std::ptr::write_volatile(b, 0) };
        }
        Ok(Self(hex))
    }

    /// Exposes the underlying secret strictly at credential boundaries (writing
    /// to Keychain or credential-helper stdout).
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString([REDACTED])")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        // SAFETY: zeroing the owned UTF-8 buffer with ASCII NUL bytes keeps it valid UTF-8.
        let bytes = unsafe { self.0.as_bytes_mut() };
        for b in bytes {
            unsafe { std::ptr::write_volatile(b, 0) };
        }
        self.0.clear();
    }
}

#[cfg(unix)]
fn fill_random_bytes(buf: &mut [u8]) -> Result<(), SecretStoreError> {
    use std::{fs::File, io::Read};
    let mut f = File::open("/dev/urandom").map_err(|err| SecretStoreError::Backend {
        reason: format!("failed to open /dev/urandom for passphrase generation: {err}"),
    })?;
    f.read_exact(buf).map_err(|err| SecretStoreError::Backend {
        reason: format!("failed to read random bytes from /dev/urandom: {err}"),
    })
}

#[cfg(not(unix))]
fn fill_random_bytes(buf: &mut [u8]) -> Result<(), SecretStoreError> {
    let mut offset = 0;
    while offset < buf.len() {
        let u = uuid::Uuid::new_v4();
        let src = u.as_bytes();
        let n = (buf.len() - offset).min(src.len());
        buf[offset..offset + n].copy_from_slice(&src[..n]);
        offset += n;
    }
    Ok(())
}

/// Errors returned by [`SecretStore`] implementations.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SecretStoreError {
    #[error("invalid credential key `{0}` for credential lookup")]
    InvalidAccountId(String),
    #[error("credential already exists for `{account_id}`")]
    AlreadyExists { account_id: String },
    #[error("no credential stored for `{account_id}`")]
    NotFound { account_id: String },
    #[error("authentication required: credential store is locked or unavailable ({reason})")]
    AuthenticationRequired { reason: String },
    #[error("credential store backend error: {reason}")]
    Backend { reason: String },
}

/// Validates a Keychain credential key: either `<install-id>/<account-id>`
/// (each component a single safe identifier `[A-Za-z0-9_-]`) or a single safe
/// identifier. Rejects traversal (`..`), empty segments, or multiple slashes.
pub fn validate_credential_key(key: &str) -> Result<(), SecretStoreError> {
    let mut parts = key.split('/');
    let Some(first) = parts.next() else {
        return Err(SecretStoreError::InvalidAccountId(key.to_string()));
    };
    validate_safe_path_id("credential_key", first)
        .map_err(|_| SecretStoreError::InvalidAccountId(key.to_string()))?;
    if let Some(second) = parts.next() {
        validate_safe_path_id("account_id", second)
            .map_err(|_| SecretStoreError::InvalidAccountId(key.to_string()))?;
        if parts.next().is_some() {
            return Err(SecretStoreError::InvalidAccountId(key.to_string()));
        }
    }
    Ok(())
}

/// Formats the per-state scoped Keychain credential key `<install-id>/<account-id>`.
pub fn scoped_credential_key(
    install_id: &str,
    account_id: &str,
) -> Result<String, SecretStoreError> {
    validate_safe_path_id("install_id", install_id)
        .map_err(|_| SecretStoreError::InvalidAccountId(install_id.to_string()))?;
    validate_safe_path_id("account_id", account_id)
        .map_err(|_| SecretStoreError::InvalidAccountId(account_id.to_string()))?;
    Ok(format!("{install_id}/{account_id}"))
}

/// Injectable secret storage boundary for account rclone-config passphrases (§8).
///
/// Production code uses [`SystemSecretStore`] (macOS Keychain generic password
/// under service `dev.foldersync`); tests always inject [`InMemorySecretStore`].
pub trait SecretStore: Send + Sync {
    fn get_secret(&self, key: &str) -> Result<SecretString, SecretStoreError>;
    /// Create-only credential registration: fails with
    /// [`SecretStoreError::AlreadyExists`] if an item for `key` already exists.
    fn create_secret(&self, key: &str, secret: &SecretString) -> Result<(), SecretStoreError>;
    fn set_secret(&self, key: &str, secret: &SecretString) -> Result<(), SecretStoreError>;
    fn delete_secret(&self, key: &str) -> Result<(), SecretStoreError>;
}

/// Pure in-memory fake [`SecretStore`] for tests (never touches OS Keychain or disk).
#[derive(Debug, Default)]
pub struct InMemorySecretStore {
    state: Mutex<InMemoryStoreState>,
}

#[derive(Debug, Default)]
struct InMemoryStoreState {
    entries: BTreeMap<String, SecretString>,
    locked: bool,
    unavailable: bool,
}

impl InMemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Simulates a locked OS credential store (all operations return
    /// [`SecretStoreError::AuthenticationRequired`]).
    pub fn set_locked(&self, locked: bool) {
        self.state.lock().unwrap().locked = locked;
    }

    /// Simulates an unavailable OS credential store (all operations return
    /// [`SecretStoreError::AuthenticationRequired`]).
    pub fn set_unavailable(&self, unavailable: bool) {
        self.state.lock().unwrap().unavailable = unavailable;
    }

    pub fn contains(&self, key: &str) -> bool {
        self.state.lock().unwrap().entries.contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.state.lock().unwrap().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.state.lock().unwrap().entries.is_empty()
    }

    fn check_available(state: &InMemoryStoreState) -> Result<(), SecretStoreError> {
        if state.unavailable {
            return Err(SecretStoreError::AuthenticationRequired {
                reason: "credential store is unavailable".to_string(),
            });
        }
        if state.locked {
            return Err(SecretStoreError::AuthenticationRequired {
                reason: "credential store is locked".to_string(),
            });
        }
        Ok(())
    }
}

impl SecretStore for InMemorySecretStore {
    fn get_secret(&self, key: &str) -> Result<SecretString, SecretStoreError> {
        validate_credential_key(key)?;
        let state = self.state.lock().unwrap();
        Self::check_available(&state)?;
        state
            .entries
            .get(key)
            .cloned()
            .ok_or_else(|| SecretStoreError::NotFound {
                account_id: key.to_string(),
            })
    }

    fn create_secret(&self, key: &str, secret: &SecretString) -> Result<(), SecretStoreError> {
        validate_credential_key(key)?;
        if secret.is_empty() {
            return Err(SecretStoreError::Backend {
                reason: "secret passphrase must not be empty".to_string(),
            });
        }
        let mut state = self.state.lock().unwrap();
        Self::check_available(&state)?;
        if state.entries.contains_key(key) {
            return Err(SecretStoreError::AlreadyExists {
                account_id: key.to_string(),
            });
        }
        state.entries.insert(key.to_string(), secret.clone());
        Ok(())
    }

    fn set_secret(&self, key: &str, secret: &SecretString) -> Result<(), SecretStoreError> {
        validate_credential_key(key)?;
        if secret.is_empty() {
            return Err(SecretStoreError::Backend {
                reason: "secret passphrase must not be empty".to_string(),
            });
        }
        let mut state = self.state.lock().unwrap();
        Self::check_available(&state)?;
        state.entries.insert(key.to_string(), secret.clone());
        Ok(())
    }

    fn delete_secret(&self, key: &str) -> Result<(), SecretStoreError> {
        validate_credential_key(key)?;
        let mut state = self.state.lock().unwrap();
        Self::check_available(&state)?;
        state.entries.remove(key);
        Ok(())
    }
}

/// Executes the `sync-cli credential-helper --account <key>` operation against an
/// injected [`SecretStore`], writing ONLY the password bytes + newline to `out`.
///
/// Never reads stdin and never emits secret material in errors.
pub fn run_credential_helper(
    store: &dyn SecretStore,
    key: &str,
    out: &mut dyn Write,
) -> Result<(), SecretStoreError> {
    let secret = store.get_secret(key)?;
    out.write_all(secret.expose().as_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush())
        .map_err(|err| SecretStoreError::Backend {
            reason: format!("failed to write credential to stdout: {err}"),
        })
}

/// Real macOS Keychain backend (`Security.framework` generic password under
/// service `dev.foldersync` and account `<install-id>/<account-id>`).
///
/// Always sets `kSecUseAuthenticationUI = kSecUseAuthenticationUIFail` so a
/// locked Keychain fails immediately with [`SecretStoreError::AuthenticationRequired`]
/// instead of blocking on a hidden prompt.
#[cfg(target_os = "macos")]
#[derive(Debug, Default, Clone, Copy)]
pub struct KeychainSecretStore;

#[cfg(target_os = "macos")]
mod macos_keychain {
    use std::ffi::c_void;
    use std::ptr;

    use super::{
        KEYCHAIN_SERVICE, KeychainSecretStore, SecretStore, SecretStoreError, SecretString,
        validate_credential_key,
    };

    type CFAllocatorRef = *const c_void;
    type CFTypeRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFDataRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type CFBooleanRef = *const c_void;
    type CFIndex = isize;
    type OSStatus = i32;

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

    const ERR_SEC_SUCCESS: OSStatus = 0;
    const ERR_SEC_USER_CANCELED: OSStatus = -128;
    const ERR_SEC_NOT_AVAILABLE: OSStatus = -25291;
    const ERR_SEC_AUTH_FAILED: OSStatus = -25293;
    const ERR_SEC_DUPLICATE_ITEM: OSStatus = -25299;
    const ERR_SEC_ITEM_NOT_FOUND: OSStatus = -25300;
    const ERR_SEC_INTERACTION_NOT_ALLOWED: OSStatus = -25308;
    const ERR_SEC_DECODE: OSStatus = -26275;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFAllocatorDefault: CFAllocatorRef;
        static kCFBooleanTrue: CFBooleanRef;
        static kCFTypeDictionaryKeyCallBacks: c_void;
        static kCFTypeDictionaryValueCallBacks: c_void;

        fn CFRelease(cf: CFTypeRef);
        fn CFStringCreateWithBytes(
            alloc: CFAllocatorRef,
            bytes: *const u8,
            num_bytes: CFIndex,
            encoding: u32,
            is_external_representation: u8,
        ) -> CFStringRef;
        fn CFDataCreate(alloc: CFAllocatorRef, bytes: *const u8, length: CFIndex) -> CFDataRef;
        fn CFDataGetLength(the_data: CFDataRef) -> CFIndex;
        fn CFDataGetBytePtr(the_data: CFDataRef) -> *const u8;
        fn CFDictionaryCreate(
            allocator: CFAllocatorRef,
            keys: *const *const c_void,
            values: *const *const c_void,
            num_values: CFIndex,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> CFDictionaryRef;
    }

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        static kSecClass: CFStringRef;
        static kSecClassGenericPassword: CFStringRef;
        static kSecAttrService: CFStringRef;
        static kSecAttrAccount: CFStringRef;
        static kSecValueData: CFStringRef;
        static kSecReturnData: CFStringRef;
        static kSecMatchLimit: CFStringRef;
        static kSecMatchLimitOne: CFStringRef;
        static kSecUseAuthenticationUI: CFStringRef;
        static kSecUseAuthenticationUIFail: CFStringRef;

        fn SecItemCopyMatching(query: CFDictionaryRef, result: *mut CFTypeRef) -> OSStatus;
        fn SecItemAdd(attributes: CFDictionaryRef, result: *mut CFTypeRef) -> OSStatus;
        fn SecItemUpdate(query: CFDictionaryRef, attributes_to_update: CFDictionaryRef)
        -> OSStatus;
        fn SecItemDelete(query: CFDictionaryRef) -> OSStatus;
    }

    struct OwnedCF(CFTypeRef);

    impl OwnedCF {
        fn new(ptr: CFTypeRef) -> Result<Self, SecretStoreError> {
            if ptr.is_null() {
                Err(SecretStoreError::Backend {
                    reason: "CoreFoundation allocation returned null".to_string(),
                })
            } else {
                Ok(Self(ptr))
            }
        }

        fn as_ptr(&self) -> CFTypeRef {
            self.0
        }
    }

    impl Drop for OwnedCF {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0) };
            }
        }
    }

    fn cf_string(s: &str) -> Result<OwnedCF, SecretStoreError> {
        let ptr = unsafe {
            CFStringCreateWithBytes(
                kCFAllocatorDefault,
                s.as_ptr(),
                s.len() as CFIndex,
                K_CF_STRING_ENCODING_UTF8,
                0,
            )
        };
        OwnedCF::new(ptr)
    }

    fn cf_data(bytes: &[u8]) -> Result<OwnedCF, SecretStoreError> {
        let ptr =
            unsafe { CFDataCreate(kCFAllocatorDefault, bytes.as_ptr(), bytes.len() as CFIndex) };
        OwnedCF::new(ptr)
    }

    fn cf_dict(pairs: &[(CFTypeRef, CFTypeRef)]) -> Result<OwnedCF, SecretStoreError> {
        let keys: Vec<*const c_void> = pairs.iter().map(|(k, _)| *k).collect();
        let vals: Vec<*const c_void> = pairs.iter().map(|(_, v)| *v).collect();
        let ptr = unsafe {
            CFDictionaryCreate(
                kCFAllocatorDefault,
                keys.as_ptr(),
                vals.as_ptr(),
                pairs.len() as CFIndex,
                &kCFTypeDictionaryKeyCallBacks as *const c_void,
                &kCFTypeDictionaryValueCallBacks as *const c_void,
            )
        };
        OwnedCF::new(ptr)
    }

    fn map_os_status(status: OSStatus, key: &str) -> SecretStoreError {
        match status {
            ERR_SEC_DUPLICATE_ITEM => SecretStoreError::AlreadyExists {
                account_id: key.to_string(),
            },
            ERR_SEC_ITEM_NOT_FOUND => SecretStoreError::NotFound {
                account_id: key.to_string(),
            },
            ERR_SEC_INTERACTION_NOT_ALLOWED => SecretStoreError::AuthenticationRequired {
                reason: "macOS Keychain is locked or non-interactive access was denied (errSecInteractionNotAllowed)".to_string(),
            },
            ERR_SEC_AUTH_FAILED => SecretStoreError::AuthenticationRequired {
                reason: "macOS Keychain authentication failed (errSecAuthFailed)".to_string(),
            },
            ERR_SEC_NOT_AVAILABLE => SecretStoreError::AuthenticationRequired {
                reason: "macOS Keychain is unavailable (errSecNotAvailable)".to_string(),
            },
            ERR_SEC_USER_CANCELED => SecretStoreError::AuthenticationRequired {
                reason: "macOS Keychain authorization was canceled (errSecUserCanceled)".to_string(),
            },
            ERR_SEC_DECODE => SecretStoreError::AuthenticationRequired {
                reason: "macOS Keychain item could not be decoded (errSecDecode)".to_string(),
            },
            other => SecretStoreError::Backend {
                reason: format!("macOS Keychain returned OSStatus {other}"),
            },
        }
    }

    impl SecretStore for KeychainSecretStore {
        fn get_secret(&self, key: &str) -> Result<SecretString, SecretStoreError> {
            validate_credential_key(key)?;
            let service_cf = cf_string(KEYCHAIN_SERVICE)?;
            let account_cf = cf_string(key)?;

            let query = unsafe {
                cf_dict(&[
                    (kSecClass, kSecClassGenericPassword),
                    (kSecAttrService, service_cf.as_ptr()),
                    (kSecAttrAccount, account_cf.as_ptr()),
                    (kSecReturnData, kCFBooleanTrue),
                    (kSecMatchLimit, kSecMatchLimitOne),
                    (kSecUseAuthenticationUI, kSecUseAuthenticationUIFail),
                ])?
            };

            let mut result: CFTypeRef = ptr::null();
            let status = unsafe { SecItemCopyMatching(query.as_ptr(), &mut result) };
            if status != ERR_SEC_SUCCESS {
                return Err(map_os_status(status, key));
            }
            let data_cf = OwnedCF::new(result)?;
            let len = unsafe { CFDataGetLength(data_cf.as_ptr()) };
            if len < 0 {
                return Err(SecretStoreError::Backend {
                    reason: "Keychain returned negative data length".to_string(),
                });
            }
            let ptr = unsafe { CFDataGetBytePtr(data_cf.as_ptr()) };
            if ptr.is_null() && len > 0 {
                return Err(SecretStoreError::Backend {
                    reason: "Keychain returned null byte pointer".to_string(),
                });
            }
            let slice = if len == 0 {
                &[]
            } else {
                unsafe { std::slice::from_raw_parts(ptr, len as usize) }
            };
            let text = std::str::from_utf8(slice)
                .map_err(|_| SecretStoreError::Backend {
                    reason: "Keychain item bytes are not valid UTF-8".to_string(),
                })?
                .to_string();
            Ok(SecretString::new(text))
        }

        fn create_secret(&self, key: &str, secret: &SecretString) -> Result<(), SecretStoreError> {
            validate_credential_key(key)?;
            if secret.is_empty() {
                return Err(SecretStoreError::Backend {
                    reason: "secret passphrase must not be empty".to_string(),
                });
            }
            let service_cf = cf_string(KEYCHAIN_SERVICE)?;
            let account_cf = cf_string(key)?;
            let value_cf = cf_data(secret.expose().as_bytes())?;

            let add_attrs = unsafe {
                cf_dict(&[
                    (kSecClass, kSecClassGenericPassword),
                    (kSecAttrService, service_cf.as_ptr()),
                    (kSecAttrAccount, account_cf.as_ptr()),
                    (kSecValueData, value_cf.as_ptr()),
                    (kSecUseAuthenticationUI, kSecUseAuthenticationUIFail),
                ])?
            };
            let status = unsafe { SecItemAdd(add_attrs.as_ptr(), ptr::null_mut()) };
            if status == ERR_SEC_SUCCESS {
                Ok(())
            } else {
                Err(map_os_status(status, key))
            }
        }

        fn set_secret(&self, key: &str, secret: &SecretString) -> Result<(), SecretStoreError> {
            match self.create_secret(key, secret) {
                Ok(()) => Ok(()),
                Err(SecretStoreError::AlreadyExists { .. }) => {
                    let service_cf = cf_string(KEYCHAIN_SERVICE)?;
                    let account_cf = cf_string(key)?;
                    let value_cf = cf_data(secret.expose().as_bytes())?;
                    let query = unsafe {
                        cf_dict(&[
                            (kSecClass, kSecClassGenericPassword),
                            (kSecAttrService, service_cf.as_ptr()),
                            (kSecAttrAccount, account_cf.as_ptr()),
                            (kSecUseAuthenticationUI, kSecUseAuthenticationUIFail),
                        ])?
                    };
                    let update = unsafe { cf_dict(&[(kSecValueData, value_cf.as_ptr())])? };
                    let update_status = unsafe { SecItemUpdate(query.as_ptr(), update.as_ptr()) };
                    if update_status == ERR_SEC_SUCCESS {
                        Ok(())
                    } else {
                        Err(map_os_status(update_status, key))
                    }
                }
                Err(other) => Err(other),
            }
        }

        fn delete_secret(&self, key: &str) -> Result<(), SecretStoreError> {
            validate_credential_key(key)?;
            let service_cf = cf_string(KEYCHAIN_SERVICE)?;
            let account_cf = cf_string(key)?;

            let query = unsafe {
                cf_dict(&[
                    (kSecClass, kSecClassGenericPassword),
                    (kSecAttrService, service_cf.as_ptr()),
                    (kSecAttrAccount, account_cf.as_ptr()),
                    (kSecUseAuthenticationUI, kSecUseAuthenticationUIFail),
                ])?
            };
            let status = unsafe { SecItemDelete(query.as_ptr()) };
            if status == ERR_SEC_SUCCESS || status == ERR_SEC_ITEM_NOT_FOUND {
                Ok(())
            } else {
                Err(map_os_status(status, key))
            }
        }
    }
}

/// Platform default OS [`SecretStore`]: macOS Keychain on macOS, or an explicit
/// `AuthenticationRequired` stub on platforms without a qualified backend yet.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemSecretStore;

impl SecretStore for SystemSecretStore {
    fn get_secret(&self, key: &str) -> Result<SecretString, SecretStoreError> {
        #[cfg(target_os = "macos")]
        {
            KeychainSecretStore.get_secret(key)
        }
        #[cfg(not(target_os = "macos"))]
        {
            validate_credential_key(key)?;
            Err(SecretStoreError::AuthenticationRequired {
                reason: "OS secret store backend is not qualified on this platform yet".to_string(),
            })
        }
    }

    fn create_secret(&self, key: &str, secret: &SecretString) -> Result<(), SecretStoreError> {
        #[cfg(target_os = "macos")]
        {
            KeychainSecretStore.create_secret(key, secret)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = secret;
            validate_credential_key(key)?;
            Err(SecretStoreError::AuthenticationRequired {
                reason: "OS secret store backend is not qualified on this platform yet".to_string(),
            })
        }
    }

    fn set_secret(&self, key: &str, secret: &SecretString) -> Result<(), SecretStoreError> {
        #[cfg(target_os = "macos")]
        {
            KeychainSecretStore.set_secret(key, secret)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = secret;
            validate_credential_key(key)?;
            Err(SecretStoreError::AuthenticationRequired {
                reason: "OS secret store backend is not qualified on this platform yet".to_string(),
            })
        }
    }

    fn delete_secret(&self, key: &str) -> Result<(), SecretStoreError> {
        #[cfg(target_os = "macos")]
        {
            KeychainSecretStore.delete_secret(key)
        }
        #[cfg(not(target_os = "macos"))]
        {
            validate_credential_key(key)?;
            Err(SecretStoreError::AuthenticationRequired {
                reason: "OS secret store backend is not qualified on this platform yet".to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_string_redacts_debug_and_generates_256bit_hex_passphrase() {
        let s = SecretString::new("top-secret-value-123");
        assert_eq!(format!("{s:?}"), "SecretString([REDACTED])");
        assert!(!format!("{s:?}").contains("top-secret-value-123"));

        let p1 = SecretString::generate_random_passphrase().unwrap();
        let p2 = SecretString::generate_random_passphrase().unwrap();
        assert_eq!(p1.expose().len(), 64);
        assert!(p1.expose().chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(p1.expose(), p2.expose());
    }

    #[test]
    fn credential_helper_reads_in_memory_fake_and_surfaces_authentication_required_when_locked() {
        let store = InMemorySecretStore::new();
        let key = scoped_credential_key("device-abc", "acc-1").unwrap();
        store
            .create_secret(&key, &SecretString::new("pw-alpha-999"))
            .unwrap();
        // create_secret is strictly create-only.
        assert!(matches!(
            store.create_secret(&key, &SecretString::new("pw-overwrite")),
            Err(SecretStoreError::AlreadyExists { .. })
        ));

        let mut stdout = Vec::new();
        run_credential_helper(&store, &key, &mut stdout).unwrap();
        assert_eq!(String::from_utf8(stdout).unwrap(), "pw-alpha-999\n");

        // Locked store -> AuthenticationRequired, never prints anything.
        store.set_locked(true);
        let mut locked_out = Vec::new();
        let err = run_credential_helper(&store, &key, &mut locked_out).unwrap_err();
        assert!(matches!(
            err,
            SecretStoreError::AuthenticationRequired { .. }
        ));
        assert!(locked_out.is_empty());

        // Unavailable store -> AuthenticationRequired.
        store.set_locked(false);
        store.set_unavailable(true);
        let mut unavail_out = Vec::new();
        let err2 = run_credential_helper(&store, &key, &mut unavail_out).unwrap_err();
        assert!(matches!(
            err2,
            SecretStoreError::AuthenticationRequired { .. }
        ));
        assert!(unavail_out.is_empty());

        // Reject unsafe keys (traversal, extra slashes).
        store.set_unavailable(false);
        let mut bad_out = Vec::new();
        assert!(matches!(
            run_credential_helper(&store, "../escape", &mut bad_out),
            Err(SecretStoreError::InvalidAccountId(_))
        ));
        assert!(matches!(
            run_credential_helper(&store, "a/b/c", &mut bad_out),
            Err(SecretStoreError::InvalidAccountId(_))
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "touches real macOS Keychain; run manually during owner qualification"]
    fn keychain_generic_password_roundtrip_manual_only() {
        let store = KeychainSecretStore;
        let key = scoped_credential_key("manual-qual", "foldersync-test").unwrap();
        let secret = SecretString::new("manual-qual-passphrase");
        let _ = store.delete_secret(&key);
        store.create_secret(&key, &secret).unwrap();
        assert!(matches!(
            store.create_secret(&key, &secret),
            Err(SecretStoreError::AlreadyExists { .. })
        ));
        let got = store.get_secret(&key).unwrap();
        assert_eq!(got.expose(), secret.expose());
        store.delete_secret(&key).unwrap();
        assert!(matches!(
            store.get_secret(&key),
            Err(SecretStoreError::NotFound { .. })
        ));
    }
}
