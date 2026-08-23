use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use czi_core::{
    BlockCache, BlockCacheConfig, BlockCacheError, MemorySource, RandomAccessSource, SourceError,
    SourceInfo,
};

#[test]
fn validates_configuration_and_defaults() {
    assert_eq!(BlockCacheConfig::default().block_size, 1024 * 1024);
    assert_eq!(BlockCacheConfig::default().max_bytes, 256 * 1024 * 1024);

    let source = InstrumentedSource::new(8, 7);
    assert!(matches!(
        BlockCache::new(source.clone(), BlockCacheConfig::new(0, 8)),
        Err(BlockCacheError::ZeroBlockSize)
    ));
    assert!(matches!(
        BlockCache::new(source.clone(), BlockCacheConfig::new(4, 0)),
        Err(BlockCacheError::ZeroMaxBytes)
    ));
    assert!(matches!(
        BlockCache::new(source, BlockCacheConfig::new(8, 4)),
        Err(BlockCacheError::BlockExceedsBudget { .. })
    ));
}

#[test]
fn preserves_bounds_and_empty_read_semantics() {
    let source = InstrumentedSource::new(8, 7);
    let reads = Arc::clone(&source.reads);
    let cache = BlockCache::new(source, BlockCacheConfig::new(4, 8)).expect("cache");

    let mut empty = [];
    cache.read_at(8, &mut empty).expect("empty read at EOF");
    assert_eq!(reads.load(Ordering::SeqCst), 0);

    let mut beyond = [];
    assert!(matches!(
        cache.read_at(9, &mut beyond),
        Err(SourceError::OutOfBounds {
            offset: 9,
            end: 9,
            length: 8
        })
    ));
    let mut overflow = [0];
    assert!(matches!(
        cache.read_at(u64::MAX, &mut overflow),
        Err(SourceError::RangeOverflow { .. })
    ));
    let mut out = [0xaa; 2];
    assert!(matches!(
        cache.read_at(7, &mut out),
        Err(SourceError::OutOfBounds { .. })
    ));
    assert_eq!(out, [0xaa; 2]);
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}

#[test]
fn reads_cross_blocks_and_caches_final_partial_block() {
    let source = InstrumentedSource::new(10, 7);
    let reads = Arc::clone(&source.reads);
    let cache = BlockCache::new(source, BlockCacheConfig::new(4, 8)).expect("cache");

    let mut cross = [0; 6];
    cache.read_at(2, &mut cross).expect("cross-block read");
    assert_eq!(cross, [2, 3, 4, 5, 6, 7]);
    assert_eq!(reads.load(Ordering::SeqCst), 2);

    let mut final_block = [0; 2];
    cache
        .read_at(8, &mut final_block)
        .expect("final block read");
    assert_eq!(final_block, [8, 9]);
    assert_eq!(cache.stats().resident_bytes, 6);
    cache
        .read_at(8, &mut final_block)
        .expect("cached final block read");
    assert_eq!(reads.load(Ordering::SeqCst), 3);
    assert_eq!(cache.stats().hits, 1);
}

#[test]
fn repeated_and_overlapping_reads_hit_each_block_once() {
    let source = InstrumentedSource::new(12, 7);
    let reads = Arc::clone(&source.reads);
    let cache = BlockCache::new(source, BlockCacheConfig::new(4, 12)).expect("cache");

    let mut first = [0; 5];
    cache.read_at(1, &mut first).expect("first read");
    let mut overlap = [0; 4];
    cache.read_at(2, &mut overlap).expect("overlapping read");
    assert_eq!(first, [1, 2, 3, 4, 5]);
    assert_eq!(overlap, [2, 3, 4, 5]);
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    assert_eq!(cache.stats().misses, 2);
    assert_eq!(cache.stats().hits, 2);
}

#[test]
fn evicts_ready_blocks_by_lru_and_refetches_them() {
    let source = InstrumentedSource::new(12, 7);
    let reads = Arc::clone(&source.reads);
    let cache = BlockCache::new(source, BlockCacheConfig::new(4, 8)).expect("cache");
    let mut block = [0; 4];

    cache.read_at(0, &mut block).expect("block zero");
    cache.read_at(4, &mut block).expect("block one");
    cache.read_at(0, &mut block).expect("refresh block zero");
    cache.read_at(8, &mut block).expect("evict block one");
    assert_eq!(cache.stats().resident_bytes, 8);
    assert_eq!(cache.stats().evictions, 1);
    cache.read_at(4, &mut block).expect("refetch block one");

    assert_eq!(reads.load(Ordering::SeqCst), 4);
    assert_eq!(cache.stats().evictions, 2);
}

