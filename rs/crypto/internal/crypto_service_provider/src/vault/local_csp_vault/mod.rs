mod basic_sig;
mod idkg;
mod multi_sig;
mod ni_dkg;
mod public_seed;
mod secret_key_store;
mod tecdsa;
#[cfg(test)]
mod test_utils;
#[cfg(test)]
mod tests;
mod threshold_sig;
mod tls;

use crate::key_id::KeyId;
use crate::secret_key_store::proto_store::ProtoSecretKeyStore;
use crate::secret_key_store::volatile_store::VolatileSecretKeyStore;
use crate::secret_key_store::{SecretKeyStore, SecretKeyStoreError};
use crate::types::CspSecretKey;
use crate::CspRwLock;
use ic_crypto_internal_logmon::metrics::CryptoMetrics;
use ic_logger::replica_logger::no_op_logger;
use ic_logger::ReplicaLogger;
use parking_lot::{RwLockReadGuard, RwLockWriteGuard};
use rand::rngs::OsRng;
use rand::{CryptoRng, Rng};
use std::sync::Arc;

/// An implementation of `CspVault`-trait that runs in-process
/// and uses local secret key stores.
///
/// # Remarks
///
/// Public methods of this struct may be called by implementers of the
/// [crate::vault::remote_csp_vault::TarpcCspVault] trait in a separate
/// thread. Panicking should therefore be avoided not to kill that thread.
pub struct LocalCspVault<R: Rng + CryptoRng, S: SecretKeyStore, C: SecretKeyStore> {
    // CSPRNG stands for cryptographically secure random number generator.
    csprng: CspRwLock<R>,
    node_secret_key_store: CspRwLock<S>,
    #[allow(dead_code)]
    canister_secret_key_store: CspRwLock<C>,
    logger: ReplicaLogger,
    metrics: Arc<CryptoMetrics>,
}

impl LocalCspVault<OsRng, ProtoSecretKeyStore, ProtoSecretKeyStore> {
    /// Creates a production-grade local CSP vault.
    ///
    /// The `node_secret_key_store` and the `canister_secret_key_store`
    /// must be ProtoSecretKeyStore using distinct protobuf files.
    pub fn new(
        node_secret_key_store: ProtoSecretKeyStore,
        canister_secret_key_store: ProtoSecretKeyStore,
        metrics: Arc<CryptoMetrics>,
        logger: ReplicaLogger,
    ) -> Self {
        if node_secret_key_store.proto_file_path() == canister_secret_key_store.proto_file_path() {
            panic!("The node secret-key-store and the canister secret-key-store must use different files")
        }
        LocalCspVault::new_with_os_rng(
            node_secret_key_store,
            canister_secret_key_store,
            metrics,
            logger,
        )
    }
}

impl<S: SecretKeyStore, C: SecretKeyStore> LocalCspVault<OsRng, S, C> {
    /// Creates a local CSP vault setting the `csprng` to use the OS Rng.
    pub fn new_with_os_rng(
        node_secret_key_store: S,
        canister_secret_key_store: C,
        metrics: Arc<CryptoMetrics>,
        logger: ReplicaLogger,
    ) -> Self {
        let csprng = OsRng::default();
        let csprng = CspRwLock::new_for_rng(csprng, Arc::clone(&metrics));
        LocalCspVault {
            csprng,
            node_secret_key_store: CspRwLock::new_for_sks(
                node_secret_key_store,
                Arc::clone(&metrics),
            ),
            canister_secret_key_store: CspRwLock::new_for_csks(
                canister_secret_key_store,
                Arc::clone(&metrics),
            ),
            logger,
            metrics,
        }
    }
}

impl<R: Rng + CryptoRng, S: SecretKeyStore> LocalCspVault<R, S, VolatileSecretKeyStore> {
    /// Creates a local CSP vault for testing.
    ///
    /// Note: This MUST NOT be used in production as the secrecy of the secret
    /// key store is not guaranteed.
    pub fn new_for_test(csprng: R, node_secret_key_store: S) -> Self {
        let metrics = Arc::new(CryptoMetrics::none());
        LocalCspVault {
            csprng: CspRwLock::new_for_rng(csprng, Arc::clone(&metrics)),
            node_secret_key_store: CspRwLock::new_for_sks(
                node_secret_key_store,
                Arc::clone(&metrics),
            ),
            canister_secret_key_store: CspRwLock::new_for_csks(
                VolatileSecretKeyStore::new(),
                Arc::clone(&metrics),
            ),
            logger: no_op_logger(),
            metrics,
        }
    }
}

// CRP-1248: inline the following methods
impl<R: Rng + CryptoRng, S: SecretKeyStore, C: SecretKeyStore> LocalCspVault<R, S, C> {
    fn rng_write_lock(&self) -> RwLockWriteGuard<'_, R> {
        self.csprng.write()
    }

    fn sks_write_lock(&self) -> RwLockWriteGuard<'_, S> {
        self.node_secret_key_store.write()
    }

    fn sks_read_lock(&self) -> RwLockReadGuard<'_, S> {
        self.node_secret_key_store.read()
    }

    fn canister_sks_write_lock(&self) -> RwLockWriteGuard<'_, C> {
        self.canister_secret_key_store.write()
    }

    fn canister_sks_read_lock(&self) -> RwLockReadGuard<'_, C> {
        self.canister_secret_key_store.read()
    }

    fn store_secret_key(
        &self,
        csp_secret_key: CspSecretKey,
        key_id: KeyId,
    ) -> Result<(), SecretKeyStoreError> {
        self.sks_write_lock().insert(key_id, csp_secret_key, None)
    }
}
