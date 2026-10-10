//! Who a process acts as: its user and group ids, as on Linux.
//!
//! Each process has three user ids. The *real* id is who started it, the
//! *effective* id is what permission checks use, and the *saved* id
//! remembers an effective id given up, so a setuid program can switch back
//! and forth. Groups work the same way, plus a list of supplementary groups
//! (the other groups in /etc/group the user belongs to).
//!
//! User 0 (root) passes every permission check, except that it can only run
//! files with at least one execute bit. There are no finer-grained
//! capabilities.
//!
//! A process's credentials are shared, immutable [`Credentials`] values:
//! changing them installs a new one. Permission checks take a snapshot
//! without holding any lock, and threads that are mid-system-call keep a
//! consistent view.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::errno::{Errno, Result};
use crate::sync::Once;

pub type Uid = u32;
pub type Gid = u32;

/// The most supplementary groups a process may have (Linux's limit is far
/// higher, but nobody gets near this).
pub const MAX_GROUPS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub uid: Uid,
    pub euid: Uid,
    pub suid: Uid,
    pub gid: Gid,
    pub egid: Gid,
    pub sgid: Gid,
    pub groups: Vec<Gid>,
}

impl Credentials {
    pub const fn root() -> Self {
        Self { uid: 0, euid: 0, suid: 0, gid: 0, egid: 0, sgid: 0, groups: Vec::new() }
    }

    /// A process running entirely as one user and group.
    #[cfg(test)]
    pub fn user(uid: Uid, gid: Gid) -> Self {
        Self { uid, euid: uid, suid: uid, gid, egid: gid, sgid: gid, groups: Vec::new() }
    }

    /// Whether permission checks are skipped.
    pub fn is_root(&self) -> bool {
        self.euid == 0
    }

    pub fn in_group(&self, gid: Gid) -> bool {
        self.egid == gid || self.groups.contains(&gid)
    }

    /// The same process with its real ids as the effective ones: what
    /// `access` checks against.
    pub fn as_real(&self) -> Self {
        Self { euid: self.uid, egid: self.gid, ..self.clone() }
    }

    /// Whether a process with these credentials may send a signal to one
    /// with `target`'s: root may signal anyone, others only processes of the
    /// same user.
    pub fn may_signal(&self, target: &Credentials) -> bool {
        self.is_root() || [self.uid, self.euid].iter().any(|&id| id == target.uid || id == target.suid)
    }

    /// The credentials after `execve` of a file with these permissions and
    /// owner: setuid and setgid programs run as the file's owner and group.
    /// The saved ids always take the new effective ones.
    pub fn after_exec(&self, permissions: u32, owner: Uid, group: Gid) -> Self {
        let mut new = self.clone();
        if permissions & 0o4000 != 0 {
            new.euid = owner;
        }
        // Setgid without group execute means mandatory locking, not setgid.
        if permissions & 0o2010 == 0o2010 {
            new.egid = group;
        }
        new.suid = new.euid;
        new.sgid = new.egid;
        new
    }

    /// Whether the program should run in "secure" mode (`AT_SECURE`): it
    /// gained privileges, so its loader ignores variables like
    /// `LD_LIBRARY_PATH` that the caller controls.
    pub fn is_elevated(&self) -> bool {
        self.euid != self.uid || self.egid != self.gid
    }

    /// `setresuid`: `None` leaves an id alone. Without root, each new id
    /// must be one of the current three.
    pub fn set_user_ids(&self, real: Option<Uid>, effective: Option<Uid>, saved: Option<Uid>) -> Result<Self> {
        let allowed = |id: Uid| self.is_root() || [self.uid, self.euid, self.suid].contains(&id);
        if ![real, effective, saved].into_iter().flatten().all(allowed) {
            return Err(Errno::EPERM);
        }
        Ok(Self {
            uid: real.unwrap_or(self.uid),
            euid: effective.unwrap_or(self.euid),
            suid: saved.unwrap_or(self.suid),
            ..self.clone()
        })
    }

    /// `setresgid`, like [`set_user_ids`](Self::set_user_ids).
    pub fn set_group_ids(&self, real: Option<Gid>, effective: Option<Gid>, saved: Option<Gid>) -> Result<Self> {
        let allowed = |id: Gid| self.is_root() || [self.gid, self.egid, self.sgid].contains(&id);
        if ![real, effective, saved].into_iter().flatten().all(allowed) {
            return Err(Errno::EPERM);
        }
        Ok(Self {
            gid: real.unwrap_or(self.gid),
            egid: effective.unwrap_or(self.egid),
            sgid: saved.unwrap_or(self.sgid),
            ..self.clone()
        })
    }

    /// `setuid`: root sets all three ids (giving root up for good); others
    /// may only switch the effective id to their real or saved one.
    pub fn set_uid(&self, id: Uid) -> Result<Self> {
        if self.is_root() {
            self.set_user_ids(Some(id), Some(id), Some(id))
        } else {
            self.set_user_ids(None, Some(id), None)
        }
    }

