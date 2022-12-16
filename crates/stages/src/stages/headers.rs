use crate::{
    db::Transaction, DatabaseIntegrityError, ExecInput, ExecOutput, Stage, StageError, StageId,
    UnwindInput, UnwindOutput,
};
use futures_util::StreamExt;
use reth_db::{
    cursor::{DbCursorRO, DbCursorRW},
    database::Database,
    models::blocks::BlockNumHash,
    tables,
    transaction::{DbTx, DbTxMut},
};
use reth_interfaces::{
    consensus::{Consensus, ForkchoiceState},
    p2p::{
        error::DownloadError,
        headers::{
            client::{HeadersClient, StatusUpdater},
            downloader::{ensure_parent, HeaderDownloader},
        },
    },
};
use reth_primitives::{BlockNumber, Header, SealedHeader, H256, U256};
use std::{fmt::Debug, sync::Arc};
use tracing::*;

const HEADERS: StageId = StageId("Headers");

/// The headers stage.
///
/// The headers stage downloads all block headers from the highest block in the local database to
/// the perceived highest block on the network.
///
/// The headers are processed and data is inserted into these tables:
///
/// - [`HeaderNumbers`][reth_interfaces::db::tables::HeaderNumbers]
/// - [`Headers`][reth_interfaces::db::tables::Headers]
/// - [`CanonicalHeaders`][reth_interfaces::db::tables::CanonicalHeaders]
/// - [`HeaderTD`][reth_interfaces::db::tables::HeaderTD]
///
/// NOTE: This stage commits the header changes to the database (everything except the changes to
/// [`HeaderTD`][reth_interfaces::db::tables::HeaderTD] table). The stage does not return the
/// control flow to the pipeline in order to preserve the context of the chain tip.
#[derive(Debug)]
pub struct HeaderStage<D: HeaderDownloader, C: Consensus, H: HeadersClient, S: StatusUpdater> {
    /// Strategy for downloading the headers
    pub downloader: D,
    /// Consensus client implementation
    pub consensus: Arc<C>,
    /// Downloader client implementation
    pub client: Arc<H>,
    /// Network handle for updating status
    pub network_handle: S,
    /// The number of block headers to commit at once
    pub commit_threshold: usize,
}

