// Copyright (c) 2023 Espresso Systems (espressosys.com)
// This file is part of the Espresso Sequencer-Polygon zkEVM integration demo.
//
// This program is free software: you can redistribute it and/or modify it under the terms of the GNU Affero General Public License as published by the Free Software Foundation, either version 3 of the License, or any later version.
// This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU Affero General Public License for more details.
// You should have received a copy of the GNU Affero General Public License along with this program. If not, see <https://www.gnu.org/licenses/>.

use anyhow::{Error, Result};
use async_std::{channel::Receiver, sync::RwLock, task::JoinHandle};
use clap::Parser;
use ethers::{
    prelude::SignerMiddleware,
    providers::{Http, Middleware as _, Provider, StreamExt, Ws},
    signers::{coins_bip39::English, LocalWallet, MnemonicBuilder, Signer},
    types::{Address, TransactionRequest, H256, U256},
    utils::{parse_ether, ConversionError},
};
use std::{
    collections::{BinaryHeap, HashMap, VecDeque},
    num::ParseIntError,
    ops::Index,
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;
use url::Url;

pub type Middleware = SignerMiddleware<Provider<Http>, LocalWallet>;

#[derive(Parser, Debug, Clone)]
pub struct Options {
    /// Number of Ethereum accounts to use for the faucet.
    ///
    /// This is the number of faucet grant requests that can be executed in
    /// parallel. Each client can only do about one request per block_time
    /// (which is 12 seconds for public Ethereum networks.)
    ///
    /// When initially setting and increasing the number of wallets the faucet
    /// will make sure they are all funded before serving any faucet requests.
    /// However when reducing the number of wallets the faucet will not collect
    /// the funds in the wallets that are no longer used.
    #[arg(long, env = "ESPRESSO_ZKEVM_FAUCET_NUM_CLIENTS", default_value = "10")]
    pub num_clients: usize,

    /// The mnemonic of the faucet wallet.
    #[arg(long, env = "ESPRESSO_ZKEVM_FAUCET_MNEMONIC")]
    pub mnemonic: String,

    /// Port on which to serve the API.
    #[arg(
        short,
        long,
        env = "ESPRESSO_ZKEVM_FAUCET_PORT",
        default_value = "8111"
    )]
    pub port: u16,

    /// The amount of funds to grant to each account on startup in Ethers.
    #[arg(
        long,
        env = "ESPRESSO_ZKEVM_FAUCET_GRANT_AMOUNT_ETHERS",
        value_parser = |arg: &str| -> Result<U256, ConversionError> { Ok(parse_ether(arg)?) }
    )]
    pub faucet_grant_amount: U256,

    /// The time after which a transfer is considered timed out and will be re-sent
    #[arg(
        long,
        env = "ESPRESSO_ZKEVM_FAUCET_TRANSACTION_TIMEOUT_SECS",
        default_value = "300",
        value_parser = |arg: &str| -> Result<Duration, ParseIntError> { Ok(Duration::from_secs(arg.parse::<u64>()?)) }
    )]
    pub transaction_timeout: Duration,

    /// The URL of the JsonRPC the faucet connects to.
    #[arg(long, env = "ESPRESSO_ZKEVM_FAUCET_WEB3_PROVIDER_URL_WS")]
    pub provider_url_ws: Url,

    /// The URL of the JsonRPC the faucet connects to.
    #[arg(long, env = "ESPRESSO_ZKEVM_FAUCET_WEB3_PROVIDER_URL_HTTP")]
    pub provider_url_http: Url,

    /// The authentication token for the discord bot.
    #[arg(long, env = "ESPRESSO_ZKEVM_FAUCET_DISCORD_TOKEN")]
    pub discord_token: Option<String>,

    /// Enable the funding on startup (currently broken).
    #[arg(long, env = "ESPRESSO_ZKEVM_FAUCET_ENABLE_FUNDING")]
    pub enable_funding: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            num_clients: 10,
            mnemonic: "test test test test test test test test test test test junk".to_string(),
            port: 8111,
            faucet_grant_amount: parse_ether("100").unwrap(),
            transaction_timeout: Duration::from_secs(300),
            provider_url_ws: Url::parse("ws://localhost:8545").unwrap(),
            provider_url_http: Url::parse("http://localhost:8545").unwrap(),
            discord_token: None,
            enable_funding: true,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum TransferRequest {
    Faucet {
        to: Address,
        amount: U256,
    },
    Funding {
        to: Address,
        average_wallet_balance: U256,
    },
}

