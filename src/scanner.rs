use std::fmt;
use std::mem;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::iter::{FusedIterator, IntoIterator};

use bytes::{
    Bytes,
    BytesMut,
};
use krpc::Proxy;
use futures::{
    Async,
    Future,
    Stream,
    Poll,
};

use Column;
use Error;
use Result;
use Row;
use ScannerId;
use Schema;
use TabletId;
use tablet::Tablet;
use meta_cache::{
    Lookup,
    Entry,
    TableLocations,
};
use pb::{
    ColumnSchemaPb,
    ExpectField,
    RowwiseRowBlockPb,
};
use pb::tserver::{
    NewScanRequestPb,
    ScanRequestPb,
    ScanResponsePb,
    TabletServerService,
};
use replica::{
    ReplicaRpc,
    Selection,
    Speculation,
};
use backoff::Backoff;

#[derive(Clone)]
pub struct ScanBuilder {
    table_schema: Schema,
    table_locations: TableLocations,
    projected_columns: Vec<usize>,
}

fn column_to_pb(column: &Column) -> ColumnSchemaPb {
    ColumnSchemaPb {
        name: column.name().to_owned(),
        type_: column.data_type().to_pb(),
        is_nullable: Some(column.is_nullable()),
        ..Default::default()
    }
}

impl ScanBuilder {

    pub(crate) fn new(table_schema: Schema, table_locations: TableLocations) -> ScanBuilder {
        let projected_columns = (0..table_schema.columns().len()).collect::<Vec<_>>();
        ScanBuilder {
            table_schema,
            table_locations,
            projected_columns,
        }
    }

    pub fn projected_columns<I>(mut self, column_indexes: I) -> Result<ScanBuilder>
        where I: IntoIterator<Item=usize>
    {
        self.projected_columns.clear();
        self.projected_columns.extend(column_indexes);
        for &idx in &self.projected_columns {
            self.table_schema.check_index(idx)?;
        }
        Ok(self)
    }

    pub fn projected_column_names<N, I>(mut self, column_names: I) -> Result<ScanBuilder>
        where N: AsRef<str>,
              I: IntoIterator<Item=N>
    {
        self.projected_columns.clear();
        for column in column_names {
            let column = column.as_ref();
            match self.table_schema.column_index(column) {
                Some(idx) => self.projected_columns.push(idx),
                None => return Err(Error::InvalidArgument(format!("unknown column {}", column))),
            }
        }
        Ok(self)
    }

    pub fn build(self) -> Scan {
        let mut columns = Vec::new();
        for idx in self.projected_columns {
            columns.push(self.table_schema.columns()[idx].clone());
        }
        let projected_schema = Schema::new(columns, 0);

        let state = ScannerState::Lookup(self.table_locations.entry(&[]));
        Scan {
            projected_schema,
            table_locations: self.table_locations,
            state,
        }
    }
}

pub struct Scan {
    projected_schema: Schema,
    table_locations: TableLocations,
    state: ScannerState,
}

enum ScannerState {
    Lookup(Lookup<Entry>),
    Scan {
        tablet: Arc<Tablet>,
        tablet_scan: TabletScan,
    },
    Finished,
}

impl Scan {
    fn new_scan_request(&self, tablet: TabletId) -> NewScanRequestPb {
        let projected_columns = self.projected_schema
                                    .columns()
                                    .iter()
                                    .map(column_to_pb)
                                    .collect::<Vec<_>>();

        NewScanRequestPb {
            tablet_id: tablet.to_string().into_bytes(),
            projected_columns,
            ..Default::default()
        }
    }
}

