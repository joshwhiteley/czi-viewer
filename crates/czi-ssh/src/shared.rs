use std::sync::{Arc, Mutex, TryLockError};

use crate::{SftpError, SftpSession};

/// A cloneable, serialized owner of one authenticated SFTP session.
///
/// Every operation holds the same mutex for its full request/response exchange. The underlying
/// OpenSSH transport is shut down only when the final clone is dropped.
#[derive(Clone)]
pub struct SharedSftpSession {
    inner: Arc<Mutex<SftpSession>>,
}

impl SharedSftpSession {
    /// Wrap an authenticated session for serialized shared use.
    #[must_use]
    pub fn new(session: SftpSession) -> Self {
        Self {
            inner: Arc::new(Mutex::new(session)),
        }
    }

    /// Run one complete SFTP operation while exclusively holding the session.
    ///
    /// # Errors
    ///
    /// Returns the operation error or [`SftpError::SessionPoisoned`] if a previous holder panicked.
    pub fn with_session<T>(
        &self,
        operation: impl FnOnce(&mut SftpSession) -> Result<T, SftpError>,
    ) -> Result<T, SftpError> {
        let mut session = self.inner.lock().map_err(|_| SftpError::SessionPoisoned)?;
        operation(&mut session)
    }

    pub(crate) fn try_with_session<T>(
        &self,
        operation: impl FnOnce(&mut SftpSession) -> Result<T, SftpError>,
    ) -> Option<Result<T, SftpError>> {
        match self.inner.try_lock() {
            Ok(mut session) => Some(operation(&mut session)),
            Err(TryLockError::WouldBlock) => None,
            Err(TryLockError::Poisoned(_)) => Some(Err(SftpError::SessionPoisoned)),
        }
    }
}

impl std::fmt::Debug for SharedSftpSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SharedSftpSession")
            .finish_non_exhaustive()
    }
}
