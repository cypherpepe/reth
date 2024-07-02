use std::{marker::PhantomData, sync::Arc};

use reth_db::database::Database;
use reth_provider::providers::StaticFileProvider;
use reth_prune_types::{PruneMode, PrunePurpose};
use reth_static_file_types::StaticFileSegment;

use crate::{segments, Segment};

/// An iterator over a mutable ring of tables, that yields next segments to prune. Advancing
/// iterator, mutates current position in ring. Returns `None` after the first cycle.
#[derive(Debug)]
pub struct SegmentIter<'a, T> {
    ring: &'a mut T,
}

impl<'a, T> Iterator for SegmentIter<'a, T>
where
    T: CycleSegments,
{
    type Item = Option<(Arc<dyn Segment<<T as CycleSegments>::Db>>, PrunePurpose)>;

    /// Returns next prunable segment in ring, or `None` if iterator has walked one cycle.
    fn next(&mut self) -> Option<Self::Item> {
        // return after one cycle
        if self.ring.is_cycle() {
            self.ring.reset_cycle();
            return None
        }

        Some(self.ring.next_segment())
    }
}

/// Cycles prunable segments.
pub trait CycleSegments {
    type Db: Database;
    type TableRef: Eq;

    /// Returns the starting position in the ring. This is needed for counting cycles in the ring.
    fn start_table(&self) -> Self::TableRef;

    /// Returns the current table in the ring. This table has not been pruned yet in the current
    /// cycle.
    fn current_table(&self) -> Self::TableRef;

    /// Returns the table corresponding to the [`Segment`] most recently returned by
    /// [`next_segment`](CycleSegments::next_segment).
    fn prev_table(&self) -> Option<Self::TableRef>;

    /// Returns the next position in the ring. This table will be pruned after the current table.
    fn peek_next_table(&self) -> Self::TableRef;

    /// Advances current position in ring.
    fn next_table(&mut self);

    /// Returns the next [`Segment`] to prune, if any entries to prune for the current table.
    /// Advances current position in ring.
    #[allow(clippy::type_complexity)]
    fn next_segment(&mut self) -> Option<(Arc<dyn Segment<Self::Db>>, PrunePurpose)>;

    /// Returns an iterator cycling once over the ring of tables. Yields an item for each table,
    /// either a segment or `None` if there is currently nothing to prune for the table. Advances
    /// the current position in the ring.
    fn next_cycle(
        &mut self,
    ) -> impl Iterator<Item = Option<(Arc<dyn Segment<Self::Db>>, PrunePurpose)>>
    where
        Self: Sized,
    {
        self.reset_cycle();

        SegmentIter { ring: self }
    }

    /// Returns an iterator over prunable segments. Unlike
    /// [`next_cycle`](CycleSegments::next_cycle), skips tables that currently have nothing to
    fn iter(&mut self) -> impl Iterator<Item = (Arc<dyn Segment<Self::Db>>, PrunePurpose)>
    where
        Self: Sized,
    {
        self.next_cycle().filter(|segment| segment.is_some()).flatten()
    }

    /// Returns `true` if the ring has completed one cycle.
    fn is_cycle(&self) -> bool {
        self.prev_table().is_some() && self.current_table() == self.start_table()
    }

    /// Resets cycle.
    fn reset_cycle(&mut self);
}

/// Opaque reference to a table.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TableRef {
    StaticFiles(StaticFileTableRef),
    Other(usize),
}

impl Default for TableRef {
    fn default() -> Self {
        Self::StaticFiles(StaticFileTableRef::default())
    }
}

/// A ring over prunable tables.
#[derive(Debug)]
pub struct TableRing<DB> {
    start: TableRef,
    current: TableRef,
    prev: Option<TableRef>,
    segments: Vec<Arc<dyn Segment<DB>>>,
    static_file_ring: StaticFileTableRing<DB>,
}

impl<DB> TableRing<DB> {
    pub fn new(
        provider: StaticFileProvider,
        start: TableRef,
        segments: Vec<Arc<dyn Segment<DB>>>,
    ) -> Result<Self, &'static str> {
        let static_file_start = match start {
            TableRef::StaticFiles(table_ref) => table_ref,
            _ => StaticFileTableRef::default(),
        };

        if let TableRef::Other(index) = start {
            if segments.is_empty() || index > segments.len() - 1 {
                return Err("segments index out of bounds")
            }
        }

        Ok(Self {
            start,
            current: start,
            prev: None,
            segments,
            static_file_ring: StaticFileTableRing::new(provider, static_file_start),
        })
    }
}

