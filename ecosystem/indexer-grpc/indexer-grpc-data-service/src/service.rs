// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::metrics::{
    ERROR_COUNT, LATEST_PROCESSED_VERSION, OBSERVED_LATEST_PROCESSED_VERSION, PROCESSED_BATCH_SIZE,
    PROCESSED_LATENCY_IN_SECS, PROCESSED_LATENCY_IN_SECS_ALL, PROCESSED_VERSIONS_COUNT,
};
use aptos_indexer_grpc_utils::{
    build_protobuf_encoded_transaction_wrappers,
    cache_operator::{CacheBatchGetStatus, CacheOperator},
    config::IndexerGrpcConfig,
    constants::{GRPC_AUTH_TOKEN_HEADER, GRPC_REQUEST_NAME_HEADER},
    decode_transaction_bytes,
    file_store_operator::FileStoreOperator,
    time_diff_since_pb_timestamp_in_secs, EncodedTransactionWithVersion,
};
use aptos_logger::{error, info, warn};
use aptos_moving_average::MovingAverage;
use aptos_protos::datastream::v1::{
    indexer_stream_server::IndexerStream,
    raw_datastream_response::Response as DatastreamProtoResponse, RawDatastreamRequest,
    RawDatastreamResponse, StreamStatus, TransactionOutput, TransactionsOutput,
};
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::{pin::Pin, sync::Arc, time::Duration};
use tokio::sync::{
    mpsc::{channel, error::TrySendError},
    watch::channel as watch_channel,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use uuid::Uuid;

type ResponseStream = Pin<Box<dyn Stream<Item = Result<RawDatastreamResponse, Status>> + Send>>;

#[derive(Clone, Serialize, Deserialize)]
struct RequestMetadata {
    pub request_id: String,
    pub request_remote_addr: String,
    pub request_token: String,
    pub request_name: String,
    pub request_source: String,
}

const MOVING_AVERAGE_WINDOW_SIZE: u64 = 10_000;
// When trying to fetch beyond the current head of cache, the server will retry after this duration.
const AHEAD_OF_CACHE_RETRY_SLEEP_DURATION_MS: u64 = 50;
// When error happens when fetching data from cache and file store, the server will retry after this duration.
// TODO(larry): fix all errors treated as transient errors.
const TRANSIENT_DATA_ERROR_RETRY_SLEEP_DURATION_MS: u64 = 1000;

// TODO(larry): replace this with a exponential backoff.
// The server will not fetch more data from the cache and file store until the channel is not full.
const RESPONSE_CHANNEL_FULL_BACKOFF_DURATION_MS: u64 = 1000;
// Up to MAX_RESPONSE_CHANNEL_SIZE response can be buffered in the channel. If the channel is full,
// the server will not fetch more data from the cache and file store until the channel is not full.
const MAX_RESPONSE_CHANNEL_SIZE: usize = 40;

pub struct DatastreamServer {
    pub redis_client: Arc<redis::Client>,
    pub server_config: IndexerGrpcConfig,
}

impl DatastreamServer {
    pub fn new(config: IndexerGrpcConfig) -> Self {
        Self {
            redis_client: Arc::new(
                redis::Client::open(format!("redis://{}", config.redis_address))
                    .expect("Create redis client failed."),
            ),
            server_config: config,
        }
    }
}

/// Enum to represent the status of the data fetching overall.
enum TransactionsDataStatus {
    // Data fetching is successful.
    Success(Vec<EncodedTransactionWithVersion>),
    // Ahead of current head of cache.
    AheadOfCache,
    // Fatal error when gap detected between cache and file store.
    DataGap,
}

/// DatastreamServer handles the raw datastream requests from cache and file store.
#[tonic::async_trait]
impl IndexerStream for DatastreamServer {
    type RawDatastreamStream = ResponseStream;

    /// RawDatastream is a streaming GRPC endpoint:
    /// 1. Fetches data from cache and file store.
    ///    1.1. If the data is beyond the current head of cache, retry after a short sleep.
    ///    1.2. If the data is not in cache, fetch the data from file store.
    ///    1.3. If the data is not in file store, stream connection will break.
    ///    1.4  If error happens, retry after a short sleep.
    /// 2. Push data into channel to stream to the client.
    ///    2.1. If the channel is full, do not fetch and retry after a short sleep.
    async fn raw_datastream(
        &self,
        req: Request<RawDatastreamRequest>,
    ) -> Result<Response<Self::RawDatastreamStream>, Status> {
        let request_metadata = match get_request_metadata(&req) {
            Ok(request_metadata) => request_metadata,
            Err(e) => return Result::Err(e),
        };

        // Response channel to stream the data to the client.
        let (tx, rx) = channel(MAX_RESPONSE_CHANNEL_SIZE);
        let mut current_version = match req.into_inner().starting_version {
            Some(version) => version,
            None => {
                return Result::Err(Status::aborted("Starting version is not set"));
            },
        };
        // This is to monitor the latest processed version.
        let (watch_sender, mut watch_receiver) = watch_channel(current_version);

        let file_store_bucket_name = self.server_config.file_store_bucket_name.clone();
        let redis_client = self.redis_client.clone();
        let request_metadata_clone = request_metadata.clone();
        tokio::spawn(async move {
            let request_metadata = request_metadata_clone;
            let conn = match redis_client.get_async_connection().await {
                Ok(conn) => conn,
                Err(e) => {
                    ERROR_COUNT
                        .with_label_values(&["redis_connection_failed"])
                        .inc();
                    tx.send(Err(Status::unavailable(
                        "[Indexer Data] Cannot connect to Redis; please retry.",
                    )))
                    .await
                    .unwrap();
                    error!(
                        request_metadata = request_metadata,
                        error = e.to_string(),
                        "[Indexer Data] Failed to get redis connection."
                    );
                    return;
                },
            };
            let mut cache_operator = CacheOperator::new(conn);
            let file_store_operator = FileStoreOperator::new(file_store_bucket_name);
            file_store_operator.verify_storage_bucket_existence().await;

            let chain_id = match cache_operator.get_chain_id().await {
                Ok(chain_id) => chain_id,
                Err(e) => {
                    ERROR_COUNT
                        .with_label_values(&["redis_get_chain_id_failed"])
                        .inc();
                    tx.send(Err(Status::unavailable(
                        "[Indexer Data] Cannot get the chain id; please retry.",
                    )))
                    .await
                    .unwrap();
                    error!(
                        request_metadata = request_metadata,
                        error = e.to_string(),
                        "[Indexer Data] Failed to get chain id."
                    );
                    return;
                },
            };
            // Data service metrics.
            let mut tps_calculator = MovingAverage::new(MOVING_AVERAGE_WINDOW_SIZE);

            info!(
                chain_id = chain_id,
                current_version = current_version,
                request_metadata = request_metadata,
                "[Indexer Data] New request received."
            );
            tx.send(Ok(RawDatastreamResponse {
                chain_id: chain_id as u32,
                response: Some(DatastreamProtoResponse::Status(StreamStatus {
                    r#type: 1,
                    start_version: current_version,
                    ..StreamStatus::default()
                })),
            }))
            .await
            .unwrap();
            loop {
                // 1. Fetch data from cache and file store.
                let transaction_data =
                    match data_fetch(current_version, &mut cache_operator, &file_store_operator)
                        .await
                    {
                        Ok(TransactionsDataStatus::Success(transactions)) => transactions,
                        Ok(TransactionsDataStatus::AheadOfCache) => {
                            ahead_of_cache_data_handling().await;
                            // Retry after a short sleep.
                            continue;
                        },
                        Ok(TransactionsDataStatus::DataGap) => {
                            data_gap_handling(current_version, &request_metadata);
                            // End the data stream.
                            break;
                        },
                        Err(e) => {
                            ERROR_COUNT.with_label_values(&["data_fetch_failed"]).inc();
                            data_fetch_error_handling(
                                e,
                                current_version,
                                chain_id,
                                &request_metadata,
                            )
                            .await;
                            // Retry after a short sleep.
                            continue;
                        },
                    };

                // 2. Push the data to the response channel, i.e. stream the data to the client.
                let current_batch_size = transaction_data.len();
                let end_of_batch_version = transaction_data.last().unwrap().1;
                let first_transaction_in_batch =
                    decode_transaction_bytes(transaction_data.first().unwrap().0.as_ref()).unwrap();
                let data_latency_in_secs = first_transaction_in_batch
                    .timestamp
                    .as_ref()
                    .map(time_diff_since_pb_timestamp_in_secs);
                let resp_item = raw_datastream_response_builder(transaction_data, chain_id as u32);
                match tx.try_send(Result::<RawDatastreamResponse, Status>::Ok(resp_item)) {
                    Ok(_) => {
                        PROCESSED_BATCH_SIZE
                            .with_label_values(&[
                                request_metadata.request_token.as_str(),
                                request_metadata.request_name.as_str(),
                            ])
                            .set(current_batch_size as i64);
                        LATEST_PROCESSED_VERSION
                            .with_label_values(&[
                                request_metadata.request_token.as_str(),
                                request_metadata.request_name.as_str(),
                            ])
                            .set(end_of_batch_version as i64);
                        PROCESSED_VERSIONS_COUNT
                            .with_label_values(&[
                                request_metadata.request_token.as_str(),
                                request_metadata.request_name.as_str(),
                            ])
                            .inc_by(current_batch_size as u64);
                        if let Some(data_latency_in_secs) = data_latency_in_secs {
                            PROCESSED_LATENCY_IN_SECS
                                .with_label_values(&[
                                    request_metadata.request_token.as_str(),
                                    request_metadata.request_name.as_str(),
                                ])
                                .set(data_latency_in_secs);
                            PROCESSED_LATENCY_IN_SECS_ALL
                                .with_label_values(&[request_metadata.request_source.as_str()])
                                .observe(data_latency_in_secs);
                        }
                    },
                    Err(TrySendError::Full(_)) => {
                        warn!(
                            request_metadata = request_metadata,
                            "[Indexer Data] Receiver is full; retrying."
                        );
                        tokio::time::sleep(Duration::from_millis(
                            RESPONSE_CHANNEL_FULL_BACKOFF_DURATION_MS,
                        ))
                        .await;
                        continue;
                    },
                    Err(TrySendError::Closed(_)) => {
                        ERROR_COUNT
                            .with_label_values(&["response_channel_closed"])
                            .inc();
                        warn!(
                            request_metadata = request_metadata,
                            "[Indexer Data] Receiver is closed; exiting."
                        );
                        break;
                    },
                }
                // 3. Update the current version and record current tps.
                tps_calculator.tick_now(current_batch_size as u64);
                current_version = end_of_batch_version + 1;
                if watch_sender.send(current_version).is_err() {
                    error!(
                        request_metadata = request_metadata,
                        "[Indexer Data] Failed to send the current version to the watch channel."
                    );
                    break;
                }
                info!(
                    request_metadata = request_metadata,
                    current_version = current_version,
                    end_version = end_of_batch_version,
                    batch_size = current_batch_size,
                    tps = (tps_calculator.avg() * 1000.0) as u64,
                    "[Indexer Data] Sending batch."
                );
            }
            info!(
                request_metadata = request_metadata,
                "[Indexer Data] Client disconnected."
            );
        });

        tokio::spawn(async move {
            let request_token = request_metadata.request_token.as_str();
            let request_name = request_metadata.request_name.as_str();
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                match watch_receiver.changed().await.is_ok() {
                    true => {
                        let current_processed_version = *watch_receiver.borrow();
                        OBSERVED_LATEST_PROCESSED_VERSION
                            .with_label_values(&[request_token, request_name])
                            .set(current_processed_version as i64);
                    },
                    false => {
                        info!(
                            request_metadata = request_metadata,
                            "[Indexer Data] Version watch receiver is closed; exiting."
                        );
                        break;
                    },
                }
            }
        });
        let output_stream = ReceiverStream::new(rx);
        Ok(Response::new(
            Box::pin(output_stream) as Self::RawDatastreamStream
        ))
    }
}

