// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use risingwave_expr::expr::build_from_prost;

use super::*;
use crate::executor::ProjectExecutor;

pub struct ProjectExecutorBuilder;

impl ExecutorBuilder for ProjectExecutorBuilder {
    fn new_boxed_executor(
        mut params: ExecutorParams,
        node: &StreamNode,
        _store: impl StateStore,
        _stream: &mut LocalStreamManagerCore,
    ) -> Result<BoxedExecutor> {
        let node = try_match_expand!(node.get_node_body().unwrap(), NodeBody::Project)?;
        let project_exprs = node
            .get_select_list()
            .iter()
            .map(build_from_prost)
            .collect::<Result<Vec<_>>>()?;

        Ok(ProjectExecutor::new(
            params.input.remove(0),
            params.pk_indices,
            project_exprs,
            params.executor_id,
        )
        .boxed())
    }
}