impl Stream for Scan {
    type Item = RowBatch;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<RowBatch>, Error> {
        trace!("Scan::poll");
        loop {
            match mem::replace(&mut self.state, ScannerState::Finished) {
                ScannerState::Lookup(mut lookup) => {
                    match lookup.poll()? {
                        Async::Ready(Entry::Tablet(tablet)) => {
                            let tablet_scan = TabletScan::new(self.projected_schema.clone(),
                                                              tablet.clone(),
                                                              self.new_scan_request(tablet.id()));
                            self.state = ScannerState::Scan { tablet, tablet_scan };
                        },
                        Async::Ready(Entry::NonCoveredRange { upper_bound, .. }) => if !upper_bound.is_empty() {
                            let lookup = self.table_locations.entry(&upper_bound);
                            self.state = ScannerState::Lookup(lookup);
                        },
                        Async::NotReady => {
                            self.state = ScannerState::Lookup(lookup);
                            return Ok(Async::NotReady);
                        }
                    }
                },
                ScannerState::Scan { tablet, mut tablet_scan } => {
                    match tablet_scan.poll()? {
                        Async::Ready(Some(batch)) => {
                            self.state = ScannerState::Scan { tablet, tablet_scan };
                            return Ok(Async::Ready(Some(batch)))
                        },
                        Async::Ready(None) => if !tablet.upper_bound().is_empty() {
                            let lookup = self.table_locations.entry(tablet.upper_bound());
                            self.state = ScannerState::Lookup(lookup);
                        },
                        Async::NotReady => {
                            self.state = ScannerState::Scan { tablet, tablet_scan };
                            return Ok(Async::NotReady);
                        },
                    }
                },
                ScannerState::Finished => return Ok(Async::Ready(None)),
            }
        }
    }
}

impl fmt::Debug for Scan {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Scan")
            .finish()
    }
}

pub struct RowBatch {
    projected_schema: Schema,
    len: usize,
    data: Bytes,
    indirect_data: Bytes,
}

impl RowBatch {
    fn new(projected_schema: Schema, block: RowwiseRowBlockPb, mut sidecars: Vec<BytesMut>) -> Result<RowBatch> {
        trace!("RowBatch::new; block: {:?}, sidecars: {:?}", block, sidecars);
        let mut data = match block.rows_sidecar {
            Some(idx) if idx < 0 => return Err(
                Error::Serialization("RowwiseRowBlockPb.row_sidecar is negative".to_string())),
            Some(idx) => match sidecars.get_mut(idx as usize) {
                Some(sidecar) => mem::replace(sidecar, BytesMut::new()),
                None => return Err(
                    Error::Serialization("ScanResponsePb does not include a row sidecar".to_string())),
            }
            None => return Err(
                Error::Serialization("RowwiseRowBlockPb does not include a row sidecar".to_string())),
        };

        let indirect_data = match block.indirect_data_sidecar {
            Some(idx) if idx < 0 => return Err(
                Error::Serialization("RowwiseRowBlockPb.indirect_data_sidecar is negative".to_string())),
            Some(idx) => match sidecars.get_mut(idx as usize) {
                Some(sidecar) => mem::replace(sidecar, BytesMut::new()).freeze(),
                None => return Err(
                    Error::Serialization("ScanResponsePb does not include an indirect data sidecar".to_string())),
            }
            None => Bytes::new(),
        };

        let row_len = projected_schema.row_len()
            + projected_schema.has_nullable_columns() as usize * projected_schema.bitmap_len();

        // Sanity check that the data length matches the number of rows returned.
        let num_rows = block.num_rows() as usize;
        match num_rows.checked_mul(row_len) {
            Some(len) if len == data.len() => (),
            Some(_) => {
                return Err(Error::Serialization(
                        format!("RowwiseRowBlockPb.num_rows does not match row_sidecar length; num_rows: {}, row_sidecar.len: {}, row_len: {}",
                                num_rows, data.len(), row_len)));
            },
            None => {
                return Err(Error::Serialization(
                        format!("RowwiseRowBlockPb.num_rows is invalid: {}", num_rows)));
            },
        }

        if !projected_schema.var_len_column_offsets().is_empty() {
            for row in data.chunks_mut(row_len) {
                for &offset in projected_schema.var_len_column_offsets() {
                    unsafe {
                        // TODO: sanity check the value length falls in the indirect data buf.
                        let ptr = row.as_mut_ptr().offset(offset as isize) as *mut u64;
                        let offset = ptr.read_unaligned().to_le();
                        *ptr = ((indirect_data.as_ptr() as u64) + offset).to_le();
                    }
                }
            }

        }

        Ok(RowBatch {
            projected_schema,
            len: block.num_rows() as usize,
            data: data.freeze(),
            indirect_data,
        })
    }
}

impl <'a> IntoIterator for &'a RowBatch {
    type Item = Row<'a>;
    type IntoIter = RowBatchIter<'a>;
    fn into_iter(self) -> RowBatchIter<'a> {
        let projected_schema = &self.projected_schema;
        let row_len = projected_schema.row_len()
            + projected_schema.has_nullable_columns() as usize * projected_schema.bitmap_len();
        let iter = self.data.chunks(row_len);
        RowBatchIter {
            projected_schema: &self.projected_schema,
            iter,
        }
    }
}

