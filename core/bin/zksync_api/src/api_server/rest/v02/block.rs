//! Block part of API implementation.

// Built-in uses
use std::str::FromStr;

// External uses
use actix_web::{web, Scope};

// Workspace uses
use zksync_api_client::rest::v02::{block::BlockInfo, transaction::Transaction};
use zksync_storage::{chain::block::records::BlockDetails, ConnectionPool, QueryResult};
use zksync_types::{
    pagination::{BlockAndTxHash, Paginated, PaginationQuery},
    tx::TxHash,
    BlockNumber,
};

// Local uses
use super::{
    error::{Error, InvalidDataError},
    paginate::Paginate,
    response::ApiResult,
};
use crate::utils::block_details_cache::BlockDetailsCache;

/// Shared data between `api/v0.2/block` endpoints.
#[derive(Debug, Clone)]
struct ApiBlockData {
    pool: ConnectionPool,
    /// Verified blocks cache.
    cache: BlockDetailsCache,
}

impl ApiBlockData {
    fn new(pool: ConnectionPool, cache: BlockDetailsCache) -> Self {
        Self { pool, cache }
    }

    /// Returns information about block with the specified number.
    ///
    /// This method caches some of the verified blocks.
    async fn block_info(&self, block_number: BlockNumber) -> Result<Option<BlockDetails>, Error> {
        self.cache
            .get(&self.pool, block_number)
            .await
            .map_err(Error::storage)
    }

    async fn get_block_number_by_position(
        &self,
        block_position: &str,
    ) -> Result<BlockNumber, Error> {
        if let Ok(number) = u32::from_str(block_position) {
            Ok(BlockNumber(number))
        } else {
            match block_position {
                "last_committed" => match self.get_last_committed_block_number().await {
                    Ok(number) => Ok(number),
                    Err(err) => Err(Error::storage(err)),
                },
                "last_finalized" => match self.get_last_finalized_block_number().await {
                    Ok(number) => Ok(number),
                    Err(err) => Err(Error::storage(err)),
                },
                _ => Err(Error::from(InvalidDataError::InvalidBlockPosition)),
            }
        }
    }

    async fn block_page(
        &self,
        query: PaginationQuery<BlockNumber>,
    ) -> Result<Paginated<BlockInfo, BlockNumber>, Error> {
        let mut storage = self.pool.access_storage().await.map_err(Error::storage)?;
        storage.paginate(&query).await
    }

    async fn transaction_page(
        &self,
        block_number: BlockNumber,
        query: PaginationQuery<TxHash>,
    ) -> Result<Paginated<Transaction, BlockAndTxHash>, Error> {
        let mut storage = self.pool.access_storage().await.map_err(Error::storage)?;

        let new_query = PaginationQuery {
            from: BlockAndTxHash {
                block_number,
                tx_hash: query.from,
            },
            limit: query.limit,
            direction: query.direction,
        };

        storage.paginate(&new_query).await
    }

    async fn get_last_committed_block_number(&self) -> QueryResult<BlockNumber> {
        let mut storage = self.pool.access_storage().await?;
        storage
            .chain()
            .block_schema()
            .get_last_committed_block()
            .await
    }

    async fn get_last_finalized_block_number(&self) -> QueryResult<BlockNumber> {
        let mut storage = self.pool.access_storage().await?;
        storage
            .chain()
            .block_schema()
            .get_last_verified_confirmed_block()
            .await
    }
}

// Server implementation

async fn block_pagination(
    data: web::Data<ApiBlockData>,
    web::Query(query): web::Query<PaginationQuery<BlockNumber>>,
) -> ApiResult<Paginated<BlockInfo, BlockNumber>> {
    data.block_page(query).await.into()
}

// TODO: take `block_position` as enum.
// Currently actix path extractor doesn't work with enums: https://github.com/actix/actix-web/issues/318
async fn block_by_number(
    data: web::Data<ApiBlockData>,
    web::Path(block_position): web::Path<String>,
) -> ApiResult<Option<BlockInfo>> {
    let block_number: BlockNumber;

    match data.get_block_number_by_position(&block_position).await {
        Ok(number) => {
            block_number = number;
        }
        Err(err) => {
            return err.into();
        }
    }

    data.block_info(block_number)
        .await
        .map(|details| details.map(BlockInfo::from))
        .into()
}

