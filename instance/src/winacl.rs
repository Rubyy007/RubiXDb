//! Windows file-ACL hardening for `credentials.json` (Phase 7 gap SG-2,
//! `PHASE_RUBIXDB_SECURITY_GAP_ANALYSIS.md` C-22).
//!
//! Without this, the file's protection is whatever the containing directory
//! happens to inherit: under `%LOCALAPPDATA%` that is owner-only, but under an
//! overridden `RUBIXDB_INSTANCES_ROOT` on another drive it can be readable by
//! every local user (observed: `BUILTIN\Users: ReadAndExecute`).
//!
//! The DACL applied is `D:P(A;;FA;;;SY)(A;;FA;;;OW)`:
//! * `P`  -- protected: inheritance from the parent directory is disabled;
//! * `SY` -- LocalSystem, full access;
//! * `OW` -- OWNER RIGHTS, full access (the file's owner; no other principal).
//!
//! This module is the only `unsafe` in the crate. It is plain FFI with
//! correctly-typed `windows-sys` signatures; every allocated descriptor is
//! released with `LocalFree`.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
#[cfg(test)]
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW, SDDL_REVISION_1,
    SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    GetSecurityDescriptorDacl, ACL, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR,
};

/// The exact DACL, as SDDL.
pub(crate) const OWNER_AND_SYSTEM_ONLY_SDDL: &str = "D:P(A;;FA;;;SY)(A;;FA;;;OW)";

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Replaces the file's DACL with owner + SYSTEM only, inheritance disabled.
/// Fails closed: any error is returned and the caller must not persist the
/// secret.
pub(crate) fn restrict_to_owner_and_system(path: &Path) -> std::io::Result<()> {
    let wpath = wide(path.as_os_str());
    let sddl = wide(OsStr::new(OWNER_AND_SYSTEM_ONLY_SDDL));
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();

    // SAFETY: `sddl`/`wpath` are NUL-terminated and outlive the calls; `sd`
    // receives a LocalAlloc'd descriptor that is freed below on every path;
    // `dacl` points into `sd` and is only used while `sd` is alive.
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            null_mut(),
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl: *mut ACL = null_mut();
        let result = if GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) == 0
        {
            Err(std::io::Error::last_os_error())
        } else if present == 0 || dacl.is_null() {
            Err(std::io::Error::other("SDDL produced no DACL"))
        } else {
            let rc = SetNamedSecurityInfoW(
                wpath.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                dacl,
                null(),
            );
            if rc == ERROR_SUCCESS {
                Ok(())
            } else {
                Err(std::io::Error::from_raw_os_error(rc as i32))
            }
        };
        LocalFree(sd);
        result
    }
}

/// Reads the file's DACL back as SDDL (used by tests to prove the ACL).
#[cfg(test)]
pub(crate) fn dacl_sddl(path: &Path) -> std::io::Result<String> {
    let wpath = wide(path.as_os_str());
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: as above; the returned string buffer is LocalAlloc'd and freed.
    unsafe {
        let rc = GetNamedSecurityInfoW(
            wpath.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            &mut sd,
        );
        if rc != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(rc as i32));
        }
        let mut s: *mut u16 = null_mut();
        let ok = ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut s,
            null_mut(),
        );
        if ok == 0 {
            let e = std::io::Error::last_os_error();
            LocalFree(sd);
            return Err(e);
        }
        let mut len = 0;
        while *s.add(len) != 0 {
            len += 1;
        }
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, len));
        LocalFree(s as _);
        LocalFree(sd);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ACL call's failure must surface as an error (so `save` fails
    /// closed), never be swallowed.
    #[test]
    fn failure_to_apply_the_dacl_is_an_error() {
        let missing = std::env::temp_dir()
            .join(format!("rubixdb_no_such_dir_{}", uuid::Uuid::new_v4()))
            .join("credentials.json.tmp");
        assert!(restrict_to_owner_and_system(&missing).is_err());
    }
}
