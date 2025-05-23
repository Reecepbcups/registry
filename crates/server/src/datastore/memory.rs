use super::{DataStore, DataStoreError};
use futures::Stream;
use indexmap::{IndexMap, IndexSet};
use std::{pin::Pin, sync::Arc};
use tokio::sync::RwLock;
use warg_crypto::{hash::AnyHash, Encode, Signable};
use warg_protocol::{
    operator,
    package::{self, PackageEntry},
    registry::{
        LogId, LogLeaf, PackageName, RecordId, RegistryIndex, RegistryLen, TimestampedCheckpoint,
    },
    ProtoEnvelope, PublishedProtoEnvelope, SerdeEnvelope,
};

struct Entry<R> {
    registry_index: RegistryIndex,
    record_content: ProtoEnvelope<R>,
}

struct Log<S, R> {
    state: S,
    entries: Vec<Entry<R>>,
}

impl<S, R> Default for Log<S, R>
where
    S: Default,
{
    fn default() -> Self {
        Self {
            state: S::default(),
            entries: Vec::new(),
        }
    }
}

struct Record {
    /// Index in the log's entries.
    index: usize,
    /// Index in the registry's log.
    registry_index: RegistryIndex,
}

enum PendingRecord {
    Operator {
        record: Option<ProtoEnvelope<operator::OperatorRecord>>,
    },
    Package {
        record: Option<ProtoEnvelope<package::PackageRecord>>,
        missing: IndexSet<AnyHash>,
    },
}

enum RejectedRecord {
    Operator {
        record: ProtoEnvelope<operator::OperatorRecord>,
        reason: String,
    },
    Package {
        record: ProtoEnvelope<package::PackageRecord>,
        reason: String,
    },
}

enum RecordStatus {
    Pending(PendingRecord),
    Rejected(RejectedRecord),
    Validated(Record),
}

#[derive(Default)]
struct State {
    operators: IndexMap<LogId, Log<operator::LogState, operator::OperatorRecord>>,
    packages: IndexMap<LogId, Log<package::LogState, package::PackageRecord>>,
    package_names: IndexMap<LogId, Option<PackageName>>,
    checkpoints: IndexMap<RegistryLen, SerdeEnvelope<TimestampedCheckpoint>>,
    records: IndexMap<LogId, IndexMap<RecordId, RecordStatus>>,
    log_leafs: IndexMap<RegistryIndex, LogLeaf>,
}

/// Represents an in-memory data store.
///
/// Data is not persisted between restarts of the server.
///
/// Note: this is mainly used for testing, so it is not very efficient as
/// it shares a single RwLock for all operations.
pub struct MemoryDataStore(Arc<RwLock<State>>);

impl MemoryDataStore {
    pub fn new() -> Self {
        Self(Arc::new(RwLock::new(State::default())))
    }
}

impl Default for MemoryDataStore {
    fn default() -> Self {
        Self::new()
    }
}