impl<DB> CycleSegments for TableRing<DB>
where
    DB: Database,
{
    type Db = <StaticFileTableRing<DB> as CycleSegments>::Db;
    type TableRef = TableRef;

    fn start_table(&self) -> Self::TableRef {
        self.start
    }

    fn current_table(&self) -> Self::TableRef {
        self.current
    }

    fn prev_table(&self) -> Option<Self::TableRef> {
        self.prev
    }

    fn peek_next_table(&self) -> Self::TableRef {
        let Self { current, static_file_ring, segments, .. } = self;

        match current {
            TableRef::StaticFiles(_) => {
                if static_file_ring.is_cycle() && !segments.is_empty() {
                    TableRef::Other(0)
                } else {
                    // static files ring nested in this ring, so is one step ahead
                    TableRef::StaticFiles(static_file_ring.current_table())
                }
            }
            TableRef::Other(index) => {
                if *index < segments.len() - 1 {
                    TableRef::Other(*index + 1)
                } else {
                    // start next cycle
                    TableRef::StaticFiles(static_file_ring.current_table())
                }
            }
        }
    }

    fn next_table(&mut self) {
        self.prev = Some(self.current);
        self.current = self.peek_next_table();
    }

    fn next_segment(&mut self) -> Option<(Arc<dyn Segment<Self::Db>>, PrunePurpose)> {
        let Self { current, segments, .. } = self;

        let segment = match current {
            TableRef::StaticFiles(_) => self.static_file_ring.next_segment(),
            TableRef::Other(index) => Some((segments[*index].clone(), PrunePurpose::User)),
        };

        self.next_table();

        segment
    }

    fn reset_cycle(&mut self) {
        self.prev = None;
        self.start = self.current;
        self.static_file_ring.reset_cycle();
    }
}

/// Opaque reference to a static file table.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum StaticFileTableRef {
    #[default]
    Headers,
    Transactions,
    Receipts,
}

/// A ring over static file tables.
///
/// Iterator that returns pre-configured segments that needs to be pruned according to the highest
/// static files for [`PruneSegment::Transactions`](reth_primitives::PruneSegment::Transactions),
/// [`PruneSegment::Headers`](reth_primitives::PruneSegment::Headers) and
/// [`PruneSegment::Receipts`](reth_primitives::PruneSegment::Receipts).
#[derive(Debug)]
pub struct StaticFileTableRing<DB> {
    provider: StaticFileProvider,
    start: StaticFileTableRef,
    current: StaticFileTableRef,
    prev: Option<StaticFileTableRef>,
    _phantom: PhantomData<DB>,
}

impl<DB> StaticFileTableRing<DB> {
    pub const fn new(provider: StaticFileProvider, start: StaticFileTableRef) -> Self {
        Self { provider, start, current: start, prev: None, _phantom: PhantomData }
    }
}

impl<DB> CycleSegments for StaticFileTableRing<DB>
where
    DB: Database,
{
    type Db = DB;
    type TableRef = StaticFileTableRef;

    fn start_table(&self) -> Self::TableRef {
        self.start
    }

    fn current_table(&self) -> Self::TableRef {
        self.current
    }

    fn prev_table(&self) -> Option<Self::TableRef> {
        self.prev
    }

    fn peek_next_table(&self) -> Self::TableRef {
        use StaticFileTableRef::{Headers, Receipts, Transactions};

        match self.current {
            Headers => Transactions,
            Transactions => Receipts,
            Receipts => Headers,
        }
    }

    fn next_table(&mut self) {
        self.prev = Some(self.current);
        self.current = self.peek_next_table();
    }

    fn next_segment(&mut self) -> Option<(Arc<dyn Segment<Self::Db>>, PrunePurpose)> {
        let Self { provider, current, .. } = self;

        let segment = match current {
            StaticFileTableRef::Headers => {
                provider.get_highest_static_file_block(StaticFileSegment::Headers).map(|to_block| {
                    Arc::new(segments::Headers::new(PruneMode::before_inclusive(to_block)))
                        as Arc<dyn Segment<DB>>
                })
            }
            StaticFileTableRef::Transactions => provider
                .get_highest_static_file_block(StaticFileSegment::Transactions)
                .map(|to_block| {
                    Arc::new(segments::Transactions::new(PruneMode::before_inclusive(to_block)))
                        as Arc<dyn Segment<DB>>
                }),
            StaticFileTableRef::Receipts => provider
                .get_highest_static_file_block(StaticFileSegment::Receipts)
                .map(|to_block| {
                    Arc::new(segments::Receipts::new(PruneMode::before_inclusive(to_block)))
                        as Arc<dyn Segment<DB>>
                }),
        };

        self.next_table();

        segment.map(|sgmnt| (sgmnt, PrunePurpose::StaticFile))
    }

    fn reset_cycle(&mut self) {
        self.prev = None;
        self.start = self.current;
    }
}