impl TransferRequest {
    pub fn faucet(to: Address, amount: U256) -> Self {
        Self::Faucet { to, amount }
    }

    pub fn funding(to: Address, average_wallet_balance: U256) -> Self {
        Self::Funding {
            to,
            average_wallet_balance,
        }
    }

    pub fn to(&self) -> Address {
        match self {
            Self::Faucet { to, .. } => *to,
            Self::Funding { to, .. } => *to,
        }
    }

    pub fn required_funds(&self) -> U256 {
        match self {
            // Double the faucet amount to be on the safe side regarding gas.
            Self::Faucet { amount, .. } => *amount * 2,
            Self::Funding {
                average_wallet_balance,
                ..
            } => *average_wallet_balance,
        }
    }
}

#[derive(Debug, Clone)]
struct Transfer {
    sender: Arc<Middleware>,
    request: TransferRequest,
    timestamp: Instant,
}

impl Transfer {
    pub fn new(sender: Arc<Middleware>, request: TransferRequest) -> Self {
        Self {
            sender,
            request,
            timestamp: Instant::now(),
        }
    }
}

#[derive(Clone, Debug, Error)]
pub enum TransferError {
    #[error("Error during transfer submission: {transfer:?} {sender:?} {msg}")]
    RpcSubmitError {
        transfer: TransferRequest,
        sender: Address,
        msg: String,
    },
    #[error("No client available")]
    NoClient,
    #[error("No transfers requests available")]
    NoRequests,
}

#[derive(Debug, Clone, Default)]
struct ClientPool {
    clients: HashMap<Address, Arc<Middleware>>,
    priority: BinaryHeap<(U256, Address)>,
}

impl ClientPool {
    pub fn pop(&mut self) -> Option<(U256, Arc<Middleware>)> {
        let (balance, address) = self.priority.pop()?;
        let client = self.clients.remove(&address)?;
        Some((balance, client))
    }

    pub fn push(&mut self, balance: U256, client: Arc<Middleware>) {
        self.clients.insert(client.address(), client.clone());
        self.priority.push((balance, client.address()));
    }

    pub fn has_client_for(&self, transfer: TransferRequest) -> bool {
        self.priority
            .peek()
            .map_or(false, |(balance, _)| *balance >= transfer.required_funds())
    }
}

#[derive(Debug, Clone, Default)]
struct State {
    clients: ClientPool,
    inflight: HashMap<H256, Transfer>,
    clients_being_funded: HashMap<Address, Arc<Middleware>>,
    // Funding wallets has priority, these transfer requests must be pushed to
    // the front.
    transfer_queue: VecDeque<TransferRequest>,
    monitoring_started: bool,
}

#[derive(Debug, Clone)]
pub struct Faucet {
    config: Options,
    state: Arc<RwLock<State>>,
    /// Used to monitor Ethereum transactions.
    provider: Provider<Http>,
    /// Channel to receive faucet requests.
    faucet_receiver: Arc<RwLock<Receiver<Address>>>,
}

