//! A bounded, concurrent block cache for random-access sources.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use thiserror::Error;

use super::{RandomAccessSource, SourceError, SourceInfo, check_range, checked_end};

/// The default block size used by [`BlockCacheConfig`].
pub const DEFAULT_BLOCK_SIZE: u64 = 1024 * 1024;
/// The default resident-byte budget used by [`BlockCacheConfig`].
pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Configuration for a [`BlockCache`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockCacheConfig {
    /// The size of each cache block in bytes.
    pub block_size: u64,
    /// The maximum number of bytes held by ready cache blocks.
    pub max_bytes: u64,
}

impl BlockCacheConfig {
    /// Create a cache configuration.
    #[must_use]
    pub const fn new(block_size: u64, max_bytes: u64) -> Self {
        Self {
            block_size,
            max_bytes,
        }
    }
}

impl Default for BlockCacheConfig {
    fn default() -> Self {
        Self::new(DEFAULT_BLOCK_SIZE, DEFAULT_MAX_BYTES)
    }
}

/// Errors found while constructing a [`BlockCache`].
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum BlockCacheError {
    /// The configured block size is zero.
    #[error("block size must be nonzero")]
    ZeroBlockSize,
    /// The configured byte budget is zero.
    #[error("cache byte budget must be nonzero")]
    ZeroMaxBytes,
    /// A single block cannot fit in the cache budget.
    #[error("block size {block_size} exceeds cache budget {max_bytes}")]
    BlockExceedsBudget { block_size: u64, max_bytes: u64 },
    /// The block size cannot be represented by this platform's allocation type.
    #[error("block size {block_size} cannot be represented on this platform")]
    BlockSizeConversion { block_size: u64 },
    /// The cache budget cannot be represented by this platform's size type.
    #[error("cache budget {max_bytes} cannot be represented on this platform")]
    MaxBytesConversion { max_bytes: u64 },
}

/// Counters and resident-size information for a [`BlockCache`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Ready-block lookups.
    pub hits: u64,
    /// Missing-block load attempts.
    pub misses: u64,
    /// Ready blocks removed to make room for a successful load.
    pub evictions: u64,
    /// Bytes currently resident in ready cache blocks.
    pub resident_bytes: u64,
    /// Number of ready cache blocks currently resident.
    pub resident_blocks: u64,
}

#[derive(Debug)]
enum BlockState {
    Loading {
        flight_id: u64,
        reserved_bytes: usize,
        waiters: usize,
    },
    Ready {
        bytes: Arc<[u8]>,
        flight_id: u64,
        last_used: u64,
        waiters: usize,
    },
}

#[derive(Debug, Default)]
struct CacheState {
    blocks: HashMap<u64, BlockState>,
    resident_bytes: usize,
    loading_bytes: usize,
    next_flight_id: u64,
    use_counter: u64,
    stats: CacheStats,
}

/// A thread-safe byte-budgeted LRU cache over a random-access source.
///
/// The source metadata is captured when the cache is constructed. Reads are checked against
/// that snapshot, and each missing aligned block is fetched at most once at a time.
pub struct BlockCache<S: RandomAccessSource> {
    source: S,
    info: SourceInfo,
    config: BlockCacheConfig,
    block_size: usize,
    max_bytes: usize,
    state: Mutex<CacheState>,
    state_changed: Condvar,
}

impl<S: RandomAccessSource> BlockCache<S> {
    /// Construct a cache with the supplied configuration.
    ///
    /// The source's [`SourceInfo`] is read exactly once and retained by the cache.
    ///
    /// # Errors
    ///
    /// Returns [`BlockCacheError`] when a limit is zero, the block does not fit in the budget,
    /// or a limit cannot be represented by this platform.
    pub fn new(source: S, config: BlockCacheConfig) -> Result<Self, BlockCacheError> {
        if config.block_size == 0 {
            return Err(BlockCacheError::ZeroBlockSize);
        }
        if config.max_bytes == 0 {
            return Err(BlockCacheError::ZeroMaxBytes);
        }
        if config.block_size > config.max_bytes {
            return Err(BlockCacheError::BlockExceedsBudget {
                block_size: config.block_size,
                max_bytes: config.max_bytes,
            });
        }
        let block_size = usize::try_from(config.block_size).map_err(|_| {
            BlockCacheError::BlockSizeConversion {
                block_size: config.block_size,
            }
        })?;
        let max_bytes =
            usize::try_from(config.max_bytes).map_err(|_| BlockCacheError::MaxBytesConversion {
                max_bytes: config.max_bytes,
            })?;
        let info = source.info();
        Ok(Self {
            source,
            info,
            config,
            block_size,
            max_bytes,
            state: Mutex::new(CacheState::default()),
            state_changed: Condvar::new(),
        })
    }