#[cfg(test)]
mod test {
    use rand::Rng;
    use reth_chainspec::MAINNET;
    use reth_db::{
        tables,
        test_utils::{create_test_rw_db, create_test_static_files_dir, TempDatabase},
        transaction::DbTxMut,
        DatabaseEnv,
    };
    use reth_primitives::{B256, U256};
    use reth_provider::{ProviderFactory, StaticFileProviderFactory, StaticFileWriter};
    use reth_prune_types::PruneModes;
    use tracing::trace;

    use crate::segments::SegmentSet;

    use super::*;

    #[test]
    fn cycle_with_one_static_file_segment() {
        reth_tracing::init_test_tracing();

        let db = create_test_rw_db();
        let (_static_dir, static_dir_path) = create_test_static_files_dir();
        let provider_factory = ProviderFactory::new(
            db,
            MAINNET.clone(),
            StaticFileProvider::read_write(static_dir_path).unwrap(),
        );

        let provider_rw = provider_factory.provider_rw().unwrap();
        let tx = provider_rw.tx_ref();
        tx.put::<tables::HeaderNumbers>(B256::default(), 0).unwrap();
        tx.put::<tables::BlockBodyIndices>(0, Default::default()).unwrap();

        let segments: Vec<Arc<dyn Segment<TempDatabase<DatabaseEnv>>>> =
            SegmentSet::from_prune_modes(PruneModes::all()).into_vec();
        let segments_len = segments.len();

        let static_file_provider = provider_factory.static_file_provider();

        let (header, block_hash) = MAINNET.sealed_genesis_header().split();
        static_file_provider
            .latest_writer(StaticFileSegment::Headers)
            .expect("get static file writer for headers")
            .append_header(header, U256::ZERO, block_hash)
            .unwrap();

        static_file_provider.commit().unwrap();

        let mut ring: TableRing<_> =
            TableRing::new(static_file_provider, TableRef::default(), segments).unwrap();

        let mut total_segments = 0;
        for segment in ring.iter() {
            total_segments += 1;
            trace!(target: "pruner::test", "segment: {:?}", segment.0.segment());
        }

        // + 1 non-empty static file segments
        assert_eq!(segments_len + 1, total_segments);
        // back at start table
        assert_eq!(TableRef::default(), ring.current_table());
        // cycle reset
        assert!(ring.prev_table().is_none());
    }

    #[test]
    fn cycle_start_at_headers() {
        let db = create_test_rw_db();
        let (_static_dir, static_dir_path) = create_test_static_files_dir();
        let provider_factory = ProviderFactory::new(
            db,
            MAINNET.clone(),
            StaticFileProvider::read_write(static_dir_path).unwrap(),
        );

        let segments: Vec<Arc<dyn Segment<TempDatabase<DatabaseEnv>>>> =
            SegmentSet::from_prune_modes(PruneModes::all()).into_vec();
        let segments_len = segments.len();

        let mut ring: TableRing<_> =
            TableRing::new(provider_factory.static_file_provider(), TableRef::default(), segments)
                .unwrap();

        let cycle = SegmentIter { ring: &mut ring };
        let total_segments = cycle.count();

        // + 3 static file segments
        assert_eq!(segments_len + 3, total_segments);
        // back at start table
        assert_eq!(TableRef::default(), ring.current_table());
        // cycle reset
        assert!(ring.prev_table().is_none());
    }

