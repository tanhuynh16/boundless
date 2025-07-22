// Copyright 2025 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use alloy::{
    primitives::{Address, Signature, U256},
    signers::{Error as SignerErr, Signer},
};
use alloy_primitives::B256;
use alloy_sol_types::SolStruct;
use anyhow::{Context, Result};
use async_stream::stream;
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, Stream, StreamExt};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use siwe::Message as SiweMsg;
use std::pin::Pin;
use thiserror::Error;
use time::OffsetDateTime;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    connect_async, tungstenite, tungstenite::client::IntoClientRequest, MaybeTlsStream,
    WebSocketStream,
};
use utoipa::ToSchema;

use crate::contracts::{eip712_domain, ProofRequest, RequestError};

/// Order stream submission API path.
pub const ORDER_SUBMISSION_PATH: &str = "/api/v1/submit_order";
/// Order stream order list API path.
pub const ORDER_LIST_PATH: &str = "/api/v1/orders";
/// Order stream nonce API path.
pub const AUTH_GET_NONCE: &str = "/api/v1/nonce/";
/// Order stream health check API path.
pub const HEALTH_CHECK: &str = "/api/v1/health";
/// Order stream websocket path.
pub const ORDER_WS_PATH: &str = "/ws/v1/orders";

/// Error body for API responses
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct ErrMsg {
    /// Error type enum
    pub r#type: String,
    /// Error message body
    pub msg: String,
}
impl ErrMsg {
    /// Create a new error message.
    pub fn new(r#type: &str, msg: &str) -> Self {
        Self { r#type: r#type.into(), msg: msg.into() }
    }
}
impl std::fmt::Display for ErrMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "error_type: {} msg: {}", self.r#type, self.msg)
    }
}