/// Builds the response for the raw datastream request. Partial batch is ok, i.e., a batch with transactions < 1000.
fn raw_datastream_response_builder(
    data: Vec<EncodedTransactionWithVersion>,
    chain_id: u32,
) -> RawDatastreamResponse {
    RawDatastreamResponse {
        response: Some(DatastreamProtoResponse::Data(TransactionsOutput {
            transactions: data
                .into_iter()
                .map(|(encoded, version)| TransactionOutput {
                    encoded_proto_data: encoded,
                    version,
                    ..TransactionOutput::default()
                })
                .collect(),
        })),
        chain_id,
    }
}

/// Fetches data from cache or the file store. It returns the data if it is ready in the cache or file store.
/// Otherwise, it returns the status of the data fetching.
async fn data_fetch(
    starting_version: u64,
    cache_operator: &mut CacheOperator<redis::aio::Connection>,
    file_store_operator: &FileStoreOperator,
) -> anyhow::Result<TransactionsDataStatus> {
    let batch_get_result = cache_operator
        .batch_get_encoded_proto_data(starting_version)
        .await;

    match batch_get_result {
        // Data is not ready yet in the cache.
        Ok(CacheBatchGetStatus::NotReady) => Ok(TransactionsDataStatus::AheadOfCache),
        Ok(CacheBatchGetStatus::Ok(transactions)) => Ok(TransactionsDataStatus::Success(
            build_protobuf_encoded_transaction_wrappers(transactions, starting_version),
        )),
        Ok(CacheBatchGetStatus::EvictedFromCache) => {
            // Data is evicted from the cache. Fetch from file store.
            let file_store_batch_get_result =
                file_store_operator.get_transactions(starting_version).await;
            match file_store_batch_get_result {
                Ok(transactions) => Ok(TransactionsDataStatus::Success(
                    build_protobuf_encoded_transaction_wrappers(transactions, starting_version),
                )),
                Err(e) => {
                    if e.to_string().contains("Transactions file not found") {
                        Ok(TransactionsDataStatus::DataGap)
                    } else {
                        Err(e)
                    }
                },
            }
        },
        Err(e) => Err(e),
    }
}

