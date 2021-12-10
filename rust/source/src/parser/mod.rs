use std::fmt::Debug;

pub use json_parser::*;
pub use protobuf_parser::*;
use risingwave_common::error::Result;
use risingwave_common::types::Datum;

use crate::SourceColumnDesc;

mod json_parser;
mod protobuf_parser;

/// `SourceParser` is the message parser, `ChunkReader` will parse the messages in `SourceReader`
/// one by one through `SourceParser` and assemble them into `DataChunk`
/// Note that the `skip_parse` parameter in `SourceColumnDesc`, when it is true, should skip the
/// parse and return `Datum` of `None`
pub trait SourceParser: Send + Sync + Debug + 'static {
    /// parse needs to be a member method because some format like Protobuf needs to be pre-compiled
    fn parse(&self, payload: &[u8], columns: &[SourceColumnDesc]) -> Result<Vec<Datum>>;
}