#[axum::async_trait]
impl DataStore for MemoryDataStore {
    async fn get_all_checkpoints(
        &self,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<TimestampedCheckpoint, DataStoreError>> + Send>>,
        DataStoreError,
    > {
        Ok(Box::pin(futures::stream::empty()))
    }

    async fn get_all_validated_records(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<LogLeaf, DataStoreError>> + Send>>, DataStoreError>
    {
        Ok(Box::pin(futures::stream::empty()))
    }

    async fn get_log_leafs_starting_with_registry_index(
        &self,
        starting_index: RegistryIndex,
        limit: usize,
    ) -> Result<Vec<(RegistryIndex, LogLeaf)>, DataStoreError> {
        let state = self.0.read().await;

        let limit = if limit > state.log_leafs.len() - starting_index {
            state.log_leafs.len() - starting_index
        } else {
            limit
        };

        let mut leafs = Vec::with_capacity(limit);
        for entry in starting_index..starting_index + limit {
            match state.log_leafs.get(&entry) {
                Some(log_leaf) => leafs.push((entry, log_leaf.clone())),
                None => break,
            }
        }

        Ok(leafs)
    }

    async fn get_log_leafs_with_registry_index(
        &self,
        entries: &[RegistryIndex],
    ) -> Result<Vec<LogLeaf>, DataStoreError> {
        let state = self.0.read().await;

        let mut leafs = Vec::with_capacity(entries.len());
        for entry in entries {
            match state.log_leafs.get(entry) {
                Some(log_leaf) => leafs.push(log_leaf.clone()),
                None => return Err(DataStoreError::LogLeafNotFound(*entry)),
            }
        }

        Ok(leafs)
    }

    async fn get_package_names(
        &self,
        log_ids: &[LogId],
    ) -> Result<IndexMap<LogId, Option<PackageName>>, DataStoreError> {
        let state = self.0.read().await;

        log_ids
            .iter()
            .map(|log_id| {
                if let Some(opt_package_name) = state.package_names.get(log_id) {
                    Ok((log_id.clone(), opt_package_name.clone()))
                } else {
                    Err(DataStoreError::LogNotFound(log_id.clone()))
                }
            })
            .collect::<Result<IndexMap<LogId, Option<PackageName>>, _>>()
    }

    async fn store_operator_record(
        &self,
        log_id: &LogId,
        record_id: &RecordId,
        record: &ProtoEnvelope<operator::OperatorRecord>,
    ) -> Result<(), DataStoreError> {
        let mut state = self.0.write().await;
        let prev = state.records.entry(log_id.clone()).or_default().insert(
            record_id.clone(),
            RecordStatus::Pending(PendingRecord::Operator {
                record: Some(record.clone()),
            }),
        );

        assert!(prev.is_none());
        Ok(())
    }

    async fn reject_operator_record(
        &self,
        log_id: &LogId,
        record_id: &RecordId,
        reason: &str,
    ) -> Result<(), DataStoreError> {
        let mut state = self.0.write().await;

        let status = state
            .records
            .get_mut(log_id)
            .ok_or_else(|| DataStoreError::LogNotFound(log_id.clone()))?
            .get_mut(record_id)
            .ok_or_else(|| DataStoreError::RecordNotFound(record_id.clone()))?;

        let record = match status {
            RecordStatus::Pending(PendingRecord::Operator { record }) => record.take().unwrap(),
            _ => return Err(DataStoreError::RecordNotPending(record_id.clone())),
        };

        *status = RecordStatus::Rejected(RejectedRecord::Operator {
            record,
            reason: reason.to_string(),
        });

        Ok(())
    }

    async fn commit_operator_record(
        &self,
        log_id: &LogId,
        record_id: &RecordId,
        registry_index: RegistryIndex,
    ) -> Result<(), DataStoreError> {
        let mut state = self.0.write().await;

        let State {
            operators,
            records,
            log_leafs,
            ..
        } = &mut *state;

        let status = records
            .get_mut(log_id)
            .ok_or_else(|| DataStoreError::LogNotFound(log_id.clone()))?
            .get_mut(record_id)
            .ok_or_else(|| DataStoreError::RecordNotFound(record_id.clone()))?;

        match status {
            RecordStatus::Pending(PendingRecord::Operator { record }) => {
                let record = record.take().unwrap();
                let log = operators.entry(log_id.clone()).or_default();
                match log
                    .state
                    .clone()
                    .validate(&record)
                    .map_err(DataStoreError::from)
                {
                    Ok(s) => {
                        log.state = s;
                        let index = log.entries.len();
                        log.entries.push(Entry {
                            registry_index,
                            record_content: record,
                        });
                        *status = RecordStatus::Validated(Record {
                            index,
                            registry_index,
                        });
                        log_leafs.insert(
                            registry_index,
                            LogLeaf {
                                log_id: log_id.clone(),
                                record_id: record_id.clone(),
                            },
                        );
                        Ok(())
                    }
                    Err(e) => {
                        *status = RecordStatus::Rejected(RejectedRecord::Operator {
                            record,
                            reason: e.to_string(),
                        });
                        Err(e)
                    }
                }
            }
            _ => Err(DataStoreError::RecordNotPending(record_id.clone())),
        }
    }

    async fn store_package_record(
        &self,
        log_id: &LogId,
        package_name: &PackageName,
        record_id: &RecordId,
        record: &ProtoEnvelope<package::PackageRecord>,
        missing: &IndexSet<&AnyHash>,
    ) -> Result<(), DataStoreError> {
        // Ensure the set of missing hashes is a subset of the record contents.
        debug_assert!({
            use warg_protocol::Record;
            let contents = record.as_ref().contents();
            missing.is_subset(&contents)
        });

        let mut state = self.0.write().await;
        let prev = state.records.entry(log_id.clone()).or_default().insert(
            record_id.clone(),
            RecordStatus::Pending(PendingRecord::Package {
                record: Some(record.clone()),
                missing: missing.iter().map(|&d| d.clone()).collect(),
            }),
        );
        state
            .package_names
            .insert(log_id.clone(), Some(package_name.clone()));

        assert!(prev.is_none());
        Ok(())
    }

    async fn reject_package_record(
        &self,
        log_id: &LogId,
        record_id: &RecordId,
        reason: &str,
    ) -> Result<(), DataStoreError> {
        let mut state = self.0.write().await;

        let status = state
            .records
            .get_mut(log_id)
            .ok_or_else(|| DataStoreError::LogNotFound(log_id.clone()))?
            .get_mut(record_id)
            .ok_or_else(|| DataStoreError::RecordNotFound(record_id.clone()))?;

        let record = match status {
            RecordStatus::Pending(PendingRecord::Package { record, .. }) => record.take().unwrap(),
            _ => return Err(DataStoreError::RecordNotPending(record_id.clone())),
        };

        *status = RecordStatus::Rejected(RejectedRecord::Package {
            record,
            reason: reason.to_string(),
        });

        Ok(())
    }

    async fn commit_package_record(
        &self,
        log_id: &LogId,
        record_id: &RecordId,
        registry_index: RegistryIndex,
    ) -> Result<(), DataStoreError> {
        let mut state = self.0.write().await;

        let State {
            packages,
            records,
            log_leafs,
            ..
        } = &mut *state;

        let status = records
            .get_mut(log_id)
            .ok_or_else(|| DataStoreError::LogNotFound(log_id.clone()))?
            .get_mut(record_id)
            .ok_or_else(|| DataStoreError::RecordNotFound(record_id.clone()))?;

        match status {
            RecordStatus::Pending(PendingRecord::Package { record, .. }) => {
                let record = record.take().unwrap();
                let log = packages.entry(log_id.clone()).or_default();
                match log
                    .state
                    .clone()
                    .validate(&record)
                    .map_err(DataStoreError::from)
                {
                    Ok(state) => {
                        log.state = state;
                        let index = log.entries.len();
                        log.entries.push(Entry {
                            registry_index,
                            record_content: record,
                        });
                        *status = RecordStatus::Validated(Record {
                            index,
                            registry_index,
                        });
                        log_leafs.insert(
                            registry_index,
                            LogLeaf {
                                log_id: log_id.clone(),
                                record_id: record_id.clone(),
                            },
                        );
                        Ok(())
                    }
                    Err(e) => {
                        *status = RecordStatus::Rejected(RejectedRecord::Package {
                            record,
                            reason: e.to_string(),
                        });
                        Err(e)
                    }
                }
            }
            _ => Err(DataStoreError::RecordNotPending(record_id.clone())),
        }
    }

    async fn is_content_missing(
        &self,
        log_id: &LogId,
        record_id: &RecordId,
        digest: &AnyHash,
    ) -> Result<bool, DataStoreError> {
        let state = self.0.read().await;
        let log = state
            .records
            .get(log_id)
            .ok_or_else(|| DataStoreError::LogNotFound(log_id.clone()))?;

        let status = log
            .get(record_id)
            .ok_or_else(|| DataStoreError::RecordNotFound(record_id.clone()))?;

        match status {
            RecordStatus::Pending(PendingRecord::Operator { .. }) => {
                // Operator records have no content
                Ok(false)
            }
            RecordStatus::Pending(PendingRecord::Package { missing, .. }) => {
                Ok(missing.contains(digest))
            }
            _ => return Err(DataStoreError::RecordNotPending(record_id.clone())),
        }
    }

    async fn set_content_present(
        &self,
        log_id: &LogId,
        record_id: &RecordId,
        digest: &AnyHash,
    ) -> Result<bool, DataStoreError> {
        let mut state = self.0.write().await;
        let log = state
            .records
            .get_mut(log_id)
            .ok_or_else(|| DataStoreError::LogNotFound(log_id.clone()))?;

        let status = log
            .get_mut(record_id)
            .ok_or_else(|| DataStoreError::RecordNotFound(record_id.clone()))?;

        match status {
            RecordStatus::Pending(PendingRecord::Operator { .. }) => {
                // Operator records have no content, so conceptually already present
                Ok(false)
            }
            RecordStatus::Pending(PendingRecord::Package { missing, .. }) => {
                if missing.is_empty() {
                    return Ok(false);
                }

                // Return true if this was the last missing content
                missing.swap_remove(digest);
                Ok(missing.is_empty())
            }
            _ => return Err(DataStoreError::RecordNotPending(record_id.clone())),
        }
    }

    async fn store_checkpoint(
        &self,
        _checkpoint_id: &AnyHash,
        ts_checkpoint: SerdeEnvelope<TimestampedCheckpoint>,
    ) -> Result<(), DataStoreError> {
        let mut state = self.0.write().await;

        state
            .checkpoints
            .insert(ts_checkpoint.as_ref().checkpoint.log_length, ts_checkpoint);

        Ok(())
    }

    async fn get_latest_checkpoint(
        &self,
    ) -> Result<SerdeEnvelope<TimestampedCheckpoint>, DataStoreError> {
        let state = self.0.read().await;
        let checkpoint = state.checkpoints.values().last().unwrap();
        Ok(checkpoint.clone())
    }

    async fn get_checkpoint(
        &self,
        log_length: RegistryLen,
    ) -> Result<SerdeEnvelope<TimestampedCheckpoint>, DataStoreError> {
        let state = self.0.read().await;
        let checkpoint = state
            .checkpoints
            .get(&log_length)
            .ok_or_else(|| DataStoreError::CheckpointNotFound(log_length))?;
        Ok(checkpoint.clone())
    }

    async fn get_operator_records(
        &self,
        log_id: &LogId,
        registry_log_length: RegistryLen,
        since: Option<&RecordId>,
        limit: u16,
    ) -> Result<Vec<PublishedProtoEnvelope<operator::OperatorRecord>>, DataStoreError> {
        let state = self.0.read().await;

        let log = state
            .operators
            .get(log_id)
            .ok_or_else(|| DataStoreError::LogNotFound(log_id.clone()))?;

        if !state.checkpoints.contains_key(&registry_log_length) {
            return Err(DataStoreError::CheckpointNotFound(registry_log_length));
        };

        let start_log_idx = match since {
            Some(since) => {
                match state.records.get(log_id).and_then(|records| records.get(since)) {
                    Some(RecordStatus::Validated(record)) => record.index + 1,
                    // If record not found or not in validated state, start from beginning
                    _ => 0,
                }
            },
            None => 0,
        };

        Ok(log
            .entries
            .iter()
            .skip(start_log_idx)
            .take_while(|entry| entry.registry_index < registry_log_length)
            .map(|entry| PublishedProtoEnvelope {
                envelope: entry.record_content.clone(),
                registry_index: entry.registry_index,
            })
            .take(limit as usize)
            .collect())
    }

    async fn get_package_records(
        &self,
        log_id: &LogId,
        registry_log_length: RegistryLen,
        since: Option<&RecordId>,
        limit: u16,
    ) -> Result<Vec<PublishedProtoEnvelope<package::PackageRecord>>, DataStoreError> {
        let state = self.0.read().await;

        let log = state
            .packages
            .get(log_id)
            .ok_or_else(|| DataStoreError::LogNotFound(log_id.clone()))?;

        if !state.checkpoints.contains_key(&registry_log_length) {
            return Err(DataStoreError::CheckpointNotFound(registry_log_length));
        };

        let start_log_idx = match since {
            Some(since) => {
                match state.records.get(log_id).and_then(|records| records.get(since)) {
                    Some(RecordStatus::Validated(record)) => record.index + 1,
                    // If record not found or not in validated state, start from beginning
                    _ => 0,
                }
            },
            None => 0,
        };

        Ok(log
            .entries
            .iter()
            .skip(start_log_idx)
            .take_while(|entry| entry.registry_index < registry_log_length)
            .map(|entry| PublishedProtoEnvelope {
                envelope: entry.record_content.clone(),
                registry_index: entry.registry_index,
            })
            .take(limit as usize)
            .collect())
    }

    async fn get_operator_record(
        &self,
        log_id: &LogId,
        record_id: &RecordId,
    ) -> Result<super::Record<operator::OperatorRecord>, DataStoreError> {
        let state = self.0.read().await;
        let status = state
            .records
            .get(log_id)
            .ok_or_else(|| DataStoreError::LogNotFound(log_id.clone()))?
            .get(record_id)
            .ok_or_else(|| DataStoreError::RecordNotFound(record_id.clone()))?;

        let (status, envelope, registry_index) = match status {
            RecordStatus::Pending(PendingRecord::Operator { record, .. }) => {
                (super::RecordStatus::Pending, record.clone().unwrap(), None)
            }
            RecordStatus::Rejected(RejectedRecord::Operator { record, reason }) => (
                super::RecordStatus::Rejected(reason.into()),
                record.clone(),
                None,
            ),
            RecordStatus::Validated(r) => {
                let log = state
                    .operators
                    .get(log_id)
                    .ok_or_else(|| DataStoreError::LogNotFound(log_id.clone()))?;

                let published_length = state
                    .checkpoints
                    .last()
                    .map(|(_, c)| c.as_ref().checkpoint.log_length)
                    .unwrap_or_default();

                (
                    if r.registry_index < published_length {
                        super::RecordStatus::Published
                    } else {
                        super::RecordStatus::Validated
                    },
                    log.entries[r.index].record_content.clone(),
                    Some(r.registry_index),
                )
            }
            _ => return Err(DataStoreError::RecordNotFound(record_id.clone())),
        };

        Ok(super::Record {
            status,
            envelope,
            registry_index,
        })
    }

    async fn get_package_record(
        &self,
        log_id: &LogId,
        record_id: &RecordId,
    ) -> Result<super::Record<package::PackageRecord>, DataStoreError> {
        let state = self.0.read().await;
        let status = state
            .records
            .get(log_id)
            .ok_or_else(|| DataStoreError::LogNotFound(log_id.clone()))?
            .get(record_id)
            .ok_or_else(|| DataStoreError::RecordNotFound(record_id.clone()))?;

        let (status, envelope, registry_index) = match status {
            RecordStatus::Pending(PendingRecord::Package { record, .. }) => {
                (super::RecordStatus::Pending, record.clone().unwrap(), None)
            }
            RecordStatus::Rejected(RejectedRecord::Package { record, reason }) => (
                super::RecordStatus::Rejected(reason.into()),
                record.clone(),
                None,
            ),
            RecordStatus::Validated(r) => {
                let log = state
                    .packages
                    .get(log_id)
                    .ok_or_else(|| DataStoreError::LogNotFound(log_id.clone()))?;

                let published_length = state
                    .checkpoints
                    .last()
                    .map(|(_, c)| c.as_ref().checkpoint.log_length)
                    .unwrap_or_default();

                (
                    if r.registry_index < published_length {
                        super::RecordStatus::Published
                    } else {
                        super::RecordStatus::Validated
                    },
                    log.entries[r.index].record_content.clone(),
                    Some(r.registry_index),
                )
            }
            _ => return Err(DataStoreError::RecordNotFound(record_id.clone())),
        };

        Ok(super::Record {
            status,
            envelope,
            registry_index,
        })
    }

    async fn verify_package_record_signature(
        &self,
        log_id: &LogId,
        record: &ProtoEnvelope<package::PackageRecord>,
    ) -> Result<(), DataStoreError> {
        let state = self.0.read().await;
        let key = match state
            .packages
            .get(log_id)
            .and_then(|log| log.state.public_key(record.key_id()))
        {
            Some(key) => Some(key),
            None => match record.as_ref().entries.first() {
                Some(PackageEntry::Init { key, .. }) => Some(key),
                _ => return Err(DataStoreError::UnknownKey(record.key_id().clone())),
            },
        }
        .ok_or_else(|| DataStoreError::UnknownKey(record.key_id().clone()))?;

        package::PackageRecord::verify(key, record.content_bytes(), record.signature())
            .map_err(|_| DataStoreError::SignatureVerificationFailed(record.signature().clone()))
    }

    async fn verify_can_publish_package(
        &self,
        operator_log_id: &LogId,
        package_name: &PackageName,
    ) -> Result<(), DataStoreError> {
        let state = self.0.read().await;

        // verify namespace is defined and not imported
        match state
            .operators
            .get(operator_log_id)
            .ok_or_else(|| DataStoreError::LogNotFound(operator_log_id.clone()))?
            .state
            .namespace_state(package_name.namespace())
        {
            Some(state) => match state {
                operator::NamespaceState::Defined => {}
                operator::NamespaceState::Imported { .. } => {
                    return Err(DataStoreError::PackageNamespaceImported(
                        package_name.namespace().to_string(),
                    ))
                }
            },
            None => {
                return Err(DataStoreError::PackageNamespaceNotDefined(
                    package_name.namespace().to_string(),
                ))
            }
        }

        Ok(())
    }

    async fn verify_timestamped_checkpoint_signature(
        &self,
        operator_log_id: &LogId,
        ts_checkpoint: &SerdeEnvelope<TimestampedCheckpoint>,
    ) -> Result<(), DataStoreError> {
        let state = self.0.read().await;

        let state = &state
            .operators
            .get(operator_log_id)
            .ok_or_else(|| DataStoreError::LogNotFound(operator_log_id.clone()))?
            .state;

        TimestampedCheckpoint::verify(
            state
                .public_key(ts_checkpoint.key_id())
                .ok_or(DataStoreError::UnknownKey(ts_checkpoint.key_id().clone()))?,
            &ts_checkpoint.as_ref().encode(),
            ts_checkpoint.signature(),
        )
        .or(Err(DataStoreError::SignatureVerificationFailed(
            ts_checkpoint.signature().clone(),
        )))?;

        if !state.key_has_permission_to_sign_checkpoints(ts_checkpoint.key_id()) {
            return Err(DataStoreError::KeyUnauthorized(
                ts_checkpoint.key_id().clone(),
            ));
        }

        Ok(())
    }

    #[cfg(feature = "debug")]
    async fn debug_list_package_names(&self) -> anyhow::Result<Vec<PackageName>> {
        let state = self.0.read().await;
        Ok(state
            .package_names
            .values()
            .filter_map(|opt_package_name| opt_package_name.clone())
            .collect())
    }
}
