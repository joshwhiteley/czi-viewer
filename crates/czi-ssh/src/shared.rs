use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use crate::{EmbeddedSshCancellation, SftpError, SftpSession};

/// A cloneable, serialized owner of one authenticated SFTP session.
///
/// Every operation holds the same mutex for its full request/response exchange. Deferred file
/// handles are closed before and after each operation. The underlying OpenSSH transport is shut
/// down only when the final clone is dropped.
#[derive(Clone)]
pub struct SharedSftpSession {
    inner: Arc<SharedSftpSessionInner>,
}

struct SharedSftpSessionInner {
    session: Mutex<SftpSession>,
    deferred_close_tx: Sender<Vec<u8>>,
    deferred_close_rx: Mutex<Receiver<Vec<u8>>>,
    embedded_cancellation: Option<EmbeddedConnectionCancellation>,
}

struct EmbeddedConnectionCancellation {
    generation: u64,
    cancellation: EmbeddedSshCancellation,
}

impl SharedSftpSession {
    /// Wrap an authenticated session for serialized shared use.
    #[must_use]
    pub fn new(session: SftpSession) -> Self {
        Self::build(session, None)
    }

    /// Wrap an authenticated embedded session with its generation-scoped cancellation handle.
    #[must_use]
    pub fn new_embedded(
        session: SftpSession,
        generation: u64,
        cancellation: EmbeddedSshCancellation,
    ) -> Self {
        Self::build(
            session,
            Some(EmbeddedConnectionCancellation {
                generation,
                cancellation,
            }),
        )
    }

    fn build(
        session: SftpSession,
        embedded_cancellation: Option<EmbeddedConnectionCancellation>,
    ) -> Self {
        let (deferred_close_tx, deferred_close_rx) = mpsc::channel();
        Self {
            inner: Arc::new(SharedSftpSessionInner {
                session: Mutex::new(session),
                deferred_close_tx,
                deferred_close_rx: Mutex::new(deferred_close_rx),
                embedded_cancellation,
            }),
        }
    }

    /// Queue a remote file handle for closure without waiting on an active SFTP operation.
    pub fn defer_close(&self, handle: Vec<u8>) {
        let _ = self.inner.deferred_close_tx.send(handle);
    }

    /// Terminate this embedded connection only when `generation` owns it.
    ///
    /// Returns `true` when this session carried the matching embedded cancellation handle.
    #[must_use]
    pub fn cancel_embedded_connection(&self, generation: u64) -> bool {
        let Some(connection) = self
            .inner
            .embedded_cancellation
            .as_ref()
            .filter(|connection| connection.generation == generation)
        else {
            return false;
        };
        let _ = connection.cancellation.cancel();
        true
    }

    /// Run one complete SFTP operation while exclusively holding the session.
    ///
    /// # Errors
    ///
    /// Returns deferred-close, operation, or [`SftpError::SessionPoisoned`] failures. A server
    /// STATUS error leaves the negotiated transport available for a later operation; transport,
    /// framing, and protocol failures follow the normal [`SftpSession`] invalidation policy.
    pub fn with_session<T>(
        &self,
        operation: impl FnOnce(&mut SftpSession) -> Result<T, SftpError>,
    ) -> Result<T, SftpError> {
        let mut session = self
            .inner
            .session
            .lock()
            .map_err(|_| SftpError::SessionPoisoned)?;
        self.drain_deferred_closes(&mut session)?;
        let result = operation(&mut session);
        if result.is_err() {
            self.discard_deferred_closes();
            return result;
        }
        self.drain_deferred_closes(&mut session)?;
        result
    }

    fn drain_deferred_closes(&self, session: &mut SftpSession) -> Result<(), SftpError> {
        let deferred_close_rx = self
            .inner
            .deferred_close_rx
            .lock()
            .map_err(|_| SftpError::SessionPoisoned)?;
        let handles = deferred_close_rx.try_iter().collect::<Vec<_>>();
        drop(deferred_close_rx);
        for handle in handles {
            session.close(&handle)?;
        }
        Ok(())
    }

    fn discard_deferred_closes(&self) {
        if let Ok(deferred_close_rx) = self.inner.deferred_close_rx.lock() {
            while deferred_close_rx.try_recv().is_ok() {}
        }
    }
}

impl Drop for SharedSftpSessionInner {
    fn drop(&mut self) {
        let Ok(deferred_close_rx) = self.deferred_close_rx.get_mut() else {
            return;
        };
        let Ok(session) = self.session.get_mut() else {
            return;
        };
        while let Ok(handle) = deferred_close_rx.try_recv() {
            if session.close(&handle).is_err() {
                break;
            }
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
