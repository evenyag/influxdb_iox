//! Tombstone cache

use backoff::{Backoff, BackoffConfig};
use cache_system::{
    backend::policy::{
        lru::{LruPolicy, ResourcePool},
        remove_if::{RemoveIfHandle, RemoveIfPolicy},
        PolicyBackend,
    },
    cache::{driver::CacheDriver, metrics::CacheWithMetrics, Cache},
    loader::{metrics::MetricsLoader, FunctionLoader},
    resource_consumption::FunctionEstimator,
};
use data_types::{SequenceNumber, TableId, Tombstone};
use iox_catalog::interface::Catalog;
use iox_time::TimeProvider;
use snafu::{ResultExt, Snafu};
use std::{collections::HashMap, mem, sync::Arc};
use trace::span::Span;

use super::ram::RamSize;

const CACHE_ID: &str = "tombstone";

#[derive(Debug, Snafu)]
#[allow(missing_copy_implementations, missing_docs)]
pub enum Error {
    #[snafu(display("CatalogError refreshing tombstone cache: {}", source))]
    Catalog {
        source: iox_catalog::interface::Error,
    },
}

/// Holds decoded catalog information about a parquet file
#[derive(Debug, Clone)]
pub struct CachedTombstones {
    /// Tombstones that were cached in the catalog
    pub tombstones: Arc<Vec<Arc<Tombstone>>>,
}
impl CachedTombstones {
    fn new(tombstones: Vec<Tombstone>) -> Self {
        let tombstones: Vec<_> = tombstones.into_iter().map(Arc::new).collect();

        Self {
            tombstones: Arc::new(tombstones),
        }
    }

    fn size(&self) -> usize {
        assert_eq!(self.tombstones.len(), self.tombstones.capacity());
        mem::size_of_val(self) +
        // size of Vec
            mem::size_of_val(&self.tombstones) +
            // Size of Arcs in Vec
            (self.tombstones.capacity() * mem::size_of::<Arc<Tombstone>>()) +
            self.tombstones.iter().map(|t| t.size()).sum::<usize>()
    }

    /// return the underlying Tombestones
    pub fn to_vec(&self) -> Vec<Arc<Tombstone>> {
        self.tombstones.iter().map(Arc::clone).collect()
    }

    /// Returns the greatest tombstone sequence number stored in this cache entry
    pub(crate) fn max_tombstone_sequence_number(&self) -> Option<SequenceNumber> {
        self.tombstones.iter().map(|f| f.sequence_number).max()
    }
}

type CacheT = Box<
    dyn Cache<
        K = TableId,
        V = CachedTombstones,
        GetExtra = ((), Option<Span>),
        PeekExtra = ((), Option<Span>),
    >,
>;

/// Cache for tombstones for a particular table
#[derive(Debug)]
pub struct TombstoneCache {
    cache: CacheT,
    /// Handle that allows clearing entries for existing cache entries
    remove_if_handle: RemoveIfHandle<TableId, CachedTombstones>,
}