    /// Construct a cache with the default one-mebibyte blocks and 256-MiB budget.
    ///
    /// # Errors
    ///
    /// Returns [`BlockCacheError`] if the default limits cannot be represented by this platform.
    pub fn with_defaults(source: S) -> Result<Self, BlockCacheError> {
        Self::new(source, BlockCacheConfig::default())
    }

    /// Construct a cache from block and byte limits.
    ///
    /// # Errors
    ///
    /// Returns [`BlockCacheError`] when a limit is zero, the block does not fit in the budget,
    /// or a limit cannot be represented by this platform.
    pub fn with_limits(
        source: S,
        block_size: u64,
        max_bytes: u64,
    ) -> Result<Self, BlockCacheError> {
        Self::new(source, BlockCacheConfig::new(block_size, max_bytes))
    }

    /// Return the validated configuration.
    #[must_use]
    pub const fn config(&self) -> BlockCacheConfig {
        self.config
    }

    /// Return a snapshot of cache counters and resident-byte usage.
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        let guard = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut snapshot = guard.stats;
        snapshot.resident_bytes = u64::try_from(guard.resident_bytes).unwrap_or(u64::MAX);
        snapshot.resident_blocks = guard
            .blocks
            .values()
            .filter(|block| matches!(block, BlockState::Ready { .. }))
            .count()
            .try_into()
            .unwrap_or(u64::MAX);
        snapshot
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, CacheState>, SourceError> {
        self.state
            .lock()
            .map_err(|_| SourceError::Io(io::Error::other("block cache state lock was poisoned")))
    }

    fn next_use(state: &mut CacheState) -> u64 {
        state.use_counter = state.use_counter.saturating_add(1);
        state.use_counter
    }

    fn load_block(
        &self,
        block_index: u64,
        block_start: u64,
        block_length: usize,
    ) -> Result<Arc<[u8]>, SourceError> {
        debug_assert!(block_length <= self.block_size);
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(block_length)
            .map_err(|_| SourceError::Allocation { size: block_length })?;
        bytes.resize(block_length, 0);
        self.source.read_at(block_start, &mut bytes)?;
        let bytes: Arc<[u8]> = Arc::from(bytes.into_boxed_slice());

        debug_assert_eq!(bytes.len(), block_length);
        let mut state = self.lock_state()?;
        let (flight_id, reserved_bytes, waiters) = match state.blocks.get(&block_index) {
            Some(BlockState::Loading {
                flight_id,
                reserved_bytes,
                waiters,
            }) => (*flight_id, *reserved_bytes, *waiters),
            _ => {
                return Err(SourceError::Io(io::Error::other(
                    "block cache loading state disappeared",
                )));
            }
        };
        debug_assert_eq!(reserved_bytes, bytes.len());
        let resident_bytes = state
            .resident_bytes
            .checked_add(bytes.len())
            .ok_or(SourceError::Allocation { size: bytes.len() })?;
        state.loading_bytes = state.loading_bytes.saturating_sub(reserved_bytes);
        state.resident_bytes = resident_bytes;
        let last_used = Self::next_use(&mut state);
        state.blocks.insert(
            block_index,
            BlockState::Ready {
                bytes: Arc::clone(&bytes),
                flight_id,
                last_used,
                waiters,
            },
        );
        drop(state);
        self.state_changed.notify_all();
        Ok(bytes)
    }