    pub fn set_gid(&self, id: Gid) -> Result<Self> {
        if self.is_root() {
            self.set_group_ids(Some(id), Some(id), Some(id))
        } else {
            self.set_group_ids(None, Some(id), None)
        }
    }

    /// `setreuid`: like `setresuid`, but the saved id follows the effective
    /// one whenever the real id is set or the effective id changes to
    /// something other than the real id. Without root the real id can only
    /// become the effective one (or stay).
    pub fn set_real_effective_uid(&self, real: Option<Uid>, effective: Option<Uid>) -> Result<Self> {
        if !self.is_root() && real.is_some_and(|id| id != self.uid && id != self.euid) {
            return Err(Errno::EPERM);
        }
        let mut new = self.set_user_ids(None, effective, None)?;
        new.uid = real.unwrap_or(self.uid);
        if real.is_some() || effective.is_some_and(|id| id != self.uid) {
            new.suid = new.euid;
        }
        Ok(new)
    }

    pub fn set_real_effective_gid(&self, real: Option<Gid>, effective: Option<Gid>) -> Result<Self> {
        if !self.is_root() && real.is_some_and(|id| id != self.gid && id != self.egid) {
            return Err(Errno::EPERM);
        }
        let mut new = self.set_group_ids(None, effective, None)?;
        new.gid = real.unwrap_or(self.gid);
        if real.is_some() || effective.is_some_and(|id| id != self.gid) {
            new.sgid = new.egid;
        }
        Ok(new)
    }

    /// `setgroups`: only root may change the supplementary groups.
    pub fn set_groups(&self, groups: Vec<Gid>) -> Result<Self> {
        if !self.is_root() {
            return Err(Errno::EPERM);
        }
        if groups.len() > MAX_GROUPS {
            return Err(Errno::EINVAL);
        }
        Ok(Self { groups, ..self.clone() })
    }
}

static ROOT: Once<Arc<Credentials>> = Once::new();

/// The credentials kernel threads act with.
pub fn kernel() -> Arc<Credentials> {
    loop {
        if let Some(root) = ROOT.get() {
            return root.clone();
        }
        // Another CPU may be setting it at the same moment; then this set
        // fails and the loop waits for that one to be published.
        let _ = ROOT.set(Arc::new(Credentials::root()));
    }
}

/// The running process's credentials (root for kernel threads).
pub fn current() -> Arc<Credentials> {
    match crate::task::current_process() {
        Some(process) => process.credentials.lock().clone(),
        None => kernel(),
    }
}

/// Replaces the running process's credentials with what `change` makes of
/// them.
pub fn update(change: impl FnOnce(&Credentials) -> Result<Credentials>) -> Result<()> {
    let process = super::current();
    let mut credentials = process.credentials.lock();
    let new = change(&credentials)?;
    if new != **credentials {
        *credentials = Arc::new(new);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn only_root_changes_ids_freely() {
            let user = Credentials::user(1000, 1000);
            assert_eq!(user.set_uid(0).err(), Some(Errno::EPERM));
            assert_eq!(user.set_groups(alloc::vec![10]).err(), Some(Errno::EPERM));
            let dropped = Credentials::root().set_uid(1000).unwrap();
            assert_eq!((dropped.uid, dropped.euid, dropped.suid), (1000, 1000, 1000));
            assert_eq!(dropped.set_uid(0).err(), Some(Errno::EPERM));
        }

        fn setuid_programs_can_switch_back_and_forth() {
            // A user runs a setuid-root program.
            let running = Credentials::user(1000, 1000).after_exec(0o4755, 0, 0);
            assert_eq!((running.uid, running.euid, running.suid), (1000, 0, 0));
            assert!(running.is_elevated());
            // It drops to the user temporarily, then takes root back.
            let lowered = running.set_user_ids(None, Some(1000), None).unwrap();
            assert!(!lowered.is_root());
            let raised = lowered.set_user_ids(None, Some(0), None).unwrap();
            assert!(raised.is_root());
            // setreuid(-1, uid) keeps root in the saved id only if the
            // effective id goes back to the real one.
            let stuck = running.set_real_effective_uid(Some(1000), Some(1000)).unwrap();
            assert_eq!(stuck.suid, 1000);
            // A setgid program needs group execute too.
            assert_eq!(Credentials::user(1000, 1000).after_exec(0o2755, 0, 50).egid, 50);
            assert_eq!(Credentials::user(1000, 1000).after_exec(0o2745, 0, 50).egid, 1000);
        }

        fn signals_need_the_same_user() {
            let alice = Credentials::user(1000, 1000);
            let bob = Credentials::user(1001, 1001);
            assert!(alice.may_signal(&alice));
            assert!(!alice.may_signal(&bob));
            assert!(Credentials::root().may_signal(&bob));
            // A setuid program started by alice can still be stopped by her.
            assert!(alice.may_signal(&alice.after_exec(0o4755, 0, 0)));
        }
    }
}
