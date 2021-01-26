use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};

use serde::Serialize;

use bitcoin::{Address, OutPoint, Txid};

use crate::types::{MempoolEntry, ScriptHash, TxStatus};
use crate::util::{remove_if, xpub::Bip32Origin};
use crate::wallet::KeyOrigin;

#[cfg(feature = "track-spends")]
use crate::types::InPoint;

#[derive(Debug, Serialize, Default)]
pub struct MemoryStore {
    scripthashes: HashMap<ScriptHash, ScriptEntry>,
    transactions: HashMap<Txid, TxEntry>,
    mempool: HashMap<Txid, Option<MempoolEntry>>,
    #[cfg(feature = "track-spends")]
    txo_spends: HashMap<OutPoint, InPoint>,
}

#[derive(Debug, Serialize)]
struct ScriptEntry {
    address: Address,
    origin: KeyOrigin,
    history: BTreeSet<HistoryEntry>,
}

#[derive(Clone, Eq, PartialEq, Debug, Hash, Serialize)]
pub struct HistoryEntry {
    pub txid: Txid,
    #[serde(rename = "block_height")]
    pub status: TxStatus,
}

impl HistoryEntry {
    pub fn new(txid: Txid, status: TxStatus) -> Self {
        HistoryEntry { txid, status }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct TxEntry {
    #[serde(rename = "block_height")]
    pub status: TxStatus,
    pub funding: HashMap<u32, FundingInfo>,
    pub spending: HashMap<u32, SpendingInfo>,
}

impl TxEntry {
    pub fn new(status: TxStatus) -> Self {
        TxEntry {
            status,
            funding: HashMap::new(),
            spending: HashMap::new(),
        }
    }
    pub fn scripthashes(&self) -> HashSet<&ScriptHash> {
        let funding_scripthashes = self.funding.iter().map(|(_, f)| &f.0);
        let spending_scripthashes = self.spending.iter().map(|(_, s)| &s.0);
        funding_scripthashes.chain(spending_scripthashes).collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FundingInfo(pub ScriptHash, pub u64);

#[derive(Debug, Clone, Serialize)]
pub struct SpendingInfo(pub ScriptHash, pub OutPoint, pub u64);

impl MemoryStore {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn index_scripthash(
        &mut self,
        scripthash: &ScriptHash,
        origin: &KeyOrigin,
        address: &Address,
    ) -> bool {
        trace!(
            "tracking scripthash={:?} address={:?} origin={:?}",
            scripthash,
            address,
            origin
        );

        let mut existed = false;

        self.scripthashes
            .entry(*scripthash)
            .and_modify(|curr_entry| {
                assert_eq!(
                    curr_entry.origin, *origin,
                    "unexpected stored origin for {:?}",
                    scripthash
                );
                existed = true;
            })
            .or_insert_with(|| ScriptEntry {
                address: address.clone(),
                origin: origin.clone(),
                history: BTreeSet::new(),
            });

        if !existed {
            trace!(
                "new script entry: scripthash={} address={} origin={:?}",
                scripthash,
                address,
                origin
            );
        }

        !existed
    }

    pub fn upsert_tx(&mut self, txid: &Txid, status: TxStatus) -> bool {
        let mut status_change = None;
        let mut updated = false;

        self.transactions
            .entry(*txid)
            .and_modify(|curr_entry| {
                if curr_entry.status != status {
                    status_change = Some(curr_entry.status);
                    curr_entry.status = status;
                    updated = true;
                }
            })
            .or_insert_with(|| {
                trace!("new transaction: txid={} status={:?}", txid, status);
                updated = true;
                TxEntry::new(status)
            });

        if updated {
            match (status_change, status) {
                // update existing transactions with an updated confirmation status
                (Some(old_status), new_status) => {
                    self.update_tx_status(txid, old_status, new_status)
                }

                // add newly indexed mempool transactions to the mempool hashmap, with an empty entry.
                (None, TxStatus::Unconfirmed) => {
                    assert!(self.mempool.insert(*txid, None).is_none());
                }

                _ => (),
            }
        }

        updated
    }

    // index a single txo received by the wallet (there may be more txos from the same tx coming)
    pub fn index_tx_output_funding(
        &mut self,
        txid: &Txid,
        vout: u32,
        funding_info: FundingInfo,
    ) -> bool {
        trace!("index tx output {}:{}: {:?}", txid, vout, funding_info);
        let mut added = None;

        {
            // the tx must already exists by now
            let tx_entry = self.transactions.get_mut(txid).unwrap();
            let status = tx_entry.status;
            tx_entry.funding.entry(vout).or_insert_with(|| {
                trace!("new txo added {}:{}: {:?}", txid, vout, funding_info);
                added = Some((funding_info.0, status));
                funding_info
            });
        }

        if let Some((scripthash, status)) = added {
            self.index_history_entry(&scripthash, HistoryEntry::new(*txid, status));
            true
        } else {
            false
        }
    }

    // index the full set of spending inputs for this transaction
    pub fn index_tx_inputs_spending(
        &mut self,
        txid: &Txid,
        spending: HashMap<u32, SpendingInfo>,
        allow_overwrite: bool,
    ) {
        trace!("index new tx inputs spends {}: {:?}", txid, spending);

        let (status, added_scripthashes) = {
            // the tx must already exists by now
            let tx_entry = self.transactions.get_mut(txid).unwrap();
            assert!(allow_overwrite || tx_entry.spending.is_empty());
            tx_entry.spending = spending;
            let scripthashes: Vec<_> = tx_entry.scripthashes().into_iter().cloned().collect();
            (tx_entry.status, scripthashes)
            // drop mutable ref
        };

        let tx_hist = HistoryEntry::new(*txid, status);
        for scripthash in added_scripthashes {
            self.index_history_entry(&scripthash, tx_hist.clone());
        }
    }

    fn index_history_entry(&mut self, scripthash: &ScriptHash, txhist: HistoryEntry) -> bool {
        trace!(
            "index history entry: scripthash={} txid={} status={:?}",
            scripthash,
            txhist.txid,
            txhist.status
        );

        let added = self
            .scripthashes
            .get_mut(scripthash)
            .expect("missing expected scripthash entry")
            .history
            .insert(txhist);

        if added {
            trace!("new history entry for {:?}", scripthash);
        }

        added
    }

    #[cfg(feature = "track-spends")]
    pub fn index_txo_spend(&mut self, spent_prevout: OutPoint, spending_input: InPoint) -> bool {
        trace!(
            "index txo spend: prevout={:?} spending={:?}",
            spent_prevout,
            spending_input
        );

        let was_unspent = self
            .txo_spends
            .insert(spent_prevout, spending_input)
            .is_none();

        if was_unspent {
            trace!("new txo spend: {:?}", spent_prevout);
        }

        was_unspent
    }

    /// Update the scripthash history index to reflect the new tx status
    fn update_tx_status(&mut self, txid: &Txid, old_status: TxStatus, new_status: TxStatus) {
        trace!(
            "transition tx {:?} from={:?} to={:?}",
            txid,
            old_status,
            new_status
        );

        let tx_entry = self
            .transactions
            .get(txid)
            .expect("missing expected tx entry");

        let old_txhist = HistoryEntry::new(*txid, old_status);
        let new_txhist = HistoryEntry::new(*txid, new_status);

        for scripthash in tx_entry.scripthashes() {
            let scriptentry = self
                .scripthashes
                .get_mut(scripthash)
                .expect("missing expected script entry");
            assert!(scriptentry.history.remove(&old_txhist));
            assert!(scriptentry.history.insert(new_txhist.clone()));
        }

        match (old_status, new_status) {
            (TxStatus::Unconfirmed, _) => assert!(self.mempool.remove(txid).is_some()),
            (_, TxStatus::Unconfirmed) => assert!(self.mempool.insert(*txid, None).is_none()),
            _ => (),
        };
    }

    pub fn purge_tx(&mut self, txid: &Txid) -> bool {
        // XXX should replaced transactions be kept around instead of purged entirely?
        if let Some(old_entry) = self.transactions.remove(txid) {
            debug!("purge tx {:?}", txid);

            if old_entry.status.is_unconfirmed() {
                assert!(self.mempool.remove(txid).is_some());
            }

            let old_txhist = HistoryEntry {
                status: old_entry.status,
                txid: *txid,
            };
            for scripthash in old_entry.scripthashes() {
                // remove the history entry, and remove the script entry entirely if it has no
                // remaining history entries
                let had_entry = remove_if(&mut self.scripthashes, *scripthash, |script_entry| {
                    assert!(script_entry.history.remove(&old_txhist));
                    script_entry.history.is_empty()
                });
                assert!(had_entry)
            }

            #[cfg(feature = "track-spends")]
            for (_, SpendingInfo(_, prevout, _)) in old_entry.spending {
                // remove prevout spending edge, but only if it still references the purged tx
                let had_entry = remove_if(&mut self.txo_spends, prevout, |spending_input| {
                    spending_input.txid == *txid
                });
                assert!(had_entry)
            }

            true
        } else {
            false
        }
    }

    /// Get a mutable reference to the mempool.
    pub fn mempool_mut(&mut self) -> &mut HashMap<Txid, Option<MempoolEntry>> {
        &mut self.mempool
    }

    /// Get a mempool entry. Returns `None` for non-mempool transactions, as well as for
    /// mempool transactions that don't have the MempoolEntry data populated yet.
    pub fn get_mempool_entry(&self, txid: &Txid) -> Option<&MempoolEntry> {
        self.mempool
            .get(txid)
            .and_then(|opt_entry| opt_entry.as_ref())
    }

    pub fn lookup_txo_fund(&self, outpoint: &OutPoint) -> Option<FundingInfo> {
        self.transactions
            .get(&outpoint.txid)?
            .funding
            .get(&outpoint.vout)
            .cloned()
    }

    #[cfg(feature = "track-spends")]
    pub fn lookup_txo_spend(&self, outpoint: &OutPoint) -> Option<InPoint> {
        self.txo_spends.get(outpoint).copied()
    }

    pub fn get_history(&self, scripthash: &ScriptHash) -> Option<&BTreeSet<HistoryEntry>> {
        Some(&self.scripthashes.get(scripthash)?.history)
    }

    pub fn has_history(&self, scripthash: &ScriptHash) -> bool {
        // if the scriptentry exists, it must have some history
        self.scripthashes.contains_key(scripthash)
    }

    pub fn get_tx_count(&self, scripthash: &ScriptHash) -> usize {
        self.scripthashes
            .get(scripthash)
            .map_or(0, |script_entry| script_entry.history.len())
    }

    pub fn get_tx_entry(&self, txid: &Txid) -> Option<&TxEntry> {
        self.transactions.get(txid)
    }

    pub fn get_tx_status(&self, txid: &Txid) -> Option<TxStatus> {
        Some(self.transactions.get(txid)?.status)
    }

    pub fn get_script_info(&self, scripthash: &ScriptHash) -> Option<ScriptInfo> {
        let script_entry = self.scripthashes.get(scripthash)?;
        Some(ScriptInfo::from_entry(*scripthash, script_entry))
    }

    pub fn get_script_address(&self, scripthash: &ScriptHash) -> Option<Address> {
        Some(self.scripthashes.get(scripthash)?.address.clone())
    }

    /// Get all history entries for all scripthashes since `min_block_height` (including
    /// unconfirmed transactions) as refs, ordered with oldest first.
    pub fn get_history_since(&self, min_block_height: u32) -> Vec<&HistoryEntry> {
        // TODO consider keeping a combined sorted index to avoid a full scan
        let mut entries = self
            .scripthashes
            .values()
            .map(|script_entry| {
                script_entry
                    .history
                    .iter()
                    .rev()
                    .take_while(|txhist| match txhist.status {
                        TxStatus::Confirmed(block_height) => block_height >= min_block_height,
                        TxStatus::Unconfirmed => true,
                        TxStatus::Conflicted => unreachable!(),
                    })
            })
            .flatten()
            .collect::<Vec<_>>();
        entries.sort_unstable();
        entries.dedup();
        entries
    }

    pub fn stats(&self) -> StoreStats {
        StoreStats {
            transaction_count: self.transactions.len(),
            scripthash_count: self.scripthashes.len(),
        }
    }
}

#[derive(Serialize, Debug, Clone)]
pub struct ScriptInfo {
    pub address: Address,
    pub scripthash: ScriptHash,
    #[serde(skip_serializing_if = "KeyOrigin::is_standalone")]
    pub origin: KeyOrigin,

    // The descriptor and bip32 origins are only provided in some contexts, not always (even if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bip32_origins: Option<Vec<Bip32Origin>>,
}

impl ScriptInfo {
    pub fn from_desc(
        origin: KeyOrigin,
        address: Address,
        desc: String,
        bip32_origins: Vec<Bip32Origin>,
    ) -> Self {
        ScriptInfo {
            scripthash: ScriptHash::from(&address),
            address,
            origin,
            desc: Some(desc),
            bip32_origins: Some(bip32_origins),
        }
    }
    pub fn from_address(address: Address, origin: KeyOrigin) -> Self {
        ScriptInfo {
            scripthash: ScriptHash::from(&address),
            address,
            origin,
            desc: None,
            bip32_origins: None,
        }
    }
    fn from_entry(scripthash: ScriptHash, script_entry: &ScriptEntry) -> Self {
        ScriptInfo {
            scripthash: scripthash,
            address: script_entry.address.clone(),
            origin: script_entry.origin.clone(),
            desc: None,
            bip32_origins: None,
        }
    }
}

impl Ord for HistoryEntry {
    fn cmp(&self, other: &HistoryEntry) -> Ordering {
        self.status
            .cmp(&other.status)
            .then_with(|| self.txid.cmp(&other.txid))
    }
}

impl PartialOrd for HistoryEntry {
    fn partial_cmp(&self, other: &HistoryEntry) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub struct StoreStats {
    pub transaction_count: usize,
    pub scripthash_count: usize,
}