/// Error type for the Order
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum OrderError {
    #[error("invalid signature: {0}")]
    /// Invalid signature error.
    InvalidSignature(SignerErr),
    #[error("request error: {0}")]
    /// Request error.
    RequestError(#[from] RequestError),
}

/// Order struct, containing a ProofRequest and its Signature
///
/// The contents of this struct match the calldata of the `submitOrder` function in the `BoundlessMarket` contract.
#[derive(Serialize, Deserialize, ToSchema, Debug, Clone, PartialEq)]
pub struct Order {
    /// Order request
    #[schema(value_type = Object)]
    pub request: ProofRequest,
    /// Request digest
    #[schema(value_type = Object)]
    pub request_digest: B256,
    /// Order signature
    #[schema(value_type = Object)]
    pub signature: Signature,
}

/// Order data + order-stream id
#[derive(Serialize, Deserialize, ToSchema, Debug, Clone)]
pub struct OrderData {
    /// Order stream id
    pub id: i64,
    /// Order data
    pub order: Order,
    /// Time the order was submitted
    #[schema(value_type = String)]
    pub created_at: DateTime<Utc>,
}

/// Nonce object for authentication to order-stream websocket
#[derive(Serialize, Deserialize, ToSchema, Debug, Clone)]
pub struct Nonce {
    /// Nonce hex encoded
    pub nonce: String,
}

/// Response for submitting a new order
#[derive(Serialize, Deserialize, ToSchema, Debug, Clone)]
pub struct SubmitOrderRes {
    /// Status of the order submission
    pub status: String,
    /// Request ID submitted
    #[schema(value_type = Object)]
    pub request_id: U256,
}

impl Order {
    /// Create a new Order
    pub fn new(request: ProofRequest, request_digest: B256, signature: Signature) -> Self {
        Self { request, request_digest, signature }
    }

    /// Validate the Order
    pub fn validate(&self, market_address: Address, chain_id: u64) -> Result<(), OrderError> {
        self.request.validate()?;
        let domain = eip712_domain(market_address, chain_id);
        let hash = self.request.eip712_signing_hash(&domain.alloy_struct());
        if hash != self.request_digest {
            return Err(OrderError::RequestError(RequestError::DigestMismatch));
        }
        self.request.verify_signature(
            &self.signature.as_bytes().into(),
            market_address,
            chain_id,
        )?;
        Ok(())
    }
}

/// Authentication message for connecting to order-stream websock
#[derive(Deserialize, Serialize, ToSchema, Debug, Clone)]
pub struct AuthMsg {
    /// SIWE message body
    #[schema(value_type = Object)]
    message: SiweMsg,
    /// SIWE Signature of `message` field
    #[schema(value_type = Object)]
    signature: Signature,
}

impl AuthMsg {
    /// Creates a new authentication message from a nonce, origin, signer
    pub async fn new(nonce: Nonce, origin: &Url, signer: &impl Signer) -> Result<Self> {
        let message = format!(
            "{} wants you to sign in with your Ethereum account:\n{}\n\nBoundless Order Stream\n\nURI: {}\nVersion: 1\nChain ID: 1\nNonce: {}\nIssued At: {}",
            origin.authority(), signer.address(), origin, nonce.nonce, Utc::now().to_rfc3339(),
        );
        let message: SiweMsg = message.parse()?;

        let signature = signer
            .sign_hash(&message.eip191_hash().context("Failed to generate eip191 hash")?.into())
            .await?;

        Ok(Self { message, signature })
    }

    /// Verify a [AuthMsg] message + signature
    pub async fn verify(&self, domain: &str, nonce: &str) -> Result<()> {
        let opts = siwe::VerificationOpts {
            domain: Some(domain.parse().context("Invalid domain")?),
            nonce: Some(nonce.into()),
            timestamp: Some(OffsetDateTime::now_utc()),
        };

        self.message
            .verify(&self.signature.as_bytes(), &opts)
            .await
            .context("Failed to verify SIWE message")
    }

    /// [AuthMsg] address in alloy format
    pub fn address(&self) -> Address {
        Address::from(self.message.address)
    }
}

/// Client for interacting with the order stream server
#[derive(Clone, Debug)]
pub struct OrderStreamClient {
    /// HTTP client
    pub client: reqwest::Client,
    /// Base URL of the order stream server
    pub base_url: Url,
    /// Address of the market contract
    pub boundless_market_address: Address,
    /// Chain ID of the network
    pub chain_id: u64,
}

impl OrderStreamClient {
    /// Create a new client
    pub fn new(base_url: Url, boundless_market_address: Address, chain_id: u64) -> Self {
        Self { client: reqwest::Client::new(), base_url, boundless_market_address, chain_id }
    }

    /// Submit a proof request to the order stream server
    pub async fn submit_request(
        &self,
        request: &ProofRequest,
        signer: &impl Signer,
    ) -> Result<Order> {
        let url = self.base_url.join(ORDER_SUBMISSION_PATH)?;
        let signature =
            request.sign_request(signer, self.boundless_market_address, self.chain_id).await?;
        let domain = eip712_domain(self.boundless_market_address, self.chain_id);
        let request_digest = request.eip712_signing_hash(&domain.alloy_struct());
        let order = Order { request: request.clone(), request_digest, signature };
        order.validate(self.boundless_market_address, self.chain_id)?;
        let order_json = serde_json::to_value(&order)?;
        let response = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .json(&order_json)
            .send()
            .await?;

        // Check for any errors in the response
        if let Err(err) = response.error_for_status_ref() {
            let error_message = match response.json::<serde_json::Value>().await {
                Ok(json_body) => {
                    json_body["msg"].as_str().unwrap_or("Unknown server error").to_string()
                }
                Err(_) => "Failed to read server error message".to_string(),
            };

            return Err(anyhow::Error::new(err).context(error_message));
        }

        Ok(order)
    }

    /// Fetch an order from the order stream server.
    ///
    /// If multiple orders are found, the `request_digest` must be provided to select the correct order.
    pub async fn fetch_order(&self, id: U256, request_digest: Option<B256>) -> Result<Order> {
        let url = self.base_url.join(&format!("{ORDER_LIST_PATH}/{id}"))?;
        let response = self.client.get(url).send().await?;

        if !response.status().is_success() {
            let error_message = match response.json::<serde_json::Value>().await {
                Ok(json_body) => {
                    json_body["msg"].as_str().unwrap_or("Unknown server error").to_string()
                }
                Err(_) => "Failed to read server error message".to_string(),
            };

            return Err(anyhow::Error::msg(error_message));
        }

        let order_data: Vec<OrderData> = response.json().await?;
        let orders: Vec<Order> = order_data.into_iter().map(|data| data.order).collect();
        if orders.is_empty() {
            return Err(anyhow::Error::msg("No order found"));
        } else if orders.len() == 1 {
            return Ok(orders[0].clone());
        }
        match request_digest {
            Some(digest) => {
                for order in orders {
                    if order.request_digest == digest {
                        return Ok(order);
                    }
                }
                Err(anyhow::Error::msg("No order found"))
            }
            None => {
                Err(anyhow::Error::msg("Multiple orders found, please provide a request digest"))
            }
        }
    }

    /// Get the nonce from the order stream service for websocket auth
    pub async fn get_nonce(&self, address: Address) -> Result<Nonce> {
        let url = self.base_url.join(AUTH_GET_NONCE)?.join(&address.to_string())?;
        let res = self.client.get(url).send().await?;
        if !res.status().is_success() {
            anyhow::bail!("Http error {} fetching nonce", res.status())
        }
        let nonce = res.json().await?;

        Ok(nonce)
    }

    /// Return a WebSocket stream connected to the order stream server
    ///
    /// An authentication message is sent to the server via the `X-Auth-Data` header.
    /// The authentication message must contain a valid claim of an address holding a (pre-configured)
    /// minimum balance on the boundless market in order to connect to the server.
    /// Only one connection per address is allowed.
    pub async fn connect_async(
        &self,
        signer: &impl Signer,
    ) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
        let nonce = self
            .get_nonce(signer.address())
            .await
            .context("Failed to fetch nonce from order-stream")?;

        let auth_msg = AuthMsg::new(nonce, &self.base_url, signer).await?;

        // Serialize the `AuthMsg` to JSON
        let auth_json =
            serde_json::to_string(&auth_msg).context("failed to serialize auth message")?;

        // Construct the WebSocket URL
        let host = self.base_url.host().context("missing host")?.to_string();
        // Select TLS vs not
        let ws_scheme = if self.base_url.scheme() == "https" { "wss" } else { "ws" };

        let ws_url = match self.base_url.port() {
            Some(port) => format!("{ws_scheme}://{host}:{port}{ORDER_WS_PATH}"),
            None => format!("{ws_scheme}://{host}{ORDER_WS_PATH}"),
        };

        // Create the WebSocket request
        let mut request =
            ws_url.clone().into_client_request().context("failed to create request")?;
        request
            .headers_mut()
            .insert("X-Auth-Data", auth_json.parse().context("failed to parse auth message")?);

        // Connect to the WebSocket server and return the socket
        let (socket, _) = match connect_async(request).await {
            Ok(res) => res,
            Err(tokio_tungstenite::tungstenite::Error::Http(err)) => {
                let http_err = if let Some(http_body) = err.body() {
                    String::from_utf8_lossy(http_body)
                } else {
                    "Empty http error body".into()
                };
                anyhow::bail!(
                    "Failed to connect to ws endpoint ({}): {} {}",
                    ws_url,
                    self.base_url,
                    http_err
                );
            }
            Err(err) => {
                anyhow::bail!(
                    "Failed to connect to ws endpoint ({}): {} {}",
                    ws_url,
                    self.base_url,
                    err
                );
            }
        };
        Ok(socket)
    }
}