pub struct RowBatchIter<'a> {
    projected_schema: &'a Schema,
    iter: ::std::slice::Chunks<'a, u8>,
}

impl <'a> Iterator for RowBatchIter<'a> {
    type Item = Row<'a>;
    fn next(&mut self) -> Option<Row<'a>> {
        self.iter.next().map(|data| Row::contiguous(self.projected_schema.clone(), data))
    }
}

impl <'a> ExactSizeIterator for RowBatchIter<'a> {
    fn len(&self) -> usize {
        self.iter.len()
    }
}

impl <'a> DoubleEndedIterator for RowBatchIter<'a> {
    fn next_back(&mut self) -> Option<Row<'a>> {
        self.iter.next_back().map(|data| Row::contiguous(self.projected_schema.clone(), data))
    }
}

// TODO: compile-time assert that Chunks is fused.
impl <'a> FusedIterator for RowBatchIter<'a> {}

enum TabletScan {
    New {
        projected_schema: Schema,
        rpc: ReplicaRpc<Arc<Tablet>, ScanRequestPb, ScanResponsePb>,
    },
    Continue {
        projected_schema: Schema,
        scanner_id: ScannerId,
        call_seq_id: u32,
        rpc: ReplicaRpc<Proxy, ScanRequestPb, ScanResponsePb>,
    },
    Finished,
}

impl TabletScan {

    fn new(projected_schema: Schema,
           tablet: Arc<Tablet>,
           new_scan_request: NewScanRequestPb) -> TabletScan {
        debug!("TabletScan::new; tablet: {:?}", &*tablet);
        let mut request = ScanRequestPb::default();
        request.new_scan_request = Some(new_scan_request);

        let call = TabletServerService::scan(Arc::new(request),
                                             Instant::now() + Duration::from_secs(60));
        let rpc = ReplicaRpc::new(tablet,
                                  call,
                                  Speculation::Staggered(Duration::from_millis(100)),
                                  Selection::Closest,
                                  Backoff::default());
        TabletScan::New { projected_schema, rpc }
    }

    fn cont(projected_schema: Schema, scanner_id: ScannerId, call_seq_id: u32, proxy: Proxy) -> TabletScan {
        let mut request = ScanRequestPb::default();
        request.scanner_id = Some(scanner_id.to_string().into_bytes());
        request.call_seq_id = Some(call_seq_id);

        let call = TabletServerService::scan(Arc::new(request),
                                                Instant::now() + Duration::from_secs(60));

        let rpc = ReplicaRpc::new(proxy,
                                  call,
                                  Speculation::Full,
                                  Selection::Closest,
                                  Backoff::default());
        TabletScan::Continue { projected_schema, scanner_id, call_seq_id, rpc }
    }
}

impl Stream for TabletScan {
    type Item = RowBatch;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<RowBatch>, Error> {
        trace!("TabletScan::poll");
        match self {
            TabletScan::New { projected_schema, rpc } => {
                let (proxy, mut response, sidecars) = try_ready!(rpc.poll());
                let batch = RowBatch::new(projected_schema.clone(),
                                          response.data.take().unwrap_or_default(),
                                          sidecars)?;
                *self = if response.has_more_results() {
                    let scanner_id = ScannerId::parse_bytes(&response.scanner_id
                                                                     .expect_field("ScanResponsePb",
                                                                                   "scanner_id")?)?;
                    // NLL hack: these schema clones are nasty.
                    TabletScan::cont(projected_schema.clone(), scanner_id, 1, proxy)
                } else {
                    TabletScan::Finished
                };

                Ok(Async::Ready(Some(batch)))
            },
            TabletScan::Continue { projected_schema, scanner_id, call_seq_id, rpc } => {
                let (proxy, mut response, sidecars) = try_ready!(rpc.poll());
                let batch = RowBatch::new(projected_schema.clone(),
                                          response.data.take().unwrap_or_default(),
                                          sidecars)?;

                *self = if response.has_more_results() {
                    TabletScan::cont(projected_schema.clone(), *scanner_id, *call_seq_id + 1, proxy)
                } else {
                    TabletScan::Finished
                };

                Ok(Async::Ready(Some(batch)))
            },
            TabletScan::Finished => Ok(Async::Ready(None)),
        }
    }
}

#[cfg(test)]
mod test {

    use std::iter;