#[test]
fn failed_loads_are_not_cached_and_can_recover() {
    let source = InstrumentedSource::new(8, 7);
    source.failures.store(1, Ordering::SeqCst);
    let reads = Arc::clone(&source.reads);
    let failures = Arc::clone(&source.failures);
    let cache = BlockCache::new(source, BlockCacheConfig::new(4, 4)).expect("cache");
    let mut bytes = [0; 4];

    assert!(matches!(
        cache.read_at(0, &mut bytes),
        Err(SourceError::Io(_))
    ));
    assert_eq!(cache.stats().resident_bytes, 0);
    cache.read_at(0, &mut bytes).expect("retry");
    assert_eq!(bytes, [0, 1, 2, 3]);
    assert_eq!(reads.load(Ordering::SeqCst), 2);

    failures.store(1, Ordering::SeqCst);
    assert!(matches!(
        cache.read_at(4, &mut bytes),
        Err(SourceError::Io(_))
    ));
    cache.read_at(4, &mut bytes).expect("retry failed block");
    assert_eq!(bytes, [4, 5, 6, 7]);
    assert_eq!(reads.load(Ordering::SeqCst), 4);
}

#[test]
fn concurrent_readers_single_flight_one_block_load() {
    let mut source = InstrumentedSource::new(8, 7);
    let gate = Arc::new(Gate::default());
    source.gate = Some(Arc::clone(&gate));
    let reads = Arc::clone(&source.reads);
    let cache = Arc::new(BlockCache::new(source, BlockCacheConfig::new(4, 8)).expect("cache"));

    let first_cache = Arc::clone(&cache);
    let first = thread::spawn(move || {
        let mut bytes = [0; 4];
        first_cache.read_at(0, &mut bytes).map(|()| bytes)
    });
    gate.wait_until_started();

    let second_cache = Arc::clone(&cache);
    let second = thread::spawn(move || {
        let mut bytes = [0; 4];
        second_cache.read_at(0, &mut bytes).map(|()| bytes)
    });
    thread::sleep(Duration::from_millis(20));
    gate.release();

    assert_eq!(
        first.join().expect("first thread").expect("first read"),
        [0, 1, 2, 3]
    );
    assert_eq!(
        second.join().expect("second thread").expect("second read"),
        [0, 1, 2, 3]
    );
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert_eq!(cache.stats().misses, 1);
    assert_eq!(cache.stats().hits, 1);
}

#[test]
fn distinct_block_misses_load_concurrently_when_budget_allows() {
    let mut source = InstrumentedSource::new(8, 7);
    let gate = Arc::new(Gate::default());
    source.gate = Some(Arc::clone(&gate));
    let reads = Arc::clone(&source.reads);
    let peak_active_bytes = Arc::clone(&source.peak_active_bytes);
    let cache = Arc::new(BlockCache::new(source, BlockCacheConfig::new(4, 8)).expect("cache"));

    let readers = [0_u64, 4].map(|offset| {
        let cache = Arc::clone(&cache);
        thread::spawn(move || {
            let mut bytes = [0; 4];
            cache.read_at(offset, &mut bytes).map(|()| bytes)
        })
    });
    let concurrent = gate.wait_until_started_count(2, Duration::from_secs(1));
    let observed_reads = reads.load(Ordering::SeqCst);
    let observed_peak = peak_active_bytes.load(Ordering::SeqCst);
    gate.release();
    let [first, second] = readers;

    assert_eq!(
        first.join().expect("first thread").expect("first read"),
        [0, 1, 2, 3]
    );
    assert_eq!(
        second.join().expect("second thread").expect("second read"),
        [4, 5, 6, 7]
    );
    assert!(
        concurrent,
        "distinct misses did not reach the source together"
    );
    assert_eq!(observed_reads, 2, "distinct misses serialized");
    assert_eq!(observed_peak, 8);
}