/// Stream of Order messages from a WebSocket
///
/// This function takes a WebSocket stream and returns a stream of `Order` messages.
/// Example usage:
/// ```no_run
/// use alloy::signers::Signer;
/// use boundless_market::order_stream_client::{OrderStreamClient, order_stream, OrderData};
/// use futures_util::StreamExt;
/// async fn example_stream(client: OrderStreamClient, signer: &impl Signer) {
///     let socket = client.connect_async(signer).await.unwrap();
///     let mut order_stream = order_stream(socket);
///     while let Some(order) = order_stream.next().await {
///         println!("Received order: {:?}", order)
///     }
/// }
/// ```
#[allow(clippy::type_complexity)]
pub fn order_stream(
    mut socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> Pin<Box<dyn Stream<Item = OrderData> + Send>> {
    Box::pin(stream! {
        // NEW: Reduce ping interval for faster connection recovery
        let ping_duration = match std::env::var("ORDER_STREAM_CLIENT_PING_MS") {
            Ok(ms) => match ms.parse::<u64>() {
                Ok(ms) => {
                    tracing::debug!("Using custom ping interval of {}ms", ms);
                    tokio::time::Duration::from_millis(ms)
                },
                Err(_) => {
                    tracing::warn!("Invalid ORDER_STREAM_CLIENT_PING_MS value: {}, using default", ms);
                    tokio::time::Duration::from_secs(10) // NEW: Reduced from 30s to 10s
                }
            },
            Err(_) => tokio::time::Duration::from_secs(10), // NEW: Reduced from 30s to 10s
        };

        let mut ping_interval = tokio::time::interval(ping_duration);
        // Track the last ping we sent
        let mut ping_data: Option<Vec<u8>> = None;
        
        // NEW: Pre-allocate message buffer for faster processing
        let mut message_buffer = String::with_capacity(4096);

        loop {
            tokio::select! {
                // NEW: Use biased select to prioritize message processing
                biased;
                
                // Handle incoming messages
                msg_result = socket.next() => {
                    match msg_result {
                        Some(Ok(tungstenite::Message::Text(msg))) => {
                            // NEW: Use pre-allocated buffer for faster parsing
                            message_buffer.clear();
                            message_buffer.push_str(&msg);
                            
                            match serde_json::from_str::<OrderData>(&message_buffer) {
                                Ok(order) => yield order,
                                Err(err) => {
                                    tracing::warn!("Failed to parse order: {:?}", err);
                                    continue;
                                }
                            }
                        }
                        // Reply to Ping's inline
                        Some(Ok(tungstenite::Message::Ping(data))) => {
                            tracing::trace!("Responding to ping");
                            if let Err(err) = socket.send(tungstenite::Message::Pong(data)).await {
                                tracing::warn!("Failed to send pong: {:?}", err);
                                break;
                            }
                        }
                        // Handle Pong responses
                        Some(Ok(tungstenite::Message::Pong(data))) => {
                            tracing::trace!("Received pong from server");
                            if let Some(expected_data) = ping_data.take() {
                                if data != expected_data {
                                    tracing::warn!("Server responded with invalid pong data");
                                    break;
                                }
                            } else {
                                tracing::warn!("Received unexpected pong from order-stream server");
                            }
                        }
                        Some(Ok(tungstenite::Message::Close(_))) => {
                            tracing::info!("WebSocket connection closed by server");
                            break;
                        }
                        Some(Ok(tungstenite::Message::Binary(_))) => {
                            tracing::warn!("Received unexpected binary message from server");
                            continue;
                        }
                        Some(Ok(tungstenite::Message::Frame(_))) => {
                            tracing::warn!("Received unexpected frame message from server");
                            continue;
                        }
                        Some(Err(err)) => {
                            tracing::error!("WebSocket error: {:?}", err);
                            break;
                        }
                        None => {
                            tracing::info!("WebSocket stream ended");
                            break;
                        }
                    }
                }
                
                // NEW: More frequent ping for better connection stability
                _ = ping_interval.tick() => {
                    let ping_bytes = rand::random::<[u8; 4]>();
                    ping_data = Some(ping_bytes.to_vec());
                    
                    if let Err(err) = socket.send(tungstenite::Message::Ping(ping_bytes.to_vec())).await {
                        tracing::warn!("Failed to send ping: {:?}", err);
                        break;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::signers::local::LocalSigner;

    #[tokio::test]
    async fn auth_msg_verify() {
        let signer = LocalSigner::random();
        let nonce = Nonce { nonce: "TEST_NONCE".to_string() };
        let origin = "http://localhost:8585".parse().unwrap();
        let auth_msg = AuthMsg::new(nonce.clone(), &origin, &signer).await.unwrap();
        auth_msg.verify("localhost:8585", &nonce.nonce).await.unwrap();
    }

    #[tokio::test]
    #[should_panic(expected = "Message domain does not match")]
    async fn auth_msg_bad_origin() {
        let signer = LocalSigner::random();
        let nonce = Nonce { nonce: "TEST_NONCE".to_string() };
        let origin = "http://localhost:8585".parse().unwrap();
        let auth_msg = AuthMsg::new(nonce.clone(), &origin, &signer).await.unwrap();
        auth_msg.verify("boundless.xyz", &nonce.nonce).await.unwrap();
    }

    #[tokio::test]
    #[should_panic(expected = "Message nonce does not match")]
    async fn auth_msg_bad_nonce() {
        let signer = LocalSigner::random();
        let nonce = Nonce { nonce: "TEST_NONCE".to_string() };
        let origin = "http://localhost:8585".parse().unwrap();
        let auth_msg = AuthMsg::new(nonce.clone(), &origin, &signer).await.unwrap();
        auth_msg.verify("localhost:8585", "BAD_NONCE").await.unwrap();
    }
}