    fn clear_loading(&self, block_index: u64) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(BlockState::Loading { reserved_bytes, .. }) =
                state.blocks.remove(&block_index)
            {
                state.loading_bytes = state.loading_bytes.saturating_sub(reserved_bytes);
            }
            drop(state);
            self.state_changed.notify_all();
        }
    }

    fn block(
        &self,
        block_index: u64,
        block_start: u64,
        block_length: u64,
    ) -> Result<Arc<[u8]>, SourceError> {
        let block_length_usize =
            usize::try_from(block_length).map_err(|_| SourceError::LengthConversion {
                length: block_length,
            })?;
        debug_assert!(block_length_usize <= self.block_size);
        let mut state = self.lock_state()?;
        let mut waiting_for_load = None;
        loop {
            if let Some(BlockState::Ready {
                bytes, flight_id, ..
            }) = state.blocks.get(&block_index)
            {
                let bytes = Arc::clone(bytes);
                if waiting_for_load == Some(*flight_id) {
                    if let Some(BlockState::Ready { waiters, .. }) =
                        state.blocks.get_mut(&block_index)
                    {
                        *waiters = waiters.saturating_sub(1);
                    }
                    self.state_changed.notify_all();
                }
                let last_used = Self::next_use(&mut state);
                if let Some(BlockState::Ready {
                    last_used: used, ..
                }) = state.blocks.get_mut(&block_index)
                {
                    *used = last_used;
                }
                state.stats.hits = state.stats.hits.saturating_add(1);
                return Ok(bytes);
            }

            if matches!(
                state.blocks.get(&block_index),
                Some(BlockState::Loading { .. })
            ) {
                let flight_id = match state.blocks.get(&block_index) {
                    Some(BlockState::Loading { flight_id, .. }) => *flight_id,
                    _ => unreachable!("loading block disappeared while locked"),
                };
                if waiting_for_load != Some(flight_id) {
                    if let Some(BlockState::Loading { waiters, .. }) =
                        state.blocks.get_mut(&block_index)
                    {
                        *waiters = waiters.saturating_add(1);
                    }
                    waiting_for_load = Some(flight_id);
                }
                state = self.state_changed.wait(state).map_err(|_| {
                    SourceError::Io(io::Error::other("block cache state lock was poisoned"))
                })?;
                continue;
            }

            let used_bytes = state.resident_bytes.saturating_add(state.loading_bytes);
            let has_room =
                used_bytes <= self.max_bytes && block_length_usize <= self.max_bytes - used_bytes;
            if !has_room {
                if let Some(victim) = least_recently_used(&state) {
                    if let Some(BlockState::Ready { bytes, .. }) = state.blocks.remove(&victim) {
                        state.resident_bytes = state.resident_bytes.saturating_sub(bytes.len());
                        state.stats.evictions = state.stats.evictions.saturating_add(1);
                    }
                } else {
                    state = self.state_changed.wait(state).map_err(|_| {
                        SourceError::Io(io::Error::other("block cache state lock was poisoned"))
                    })?;
                }
                continue;
            }

            state.loading_bytes += block_length_usize;
            state.next_flight_id = state.next_flight_id.saturating_add(1);
            let flight_id = state.next_flight_id;
            state.blocks.insert(
                block_index,
                BlockState::Loading {
                    flight_id,
                    reserved_bytes: block_length_usize,
                    waiters: 0,
                },
            );
            state.stats.misses = state.stats.misses.saturating_add(1);
            drop(state);
            let result = self.load_block(block_index, block_start, block_length_usize);
            if result.is_err() {
                self.clear_loading(block_index);
            }
            return result;
        }
    }
}

impl<S: RandomAccessSource> RandomAccessSource for BlockCache<S> {
    fn info(&self) -> SourceInfo {
        self.info
    }

    fn read_at(&self, offset: u64, destination: &mut [u8]) -> Result<(), SourceError> {
        check_range(offset, destination.len(), self.info.length)?;
        if destination.is_empty() {
            return Ok(());
        }

        let destination_length = u64::try_from(destination.len())
            .map_err(|_| SourceError::LengthConversion { length: u64::MAX })?;
        let end = checked_end(offset, destination_length)?;
        let mut position = offset;
        let mut destination_offset = 0usize;
        while position < end {
            let block_index = position / self.config.block_size;
            let block_start = block_index.checked_mul(self.config.block_size).ok_or(
                SourceError::RangeOverflow {
                    offset: block_index,
                    size: self.config.block_size,
                },
            )?;
            let block_length = self
                .info
                .length
                .saturating_sub(block_start)
                .min(self.config.block_size);
            let block_offset = position - block_start;
            let copy_length_u64 = block_length
                .saturating_sub(block_offset)
                .min(end - position);
            let block = self.block(block_index, block_start, block_length)?;
            let block_offset =
                usize::try_from(block_offset).map_err(|_| SourceError::LengthConversion {
                    length: block_offset,
                })?;
            let copy_length =
                usize::try_from(copy_length_u64).map_err(|_| SourceError::LengthConversion {
                    length: copy_length_u64,
                })?;
            let block_end =
                block_offset
                    .checked_add(copy_length)
                    .ok_or(SourceError::RangeOverflow {
                        offset: u64::try_from(block_offset).unwrap_or(u64::MAX),
                        size: copy_length_u64,
                    })?;
            let destination_end = destination_offset
                .checked_add(copy_length)
                .ok_or(SourceError::Allocation { size: copy_length })?;
            destination[destination_offset..destination_end]
                .copy_from_slice(&block[block_offset..block_end]);
            destination_offset = destination_end;
            position = position
                .checked_add(copy_length_u64)
                .ok_or(SourceError::RangeOverflow {
                    offset: position,
                    size: copy_length_u64,
                })?;
        }
        Ok(())
    }
}

fn least_recently_used(state: &CacheState) -> Option<u64> {
    state
        .blocks
        .iter()
        .filter_map(|(index, block)| match block {
            BlockState::Ready {
                last_used,
                waiters: 0,
                ..
            } => Some((*index, *last_used)),
            BlockState::Loading { .. } | BlockState::Ready { .. } => None,
        })
        .min_by_key(|(_, last_used)| *last_used)
        .map(|(index, _)| index)
}
