/// A concurrent hashmap using a sharding strategy.
///
/// # Examples
/// ```
/// use tokio::runtime::Runtime;
/// use std::sync::Arc;
/// use whirlwind::ShardMap;
///
/// let rt = Runtime::new().unwrap();
/// let map = Arc::new(ShardMap::new());
/// rt.block_on(async {
///    map.insert("foo", "bar").await;
///    assert_eq!(map.len(), 1);
///    assert_eq!(map.contains_key(&"foo").await, true);
///    assert_eq!(map.contains_key(&"bar").await, false);
///
///    assert_eq!(map.get(&"foo").await.unwrap().value(), &"bar");
///    assert_eq!(map.remove(&"foo").await, Some("bar"));
/// });
use std::{
    hash::{BuildHasher, RandomState},
    sync::{atomic::AtomicUsize, Arc, OnceLock},
};

use crossbeam_utils::CachePadded;

use crate::{
    mapref::{MapRef, MapRefMut},
    shard::Shard,
};

struct Inner<K, V, S = RandomState> {
    shards: Box<[CachePadded<Shard<K, V>>]>,
    length: AtomicUsize,
    hasher: S,
}

impl<K, V, S> std::ops::Deref for Inner<K, V, S> {
    type Target = Box<[CachePadded<Shard<K, V>>]>;

    fn deref(&self) -> &Self::Target {
        &self.shards
    }
}

impl<K, V, S> std::ops::DerefMut for Inner<K, V, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.shards
    }
}

/// A concurrent hashmap using a sharding strategy.
///
/// # Examples
/// ```
/// use tokio::runtime::Runtime;
/// use std::sync::Arc;
/// use whirlwind::ShardMap;
///
/// let rt = Runtime::new().unwrap();
/// let map = Arc::new(ShardMap::new());
/// rt.block_on(async {
///    map.insert("foo", "bar").await;
///    assert_eq!(map.len(), 1);
///    assert_eq!(map.contains_key(&"foo").await, true);
///    assert_eq!(map.contains_key(&"bar").await, false);
///
///    assert_eq!(map.get(&"foo").await.unwrap().value(), &"bar");
///    assert_eq!(map.remove(&"foo").await, Some("bar"));
/// });
/// ```
pub struct ShardMap<K, V, S = std::hash::RandomState> {
    inner: Arc<Inner<K, V, S>>,
}

impl<K, V, H> Clone for ShardMap<K, V, H> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[inline(always)]
fn calculate_shard_count() -> usize {
    (std::thread::available_parallelism().map_or(1, usize::from) * 4).next_power_of_two()
}

#[inline(always)]
fn shard_count() -> usize {
    static SHARD_COUNT: OnceLock<usize> = OnceLock::new();
    *SHARD_COUNT.get_or_init(calculate_shard_count)
}

impl<K, V> ShardMap<K, V, RandomState>
where
    K: Eq + std::hash::Hash + 'static,
    V: 'static,
{
    pub fn new() -> Self {
        Self::with_shards(shard_count())
    }

    pub fn with_shards(shards: usize) -> Self {
        Self::with_shards_and_hasher(shards, RandomState::new())
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_capacity_and_hasher(capacity, RandomState::new())
    }

    pub fn with_shards_and_capacity(shards: usize, cap: usize) -> Self {
        Self::with_shards_and_capacity_and_hasher(shards, cap, RandomState::new())
    }
}