impl Faucet {
    /// Create a new faucet.
    ///
    /// Creates `num_clients` wallets and transfers funds and queues transfers
    /// from the ones with most balance to the ones with less than average
    /// balance.
    pub async fn create(options: Options, faucet_receiver: Receiver<Address>) -> Result<Self> {
        // Use a http provider for non-subscribe requests
        let provider = Provider::<Http>::try_from(options.provider_url_http.to_string())?;
        let chain_id = provider.get_chainid().await?.as_u64();

        let mut state = State::default();
        let mut clients = vec![];
        let mut total_balance = U256::zero();

        // Create clients
        for index in 0..options.num_clients {
            let wallet = MnemonicBuilder::<English>::default()
                .phrase(options.mnemonic.as_str())
                .index(index as u32)?
                .build()?
                .with_chain_id(chain_id);
            let client = Arc::new(Middleware::new(provider.clone(), wallet));

            // On startup we may get a "[-32000] failed to get the last block
            // number from state" error even after the request for getChainId is
            // successful.
            let balance = loop {
                if let Ok(balance) = provider.get_balance(client.address(), None).await {
                    break balance;
                }
                tracing::info!("Failed to get balance for client, retrying...");
                async_std::task::sleep(Duration::from_secs(1)).await;
            };

            tracing::info!(
                "Created client {index} {:?} with balance {balance}",
                client.address(),
            );

            total_balance += balance;
            clients.push((balance, client));
        }

        let desired_balance = total_balance / options.num_clients * 8 / 10;

        for (balance, client) in clients {
            // Fund all clients who have significantly less than average balance.
            if options.enable_funding && balance < desired_balance {
                tracing::info!("Queuing funding transfer for {:?}", client.address());
                let transfer = TransferRequest::funding(client.address(), desired_balance);
                state.transfer_queue.push_back(transfer);
                state.clients_being_funded.insert(client.address(), client);
            } else {
                state.clients.push(balance, client);
            }
        }

        Ok(Self {
            config: options,
            state: Arc::new(RwLock::new(state)),
            provider,
            faucet_receiver: Arc::new(RwLock::new(faucet_receiver)),
        })
    }

    pub async fn start(
        self,
    ) -> JoinHandle<(
        Result<(), Error>,
        Result<(), Error>,
        Result<(), Error>,
        Result<(), Error>,
    )> {
        let futures = async move {
            futures::join!(
                self.monitor_transactions(),
                self.monitor_faucet_requests(),
                self.monitor_transaction_timeouts(),
                self.execute_transfers_loop()
            )
        };
        async_std::task::spawn(futures)
    }

    async fn balance(&self, address: Address) -> Result<U256> {
        Ok(self.provider.get_balance(address, None).await?)
    }

    async fn request_transfer(&self, transfer: TransferRequest) {
        tracing::info!("Adding transfer to queue: {:?}", transfer);
        self.state.write().await.transfer_queue.push_back(transfer);
    }

    async fn execute_transfers_loop(&self) -> Result<()> {
        loop {
            if self.state.read().await.monitoring_started {
                break;
            } else {
                tracing::info!("Waiting for transaction monitoring to start...");
                async_std::task::sleep(Duration::from_secs(1)).await;
            }
        }
        loop {
            if let Err(err) = self.execute_transfer().await {
                match err {
                    TransferError::RpcSubmitError { .. } => {
                        tracing::error!("Failed to execute transfer: {:?}", err)
                    }
                    TransferError::NoClient => {
                        tracing::info!("No clients to handle transfer requests.")
                    }
                    TransferError::NoRequests => {}
                };
                // Avoid creating a busy loop.
                async_std::task::sleep(Duration::from_secs(1)).await;
            };
        }
    }

    async fn execute_transfer(&self) -> Result<H256, TransferError> {
        let mut state = self.state.write().await;
        if state.transfer_queue.is_empty() {
            Err(TransferError::NoRequests)?;
        }
        let transfer = state.transfer_queue.index(0);
        if !state.clients.has_client_for(*transfer) {
            Err(TransferError::NoClient)?;
        }
        let (balance, sender) = state.clients.pop().unwrap();
        let transfer = state.transfer_queue.pop_front().unwrap();

        // Drop the guard while we are doing the request to the RPC.
        drop(state);

        let amount = match transfer {
            TransferRequest::Faucet { amount, .. } => amount,
            TransferRequest::Funding { .. } => balance / 2,
        };
        match sender
            .clone()
            .send_transaction(TransactionRequest::pay(transfer.to(), amount), None)
            .await
        {
            Ok(tx) => {
                tracing::info!("Sending transfer: {:?} hash={:?}", transfer, tx.tx_hash());
                // Note: if running against an *extremely* fast chain , it is possible
                // that the transaction is mined before we have a chance to add it to
                // the inflight transfers. In that case, the receipt handler may not yet
                // find the transaction and fail to process it correctly. I think the
                // risk of this happening outside of local testing is neglible. We could
                // sign the tx locally first and then insert it but this also means we
                // would have to remove it again if the submission fails.
                self.state
                    .write()
                    .await
                    .inflight
                    .insert(tx.tx_hash(), Transfer::new(sender.clone(), transfer));
                Ok(tx.tx_hash())
            }
            Err(err) => {
                // Make the client available again.
                self.state
                    .write()
                    .await
                    .clients
                    .push(balance, sender.clone());

                // Requeue the transfer.
                self.request_transfer(transfer).await;

                Err(TransferError::RpcSubmitError {
                    transfer,
                    sender: sender.address(),
                    msg: err.to_string(),
                })?
            }
        }
    }

