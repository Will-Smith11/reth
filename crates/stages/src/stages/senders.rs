use crate::{
    db::Transaction, ExecInput, ExecOutput, Stage, StageError, StageId, UnwindInput, UnwindOutput,
};
use itertools::Itertools;
use rayon::prelude::*;
use reth_db::{
    cursor::{DbCursorRO, DbCursorRW},
    database::Database,
    tables,
    transaction::{DbTx, DbTxMut},
    Error as DbError,
};
use reth_primitives::TxNumber;
use std::fmt::Debug;
use thiserror::Error;

const SENDERS: StageId = StageId("Senders");

/// The senders stage iterates over existing transactions,
/// recovers the transaction signer and stores them
/// in [`TxSenders`][reth_interfaces::db::tables::TxSenders] table.
#[derive(Debug)]
pub struct SendersStage {
    /// The size of the chunk for parallel sender recovery
    pub batch_size: usize,
    /// The size of inserted items after which the control
    /// flow will be returned to the pipeline for commit
    pub commit_threshold: u64,
}

// TODO(onbjerg): Should unwind
#[derive(Error, Debug)]
enum SendersStageError {
    #[error("Sender recovery failed for transaction {tx}.")]
    SenderRecovery { tx: TxNumber },
}

impl From<SendersStageError> for StageError {
    fn from(error: SendersStageError) -> Self {
        StageError::Fatal(Box::new(error))
    }
}

#[async_trait::async_trait]
impl<DB: Database> Stage<DB> for SendersStage {
    /// Return the id of the stage
    fn id(&self) -> StageId {
        SENDERS
    }

    /// Retrieve the range of transactions to iterate over by querying
    /// [`CumulativeTxCount`][reth_interfaces::db::tables::CumulativeTxCount],
    /// collect transactions within that range,
    /// recover signer for each transaction and store entries in
    /// the [`TxSenders`][reth_interfaces::db::tables::TxSenders] table.
    async fn execute(
        &mut self,
        tx: &mut Transaction<'_, DB>,
        input: ExecInput,
    ) -> Result<ExecOutput, StageError> {
        let stage_progress = input.stage_progress.unwrap_or_default();
        let previous_stage_progress = input.previous_stage_progress();
        let max_block_num = previous_stage_progress.min(stage_progress + self.commit_threshold);

        if max_block_num <= stage_progress {
            return Ok(ExecOutput { stage_progress, reached_tip: true, done: true })
        }

        // Look up the start index for the transaction range
        let start_tx_index = tx.get_block_body_by_num(stage_progress + 1)?.start_tx_id;

        // Look up the end index for transaction range (inclusive)
        let end_tx_index = tx.get_block_body_by_num(max_block_num)?.last_tx_index();

        // No transactions to walk over
        if start_tx_index > end_tx_index {
            return Ok(ExecOutput { stage_progress: max_block_num, done: true, reached_tip: true })
        }

        // Acquire the cursor for inserting elements
        let mut senders_cursor = tx.cursor_mut::<tables::TxSenders>()?;

        // Acquire the cursor over the transactions
        let mut tx_cursor = tx.cursor::<tables::Transactions>()?;
        // Walk the transactions from start to end index (inclusive)
        let entries = tx_cursor
            .walk(start_tx_index)?
            .take_while(|res| res.as_ref().map(|(k, _)| *k <= end_tx_index).unwrap_or_default());

        // Iterate over transactions in chunks
        for chunk in &entries.chunks(self.batch_size) {
            let transactions = chunk.collect::<Result<Vec<_>, DbError>>()?;
            // Recover signers for the chunk in parallel
            let recovered = transactions
                .into_par_iter()
                .map(|(id, transaction)| {
                    let signer =
                        transaction.recover_signer().ok_or_else::<StageError, _>(|| {
                            SendersStageError::SenderRecovery { tx: id }.into()
                        })?;
                    Ok((id, signer))
                })
                .collect::<Result<Vec<_>, StageError>>()?;
            // Append the signers to the table
            recovered.into_iter().try_for_each(|(id, sender)| senders_cursor.append(id, sender))?;
        }

        let done = max_block_num >= previous_stage_progress;
        Ok(ExecOutput { stage_progress: max_block_num, done, reached_tip: done })
    }