#[async_trait::async_trait]
impl<DB: Database, D: HeaderDownloader, C: Consensus, H: HeadersClient, S: StatusUpdater> Stage<DB>
    for HeaderStage<D, C, H, S>
{
    /// Return the id of the stage
    fn id(&self) -> StageId {
        HEADERS
    }

    /// Download the headers in reverse order
    /// starting from the tip
    async fn execute(
        &mut self,
        db: &mut Transaction<'_, DB>,
        input: ExecInput,
    ) -> Result<ExecOutput, StageError> {
        let stage_progress = input.stage_progress.unwrap_or_default();
        self.update_head::<DB>(db, stage_progress).await?;

        // Lookup the head and tip of the sync range
        let (head, tip) = self.get_head_and_tip(db, stage_progress).await?;
        debug!(
            target: "sync::stages::headers",
            "Syncing from tip {:?} to head {:?}",
            tip,
            head.hash()
        );

        let mut current_progress = stage_progress;
        let mut stream = self.downloader.stream(head.clone(), tip).chunks(self.commit_threshold);

        // The stage relies on the downloader to return the headers
        // in descending order starting from the tip down to
        // the local head (latest block in db)
        while let Some(headers) = stream.next().await {
            match headers.into_iter().collect::<Result<Vec<_>, _>>() {
                Ok(res) => {
                    info!(
                        target: "sync::stages::headers",
                        len = res.len(),
                        "Received headers"
                    );

                    // Perform basic response validation
                    self.validate_header_response(&res)?;
                    let write_progress =
                        self.write_headers::<DB>(db, res).await?.unwrap_or_default();
                    db.commit()?;
                    current_progress = current_progress.max(write_progress);
                }
                Err(e) => match e {
                    DownloadError::Timeout => {
                        warn!(
                            target: "sync::stages::headers",
                            "No response for header request"
                        );
                        return Err(StageError::Recoverable(DownloadError::Timeout.into()))
                    }
                    DownloadError::HeaderValidation { hash, error } => {
                        error!(
                            target: "sync::stages::headers",
                            "Validation error for header {hash}: {error}"
                        );
                        return Err(StageError::Validation { block: stage_progress, error })
                    }
                    error => {
                        error!(
                            target: "sync::stages::headers",
                            ?error,
                            "An unexpected error occurred"
                        );
                        return Err(StageError::Recoverable(error.into()))
                    }
                },
            }
        }

        // Write total difficulty values after all headers have been inserted
        self.write_td::<DB>(db, &head)?;

        let stage_progress = current_progress.max(
            db.cursor::<tables::CanonicalHeaders>()?
                .last()?
                .map(|(num, _)| num)
                .unwrap_or_default(),
        );

        Ok(ExecOutput { stage_progress, reached_tip: true, done: true })
    }

    /// Unwind the stage.
    async fn unwind(
        &mut self,
        db: &mut Transaction<'_, DB>,
        input: UnwindInput,
    ) -> Result<UnwindOutput, Box<dyn std::error::Error + Send + Sync>> {
        // TODO: handle bad block
        db.unwind_table_by_walker::<tables::CanonicalHeaders, tables::HeaderNumbers>(
            input.unwind_to + 1,
        )?;
        db.unwind_table_by_num::<tables::CanonicalHeaders>(input.unwind_to)?;
        db.unwind_table_by_num_hash::<tables::Headers>(input.unwind_to)?;
        db.unwind_table_by_num_hash::<tables::HeaderTD>(input.unwind_to)?;
        Ok(UnwindOutput { stage_progress: input.unwind_to })
    }
}