    async fn handle_receipt(&self, tx_hash: H256) -> Result<()> {
        tracing::debug!("Got tx hash {:?}", tx_hash);

        let Transfer {
            sender, request, ..
        } = {
            if let Some(inflight) = self.state.read().await.inflight.get(&tx_hash) {
                inflight.clone()
            } else {
                // Not a transaction we are monitoring.
                return Ok(());
            }
        };

        // In case there is a race condition and the receipt is not yet available, wait for it.
        let receipt = loop {
            if let Ok(Some(tx)) = self.provider.get_transaction_receipt(tx_hash).await {
                break tx;
            }
            tracing::warn!("No receipt for tx_hash={tx_hash:?}, will retry");
            async_std::task::sleep(Duration::from_secs(1)).await;
        };

        tracing::info!("Received receipt for {:?}", request);

        // Do all external calls before state modifications
        let new_sender_balance = self.balance(sender.address()).await?;

        // For successful funding transfers, we also need to update the receiver's balance.
        let receiver_update = if receipt.status == Some(1.into()) {
            if let TransferRequest::Funding { to: receiver, .. } = request {
                Some((receiver, self.balance(receiver).await?))
            } else {
                None
            }
        } else {
            None
        };

        // Update state, the rest of the operations must be atomic.
        let mut state = self.state.write().await;

        // Make the sender available
        state.clients.push(new_sender_balance, sender.clone());

        // Apply the receiver update, if there is one.
        if let Some((receiver, balance)) = receiver_update {
            if let Some(client) = state.clients_being_funded.remove(&receiver) {
                tracing::info!("Funded client {:?} with {:?}", receiver, balance);
                state.clients.push(balance, client);
            } else {
                tracing::warn!(
                    "Received funding transfer for unknown client {:?}",
                    receiver
                );
            }
        }

        // If the transaction failed, schedule it again.
        if receipt.status == Some(0.into()) {
            // TODO: this code is currently untested.
            tracing::warn!(
                "Transfer failed tx_hash={:?}, will resend: {:?}",
                tx_hash,
                request
            );
            state.transfer_queue.push_back(request);
        };

        // Finally remove the transaction from the inflight list.
        state.inflight.remove(&tx_hash);

        // TODO: I think for transactions with bad nonces we would not even get
        // a transactions receipt. As a result the sending client would remain
        // stuck. As a workaround we could add a timeout to the inflight clients
        // and unlock them after a while. It may be difficult to set a good
        // fixed value for the timeout because the zkevm-node currently waits
        // for hotshot blocks being sequenced in the contract.

        Ok(())
    }