    #[test]
    fn cycle_twice_start_at_headers() {
        reth_tracing::init_test_tracing();

        let db = create_test_rw_db();
        let (_static_dir, static_dir_path) = create_test_static_files_dir();
        let provider_factory = ProviderFactory::new(
            db,
            MAINNET.clone(),
            StaticFileProvider::read_write(static_dir_path).unwrap(),
        );

        let segments: Vec<Arc<dyn Segment<TempDatabase<DatabaseEnv>>>> =
            SegmentSet::from_prune_modes(PruneModes::all()).into_vec();
        let segments_len = segments.len();

        let mut ring: TableRing<_> =
            TableRing::new(provider_factory.static_file_provider(), TableRef::default(), segments)
                .unwrap();

        let mut total_segments = 0;

        for segment in ring.iter() {
            total_segments += 1;
            trace!(target: "pruner::test", "segment: {:?}", segment.0.segment());
        }

        for segment in ring.iter() {
            total_segments += 1;
            trace!(target: "pruner::test", "segment: {:?}", segment.0.segment());
        }

        // + 3 empty static file segments
        assert_eq!(2 * segments_len, total_segments);
        // back at start table
        assert_eq!(TableRef::default(), ring.current_table());
        // cycle reset
        assert!(ring.prev_table().is_none());
    }

    #[test]
    fn cycle_twice_incomplete_first_cycle() {
        reth_tracing::init_test_tracing();

        const TOTAL_SEGMENTS_FIRST_ITERATION: usize = 2;

        let db = create_test_rw_db();
        let (_static_dir, static_dir_path) = create_test_static_files_dir();
        let provider_factory = ProviderFactory::new(
            db,
            MAINNET.clone(),
            StaticFileProvider::read_write(static_dir_path).unwrap(),
        );

        let segments: Vec<Arc<dyn Segment<TempDatabase<DatabaseEnv>>>> =
            SegmentSet::from_prune_modes(PruneModes::all()).into_vec();
        let segments_len = segments.len();

        let mut ring: TableRing<_> =
            TableRing::new(provider_factory.static_file_provider(), TableRef::default(), segments)
                .unwrap();

        let mut total_segments = 0;

        for segment in ring.iter() {
            total_segments += 1;
            trace!(target: "pruner::test", "segment: {:?}", segment.0.segment());
            if total_segments == TOTAL_SEGMENTS_FIRST_ITERATION {
                break
            }
        }

        assert_eq!(ring.current_table(), TableRef::Other(2));

        for segment in ring.iter() {
            total_segments += 1;
            trace!(target: "pruner::test", "segment: {:?}", segment.0.segment());
        }

        // + 3 empty static file segments
        assert_eq!(
            2 * segments_len - (segments_len - TOTAL_SEGMENTS_FIRST_ITERATION),
            total_segments
        );
        assert_eq!(TableRef::Other(2), ring.current_table());
        assert!(ring.prev_table().is_none());
    }

    #[test]
    fn cycle_start_random_non_static_files_segment() {
        let db = create_test_rw_db();
        let (_static_dir, static_dir_path) = create_test_static_files_dir();
        let provider_factory = ProviderFactory::new(
            db,
            MAINNET.clone(),
            StaticFileProvider::read_write(static_dir_path).unwrap(),
        );

        let segments: Vec<Arc<dyn Segment<TempDatabase<DatabaseEnv>>>> =
            SegmentSet::from_prune_modes(PruneModes::all()).into_vec();
        let segments_len = segments.len();

        let index = rand::thread_rng().gen_range(0..segments_len);
        let start = TableRef::Other(index);
        let mut ring: TableRing<_> =
            TableRing::new(provider_factory.static_file_provider(), start, segments).unwrap();

        let cycle = SegmentIter { ring: &mut ring };
        let total_segments = cycle.count();

        // + 3 static file segments
        assert_eq!(segments_len + 3, total_segments);
        // back at start table
        assert_eq!(start, ring.current_table());
        // cycle reset
        assert!(ring.prev_table().is_none());
    }

    fn random_static_file_table_ref() -> StaticFileTableRef {
        use StaticFileTableRef::*;

        match rand::thread_rng().gen_range(0..3) {
            0 => Headers,
            1 => Transactions,
            _ => Receipts,
        }
    }

    #[test]
    fn cycle_static_files_start_random_segment() {
        let db = create_test_rw_db();
        let (_static_dir, static_dir_path) = create_test_static_files_dir();
        let provider_factory = ProviderFactory::new(
            db,
            MAINNET.clone(),
            StaticFileProvider::read_write(static_dir_path).unwrap(),
        );

        let start = random_static_file_table_ref();
        let mut ring: StaticFileTableRing<TempDatabase<DatabaseEnv>> =
            StaticFileTableRing::new(provider_factory.static_file_provider(), start);

        let cycle = SegmentIter { ring: &mut ring };
        let total_segments = cycle.count();

        // 3 static file segments
        assert_eq!(3, total_segments);
        // back at start table
        assert_eq!(start, ring.current_table());
        // cycle reset
        assert!(ring.prev_table().is_none());
    }
}