async fn block_transactions(
    data: web::Data<ApiBlockData>,
    web::Path(block_position): web::Path<String>,
    web::Query(query): web::Query<PaginationQuery<TxHash>>,
) -> ApiResult<Paginated<Transaction, BlockAndTxHash>> {
    let block_number: BlockNumber;

    match data.get_block_number_by_position(&block_position).await {
        Ok(number) => {
            block_number = number;
        }
        Err(err) => {
            return err.into();
        }
    }

    data.transaction_page(block_number, query).await.into()
}

pub fn api_scope(pool: ConnectionPool, cache: BlockDetailsCache) -> Scope {
    let data = ApiBlockData::new(pool, cache);

    web::scope("block")
        .data(data)
        .route("", web::get().to(block_pagination))
        .route("{block_number}", web::get().to(block_by_number))
        .route(
            "{block_number}/transaction",
            web::get().to(block_transactions),
        )
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            test_utils::{deserialize_response_result, TestServerConfig},
            SharedData,
        },
        *,
    };
    use zksync_api_client::rest::v02::ApiVersion;
    use zksync_types::pagination::PaginationDirection;

    #[actix_rt::test]
    #[cfg_attr(
        not(feature = "api_test"),
        ignore = "Use `zk test rust-api` command to perform this test"
    )]
    async fn v02_test_block_scope() -> anyhow::Result<()> {
        let cfg = TestServerConfig::default();
        cfg.fill_database().await?;

        let shared_data = SharedData {
            net: cfg.config.chain.eth.network,
            api_version: ApiVersion::V02,
        };
        let (client, server) = cfg.start_server(
            |cfg: &TestServerConfig| api_scope(cfg.pool.clone(), BlockDetailsCache::new(10)),
            shared_data,
        );

        let query = PaginationQuery {
            from: BlockNumber(1),
            limit: 3,
            direction: PaginationDirection::Newer,
        };
        let expected_blocks: Paginated<BlockInfo, BlockNumber> = {
            let mut storage = cfg.pool.access_storage().await?;
            storage
                .paginate(&query)
                .await
                .map_err(|err| anyhow::anyhow!(err.message))?
        };

        let response = client.block_by_number_v02("2").await?;
        let block: BlockInfo = deserialize_response_result(response)?;
        assert_eq!(block, expected_blocks.list[1]);

        let response = client.block_pagination_v02(&query).await?;
        let paginated: Paginated<BlockInfo, BlockNumber> = deserialize_response_result(response)?;
        assert_eq!(paginated, expected_blocks);

        let block_number = BlockNumber(3);
        let expected_txs = {
            let mut storage = cfg.pool.access_storage().await?;
            storage
                .chain()
                .block_schema()
                .get_block_transactions(block_number)
                .await?
        };
        assert!(expected_txs.len() >= 3);
        let tx_hash = expected_txs
            .first()
            .unwrap()
            .tx_hash
            .as_str()
            .replace("0x", "sync-tx:");
        let tx_hash = TxHash::from_str(tx_hash.as_str()).unwrap();

        let query = PaginationQuery {
            from: tx_hash,
            limit: 2,
            direction: PaginationDirection::Older,
        };

        let response = client
            .block_transactions_v02(&query, &*block_number.to_string())
            .await?;
        let paginated: Paginated<Transaction, BlockAndTxHash> =
            deserialize_response_result(response)?;
        assert_eq!(paginated.count as usize, expected_txs.len());
        assert_eq!(paginated.limit, query.limit);
        assert!(paginated.list.len() <= query.limit as usize);
        assert_eq!(paginated.direction, PaginationDirection::Older);
        assert_eq!(paginated.from.tx_hash, tx_hash);
        assert_eq!(paginated.from.block_number, block_number);

        for (tx, expected_tx) in paginated.list.into_iter().zip(expected_txs) {
            assert_eq!(
                tx.tx_hash.to_string().replace("sync-tx:", "0x"),
                expected_tx.tx_hash
            );
            assert_eq!(tx.created_at, expected_tx.created_at);
            assert_eq!(*tx.block_number.unwrap(), expected_tx.block_number as u32);
            assert_eq!(tx.fail_reason, expected_tx.fail_reason);
            assert_eq!(tx.op, expected_tx.op);
        }

        server.stop().await;
        Ok(())
    }
}
