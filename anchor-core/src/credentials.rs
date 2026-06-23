//! Windows Credential Manager wrapper (spec §6.3).
//!
//! `credential_key` in `connections.tomlp` is a *reference*; the actual secret lives in
//! Credential Manager under target name `Anchor:<credential_key>`, written/read/deleted
//! through [`CredentialStore`] which wraps `CredWriteW`/`CredReadW`/`CredDeleteW`
//! directly — no third-party credential crate.

use crate::error::{AnchorError, Result};

/// Target-name prefix under which every Anchor secret is stored.
pub const TARGET_PREFIX: &str = "Anchor:";

/// A retrieved secret. Best-effort zeroized on drop.
pub struct Secret(String);

impl Secret {
    /// Wrap a plaintext secret.
    pub fn new(s: impl Into<String>) -> Self {
        Secret(s.into())
    }

    /// Borrow the plaintext. Keep the borrow short-lived.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // Best-effort scrub. Won't touch spare capacity, but clears the live bytes.
        unsafe {
            for b in self.0.as_bytes_mut() {
                *b = 0;
            }
        }
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// The read side of credential storage, depended on by [`crate::mount::MountManager`].
///
/// Kept as a trait (with [`CredentialStore`] as the production implementation) so the
/// mount state machine can be unit-tested with an in-memory source rather than poking the
/// real Credential Manager. Mirrors the dependency-injection shape used elsewhere in core.
pub trait Secrets: Send + Sync {
    /// Fetch the secret stored under `key` (without the `Anchor:` prefix).
    fn retrieve(&self, key: &str) -> Result<Secret>;
}

/// Direct wrapper over the Win32 Credential Manager APIs.
#[derive(Debug, Default, Clone)]
pub struct CredentialStore;

impl CredentialStore {
    /// Construct a store. Stateless — every call hits Credential Manager directly.
    pub fn new() -> Self {
        CredentialStore
    }
}

impl Secrets for CredentialStore {
    fn retrieve(&self, key: &str) -> Result<Secret> {
        self.retrieve_impl(key)
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{ERROR_NOT_FOUND, FILETIME};
    use windows::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_FLAGS,
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn target_for(key: &str) -> Vec<u16> {
        wide(&format!("{TARGET_PREFIX}{key}"))
    }

    impl CredentialStore {
        /// Store (or overwrite) the secret for `key`.
        pub fn store(&self, key: &str, secret: &str) -> Result<()> {
            let target = target_for(key);
            let user = wide(key);
            let mut blob = secret.as_bytes().to_vec();

            let cred = CREDENTIALW {
                Flags: CRED_FLAGS(0),
                Type: CRED_TYPE_GENERIC,
                TargetName: windows::core::PWSTR(target.as_ptr() as *mut u16),
                Comment: windows::core::PWSTR::null(),
                LastWritten: FILETIME::default(),
                CredentialBlobSize: blob.len() as u32,
                CredentialBlob: blob.as_mut_ptr(),
                Persist: CRED_PERSIST_LOCAL_MACHINE,
                AttributeCount: 0,
                Attributes: std::ptr::null_mut(),
                TargetAlias: windows::core::PWSTR::null(),
                UserName: windows::core::PWSTR(user.as_ptr() as *mut u16),
            };

            // SAFETY: `cred` and its referenced buffers (`target`, `user`, `blob`) all
            // outlive this call; CredWriteW copies them.
            unsafe { CredWriteW(&cred, 0) }
                .map_err(|e| AnchorError::Credential(format!("CredWriteW failed: {e}")))
        }

        /// Delete the secret for `key`. Missing entries are treated as success.
        pub fn delete(&self, key: &str) -> Result<()> {
            let target = target_for(key);
            // SAFETY: `target` is a valid NUL-terminated wide string.
            let res = unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None) };
            match res {
                Ok(()) => Ok(()),
                Err(e) if e.code() == ERROR_NOT_FOUND.to_hresult() => Ok(()),
                Err(e) => Err(AnchorError::Credential(format!("CredDeleteW failed: {e}"))),
            }
        }

        pub(crate) fn retrieve_impl(&self, key: &str) -> Result<Secret> {
            let target = target_for(key);
            let mut pcred: *mut CREDENTIALW = std::ptr::null_mut();
            // SAFETY: out-param pattern; on success `pcred` points to a Credential
            // Manager-allocated CREDENTIALW we free with CredFree.
            let read =
                unsafe { CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None, &mut pcred) };
            match read {
                Ok(()) => {
                    // SAFETY: pcred is non-null on Ok.
                    let secret = unsafe {
                        let cred = &*pcred;
                        let bytes = std::slice::from_raw_parts(
                            cred.CredentialBlob,
                            cred.CredentialBlobSize as usize,
                        );
                        let s = String::from_utf8_lossy(bytes).into_owned();
                        CredFree(pcred as *const c_void);
                        s
                    };
                    Ok(Secret::new(secret))
                }
                Err(e) if e.code() == ERROR_NOT_FOUND.to_hresult() => Err(AnchorError::Credential(
                    format!("no stored credential for '{key}' (run `anchor set-password`)"),
                )),
                Err(e) => Err(AnchorError::Credential(format!("CredReadW failed: {e}"))),
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    impl CredentialStore {
        pub fn store(&self, _key: &str, _secret: &str) -> Result<()> {
            Err(AnchorError::Credential(
                "Credential Manager is only available on Windows".into(),
            ))
        }
        pub fn delete(&self, _key: &str) -> Result<()> {
            Err(AnchorError::Credential(
                "Credential Manager is only available on Windows".into(),
            ))
        }
        pub(crate) fn retrieve_impl(&self, _key: &str) -> Result<Secret> {
            Err(AnchorError::Credential(
                "Credential Manager is only available on Windows".into(),
            ))
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn store_retrieve_delete_roundtrip() {
        let store = CredentialStore::new();
        // Unique key so concurrent/leftover runs don't collide.
        let key = format!("anchor-test-{}", std::process::id());
        let secret = "hunter2-üñîçōde";

        store.store(&key, secret).expect("store");
        let got = store.retrieve(&key).expect("retrieve");
        assert_eq!(got.expose(), secret);

        store.delete(&key).expect("delete");
        assert!(store.retrieve(&key).is_err(), "should be gone after delete");
        // Deleting again is a no-op success.
        store.delete(&key).expect("second delete is ok");
    }
}
