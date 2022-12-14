use crate::p2p::{bodies::client::BodiesClient, error::PeerRequestResult};
use async_trait::async_trait;
use reth_eth_wire::BlockBody;
use reth_primitives::H256;
use std::fmt::{Debug, Formatter};

/// A test client for fetching bodies
pub struct TestBodiesClient<F> {
    /// The function that is called on each body request.
    pub responder: F,
}

impl<F> Debug for TestBodiesClient<F> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestBodiesClient").finish_non_exhaustive()
    }
}

#[async_trait]
impl<F> BodiesClient for TestBodiesClient<F>
where
    F: Fn(Vec<H256>) -> PeerRequestResult<Vec<BlockBody>> + Send + Sync,
{
    async fn get_block_body(&self, hashes: Vec<H256>) -> PeerRequestResult<Vec<BlockBody>> {
        (self.responder)(hashes)
    }
}