impl<K, V, S: BuildHasher> ShardMap<K, V, S>
where
    K: Eq + std::hash::Hash + 'static,
    V: 'static,
{
    pub fn with_hasher(hasher: S) -> Self {
        Self::with_shards_and_hasher(shard_count(), hasher)
    }

    pub fn with_capacity_and_hasher(cap: usize, hasher: S) -> Self {
        Self::with_shards_and_capacity_and_hasher(shard_count(), cap, hasher)
    }

    pub fn with_shards_and_hasher(shards: usize, hasher: S) -> Self {
        let shards = std::iter::repeat(())
            .take(shards.next_power_of_two())
            .map(|_| CachePadded::new(Shard::new()))
            .collect();

        Self {
            inner: Arc::new(Inner {
                shards,
                length: AtomicUsize::new(0),
                hasher,
            }),
        }
    }

    pub fn with_shards_and_capacity_and_hasher(shards: usize, cap: usize, hasher: S) -> Self {
        let capacity = (cap / shards + 1).next_power_of_two().min(4);
        let shards = std::iter::repeat(())
            .take(shards.next_power_of_two())
            .map(|_| CachePadded::new(Shard::with_capacity(capacity)))
            .collect();

        Self {
            inner: Arc::new(Inner {
                shards,
                length: AtomicUsize::new(0),
                hasher,
            }),
        }
    }

    #[inline(always)]
    fn shard(&self, key: &K) -> (&CachePadded<Shard<K, V>>, u64) {
        let hash = self.inner.hasher.hash_one(key);

        let k = const { (std::mem::size_of::<usize>() * 8) - 1 }
            - self.inner.len().leading_zeros() as usize;
        // Optimized version of hash % self.inner.len().
        // Works because self.inner.len() is always a power of 2.
        let shard_idx = hash as usize & ((1 << k) - 1);

        (unsafe { self.inner.get_unchecked(shard_idx) }, hash)
    }

    pub async fn insert(&self, key: K, value: V) -> Option<V> {
        let (shard, hash) = self.shard(&key);
        let mut writer = shard.write().await;

        let old = writer.entry(
            hash,
            |(k, _)| k == &key,
            |(k, _)| self.inner.hasher.hash_one(k),
        );
        match old {
            hashbrown::hash_table::Entry::Occupied(o) => {
                let (old, vacant) = o.remove();
                vacant.insert((key, value));
                Some(old.1)
            }
            hashbrown::hash_table::Entry::Vacant(v) => {
                v.insert((key, value));

                self.inner
                    .length
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                None
            }
        }
    }

    pub async fn get<'a>(&'a self, key: &'a K) -> Option<MapRef<'a, K, V>> {
        let (shard, hash) = self.shard(key);

        let reader = shard.read().await;

        reader
            .find(hash, |(k, _)| k == key)
            .map(|(k, v)| (k as *const K, v as *const V))
            .map(move |(k, v)| unsafe {
                // SAFETY: The key and value are guaranteed to be valid for the lifetime of the reader.
                MapRef::new(reader, &*k, &*v)
            })
    }

    pub async fn get_mut<'a>(&'a self, key: &'a K) -> Option<MapRefMut<'a, K, V>> {
        let (shard, hash) = self.shard(key);
        let mut writer = shard.write().await;

        writer
            .find_mut(hash, |(k, _)| k == key)
            .map(|(k, v)| (k as *const K, v as *mut V))
            .map(move |(k, v)| unsafe {
                // SAFETY: The key and value are guaranteed to be valid for the lifetime of the writer.
                MapRefMut::new(writer, &*k, &mut *v)
            })
    }

    pub async fn contains_key(&self, key: &K) -> bool {
        let (shard, hash) = self.shard(key);

        let reader = shard.read().await;

        reader.find(hash, |(k, _)| k == key).is_some()
    }

    pub async fn remove(&self, key: &K) -> Option<V> {
        let (shard, hash) = self.shard(key);

        match shard.write().await.find_entry(hash, |(k, _)| k == key) {
            Ok(v) => {
                let ((_, v), _) = v.remove();

                self.inner
                    .length
                    .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

                Some(v)
            }
            Err(_) => None,
        }
    }

    pub fn len(&self) -> usize {
        self.inner.length.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub async fn clear(&self) {
        for shard in self.inner.iter() {
            shard.write().await.clear();
        }
    }
}
