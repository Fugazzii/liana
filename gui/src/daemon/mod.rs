pub mod client;
pub mod embedded;
pub mod model;

use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use std::fmt::Debug;
use std::io::ErrorKind;
use std::iter::FromIterator;

use async_trait::async_trait;

use liana::{
    commands::{CoinStatus, LabelItem, TransactionInfo},
    config::Config,
    miniscript::bitcoin::{
        address, bip32::Fingerprint, psbt::Psbt, secp256k1, Address, OutPoint, Txid,
    },
    StartupError,
};

use crate::hw::HardwareWalletConfig;

#[derive(Debug)]
pub enum DaemonError {
    /// Something was wrong with the request.
    Rpc(i32, String),
    /// Something was wrong with the rpc socket communication.
    RpcSocket(Option<ErrorKind>, String),
    /// Something was wrong with the http communication.
    Http(Option<u16>, String),
    /// Something unexpected happened.
    Unexpected(String),
    /// No response.
    NoAnswer,
    /// Daemon stopped
    DaemonStopped,
    // Error at start up.
    Start(StartupError),
    // Error if the client is not supported.
    ClientNotSupported,
    /// Error when selecting coins for spend.
    CoinSelectionError,
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Rpc(code, e) => write!(f, "Daemon error rpc call: [{:?}] {}", code, e),
            Self::NoAnswer => write!(f, "Daemon returned no answer"),
            Self::DaemonStopped => write!(f, "Daemon stopped"),
            Self::RpcSocket(kind, e) => write!(f, "Daemon transport error: [{:?}] {}", kind, e),
            Self::Http(kind, e) => write!(f, "Http error: [{:?}] {}", kind, e),
            Self::Unexpected(e) => write!(f, "Daemon unexpected error: {}", e),
            Self::Start(e) => write!(f, "Daemon did not start: {}", e),
            Self::ClientNotSupported => write!(f, "Daemon communication is not supported"),
            Self::CoinSelectionError => write!(f, "Coin selection error"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonBackend {
    EmbeddedLianad,
    ExternalLianad,
    RemoteBackend,
}

#[async_trait]
pub trait Daemon: Debug {
    fn backend(&self) -> DaemonBackend;
    fn config(&self) -> Option<&Config>;
    async fn is_alive(&self) -> Result<(), DaemonError>;
    async fn stop(&self) -> Result<(), DaemonError>;
    async fn get_info(&self) -> Result<model::GetInfoResult, DaemonError>;
    async fn get_new_address(&self) -> Result<model::GetAddressResult, DaemonError>;
    async fn list_coins(
        &self,
        statuses: &[CoinStatus],
        outpoints: &[OutPoint],
    ) -> Result<model::ListCoinsResult, DaemonError>;
    async fn list_spend_txs(&self) -> Result<model::ListSpendResult, DaemonError>;
    async fn create_spend_tx(
        &self,
        coins_outpoints: &[OutPoint],
        destinations: &HashMap<Address<address::NetworkUnchecked>, u64>,
        feerate_vb: u64,
        change_address: Option<Address<address::NetworkUnchecked>>,
    ) -> Result<model::CreateSpendResult, DaemonError>;
    async fn rbf_psbt(
        &self,
        txid: &Txid,
        is_cancel: bool,
        feerate_vb: Option<u64>,
    ) -> Result<model::CreateSpendResult, DaemonError>;
    async fn update_spend_tx(&self, psbt: &Psbt) -> Result<(), DaemonError>;
    async fn delete_spend_tx(&self, txid: &Txid) -> Result<(), DaemonError>;
    async fn broadcast_spend_tx(&self, txid: &Txid) -> Result<(), DaemonError>;
    async fn start_rescan(&self, t: u32) -> Result<(), DaemonError>;
    async fn list_confirmed_txs(
        &self,
        _start: u32,
        _end: u32,
        _limit: u64,
    ) -> Result<model::ListTransactionsResult, DaemonError>;
    async fn create_recovery(
        &self,
        address: Address<address::NetworkUnchecked>,
        feerate_vb: u64,
        sequence: Option<u16>,
    ) -> Result<Psbt, DaemonError>;
    async fn list_txs(&self, txid: &[Txid]) -> Result<model::ListTransactionsResult, DaemonError>;
    async fn get_labels(
        &self,
        labels: &HashSet<LabelItem>,
    ) -> Result<HashMap<String, String>, DaemonError>;
    async fn update_labels(
        &self,
        labels: &HashMap<LabelItem, Option<String>>,
    ) -> Result<(), DaemonError>;

    // List spend transactions, optionally filtered to the specified `txids`.
    // Set `txids` to `None` for no filter (passing an empty slice returns no transactions).
    async fn list_spend_transactions(
        &self,
        txids: Option<&[Txid]>,
    ) -> Result<Vec<model::SpendTx>, DaemonError> {
        let info = self.get_info().await?;
        let mut spend_txs = Vec::new();
        let curve = secp256k1::Secp256k1::verification_only();
        // TODO: Use filters in `list_spend_txs` command.
        let mut txs = self.list_spend_txs().await?.spend_txs;
        if let Some(txids) = txids {
            txs.retain(|tx| txids.contains(&tx.psbt.unsigned_tx.txid()));
        }
        let outpoints: Vec<_> = txs
            .iter()
            .flat_map(|tx| {
                tx.psbt
                    .unsigned_tx
                    .input
                    .iter()
                    .map(|txin| txin.previous_output)
                    .collect::<Vec<_>>()
            })
            .collect();
        let coins = self.list_coins(&[], &outpoints).await?.coins;
        for tx in txs {
            let coins = coins
                .iter()
                .filter(|coin| {
                    tx.psbt
                        .unsigned_tx
                        .input
                        .iter()
                        .any(|input| input.previous_output == coin.outpoint)
                })
                .cloned()
                .collect();

            spend_txs.push(model::SpendTx::new(
                tx.updated_at,
                tx.psbt,
                coins,
                &info.descriptors.main,
                &curve,
                info.network,
            ));
        }
        load_labels(self, &mut spend_txs).await?;
        spend_txs.sort_by(|a, b| {
            if a.status == b.status {
                // last updated first
                b.updated_at.cmp(&a.updated_at)
            } else {
                // follows status enum order
                a.status.cmp(&b.status)
            }
        });
        Ok(spend_txs)
    }

    async fn txs_to_historytxs(
        &self,
        txs: Vec<TransactionInfo>,
    ) -> Result<Vec<model::HistoryTransaction>, DaemonError> {
        let info = self.get_info().await?;
        let outpoints: Vec<_> = txs
            .iter()
            .flat_map(|tx| {
                (0..tx.tx.output.len())
                    .map(|vout| {
                        OutPoint::new(
                            tx.tx.txid(),
                            vout.try_into()
                                .expect("number of transaction outputs must fit in u32"),
                        )
                    })
                    .chain(tx.tx.input.iter().map(|txin| txin.previous_output))
                    .collect::<Vec<_>>()
            })
            .collect::<HashSet<_>>() // remove duplicates
            .iter()
            .cloned()
            .collect();
        let coins = self.list_coins(&[], &outpoints).await?.coins;
        let mut txs = txs
            .into_iter()
            .map(|tx| {
                let mut tx_coins = Vec::new();
                let mut change_indexes = Vec::new();
                for coin in &coins {
                    if coin.outpoint.txid == tx.tx.txid() {
                        change_indexes.push(coin.outpoint.vout as usize)
                    } else if tx
                        .tx
                        .input
                        .iter()
                        .any(|input| input.previous_output == coin.outpoint)
                    {
                        tx_coins.push(coin.clone());
                    }
                }
                model::HistoryTransaction::new(
                    tx.tx,
                    tx.height,
                    tx.time,
                    tx_coins,
                    change_indexes,
                    info.network,
                )
            })
            .collect();
        load_labels(self, &mut txs).await?;
        Ok(txs)
    }

    async fn list_history_txs(
        &self,
        start: u32,
        end: u32,
        limit: u64,
    ) -> Result<Vec<model::HistoryTransaction>, DaemonError> {
        let txs = self
            .list_confirmed_txs(start, end, limit)
            .await?
            .transactions;
        self.txs_to_historytxs(txs).await
    }

    async fn get_history_txs(
        &self,
        txids: &[Txid],
    ) -> Result<Vec<model::HistoryTransaction>, DaemonError> {
        let txs = self.list_txs(txids).await?.transactions;
        self.txs_to_historytxs(txs).await
    }

    async fn list_pending_txs(&self) -> Result<Vec<model::HistoryTransaction>, DaemonError> {
        let info = self.get_info().await?;
        // We want coins that are inputs to and/or outputs of a pending tx,
        // which can only be unconfirmed and spending coins.
        let coins = self
            .list_coins(&[CoinStatus::Unconfirmed, CoinStatus::Spending], &[])
            .await?
            .coins;
        let mut txids: Vec<Txid> = Vec::new();
        for coin in &coins {
            if coin.block_height.is_none() && !txids.contains(&coin.outpoint.txid) {
                txids.push(coin.outpoint.txid);
            }

            if let Some(spend) = coin.spend_info {
                if !txids.contains(&spend.txid) {
                    txids.push(spend.txid);
                }
            }
        }

        if txids.is_empty() {
            return Ok(Vec::new());
        }

        let txs = self.list_txs(&txids).await?.transactions;
        let mut txs = txs
            .into_iter()
            .map(|tx| {
                let mut tx_coins = Vec::new();
                let mut change_indexes = Vec::new();
                for coin in &coins {
                    if coin.outpoint.txid == tx.tx.txid() {
                        change_indexes.push(coin.outpoint.vout as usize)
                    } else if tx
                        .tx
                        .input
                        .iter()
                        .any(|input| input.previous_output == coin.outpoint)
                    {
                        tx_coins.push(coin.clone());
                    }
                }
                model::HistoryTransaction::new(
                    tx.tx,
                    tx.height,
                    tx.time,
                    tx_coins,
                    change_indexes,
                    info.network,
                )
            })
            .collect();

        load_labels(self, &mut txs).await?;
        Ok(txs)
    }
    /// Implemented by LianaLite backend
    async fn update_wallet_metadata(
        &self,
        _fingerprint_aliases: &HashMap<Fingerprint, String>,
        _hws: &[HardwareWalletConfig],
    ) -> Result<(), DaemonError> {
        Ok(())
    }
}

async fn load_labels<T: model::Labelled, D: Daemon + ?Sized>(
    daemon: &D,
    targets: &mut Vec<T>,
) -> Result<(), DaemonError> {
    if targets.is_empty() {
        return Ok(());
    }
    let mut items = HashSet::<LabelItem>::new();
    for target in &*targets {
        for item in target.labelled() {
            items.insert(item);
        }
    }
    let labels = HashMap::from_iter(
        daemon
            .get_labels(&items)
            .await?
            .into_iter()
            .map(|(k, v)| (k, Some(v))),
    );
    for target in targets {
        target.load_labels(&labels);
    }
    Ok(())
}