    /// Unwind the stage.
    async fn unwind(
        &mut self,
        tx: &mut Transaction<'_, DB>,
        input: UnwindInput,
    ) -> Result<UnwindOutput, Box<dyn std::error::Error + Send + Sync>> {
        // Lookup latest tx id that we should unwind to
        let latest_tx_id = tx.get_block_body_by_num(input.unwind_to)?.last_tx_index();
        tx.unwind_table_by_num::<tables::TxSenders>(latest_tx_id)?;
        Ok(UnwindOutput { stage_progress: input.unwind_to })
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use reth_db::models::StoredBlockBody;
    use reth_interfaces::test_utils::generators::random_block_range;
    use reth_primitives::{BlockLocked, BlockNumber, H256};

    use super::*;
    use crate::test_utils::{
        stage_test_suite_ext, ExecuteStageTestRunner, StageTestRunner, TestRunnerError,
        TestTransaction, UnwindStageTestRunner, PREV_STAGE_ID,
    };

    stage_test_suite_ext!(SendersTestRunner);

    #[tokio::test]
    async fn execute_intermediate_commit() {
        let threshold = 50;
        let mut runner = SendersTestRunner::default();
        runner.set_threshold(threshold);
        let (stage_progress, previous_stage) = (1000, 1100); // input exceeds threshold
        let first_input = ExecInput {
            previous_stage: Some((PREV_STAGE_ID, previous_stage)),
            stage_progress: Some(stage_progress),
        };

        // Seed only once with full input range
        runner.seed_execution(first_input).expect("failed to seed execution");

        // Execute first time
        let result = runner.execute(first_input).await.unwrap();
        let expected_progress = stage_progress + threshold;
        assert_matches!(
            result,
            Ok(ExecOutput { done: false, reached_tip: false, stage_progress })
                if stage_progress == expected_progress
        );

        // Execute second time
        let second_input = ExecInput {
            previous_stage: Some((PREV_STAGE_ID, previous_stage)),
            stage_progress: Some(expected_progress),
        };
        let result = runner.execute(second_input).await.unwrap();
        assert_matches!(
            result,
            Ok(ExecOutput { done: true, reached_tip: true, stage_progress })
                if stage_progress == previous_stage
        );

        assert!(runner.validate_execution(first_input, result.ok()).is_ok(), "validation failed");
    }

    struct SendersTestRunner {
        tx: TestTransaction,
        threshold: u64,
    }

    impl Default for SendersTestRunner {
        fn default() -> Self {
            Self { threshold: 1000, tx: TestTransaction::default() }
        }
    }

    impl SendersTestRunner {
        fn set_threshold(&mut self, threshold: u64) {
            self.threshold = threshold;
        }
    }

    impl StageTestRunner for SendersTestRunner {
        type S = SendersStage;

        fn tx(&self) -> &TestTransaction {
            &self.tx
        }

        fn stage(&self) -> Self::S {
            SendersStage { batch_size: 100, commit_threshold: self.threshold }
        }
    }

    impl ExecuteStageTestRunner for SendersTestRunner {
        type Seed = Vec<BlockLocked>;

        fn seed_execution(&mut self, input: ExecInput) -> Result<Self::Seed, TestRunnerError> {
            let stage_progress = input.stage_progress.unwrap_or_default();
            let end = input.previous_stage_progress() + 1;

            let blocks = random_block_range(stage_progress..end, H256::zero());

            self.tx.commit(|tx| {
                let mut current_tx_id = 0;
                blocks.iter().try_for_each(|b| {
                    let txs = b.body.clone();

                    let num_hash = (b.number, b.hash()).into();
                    tx.put::<tables::CanonicalHeaders>(b.number, b.hash())?;
                    tx.put::<tables::BlockBodies>(
                        num_hash,
                        StoredBlockBody { start_tx_id: current_tx_id, tx_count: txs.len() as u64 },
                    )?;

                    for body_tx in txs {
                        // Insert senders for previous stage progress
                        if b.number == stage_progress {
                            tx.put::<tables::TxSenders>(
                                current_tx_id,
                                body_tx.recover_signer().expect("failed to recover sender"),
                            )?;
                        }
                        tx.put::<tables::Transactions>(current_tx_id, body_tx)?;
                        current_tx_id += 1;
                    }

                    Ok(())
                })?;
                Ok(())
            })?;

            Ok(blocks)
        }

        fn validate_execution(
            &self,
            input: ExecInput,
            output: Option<ExecOutput>,
        ) -> Result<(), TestRunnerError> {
            if let Some(output) = output {
                self.tx.query(|tx| {
                    let start_block = input.stage_progress.unwrap_or_default() + 1;
                    let end_block = output.stage_progress;

                    if start_block > end_block {
                        return Ok(())
                    }

                    let start_hash = tx.get::<tables::CanonicalHeaders>(start_block)?.unwrap();
                    let mut body_cursor = tx.cursor::<tables::BlockBodies>()?;
                    body_cursor.seek_exact((start_block, start_hash).into())?;

                    while let Some((_, body)) = body_cursor.next()? {
                        for tx_id in body.tx_id_range() {
                            let transaction = tx
                                .get::<tables::Transactions>(tx_id)?
                                .expect("no transaction entry");
                            let signer =
                                transaction.recover_signer().expect("failed to recover signer");
                            assert_eq!(Some(signer), tx.get::<tables::TxSenders>(tx_id)?);
                        }
                    }

                    Ok(())
                })?;
            } else {
                self.check_no_senders_by_block(input.stage_progress.unwrap_or_default())?;
            }

            Ok(())
        }
    }

    impl UnwindStageTestRunner for SendersTestRunner {
        fn validate_unwind(&self, input: UnwindInput) -> Result<(), TestRunnerError> {
            self.check_no_senders_by_block(input.unwind_to)
        }
    }

    impl SendersTestRunner {
        fn check_no_senders_by_block(&self, block: BlockNumber) -> Result<(), TestRunnerError> {
            let body_result = self.tx.inner().get_block_body_by_num(block);
            match body_result {
                Ok(body) => self
                    .tx
                    .check_no_entry_above::<tables::TxSenders, _>(body.last_tx_index(), |key| {
                        key
                    })?,
                Err(_) => {
                    assert!(self.tx.table_is_empty::<tables::TxSenders>()?);
                }
            };

            Ok(())
        }
    }
}
