#![allow(dead_code)]
#![warn(clippy::map_flatten)]
#![warn(clippy::doc_markdown)]
#![deny(unused_must_use)]
#![feature(trait_alias)]
#![feature(generic_associated_types)]
#![feature(binary_heap_drain_sorted)]

pub mod bummock;
pub mod hummock;
pub mod object;
pub mod row_table;

use crate::bummock::BummockResult;
use crate::bummock::BummockTable;
use crate::row_table::MemRowTable;
use risingwave_common::array::InternalError;
use risingwave_common::array::{DataChunk, StreamChunk};
use risingwave_common::catalog::Schema;
use risingwave_common::catalog::TableId;
use risingwave_common::ensure;
use risingwave_common::error::Result;
use risingwave_common::gen_error;
use risingwave_common::types::DataTypeRef;
use risingwave_pb::ToProst;
use risingwave_proto::plan::ColumnDesc;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard};

/// `Table` is an abstraction of the collection of columns and rows.
/// Each `Table` can be viewed as a flat sheet of a user created table.
#[async_trait::async_trait]
pub trait Table: Sync + Send {
    /// Append an entry to the table.
    async fn append(&self, data: DataChunk) -> Result<usize>;

    /// Get data from the table.
    async fn get_data(&self) -> Result<BummockResult>;

    /// Write a batch of changes. For now, we use `StreamChunk` to represent a write batch
    /// An assertion is put to assert only insertion operations are allowed.
    fn write(&self, chunk: &StreamChunk) -> Result<usize>;

    /// Get the column ids of the table.
    fn get_column_ids(&self) -> Vec<i32>;

    /// Get the indices of the specific column.
    fn index_of_column_id(&self, column_id: i32) -> Result<usize>;
}

#[async_trait::async_trait]
/// `TableManager` is an abstraction of managing a collection of tables.
/// The interface between executors and storage should be table-oriented.
/// `Database` is a logical concept and stored as metadata information.
pub trait TableManager: Sync + Send {
    /// Create a specific table.
    async fn create_table(
        &self,
        table_id: &TableId,
        table_columns: Vec<TableColumnDesc>,
    ) -> Result<Arc<BummockTable>>;

    /// Get a specific table.
    fn get_table(&self, table_id: &TableId) -> Result<TableImpl>;

    /// Drop a specific table.
    async fn drop_table(&self, table_id: &TableId) -> Result<()>;

    /// Create materialized view.
    fn create_materialized_view(
        &self,
        table_id: &TableId,
        columns: Vec<ColumnDesc>,
        pk_columns: Vec<usize>,
    ) -> Result<()>;
}

/// The enumeration of supported simple tables in `SimpleTableManager`.
#[derive(Clone)]
pub enum TableImpl {
    Row(Arc<MemRowTable>),
    Bummock(Arc<BummockTable>),
}

#[derive(Clone, Debug)]
pub struct TableColumnDesc {
    pub data_type: DataTypeRef,
    pub column_id: i32,
}

/// A simple implementation of in memory table for local tests.
/// It will be replaced in near future when replaced by locally
/// on-disk files.
#[derive(Default)]
pub struct SimpleTableManager {
    tables: Mutex<HashMap<TableId, TableImpl>>,
}

#[async_trait::async_trait]
impl TableManager for SimpleTableManager {
    async fn create_table(
        &self,
        table_id: &TableId,
        table_columns: Vec<TableColumnDesc>,
    ) -> Result<Arc<BummockTable>> {
        let mut tables = self.get_tables()?;

        ensure!(
            !tables.contains_key(table_id),
            "Table id already exists: {:?}",
            table_id
        );

        let column_count = table_columns.len();
        ensure!(
            column_count > 0,
            "column count must be positive: {}",
            column_count
        );
        let table = Arc::new(BummockTable::new(table_id, table_columns));
        tables.insert(table_id.clone(), TableImpl::Bummock(table.clone()));
        Ok(table)
    }

    fn get_table(&self, table_id: &TableId) -> Result<TableImpl> {
        let tables = self.get_tables()?;
        tables
            .get(table_id)
            .cloned()
            .ok_or_else(|| InternalError(format!("Table id not exists: {:?}", table_id)).into())
    }

    async fn drop_table(&self, table_id: &TableId) -> Result<()> {
        let mut tables = self.get_tables()?;
        ensure!(
            tables.contains_key(table_id),
            "Table does not exist: {:?}",
            table_id
        );
        tables.remove(table_id);
        Ok(())
    }

    fn create_materialized_view(
        &self,
        table_id: &TableId,
        columns: Vec<ColumnDesc>,
        pk_columns: Vec<usize>,
    ) -> Result<()> {
        let mut tables = self.get_tables()?;

        ensure!(
            !tables.contains_key(table_id),
            "Table id already exists: {:?}",
            table_id
        );
        let column_count = columns.len();
        ensure!(column_count > 0, "There must be more than one column in MV");
        // TODO: Remove to_prost later.
        let schema = Schema::try_from(
            &columns
                .into_iter()
                .map(|c| c.to_prost())
                .collect::<Vec<_>>(),
        )?;
        tables.insert(
            table_id.clone(),
            TableImpl::Row(Arc::new(MemRowTable::new(schema, pk_columns))),
        );

        Ok(())
    }
}

impl SimpleTableManager {
    pub fn new() -> Self {
        SimpleTableManager {
            tables: Mutex::new(HashMap::new()),
        }
    }

    fn get_tables(&self) -> Result<MutexGuard<HashMap<TableId, TableImpl>>> {
        Ok(self.tables.lock().unwrap())
    }
}

/// Reference of a `TableManager`.
pub type TableManagerRef = Arc<dyn TableManager>;

pub enum TableScanOptions {
    SequentialScan,
    SparseIndexScan,
}