    use Client;
    use Column;
    use DataType;
    use Options;
    use SchemaBuilder;
    use TableBuilder;
    use mini_cluster::MiniCluster;
    use super::*;
    use WriterConfig;

    use env_logger;
    use futures::future;
    use tokio::runtime::current_thread::Runtime;

    #[test]
    fn count() {
        let _ = env_logger::try_init();
        let mut cluster = MiniCluster::default();
        let mut runtime = Runtime::new().unwrap();

        let mut client = runtime.block_on(Client::new(cluster.master_addrs(), Options::default()))
                                .expect("client");

        let schema = SchemaBuilder::new()
            .add_column(Column::builder("key", DataType::Int32).set_not_null())
            .add_column(Column::builder("val", DataType::Int32))
            .set_primary_key(vec!["key"])
            .build()
            .unwrap();

        let mut table_builder = TableBuilder::new("count", schema.clone());
        table_builder.add_hash_partitions(vec!["key"], 4);
        table_builder.set_num_replicas(1);

        // TODO: don't wait for table creation in order to test retry logic.
        let table_id = runtime.block_on(client.create_table(table_builder)).unwrap();

        let table = runtime.block_on(client.open_table_by_id(table_id)).unwrap();

        let mut writer = table.new_writer(WriterConfig::default());

        let num_rows = 100;

        // TODO: remove lazy once apply no longer polls.
        runtime.block_on(future::lazy::<_, Result<()>>(|| {
            // Insert a bunch of values
            for i in 0..num_rows {
                let mut insert = table.schema().new_row();
                insert.set_by_name::<i32>("key", i).unwrap();
                insert.set_by_name::<i32>("val", i).unwrap();
                writer.insert(insert);
            }
            Ok(())
        })).unwrap();

        runtime.block_on(future::poll_fn(|| writer.poll_flush())).unwrap();

        let scan: Scan = runtime.block_on(::futures::future::lazy::<_, Result<Scan>>(|| {
            Ok(table.scan_builder().projected_columns(iter::empty())?.build())
        })).unwrap();

        let batches: Vec<RowBatch> = runtime.block_on(::futures::future::lazy(|| scan.collect())).unwrap();

        assert_eq!(num_rows as usize, batches.into_iter().map(|batch| batch.len).sum());
    }

    #[test]
    fn select() {
        let _ = env_logger::try_init();
        let mut cluster = MiniCluster::default();
        let mut runtime = Runtime::new().unwrap();

        let mut client = runtime.block_on(Client::new(cluster.master_addrs(), Options::default()))
                                .expect("client");

        let schema = SchemaBuilder::new()
            .add_column(Column::builder("key", DataType::Int32).set_not_null())
            .add_column(Column::builder("val", DataType::Int32))
            .set_primary_key(vec!["key"])
            .build()
            .unwrap();

        let mut table_builder = TableBuilder::new("count", schema.clone());
        table_builder.add_hash_partitions(vec!["key"], 4);
        table_builder.set_num_replicas(1);

        // TODO: don't wait for table creation in order to test retry logic.
        let table_id = runtime.block_on(client.create_table(table_builder)).unwrap();
        let table = runtime.block_on(client.open_table_by_id(table_id)).unwrap();
        let mut writer = table.new_writer(WriterConfig::default());
        let num_rows = 10;

        // TODO: remove lazy once apply no longer polls.
        runtime.block_on(future::lazy::<_, Result<()>>(|| {
            // Insert a bunch of values
            for i in 0..num_rows {
                let mut insert = table.schema().new_row();
                insert.set_by_name::<i32>("key", i).unwrap();
                insert.set_by_name::<i32>("val", i).unwrap();
                writer.insert(insert);
            }
            Ok(())
        })).unwrap();
        runtime.block_on(future::poll_fn(|| writer.poll_flush())).unwrap();

        let scan: Scan = runtime.block_on(::futures::future::lazy::<_, Result<Scan>>(|| {
            Ok(table.scan_builder().build())
        })).unwrap();

        let batches = runtime.block_on(::futures::future::lazy(|| scan.collect())).unwrap();

        let mut rows = Vec::new();
        for batch in batches {
            for row in batch.into_iter() {
                rows.push((row.get_by_name::<i32>("key").unwrap(),
                           row.get_by_name::<i32>("val").unwrap()));
            }
        }

        rows.sort();

        let expected = (0..num_rows).map(|i| (i, i)).collect::<Vec<_>>();

        assert_eq!(rows, expected);
    }
}
