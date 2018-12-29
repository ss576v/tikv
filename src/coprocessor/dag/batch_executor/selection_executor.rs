// Copyright 2018 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::convert::TryFrom;
use std::sync::Arc;

use cop_datatype::{EvalType, FieldTypeAccessor};
use tipb::expression::Expr;

use super::interface::*;
use coprocessor::codec::batch::{LazyBatchColumn, LazyBatchColumnVec};
use coprocessor::dag::executor::ExprColumnRefVisitor;
use coprocessor::dag::expr::EvalConfig;
use coprocessor::dag::rpn_expr::{RpnExpressionEvalContext, RpnExpressionNodeVec};
use coprocessor::*;

pub struct BatchSelectionExecutor<Src: BatchExecutor> {
    context: ExecutorContext,
    src: Src,
    eval_context: RpnExpressionEvalContext,
    conditions: Vec<RpnExpressionNodeVec>,

    /// The index (in `context.columns_info`) of referred columns in expression.
    referred_columns: Vec<usize>,

    /// A `LazyBatchColumnVec` to hold unused data produced in `next_batch`.
    pending_data: LazyBatchColumnVec,
    /// Unused errors during `next_batch` if any.
    pending_error: Option<Error>,
    has_thrown_error: bool,
    /// Whether underlying executor has drained or there are error during filter and no more buffer
    /// is needed to be fetched.
    has_drained: bool,
}

impl<Src: BatchExecutor> BatchSelectionExecutor<Src> {
    pub fn new(
        context: ExecutorContext,
        src: Src,
        conditions: Vec<Expr>,
        eval_config: Arc<EvalConfig>,
    ) -> Result<Self> {
        let referred_columns = {
            let mut ref_visitor = ExprColumnRefVisitor::new(context.columns_info.len());
            ref_visitor.batch_visit(&conditions)?;
            ref_visitor.column_offsets()
        };

        let pending_data = {
            let mut is_column_referred = vec![false; context.columns_info.len()];
            for idx in &referred_columns {
                is_column_referred[*idx] = true;
            }
            let mut columns = Vec::with_capacity(context.columns_info.len());
            for (idx, column_info) in context.columns_info.iter().enumerate() {
                if is_column_referred[idx] || column_info.get_pk_handle() {
                    let eval_type = EvalType::try_from(column_info.tp())
                        .map_err(|e| Error::Other(box_err!(e)))?;
                    columns.push(LazyBatchColumn::decoded_with_capacity_and_tp(
                        2048, eval_type,
                    ));
                } else {
                    columns.push(LazyBatchColumn::raw_with_capacity(2048));
                }
            }
            LazyBatchColumnVec::from(columns)
        };

        let eval_context = RpnExpressionEvalContext::new(eval_config);
        let conditions = conditions
            .into_iter()
            .map(|def| RpnExpressionNodeVec::build_from_def(def, eval_context.config.tz))
            .collect();

        Ok(Self {
            context,
            src,
            eval_context,
            conditions,
            referred_columns,

            pending_data,
            pending_error: None,
            has_thrown_error: false,
            has_drained: false,
        })
    }

    fn filter_rows(
        &mut self,
        data: &LazyBatchColumnVec,
        base_retain_map: &mut [bool],
    ) -> Result<()> {
        // use coprocessor::codec::datum;

        let rows_len = data.rows_len();

        // For each row, calculate a remain map.

        // We use 2 retain map, base and head. For each expression, its values are batch
        // evaluated as bools in head. Then head is merged into base. Finally, we use base as the
        // final retain map.

        // FIXME: Not all rows needs to be evaluated (if head[x] == false).
        // FIXME: Avoid head_retain_map being re-allocated across different filter_rows function calls.
        assert!(base_retain_map.len() >= rows_len);
        let mut head_retain_map = vec![false; rows_len];

        for condition in &self.conditions {
            condition.eval_as_bools(
                &mut self.eval_context,
                rows_len,
                data,
                head_retain_map.as_mut_slice(),
            );
            for i in 0..rows_len {
                base_retain_map[i] &= head_retain_map[i];
            }
        }

        Ok(())
    }

    /// Fetches and filter next batch rows from the underlying executor,
    /// fill into the pending buffer.
    #[inline]
    fn fill_buffer(&mut self) {
        assert!(!self.has_drained);

        // Fetch some rows
        let mut result = self.src.next_batch(1024); // TODO: Remove magic numbe

        for idx in &self.referred_columns {
            result.data.ensure_column_decoded(
                *idx,
                self.eval_context.config.tz,
                &self.context.columns_info[*idx],
            );
            // TODO: what if ensure_column_decoded failed?
            // FIXME: We should not fail due to errors from unneeded rows.
        }

        let result_is_drained = result.data.rows_len() == 0;

        // Filter fetched rows. If there are errors, less rows will be retained.
        let mut retain_map = vec![true; result.data.rows_len()];
        let filter_result = self.filter_rows(&result.data, &mut retain_map);

        // Append by retain map. Notice that after this function call, `result.data` will be none.
        self.pending_data
            .append_by_index(&mut result.data, |idx| retain_map[idx]);
        self.pending_error = self
            .pending_error
            .take()
            .or_else(|| filter_result.err())
            .or(result.error);

        self.has_drained = self.pending_error.is_some() || result_is_drained;
    }
}

impl<Src: BatchExecutor> BatchExecutor for BatchSelectionExecutor<Src> {
    #[inline]
    fn next_batch(&mut self, expect_rows: usize) -> BatchExecuteResult {
        assert!(!self.has_thrown_error);

        // Ensure there are `expect_rows` in the pending buffer if not drained.
        while !self.has_drained && self.pending_data.rows_len() < expect_rows {
            self.fill_buffer();
        }

        // Retrive first `expect_rows` from the pending buffer.
        // If pending buffer is not sifficient, pending_error is also carried.
        let data = self.pending_data.take_and_collect(expect_rows);

        let error = if data.rows_len() < expect_rows {
            self.pending_error.take()
        } else {
            None
        };

        if error.is_some() {
            self.has_thrown_error = true;
        }

        BatchExecuteResult { data, error }
    }
}