#[test]
fn distinct_misses_reserve_the_one_block_budget() {
    let mut source = InstrumentedSource::new(8, 7);
    let gate = Arc::new(Gate::default());
    source.gate = Some(Arc::clone(&gate));
    let reads = Arc::clone(&source.reads);
    let peak_active_bytes = Arc::clone(&source.peak_active_bytes);
    let second_started = Arc::new(AtomicBool::new(false));
    let cache = Arc::new(BlockCache::new(source, BlockCacheConfig::new(4, 4)).expect("cache"));

    let first_cache = Arc::clone(&cache);
    let first = thread::spawn(move || {
        let mut bytes = [0; 4];
        first_cache.read_at(0, &mut bytes)
    });
    gate.wait_until_started();

    let second_cache = Arc::clone(&cache);
    let second_started_flag = Arc::clone(&second_started);
    let second = thread::spawn(move || {
        second_started_flag.store(true, Ordering::SeqCst);
        let mut bytes = [0; 4];
        second_cache.read_at(4, &mut bytes)
    });
    while !second_started.load(Ordering::SeqCst) {
        thread::yield_now();
    }
    thread::sleep(Duration::from_millis(20));
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert_eq!(peak_active_bytes.load(Ordering::SeqCst), 4);

    gate.release();
    first.join().expect("first thread").expect("first read");
    second.join().expect("second thread").expect("second read");
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    assert_eq!(peak_active_bytes.load(Ordering::SeqCst), 4);
}

#[test]
fn snapshots_source_length_and_version() {
    let source = InstrumentedSource::new(8, 42);
    let info_calls = Arc::clone(&source.info_calls);
    let cache = BlockCache::new(source, BlockCacheConfig::new(4, 8)).expect("cache");

    assert_eq!(
        cache.info(),
        SourceInfo {
            length: 8,
            version: 42
        }
    );
    let mut bytes = [0; 1];
    cache.read_at(7, &mut bytes).expect("read");
    assert_eq!(
        cache.info(),
        SourceInfo {
            length: 8,
            version: 42
        }
    );
    assert_eq!(info_calls.load(Ordering::SeqCst), 1);
}

#[derive(Clone)]
struct InstrumentedSource {
    inner: MemorySource,
    info_value: SourceInfo,
    reads: Arc<AtomicUsize>,
    active_bytes: Arc<AtomicUsize>,
    peak_active_bytes: Arc<AtomicUsize>,
    info_calls: Arc<AtomicUsize>,
    failures: Arc<AtomicUsize>,
    gate: Option<Arc<Gate>>,
}

impl InstrumentedSource {
    fn new(length: usize, version: u64) -> Self {
        let bytes: Vec<u8> = (0..length)
            .map(|value| u8::try_from(value).expect("test byte"))
            .collect();
        Self {
            inner: MemorySource::with_version(bytes, version),
            info_value: SourceInfo {
                length: u64::try_from(length).expect("test length"),
                version,
            },
            reads: Arc::new(AtomicUsize::new(0)),
            active_bytes: Arc::new(AtomicUsize::new(0)),
            peak_active_bytes: Arc::new(AtomicUsize::new(0)),
            info_calls: Arc::new(AtomicUsize::new(0)),
            failures: Arc::new(AtomicUsize::new(0)),
            gate: None,
        }
    }
}

impl RandomAccessSource for InstrumentedSource {
    fn info(&self) -> SourceInfo {
        self.info_calls.fetch_add(1, Ordering::SeqCst);
        self.info_value
    }

    fn read_at(&self, offset: u64, destination: &mut [u8]) -> Result<(), SourceError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let active = self
            .active_bytes
            .fetch_add(destination.len(), Ordering::SeqCst)
            .saturating_add(destination.len());
        self.peak_active_bytes.fetch_max(active, Ordering::SeqCst);
        let result = if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            Err(SourceError::Io(io::Error::other("instrumented failure")))
        } else {
            if let Some(gate) = &self.gate {
                gate.wait_for_release();
            }
            self.inner.read_at(offset, destination)
        };
        self.active_bytes
            .fetch_sub(destination.len(), Ordering::SeqCst);
        result
    }
}

#[derive(Default)]
struct Gate {
    state: Mutex<(usize, bool)>,
    changed: Condvar,
}

impl Gate {
    fn wait_until_started(&self) {
        let mut state = self.state.lock().expect("gate lock");
        while state.0 == 0 {
            state = self.changed.wait(state).expect("gate wait");
        }
    }

    fn wait_until_started_count(&self, count: usize, timeout: Duration) -> bool {
        let state = self.state.lock().expect("gate lock");
        let (state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |state| state.0 < count)
            .expect("gate wait");
        state.0 >= count
    }

    fn wait_for_release(&self) {
        let mut state = self.state.lock().expect("gate lock");
        state.0 += 1;
        self.changed.notify_all();
        while !state.1 {
            state = self.changed.wait(state).expect("gate wait");
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().expect("gate lock");
        state.1 = true;
        self.changed.notify_all();
    }
}
