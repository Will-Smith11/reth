// TODO: Remove
//! Main db command
//!
//! Database debugging tool

use clap::{Parser, Subcommand};
use eyre::Result;
use reth_db::{
    cursor::{DbCursorRO, Walker},
    database::Database,
    table::Table,
    tables,
    transaction::DbTx,
};
use reth_interfaces::test_utils::generators::random_block_range;
use reth_provider::insert_canonical_block;
use reth_stages::StageDB;
use std::path::Path;
use tracing::info;

/// `reth db` command
#[derive(Debug, Parser)]
pub struct Command {
    /// Path to database folder
    #[arg(long, default_value = "~/.reth/db")]
    db: String,

    #[clap(subcommand)]
    command: Subcommands,
}

const DEFAULT_NUM_ITEMS: &str = "5";

#[derive(Subcommand, Debug)]
/// `reth db` subcommands
pub enum Subcommands {
    /// Lists all the table names, number of entries, and size in KB
    Stats,
    /// Lists the contents of a table
    List(ListArgs),
    /// Seeds the block db with random blocks on top of each other
    Seed {
        /// How many blocks to generate
        #[arg(default_value = DEFAULT_NUM_ITEMS)]
        len: u64,
    },
}

#[derive(Parser, Debug)]
/// The arguments for the `reth db list` command
pub struct ListArgs {
    /// The table name
    table: String, // TODO: Convert to enum
    /// Where to start iterating
    #[arg(long, short, default_value = "0")]
    start: usize,
    /// How many items to take from the walker
    #[arg(long, short, default_value = DEFAULT_NUM_ITEMS)]
    len: usize,
}

impl Command {
    /// Execute `node` command
    pub async fn execute(&self) -> eyre::Result<()> {
        let path = shellexpand::full(&self.db)?.into_owned();
        let expanded_db_path = Path::new(&path);
        std::fs::create_dir_all(expanded_db_path)?;

        // TODO: Auto-impl for Database trait
        let db = reth_db::mdbx::Env::<reth_db::mdbx::WriteMap>::open(
            expanded_db_path,
            reth_db::mdbx::EnvKind::RW,
        )?;
        db.create_tables()?;

        let mut tool = DbTool::new(&db)?;
        match &self.command {
            // TODO: We'll need to add this on the DB trait.
            Subcommands::Stats { .. } => {
                // Get the env from MDBX
                let env = &tool.db.inner().inner;
                let tx = env.begin_ro_txn()?;
                for table in tables::TABLES.iter().map(|(_, name)| name) {
                    let table_db = tx.open_db(Some(table))?;
                    let stats = tx.db_stat(&table_db)?;

                    // Defaults to 16KB right now but we should
                    // re-evaluate depending on the DB we end up using
                    // (e.g. REDB does not have these options as configurable intentionally)
                    let page_size = stats.page_size() as usize;
                    let leaf_pages = stats.leaf_pages();
                    let branch_pages = stats.branch_pages();
                    let overflow_pages = stats.overflow_pages();
                    let num_pages = leaf_pages + branch_pages + overflow_pages;
                    let table_size = page_size * num_pages;
                    tracing::info!(
                        "Table {} has {} entries (total size: {} KB)",
                        table,
                        stats.entries(),
                        table_size / 1024
                    );
                }
            }
            Subcommands::Seed { len } => {
                tool.seed(*len)?;
            }
            Subcommands::List(args) => {
                tool.list(args)?;
            }
        }

        Ok(())
    }
}

/// Abstraction over StageDB for writing/reading from/to the DB
/// Wraps over the StageDB and derefs to a transaction.
struct DbTool<'a, DB: Database> {
    // TODO: StageDB derefs to Tx, is this weird or not?
    pub(crate) db: StageDB<'a, DB>,
}

impl<'a, DB: Database> DbTool<'a, DB> {
    /// Takes a DB where the tables have already been created
    fn new(db: &'a DB) -> eyre::Result<Self> {
        Ok(Self { db: StageDB::new(db)? })
    }

    /// Seeds the database with some random data, only used for testing
    fn seed(&mut self, len: u64) -> Result<()> {
        tracing::info!("generating random block range from 0 to {len}");
        let chain = random_block_range(0..len, Default::default());
        chain.iter().try_for_each(|block| {
            insert_canonical_block(&*self.db, block, true)?;
            Ok::<_, eyre::Error>(())
        })?;
        self.db.commit()?;
        info!("Database committed with {len} blocks");

        Ok(())
    }

    /// Lists the given table data
    fn list(&mut self, args: &ListArgs) -> Result<()> {
        match args.table.as_str() {
            "headers" => self.list_table::<tables::Headers>(args.start, args.len)?,
            "txs" => self.list_table::<tables::Transactions>(args.start, args.len)?,
            _ => panic!(),
        };
        Ok(())
    }

    fn list_table<T: Table>(&mut self, start: usize, len: usize) -> Result<()> {
        let mut cursor = self.db.cursor::<T>()?;

        // TODO: Upstream this in the DB trait.
        let start_walker = cursor.current().transpose();
        let walker = Walker {
            cursor: &mut cursor,
            start: start_walker,
            _tx_phantom: std::marker::PhantomData,
        };

        let data = walker.skip(start).take(len).collect::<Vec<_>>();
        dbg!(&data);

        Ok(())
    }
}