/// Handles the case when the data is not ready in the cache, i.e., beyond the current head.
async fn ahead_of_cache_data_handling() {
    // TODO: add exponential backoff.
    tokio::time::sleep(Duration::from_millis(
        AHEAD_OF_CACHE_RETRY_SLEEP_DURATION_MS,
    ))
    .await;
}

/// Handles data gap errors, i.e., the data is not present in the cache or file store.
fn data_gap_handling(version: u64, request_metadata: &RequestMetadata) {
    // TODO(larry): add metrics/alerts to track the gap.
    // Do not crash the server when gap detected since other clients may still be able to get data.
    error!(
        request_metadata = request_metadata,
        current_version = version,
        "[Indexer Data] Data gap detected. Please check the logs for more details."
    );
}

/// Handles data fetch errors, including cache and file store related errors.
async fn data_fetch_error_handling(
    err: anyhow::Error,
    current_version: u64,
    chain_id: u64,
    request_metadata: &RequestMetadata,
) {
    error!(
        request_metadata = request_metadata,
        chain_id = chain_id,
        current_version = current_version,
        "[Indexer Data] Failed to fetch data from cache and file store. {:?}",
        err
    );
    tokio::time::sleep(Duration::from_millis(
        TRANSIENT_DATA_ERROR_RETRY_SLEEP_DURATION_MS,
    ))
    .await;
}

/// Gets the request metadata. Useful for logging.
fn get_request_metadata(req: &Request<RawDatastreamRequest>) -> tonic::Result<RequestMetadata> {
    // Request id.
    let request_id = Uuid::new_v4().to_string();

    let request_token = match req
        .metadata()
        .get(GRPC_AUTH_TOKEN_HEADER)
        .map(|token| token.to_str())
    {
        Some(Ok(token)) => token.to_string(),
        // It's required to have a valid request token.
        _ => return Result::Err(Status::aborted("Invalid request token")),
    };

    let request_remote_addr = match req.remote_addr() {
        Some(addr) => addr.to_string(),
        None => return Result::Err(Status::aborted("Invalid remote address")),
    };
    let request_name = match req
        .metadata()
        .get(GRPC_REQUEST_NAME_HEADER)
        .map(|desc| desc.to_str())
    {
        Some(Ok(desc)) => desc.to_string(),
        // If the request description is not provided, use "unknown".
        _ => "unknown".to_string(),
    };
    Ok(RequestMetadata {
        request_id,
        request_remote_addr,
        request_token,
        request_name,
        // TODO: after launch, support 'core', 'partner', 'community' and remove 'testing_v1'.
        request_source: "testing_v1".to_string(),
    })
}