impl<D: HeaderDownloader, C: Consensus, H: HeadersClient, S: StatusUpdater>
    HeaderStage<D, C, H, S>
{
    async fn update_head<DB: Database>(
        &self,
        db: &Transaction<'_, DB>,
        height: BlockNumber,
    ) -> Result<(), StageError> {
        let block_key = db.get_block_numhash(height)?;
        let td: U256 = *db
            .get::<tables::HeaderTD>(block_key)?
            .ok_or(DatabaseIntegrityError::TotalDifficulty { number: height })?;
        // TODO: This should happen in the last stage
        self.network_handle.update_status(height, block_key.hash(), td);
        Ok(())
    }

    /// Get the head and tip of the range we need to sync
    async fn get_head_and_tip<DB: Database>(
        &self,
        db: &Transaction<'_, DB>,
        stage_progress: u64,
    ) -> Result<(SealedHeader, H256), StageError> {
        // Create a cursor over canonical header hashes
        let mut cursor = db.cursor::<tables::CanonicalHeaders>()?;
        let mut header_cursor = db.cursor::<tables::Headers>()?;

        // Get head hash and reposition the cursor
        let (head_num, head_hash) = cursor
            .seek_exact(stage_progress)?
            .ok_or(DatabaseIntegrityError::CanonicalHeader { number: stage_progress })?;

        // Construct head
        let (_, head) = header_cursor
            .seek_exact((head_num, head_hash).into())?
            .ok_or(DatabaseIntegrityError::Header { number: head_num, hash: head_hash })?;
        let head = SealedHeader::new(head, head_hash);

        // Look up the next header
        let next_header = cursor
            .next()?
            .map(|(next_num, next_hash)| -> Result<Header, StageError> {
                let (_, next) = header_cursor
                    .seek_exact((next_num, next_hash).into())?
                    .ok_or(DatabaseIntegrityError::Header { number: next_num, hash: next_hash })?;
                Ok(next)
            })
            .transpose()?;

        // Decide the tip or error out on invalid input.
        // If the next element found in the cursor is not the "expected" next block per our current
        // progress, then there is a gap in the database and we should start downloading in
        // reverse from there. Else, it should use whatever the forkchoice state reports.
        let tip = match next_header {
            Some(header) if stage_progress + 1 != header.number => header.parent_hash,
            None => self.next_fork_choice_state(&head.hash()).await.head_block_hash,
            _ => return Err(StageError::StageProgress(stage_progress)),
        };
        Ok((head, tip))
    }

    async fn next_fork_choice_state(&self, head: &H256) -> ForkchoiceState {
        let mut state_rcv = self.consensus.fork_choice_state();
        loop {
            let _ = state_rcv.changed().await;
            let forkchoice = state_rcv.borrow();
            if !forkchoice.head_block_hash.is_zero() && forkchoice.head_block_hash != *head {
                return forkchoice.clone()
            }
        }
    }

    /// Perform basic header response validation
    fn validate_header_response(&self, headers: &[SealedHeader]) -> Result<(), StageError> {
        let mut headers_iter = headers.iter().peekable();
        while let Some(header) = headers_iter.next() {
            if let Some(parent) = headers_iter.peek() {
                ensure_parent(header, parent)
                    .map_err(|err| StageError::Download(err.to_string()))?;
            }
        }

        Ok(())
    }

    /// Write downloaded headers to the database
    async fn write_headers<DB: Database>(
        &self,
        db: &Transaction<'_, DB>,
        headers: Vec<SealedHeader>,
    ) -> Result<Option<BlockNumber>, StageError> {
        let mut cursor_header = db.cursor_mut::<tables::Headers>()?;
        let mut cursor_canonical = db.cursor_mut::<tables::CanonicalHeaders>()?;

        let mut latest = None;
        // Since the headers were returned in descending order,
        // iterate them in the reverse order
        for header in headers.into_iter().rev() {
            if header.number == 0 {
                continue
            }

            let block_hash = header.hash();
            let key: BlockNumHash = (header.number, block_hash).into();
            let header = header.unseal();
            latest = Some(header.number);

            // NOTE: HeaderNumbers are not sorted and can't be inserted with cursor.
            db.put::<tables::HeaderNumbers>(block_hash, header.number)?;
            cursor_header.insert(key, header)?;
            cursor_canonical.insert(key.number(), key.hash())?;
        }

        Ok(latest)
    }

    /// Iterate over inserted headers and write td entries
    fn write_td<DB: Database>(
        &self,
        db: &Transaction<'_, DB>,
        head: &SealedHeader,
    ) -> Result<(), StageError> {
        // Acquire cursor over total difficulty table
        let mut cursor_td = db.cursor_mut::<tables::HeaderTD>()?;

        // Get latest total difficulty
        let last_entry = cursor_td
            .seek_exact(head.num_hash().into())?
            .ok_or(DatabaseIntegrityError::TotalDifficulty { number: head.number })?;
        let mut td: U256 = last_entry.1.into();

        // Start at first inserted block during this iteration
        let start_key = db.get_block_numhash(head.number + 1)?;

        // Walk over newly inserted headers, update & insert td
        for entry in db.cursor::<tables::Headers>()?.walk(start_key)? {
            let (key, header) = entry?;
            td += header.difficulty;
            cursor_td.append(key, td.into())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{
        stage_test_suite, ExecuteStageTestRunner, StageTestRunner, UnwindStageTestRunner,
        PREV_STAGE_ID,
    };
    use assert_matches::assert_matches;
    use reth_interfaces::{p2p::error::RequestError, test_utils::generators::random_header};
    use test_runner::HeadersTestRunner;

    stage_test_suite!(HeadersTestRunner);

    /// Check that the execution errors on empty database or
    /// prev progress missing from the database.
    #[tokio::test]
    // Validate that the execution does not fail on timeout
    async fn execute_timeout() {
        let (previous_stage, stage_progress) = (500, 100);
        let mut runner = HeadersTestRunner::default();
        let input = ExecInput {
            previous_stage: Some((PREV_STAGE_ID, previous_stage)),
            stage_progress: Some(stage_progress),
        };
        runner.seed_execution(input).expect("failed to seed execution");
        runner.client.set_error(RequestError::Timeout).await;
        let rx = runner.execute(input);
        runner.consensus.update_tip(H256::from_low_u64_be(1));
        let result = rx.await.unwrap();
        // TODO: Downcast the internal error and actually check it
        assert_matches!(result, Err(StageError::Recoverable(_)));
        assert!(runner.validate_execution(input, result.ok()).is_ok(), "validation failed");
    }

    /// Check that validation error is propagated during the execution.
    #[tokio::test]
    async fn execute_validation_error() {
        let mut runner = HeadersTestRunner::default();
        runner.consensus.set_fail_validation(true);
        let (stage_progress, previous_stage) = (1000, 1200);
        let input = ExecInput {
            previous_stage: Some((PREV_STAGE_ID, previous_stage)),
            stage_progress: Some(stage_progress),
        };
        let headers = runner.seed_execution(input).expect("failed to seed execution");
        let rx = runner.execute(input);
        runner.after_execution(headers).await.expect("failed to run after execution hook");
        let result = rx.await.unwrap();
        assert_matches!(result, Err(StageError::Validation { .. }));
        assert!(runner.validate_execution(input, result.ok()).is_ok(), "validation failed");
    }

    /// Check that unexpected download errors are caught
    #[tokio::test]
    async fn execute_download_error() {
        let mut runner = HeadersTestRunner::default();
        let (stage_progress, previous_stage) = (1000, 1200);
        let input = ExecInput {
            previous_stage: Some((PREV_STAGE_ID, previous_stage)),
            stage_progress: Some(stage_progress),
        };
        let headers = runner.seed_execution(input).expect("failed to seed execution");
        let rx = runner.execute(input);

        runner.client.set_error(RequestError::BadResponse).await;

        // Update tip
        let tip = headers.last().unwrap();
        runner.consensus.update_tip(tip.hash());

        // These errors are not fatal but hand back control to the pipeline
        let result = rx.await.unwrap();
        assert_matches!(result, Err(StageError::Recoverable(_)));
        assert!(runner.validate_execution(input, result.ok()).is_ok(), "validation failed");
    }

    /// Execute the stage with linear downloader
    #[tokio::test]
    async fn execute_with_linear_downloader() {
        let mut runner = HeadersTestRunner::with_linear_downloader();
        let (stage_progress, previous_stage) = (1000, 1200);
        let input = ExecInput {
            previous_stage: Some((PREV_STAGE_ID, previous_stage)),
            stage_progress: Some(stage_progress),
        };
        let headers = runner.seed_execution(input).expect("failed to seed execution");
        let rx = runner.execute(input);

        runner.client.extend(headers.iter().rev().map(|h| h.clone().unseal())).await;

        // skip `after_execution` hook for linear downloader
        let tip = headers.last().unwrap();
        runner.consensus.update_tip(tip.hash());

        let result = rx.await.unwrap();
        assert_matches!(
            result,
            Ok(ExecOutput { done: true, reached_tip: true, stage_progress })
                if stage_progress == tip.number
        );
        assert!(runner.validate_execution(input, result.ok()).is_ok(), "validation failed");
    }

    /// Test the head and tip range lookup
    #[tokio::test]
    async fn head_and_tip_lookup() {
        let runner = HeadersTestRunner::default();
        let db = runner.db().inner();
        let stage = runner.stage();

        let consensus_tip = H256::random();
        stage.consensus.update_tip(consensus_tip);

        // Genesis
        let stage_progress = 0;
        let head = random_header(0, None);
        let gap_fill = random_header(1, Some(head.hash()));
        let gap_tip = random_header(2, Some(gap_fill.hash()));

        // Empty database
        assert_matches!(
            stage.get_head_and_tip(&db, stage_progress).await,
            Err(StageError::DatabaseIntegrity(DatabaseIntegrityError::CanonicalHeader { number }))
                if number == stage_progress
        );

        // Checkpoint and no gap
        db.put::<tables::CanonicalHeaders>(head.number, head.hash())
            .expect("falied to write canonical");
        db.put::<tables::Headers>(head.num_hash().into(), head.clone().unseal())
            .expect("failed to write header");
        assert_matches!(
            stage.get_head_and_tip(&db, stage_progress).await,
            Ok((h, t)) if h == head && t == consensus_tip
        );

        // Checkpoint and gap
        db.put::<tables::CanonicalHeaders>(gap_tip.number, gap_tip.hash())
            .expect("falied to write canonical");
        db.put::<tables::Headers>(gap_tip.num_hash().into(), gap_tip.clone().unseal())
            .expect("failed to write header");
        assert_matches!(
            stage.get_head_and_tip(&db, stage_progress).await,
            Ok((h, t)) if h == head && t == gap_tip.parent_hash
        );

        // Checkpoint and gap closed
        db.put::<tables::CanonicalHeaders>(gap_fill.number, gap_fill.hash())
            .expect("falied to write canonical");
        db.put::<tables::Headers>(gap_fill.num_hash().into(), gap_fill.clone().unseal())
            .expect("failed to write header");
        assert_matches!(
            stage.get_head_and_tip(&db, stage_progress).await,
            Err(StageError::StageProgress(progress)) if progress == stage_progress
        );
    }

    mod test_runner {
        use crate::{
            stages::headers::HeaderStage,
            test_utils::{
                ExecuteStageTestRunner, StageTestRunner, TestRunnerError, TestTransaction,
                UnwindStageTestRunner,
            },
            ExecInput, ExecOutput, UnwindInput,
        };
        use reth_db::{models::blocks::BlockNumHash, tables, transaction::DbTx};
        use reth_downloaders::headers::linear::{LinearDownloadBuilder, LinearDownloader};
        use reth_interfaces::{
            p2p::headers::downloader::HeaderDownloader,
            test_utils::{
                generators::{random_header, random_header_range},
                TestConsensus, TestHeaderDownloader, TestHeadersClient, TestStatusUpdater,
            },
        };
        use reth_primitives::{BlockNumber, SealedHeader, U256};
        use std::sync::Arc;

        pub(crate) struct HeadersTestRunner<D: HeaderDownloader> {
            pub(crate) consensus: Arc<TestConsensus>,
            pub(crate) client: Arc<TestHeadersClient>,
            downloader: Arc<D>,
            network_handle: TestStatusUpdater,
            db: TestTransaction,
        }

        impl Default for HeadersTestRunner<TestHeaderDownloader> {
            fn default() -> Self {
                let client = Arc::new(TestHeadersClient::default());
                let consensus = Arc::new(TestConsensus::default());
                Self {
                    client: client.clone(),
                    consensus: consensus.clone(),
                    downloader: Arc::new(TestHeaderDownloader::new(client, consensus, 1000)),
                    network_handle: TestStatusUpdater::default(),
                    db: TestTransaction::default(),
                }
            }
        }

        impl<D: HeaderDownloader + 'static> StageTestRunner for HeadersTestRunner<D> {
            type S = HeaderStage<Arc<D>, TestConsensus, TestHeadersClient, TestStatusUpdater>;

            fn db(&self) -> &TestTransaction {
                &self.db
            }

            fn stage(&self) -> Self::S {
                HeaderStage {
                    consensus: self.consensus.clone(),
                    client: self.client.clone(),
                    downloader: self.downloader.clone(),
                    network_handle: self.network_handle.clone(),
                    commit_threshold: 100,
                }
            }
        }

        #[async_trait::async_trait]
        impl<D: HeaderDownloader + 'static> ExecuteStageTestRunner for HeadersTestRunner<D> {
            type Seed = Vec<SealedHeader>;

            fn seed_execution(&mut self, input: ExecInput) -> Result<Self::Seed, TestRunnerError> {
                let start = input.stage_progress.unwrap_or_default();
                let head = random_header(start, None);
                self.db.insert_headers(std::iter::once(&head))?;

                // use previous progress as seed size
                let end = input.previous_stage.map(|(_, num)| num).unwrap_or_default() + 1;

                if start + 1 >= end {
                    return Ok(Vec::default())
                }

                let mut headers = random_header_range(start + 1..end, head.hash());
                headers.insert(0, head);
                Ok(headers)
            }

            /// Validate stored headers
            fn validate_execution(
                &self,
                input: ExecInput,
                output: Option<ExecOutput>,
            ) -> Result<(), TestRunnerError> {
                let initial_stage_progress = input.stage_progress.unwrap_or_default();
                match output {
                    Some(output) if output.stage_progress > initial_stage_progress => {
                        self.db.query(|tx| {
                            for block_num in (initial_stage_progress..output.stage_progress).rev() {
                                // look up the header hash
                                let hash = tx
                                    .get::<tables::CanonicalHeaders>(block_num)?
                                    .expect("no header hash");
                                let key: BlockNumHash = (block_num, hash).into();

                                // validate the header number
                                assert_eq!(tx.get::<tables::HeaderNumbers>(hash)?, Some(block_num));

                                // validate the header
                                let header = tx.get::<tables::Headers>(key)?;
                                assert!(header.is_some());
                                let header = header.unwrap().seal();
                                assert_eq!(header.hash(), hash);

                                // validate td consistency in the database
                                if header.number > initial_stage_progress {
                                    let parent_td = tx.get::<tables::HeaderTD>(
                                        (header.number - 1, header.parent_hash).into(),
                                    )?;
                                    let td: U256 = *tx.get::<tables::HeaderTD>(key)?.unwrap();
                                    assert_eq!(
                                        parent_td.map(|td| *td + header.difficulty),
                                        Some(td)
                                    );
                                }
                            }
                            Ok(())
                        })?;
                    }
                    _ => self.check_no_header_entry_above(initial_stage_progress)?,
                };
                Ok(())
            }

            async fn after_execution(&self, headers: Self::Seed) -> Result<(), TestRunnerError> {
                self.client.extend(headers.iter().map(|h| h.clone().unseal())).await;
                let tip = if !headers.is_empty() {
                    headers.last().unwrap().hash()
                } else {
                    let tip = random_header(0, None);
                    self.db.insert_headers(std::iter::once(&tip))?;
                    tip.hash()
                };
                self.consensus.update_tip(tip);
                Ok(())
            }
        }

        impl<D: HeaderDownloader + 'static> UnwindStageTestRunner for HeadersTestRunner<D> {
            fn validate_unwind(&self, input: UnwindInput) -> Result<(), TestRunnerError> {
                self.check_no_header_entry_above(input.unwind_to)
            }
        }

        impl HeadersTestRunner<LinearDownloader<TestConsensus, TestHeadersClient>> {
            #[allow(unused)]
            pub(crate) fn with_linear_downloader() -> Self {
                let client = Arc::new(TestHeadersClient::default());
                let consensus = Arc::new(TestConsensus::default());
                let downloader = Arc::new(
                    LinearDownloadBuilder::default().build(consensus.clone(), client.clone()),
                );
                Self {
                    client,
                    consensus,
                    downloader,
                    network_handle: TestStatusUpdater::default(),
                    db: TestTransaction::default(),
                }
            }
        }

        impl<D: HeaderDownloader> HeadersTestRunner<D> {
            pub(crate) fn check_no_header_entry_above(
                &self,
                block: BlockNumber,
            ) -> Result<(), TestRunnerError> {
                self.db
                    .check_no_entry_above_by_value::<tables::HeaderNumbers, _>(block, |val| val)?;
                self.db.check_no_entry_above::<tables::CanonicalHeaders, _>(block, |key| key)?;
                self.db.check_no_entry_above::<tables::Headers, _>(block, |key| key.number())?;
                self.db.check_no_entry_above::<tables::HeaderTD, _>(block, |key| key.number())?;
                Ok(())
            }
        }
    }
}