impl TombstoneCache {
    /// Create new empty cache.
    pub fn new(
        catalog: Arc<dyn Catalog>,
        backoff_config: BackoffConfig,
        time_provider: Arc<dyn TimeProvider>,
        metric_registry: &metric::Registry,
        ram_pool: Arc<ResourcePool<RamSize>>,
        testing: bool,
    ) -> Self {
        let loader = FunctionLoader::new(move |table_id: TableId, _extra: ()| {
            let catalog = Arc::clone(&catalog);
            let backoff_config = backoff_config.clone();

            async move {
                Backoff::new(&backoff_config)
                    .retry_all_errors("get tombstones", || async {
                        let cached_tombstone = CachedTombstones::new(
                            catalog
                                .repositories()
                                .await
                                .tombstones()
                                .list_by_table(table_id)
                                .await
                                .context(CatalogSnafu)?,
                        );

                        Ok(cached_tombstone) as std::result::Result<_, Error>
                    })
                    .await
                    .expect("retry forever")
            }
        });
        let loader = Arc::new(MetricsLoader::new(
            loader,
            CACHE_ID,
            Arc::clone(&time_provider),
            metric_registry,
            testing,
        ));

        let mut backend =
            PolicyBackend::new(Box::new(HashMap::new()), Arc::clone(&time_provider) as _);
        let (policy_constructor, remove_if_handle) =
            RemoveIfPolicy::create_constructor_and_handle(CACHE_ID, metric_registry);
        backend.add_policy(policy_constructor);
        backend.add_policy(LruPolicy::new(
            Arc::clone(&ram_pool),
            CACHE_ID,
            Arc::new(FunctionEstimator::new(
                |k: &TableId, v: &CachedTombstones| {
                    RamSize(mem::size_of_val(k) + mem::size_of_val(v) + v.size())
                },
            )),
        ));

        let cache = CacheDriver::new(loader, backend);
        let cache = Box::new(CacheWithMetrics::new(
            cache,
            CACHE_ID,
            time_provider,
            metric_registry,
        ));

        Self {
            cache,
            remove_if_handle,
        }
    }

    /// Get list of cached tombstones, by table id
    ///
    /// # Expiration
    /// Clear the tombstone cache if it doesn't contain any tombstones
    /// that have the specified `max_tombstone_sequence_number`.
    ///
    /// If `None` is passed, returns false and does not clear the cache.
    ///
    /// Returns true if the cache was cleared (it will be refreshed on
    /// the next call to get).
    ///
    /// This API is designed to be called with a response from the
    /// ingster so there is a single place were the invalidation logic
    /// is handled. An `Option` is accepted because the ingester may
    /// or may or may not have a `max_tombstone_sequence_number`.
    ///
    /// If a `max_tombstone_sequence_number` is supplied that is not in
    /// our cache, it means the ingester has written new data to the
    /// catalog and the cache is out of date.
    pub async fn get(
        &self,
        table_id: TableId,
        max_tombstone_sequence_number: Option<SequenceNumber>,
        span: Option<Span>,
    ) -> CachedTombstones {
        self.remove_if_handle
            .remove_if_and_get(
                &self.cache,
                table_id,
                |cached_file| {
                    if let Some(max_tombstone_sequence_number) = max_tombstone_sequence_number {
                        let max_cached = cached_file.max_tombstone_sequence_number();

                        if let Some(max_cached) = max_cached {
                            max_cached < max_tombstone_sequence_number
                        } else {
                            // a max sequence was provided but there were no
                            // files in the cache. Means we need to refresh
                            true
                        }
                    } else {
                        false
                    }
                },
                ((), span),
            )
            .await
    }

