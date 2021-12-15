use std::sync::Arc;

extern crate risingwave;
extern crate risingwave_batch;

use risingwave::stream_op::{MViewTable, ManagedMViewState, MemoryStateStore};
use risingwave_batch::executor::{Executor, RowSeqScanExecutor};
use risingwave_common::array::{Array, Row};
use risingwave_common::catalog::{Field, Schema};
use risingwave_common::error::Result;
use risingwave_common::types::{Int32Type, Scalar};
use risingwave_common::util::sort_util::OrderType;

#[tokio::test]
async fn test_row_seq_scan() -> Result<()> {
    // In this test we test if the memtable can be correctly scanned for K-V pair insertions.
    let state_store = MemoryStateStore::new();
    let prefix = b"mview-test-42".to_vec();
    let schema = Schema::new(vec![
        Field::new(Int32Type::create(false)),
        Field::new(Int32Type::create(false)),
    ]);
    let pk_columns = vec![0];
    let orderings = vec![OrderType::Ascending];
    let mut state = ManagedMViewState::new(
        prefix.clone(),
        schema.clone(),
        pk_columns.clone(),
        orderings.clone(),
        state_store.clone(),
    );

    let table = Arc::new(MViewTable::new(
        prefix.clone(),
        schema.clone(),
        pk_columns.clone(),
        orderings,
        state_store.clone(),
    ));

    let mut executor = RowSeqScanExecutor::new(
        Box::new(table.iter()),
        schema
            .fields
            .iter()
            .map(|field| field.data_type.clone())
            .collect(),
        vec![0, 1],
        schema,
    );

    state.put(
        Row(vec![Some(1_i32.to_scalar_value())]),
        Row(vec![
            Some(1_i32.to_scalar_value()),
            Some(4_i32.to_scalar_value()),
        ]),
    );
    state.put(
        Row(vec![Some(2_i32.to_scalar_value())]),
        Row(vec![
            Some(2_i32.to_scalar_value()),
            Some(5_i32.to_scalar_value()),
        ]),
    );
    state.flush().await.unwrap();

    executor.open().await.unwrap();

    let res_chunk = executor.next().await?.unwrap();
    assert_eq!(res_chunk.dimension(), 2);
    assert_eq!(
        res_chunk
            .column_at(0)?
            .array()
            .as_int32()
            .iter()
            .collect::<Vec<_>>(),
        vec![Some(1)]
    );
    assert_eq!(
        res_chunk
            .column_at(1)?
            .array()
            .as_int32()
            .iter()
            .collect::<Vec<_>>(),
        vec![Some(4)]
    );

    let res_chunk2 = executor.next().await?.unwrap();
    assert_eq!(res_chunk2.dimension(), 2);
    assert_eq!(
        res_chunk2
            .column_at(0)?
            .array()
            .as_int32()
            .iter()
            .collect::<Vec<_>>(),
        vec![Some(2)]
    );
    assert_eq!(
        res_chunk2
            .column_at(1)?
            .array()
            .as_int32()
            .iter()
            .collect::<Vec<_>>(),
        vec![Some(5)]
    );
    executor.close().await.unwrap();
    Ok(())
}