    async fn monitor_transactions(&self) -> Result<()> {
        loop {
            let provider = match Provider::<Ws>::connect(self.config.provider_url_ws.clone()).await
            {
                Ok(provider) => provider,
                Err(err) => {
                    tracing::error!("Failed to connect to provider: {}, will retry", err);
                    async_std::task::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };

            let mut stream = provider
                .subscribe_blocks()
                .await
                .unwrap()
                .flat_map(|block| futures::stream::iter(block.transactions));

            self.state.write().await.monitoring_started = true;
            tracing::info!("Transaction monitoring started ...");
            while let Some(tx_hash) = stream.next().await {
                self.handle_receipt(tx_hash).await?;
            }

            // If we get here, the subscription was closed. This happens for example
            // if the RPC server is restarted.
            tracing::warn!("Block subscription closed, will restart ...");
            async_std::task::sleep(Duration::from_secs(5)).await;
        }
    }

    async fn monitor_faucet_requests(&self) -> Result<()> {
        loop {
            if let Ok(address) = self.faucet_receiver.write().await.recv().await {
                self.request_transfer(TransferRequest::faucet(
                    address,
                    self.config.faucet_grant_amount,
                ))
                .await;
            }
        }
    }

    async fn monitor_transaction_timeouts(&self) -> Result<()> {
        loop {
            async_std::task::sleep(Duration::from_secs(60)).await;
            self.process_transaction_timeouts().await?;
        }
    }

    async fn process_transaction_timeouts(&self) -> Result<()> {
        let inflight = self.state.read().await.inflight.clone();

        for (
            tx_hash,
            Transfer {
                sender, request, ..
            },
        ) in inflight
            .iter()
            .filter(|(_, transfer)| transfer.timestamp.elapsed() > self.config.transaction_timeout)
        {
            tracing::warn!("Transfer timed out: {:?}", request);
            let balance = self.balance(sender.address()).await?;
            let mut state = self.state.write().await;
            state.transfer_queue.push_back(*request);
            state.inflight.remove(tx_hash);
            state.clients.push(balance, sender.clone());
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use async_compatibility_layer::logging::{setup_backtrace, setup_logging};
    use sequencer_utils::AnvilOptions;

    #[async_std::test]
    async fn test_faucet_inflight_timeouts() -> Result<()> {
        setup_logging();
        setup_backtrace();

        let anvil = AnvilOptions::default()
            .block_time(Duration::from_secs(3600))
            .spawn()
            .await;

        let mut ws_url = anvil.url();
        ws_url.set_scheme("ws").unwrap();

        let options = Options {
            num_clients: 1,
            provider_url_ws: ws_url,
            provider_url_http: anvil.url(),
            transaction_timeout: Duration::from_secs(0),
            ..Default::default()
        };

        let (_, receiver) = async_std::channel::unbounded();
        let faucet = Faucet::create(options.clone(), receiver).await?;

        // Manually execute a transfer.
        let transfer = TransferRequest::faucet(Address::zero(), options.faucet_grant_amount);
        faucet.request_transfer(transfer).await;
        faucet.execute_transfer().await?;

        // Assert that there is an inflight transaction.
        assert!(!faucet.state.read().await.inflight.is_empty());

        // Process the timed out transaction.
        faucet.process_transaction_timeouts().await?;
        assert!(faucet.state.read().await.inflight.is_empty());

        // Assert that the client is available again.
        faucet.state.write().await.clients.pop().unwrap();

        // Assert that the transaction was not executed.
        assert_eq!(faucet.balance(Address::zero()).await?, 0.into());

        Ok(())
    }

    // A regression test for a bug where clients that received funding transfers
    // were not made available.
    #[async_std::test]
    async fn test_faucet_funding() -> Result<()> {
        setup_logging();
        setup_backtrace();

        let anvil = AnvilOptions::default().spawn().await;

        let mut ws_url = anvil.url();
        ws_url.set_scheme("ws").unwrap();
        let options = Options {
            // 10 clients are already funded with anvil
            num_clients: 11,
            provider_url_ws: ws_url,
            provider_url_http: anvil.url(),
            ..Default::default()
        };

        let (_, receiver) = async_std::channel::unbounded();
        let faucet = Faucet::create(options.clone(), receiver).await?;

        // There is one client that needs funding.
        assert_eq!(faucet.state.read().await.clients_being_funded.len(), 1);

        let tx_hash = faucet.execute_transfer().await?;
        faucet.handle_receipt(tx_hash).await?;

        let mut state = faucet.state.write().await;
        // The newly funded client is now funded.
        assert_eq!(state.clients_being_funded.len(), 0);
        assert_eq!(state.clients.clients.len(), 11);

        // All clients now have a non-zero balance.
        while let Some((balance, _)) = state.clients.pop() {
            assert!(balance > 0.into());
        }

        Ok(())
    }
}