    /// Mark the entry for table_id as expired / needs a refresh
    #[cfg(test)]
    pub fn expire(&self, table_id: TableId) {
        self.remove_if_handle.remove_if(&table_id, |_| true);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use data_types::TombstoneId;
    use iox_tests::util::TestCatalog;

    use crate::cache::{ram::test_util::test_ram_pool, test_util::assert_histogram_metric_count};

    const METRIC_NAME: &str = "tombstone_list_by_table";

    #[tokio::test]
    async fn test_tombstones() {
        let catalog = TestCatalog::new();

        let ns = catalog.create_namespace("ns").await;
        let table1 = ns.create_table("table1").await;
        let shard1 = ns.create_shard(1).await;

        let table_and_shard = table1.with_shard(&shard1);
        let table_id = table1.table.id;

        let tombstone1 = table_and_shard.create_tombstone(7, 1, 100, "foo=1").await;

        let cache = make_cache(&catalog);
        let cached_tombstones = cache.get(table_id, None, None).await.to_vec();

        assert_eq!(cached_tombstones.len(), 1);
        assert_eq!(cached_tombstones[0].as_ref(), &tombstone1.tombstone);

        // validate a second request doens't result in a catalog request
        assert_histogram_metric_count(&catalog.metric_registry, METRIC_NAME, 1);
        cache.get(table_id, None, None).await;
        assert_histogram_metric_count(&catalog.metric_registry, METRIC_NAME, 1);
    }

    #[tokio::test]
    async fn test_multiple_tables() {
        let catalog = TestCatalog::new();

        let ns = catalog.create_namespace("ns").await;
        let table1 = ns.create_table("table1").await;
        let shard1 = ns.create_shard(1).await;
        let table_and_shard1 = table1.with_shard(&shard1);
        let table_id1 = table1.table.id;
        let tombstone1 = table_and_shard1.create_tombstone(8, 1, 100, "foo=1").await;

        let cache = make_cache(&catalog);

        let table2 = ns.create_table("table2").await;
        let shard2 = ns.create_shard(2).await;
        let table_and_shard2 = table2.with_shard(&shard2);
        let table_id2 = table2.table.id;
        let tombstone2 = table_and_shard2.create_tombstone(8, 1, 100, "foo=1").await;

        let cached_tombstones = cache.get(table_id1, None, None).await.to_vec();
        assert_eq!(cached_tombstones.len(), 1);
        assert_eq!(cached_tombstones[0].as_ref(), &tombstone1.tombstone);

        let cached_tombstones = cache.get(table_id2, None, None).await.to_vec();
        assert_eq!(cached_tombstones.len(), 1);
        assert_eq!(cached_tombstones[0].as_ref(), &tombstone2.tombstone);
    }

    #[tokio::test]
    async fn test_size() {
        let catalog = TestCatalog::new();

        let ns = catalog.create_namespace("ns").await;
        let table1 = ns.create_table("table1").await;
        let shard1 = ns.create_shard(1).await;

        let table_and_shard = table1.with_shard(&shard1);
        let table_id = table1.table.id;

        let cache = make_cache(&catalog);

        let single_tombstone_size = 101;
        let two_tombstone_size = 186;
        assert!(single_tombstone_size < two_tombstone_size);

        // Create tombstone 1
        table_and_shard.create_tombstone(7, 1, 100, "foo=1").await;

        let cached_tombstones = cache.get(table_id, None, None).await;
        assert_eq!(cached_tombstones.to_vec().len(), 1);
        assert_eq!(cached_tombstones.size(), single_tombstone_size);

        // add a second tombstone and force the cache to find it
        table_and_shard.create_tombstone(8, 1, 100, "foo=1").await;

        cache.expire(table_id);
        let cached_tombstones = cache.get(table_id, None, None).await;
        assert_eq!(cached_tombstones.to_vec().len(), 2);
        assert_eq!(cached_tombstones.size(), two_tombstone_size);
    }

    #[tokio::test]
    async fn test_non_existent_table() {
        let catalog = TestCatalog::new();
        let cache = make_cache(&catalog);

        let made_up_table = TableId::new(1337);
        let cached_tombstones = cache.get(made_up_table, None, None).await.to_vec();
        assert!(cached_tombstones.is_empty());
    }

    #[tokio::test]
    async fn test_max_persisted_sequence_number() {
        let catalog = TestCatalog::new();

        let sequence_number_1 = SequenceNumber::new(1);
        let sequence_number_2 = SequenceNumber::new(2);
        let sequence_number_10 = SequenceNumber::new(10);

        let ns = catalog.create_namespace("ns").await;
        let table1 = ns.create_table("table1").await;
        let shard1 = ns.create_shard(1).await;

        let table_and_shard = table1.with_shard(&shard1);
        let table_id = table1.table.id;

        let cache = make_cache(&catalog);

        // Create tombstone 1
        let tombstone1 = table_and_shard
            .create_tombstone(sequence_number_1.get(), 1, 100, "foo=1")
            .await
            .tombstone
            .id;

        let tombstone2 = table_and_shard
            .create_tombstone(sequence_number_2.get(), 1, 100, "foo=1")
            .await
            .tombstone
            .id;
        assert_ids(
            &cache.get(table_id, None, None).await,
            &[tombstone1, tombstone2],
        );

        // simulate request with no sequence number
        // should not expire anything
        assert_histogram_metric_count(&catalog.metric_registry, METRIC_NAME, 1);
        assert_ids(
            &cache.get(table_id, None, None).await,
            &[tombstone1, tombstone2],
        );
        assert_histogram_metric_count(&catalog.metric_registry, METRIC_NAME, 1);

        // simulate request with sequence number 2
        // should not expire anything
        assert_ids(
            &cache.get(table_id, Some(sequence_number_2), None).await,
            &[tombstone1, tombstone2],
        );
        assert_histogram_metric_count(&catalog.metric_registry, METRIC_NAME, 1);

        // add a new tombstone (at sequence 10)
        let tombstone10 = table_and_shard
            .create_tombstone(sequence_number_10.get(), 1, 100, "foo=1")
            .await
            .tombstone
            .id;

        // cache is stale,
        assert_ids(
            &cache.get(table_id, None, None).await,
            &[tombstone1, tombstone2],
        );
        assert_histogram_metric_count(&catalog.metric_registry, METRIC_NAME, 1);

        // new request includes sequence 10 and causes a cache refresh
        assert_ids(
            &cache.get(table_id, Some(sequence_number_10), None).await,
            &[tombstone1, tombstone2, tombstone10],
        );
        assert_histogram_metric_count(&catalog.metric_registry, METRIC_NAME, 2);
    }

    #[tokio::test]
    async fn test_expore_empty() {
        let catalog = TestCatalog::new();
        let sequence_number_1 = SequenceNumber::new(1);
        let ns = catalog.create_namespace("ns").await;
        let table1 = ns.create_table("table1").await;
        let shard1 = ns.create_shard(1).await;

        let table_and_shard = table1.with_shard(&shard1);
        let table_id = table1.table.id;

        let cache = make_cache(&catalog);

        // no tombstones for the table, cached
        assert!(cache.get(table_id, None, None).await.tombstones.is_empty());
        assert_histogram_metric_count(&catalog.metric_registry, METRIC_NAME, 1);

        // second request to should be cached
        assert!(cache.get(table_id, None, None).await.tombstones.is_empty());
        assert_histogram_metric_count(&catalog.metric_registry, METRIC_NAME, 1);

        // calls to expire if there are no new known tombstones should not still be cached
        assert!(cache.get(table_id, None, None).await.tombstones.is_empty());
        assert_histogram_metric_count(&catalog.metric_registry, METRIC_NAME, 1);

        // Create a tombstone
        let tombstone1 = table_and_shard
            .create_tombstone(sequence_number_1.get(), 1, 100, "foo=1")
            .await
            .tombstone
            .id;

        // cache is stale
        assert!(cache.get(table_id, None, None).await.tombstones.is_empty());
        assert_histogram_metric_count(&catalog.metric_registry, METRIC_NAME, 1);

        // Now call to expire with knowledge of new tombstone, will cause a cache refresh
        assert_ids(
            &cache.get(table_id, Some(sequence_number_1), None).await,
            &[tombstone1],
        );
        assert_histogram_metric_count(&catalog.metric_registry, METRIC_NAME, 2);
    }

    fn make_cache(catalog: &Arc<TestCatalog>) -> TombstoneCache {
        TombstoneCache::new(
            catalog.catalog(),
            BackoffConfig::default(),
            catalog.time_provider(),
            &catalog.metric_registry(),
            test_ram_pool(),
            true,
        )
    }

    /// Assert that the ids in cached_tombestones match what is in `id`
    fn assert_ids(cached_tombstone: &CachedTombstones, ids: &[TombstoneId]) {
        let cached_ids: HashSet<_> = cached_tombstone.to_vec().iter().map(|t| t.id).collect();
        let ids: HashSet<_> = ids.iter().copied().collect();

        assert_eq!(cached_ids, ids)
    }
}
