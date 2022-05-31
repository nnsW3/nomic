use super::{checkpoint::CheckpointQueue, Bitcoin, SignatorySet};
use crate::bitcoin::{adapter::Adapter, header_queue::WrappedHeader};
use crate::error::Result;
use ::bitcoin::consensus::Decodable as _;
use ::bitcoin::util::merkleblock::PartialMerkleTree;
use bitcoincore_rpc_async::bitcoin;
use bitcoincore_rpc_async::bitcoin::consensus::Encodable;
use bitcoincore_rpc_async::bitcoin::{
    consensus::Decodable,
    hashes::{hex::ToHex, Hash},
    Block, BlockHash, Script, Transaction,
};
use bitcoincore_rpc_async::json::GetBlockHeaderResult;
use bitcoincore_rpc_async::{Client as BitcoinRpcClient, RpcApi};
use orga::client::{AsyncCall, AsyncQuery};
use orga::coins::Address;
use orga::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use tokio::sync::mpsc::Receiver;

const HEADER_BATCH_SIZE: usize = 25;

type BitcoinStateClient<T> = <Bitcoin as Client<T>>::Client;
type CheckpointQueueClient<T> = <CheckpointQueue as Client<T>>::Client;

pub struct Relayer<T: Clone + Send> {
    btc_client: BitcoinRpcClient,
    app_client: BitcoinStateClient<T>,

    scripts: WatchedScriptStore,
}

impl<T: Clone + Send> Relayer<T>
where
    T: AsyncQuery<Query = <Bitcoin as Query>::Query>,
    T: for<'a> AsyncQuery<Response<'a> = &'a Bitcoin>,
    T: AsyncCall<Call = <Bitcoin as Call>::Call>,
{
    pub async fn new<P: AsRef<Path>>(
        store_path: P,
        btc_client: BitcoinRpcClient,
        app_client: BitcoinStateClient<T>,
    ) -> Result<Self> {
        let scripts = WatchedScriptStore::open(store_path, &app_client.checkpoints).await?;
        Ok(Relayer {
            btc_client,
            app_client,
            scripts,
        })
    }

    async fn sidechain_block_hash(&self) -> Result<BlockHash> {
        let hash = self.app_client.headers.hash().await??;
        let hash = BlockHash::from_slice(hash.as_slice())?;
        Ok(hash)
    }

    pub async fn start_header_relay(&mut self) -> Result<!> {
        println!("Starting header relay...");

        loop {
            if let Err(e) = self.relay_headers().await {
                eprintln!("Header relay error: {}", e);
            }

            sleep(2).await;
        }
    }

    async fn relay_headers(&mut self) -> Result<()> {
        let mut last_hash = None;

        loop {
            let fullnode_hash = self.btc_client.get_best_block_hash().await?;
            let sidechain_hash = self.sidechain_block_hash().await?;

            if fullnode_hash != sidechain_hash {
                self.relay_header_batch(fullnode_hash, sidechain_hash)
                    .await?;
                continue;
            }

            if last_hash.is_none() || last_hash.is_some_and(|h| h != &fullnode_hash) {
                last_hash = Some(fullnode_hash);
                let info = self.btc_client.get_block_info(&fullnode_hash).await?;
                println!(
                    "Sidechain header state is up-to-date:\n\thash={}\n\theight={}",
                    info.hash, info.height
                );
            }

            self.btc_client.wait_for_new_block(3_000).await?;
        }
    }

    pub async fn start_deposit_relay(&mut self, mut recv: Receiver<(Address, u32)>) -> Result<!> {
        println!("Starting deposit relay...");

        loop {
            if let Err(e) = self.relay_deposits(&mut recv).await {
                eprintln!("Deposit relay error: {}", e);
            }

            sleep(2).await;
        }
    }

    async fn relay_deposits(&mut self, recv: &mut Receiver<(Address, u32)>) -> Result<!> {
        let mut prev_tip = None;
        loop {
            sleep(2).await;

            self.insert_announced_addrs(recv).await?;

            let tip = self.sidechain_block_hash().await?;
            let prev = prev_tip.unwrap_or(tip);
            if prev_tip.is_some() && prev == tip {
                continue;
            }

            let start_height = self.common_ancestor(tip, prev).await?.height;
            let end_height = self.btc_client.get_block_header_info(&tip).await?.height;
            let num_blocks = (end_height - start_height).max(1100);

            self.scan_for_deposits(num_blocks).await?;

            prev_tip = Some(tip);
        }
    }

    async fn scan_for_deposits(&mut self, num_blocks: usize) -> Result<BlockHash> {
        let tip = self.sidechain_block_hash().await?;
        let base_height = self.btc_client.get_block_header_info(&tip).await?.height;
        let blocks = self.last_n_blocks(num_blocks, tip).await?;

        for (i, block) in blocks.into_iter().enumerate().rev() {
            let height = (base_height - i) as u32;
            for (tx, matches) in self.relevant_txs(&block) {
                for output in matches {
                    self.maybe_relay_deposit(tx, height, &block.block_hash(), output)
                        .await?;
                }
            }
        }

        Ok(tip)
    }

    pub async fn start_checkpoint_relay(&mut self) -> Result<!> {
        println!("Starting checkpoint relay...");

        loop {
            if let Err(e) = self.relay_checkpoints().await {
                eprintln!("Checkpoint relay error: {}", e);
            }

            sleep(2).await;
        }
    }

    async fn relay_checkpoints(&mut self) -> Result<()> {
        loop {
            sleep(10).await;

            let txs = self.app_client.checkpoints.completed_txs().await??;
            for tx in txs {
                use ::bitcoin::consensus::Encodable;
                let mut tx_bytes = vec![];
                tx.consensus_encode(&mut tx_bytes)?;

                match self.btc_client.send_raw_transaction(&tx_bytes).await {
                    Ok(_) => {}
                    Err(err) if err.to_string().contains("bad-txns-inputs-missingorspent") => {}
                    Err(err)
                        if err
                            .to_string()
                            .contains("Transaction already in block chain") => {}
                    Err(err) => Err(err)?,
                }
            }
        }

        Ok(())
    }

    async fn insert_announced_addrs(&mut self, recv: &mut Receiver<(Address, u32)>) -> Result<()> {
        while let Ok((addr, sigset_index)) = recv.try_recv() {
            let checkpoint_res = self.app_client.checkpoints.get(sigset_index).await?;
            let sigset = match &checkpoint_res {
                Ok(checkpoint) => &checkpoint.sigset,
                Err(err) => {
                    eprintln!("{}", err);
                    continue;
                }
            };

            self.scripts.insert(addr, sigset)?;
        }

        self.scripts.scripts.remove_expired()?;

        Ok(())
    }

    pub async fn last_n_blocks(&self, n: usize, hash: BlockHash) -> Result<Vec<Block>> {
        let mut blocks = vec![];

        let mut hash = bitcoin::BlockHash::from_inner(hash.into_inner());

        for _ in 0..n {
            let block = self.btc_client.get_block(&hash.clone()).await?;
            hash = block.header.prev_blockhash;

            let mut block_bytes = vec![];
            block.consensus_encode(&mut block_bytes).unwrap();
            let block = Block::consensus_decode(block_bytes.as_slice()).unwrap();

            blocks.push(block);
        }

        Ok(blocks)
    }

    pub fn relevant_txs<'a>(
        &'a self,
        block: &'a Block,
    ) -> impl Iterator<Item = (&'a Transaction, impl Iterator<Item = OutputMatch> + 'a)> + 'a {
        block
            .txdata
            .iter()
            .map(move |tx| (tx, self.relevant_outputs(tx)))
    }

    pub fn relevant_outputs<'a>(
        &'a self,
        tx: &'a Transaction,
    ) -> impl Iterator<Item = OutputMatch> + 'a {
        tx.output
            .iter()
            .enumerate()
            .filter_map(move |(vout, output)| {
                let mut script_bytes = vec![];
                output
                    .script_pubkey
                    .consensus_encode(&mut script_bytes)
                    .unwrap();
                let script = ::bitcoin::Script::consensus_decode(script_bytes.as_slice()).unwrap();

                self.scripts
                    .scripts
                    .get(&script)
                    .map(|(dest, sigset_index)| OutputMatch {
                        sigset_index,
                        vout: vout as u32,
                        dest,
                    })
            })
    }

    async fn maybe_relay_deposit(
        &self,
        tx: &Transaction,
        height: u32,
        block_hash: &BlockHash,
        output: OutputMatch,
    ) -> Result<()> {
        use self::bitcoin::hashes::Hash as _;
        use ::bitcoin::hashes::Hash as _;

        let txid = tx.txid();
        let outpoint = (txid.into_inner(), output.vout);

        if self
            .app_client
            .processed_outpoints
            .contains(outpoint)
            .await??
        {
            return Ok(());
        }

        let proof_bytes = self
            .btc_client
            .get_tx_out_proof(&[tx.txid()], Some(block_hash))
            .await?;
        let proof = ::bitcoin::MerkleBlock::consensus_decode(proof_bytes.as_slice())?.txn;

        {
            let mut tx_bytes = vec![];
            tx.consensus_encode(&mut tx_bytes)?;
            let tx = ::bitcoin::Transaction::consensus_decode(tx_bytes.as_slice())?;
            let tx = Adapter::new(tx.clone());

            let proof = Adapter::new(proof);

            let res = self.app_client
                .relay_deposit(
                    tx,
                    height,
                    proof,
                    output.vout,
                    output.sigset_index,
                    output.dest,
                )
                .await;

            match res {
                Err(err) if err.to_string().contains("Deposit amount is below minimum") || err.to_string().contains("Deposit amount is too small to pay its spending fee") => {
                    return Ok(());
                },
                _ => res?,
            };
        }

        println!(
            "Relayed deposit: {} sats, {}",
            tx.output[output.vout as usize].value, output.dest
        );

        Ok(())
    }

    async fn relay_header_batch(
        &mut self,
        fullnode_hash: BlockHash,
        sidechain_hash: BlockHash,
    ) -> Result<()> {
        let fullnode_info = self
            .btc_client
            .get_block_header_info(&fullnode_hash)
            .await?;
        let sidechain_info = self
            .btc_client
            .get_block_header_info(&sidechain_hash)
            .await?;

        if fullnode_info.height < sidechain_info.height {
            // full node is still syncing
            return Ok(());
        }

        let start = self.common_ancestor(fullnode_hash, sidechain_hash).await?;
        let batch = self.get_header_batch(start.hash).await?;

        println!(
            "Relaying headers...\n\thash={}\n\theight={}\n\tbatch_len={}",
            batch[0].block_hash(),
            batch[0].height(),
            batch.len(),
        );

        self.app_client.headers.add(batch.into()).await?;
        println!("Relayed headers");

        Ok(())
    }

    async fn get_header_batch(&self, from_hash: BlockHash) -> Result<Vec<WrappedHeader>> {
        let mut cursor = self.btc_client.get_block_header_info(&from_hash).await?;

        let mut headers = Vec::with_capacity(HEADER_BATCH_SIZE as usize);
        for _ in 0..HEADER_BATCH_SIZE {
            match cursor.next_block_hash {
                Some(next_hash) => {
                    cursor = self.btc_client.get_block_header_info(&next_hash).await?
                }
                None => break,
            };

            let header = self.btc_client.get_block_header(&cursor.hash).await?;
            let mut header_bytes = vec![];
            header.consensus_encode(&mut header_bytes).unwrap();
            let header = ::bitcoin::BlockHeader::consensus_decode(header_bytes.as_slice()).unwrap();

            let header = WrappedHeader::from_header(&header, cursor.height as u32);

            headers.push(header);
        }

        Ok(headers)
    }

    async fn common_ancestor(&self, a: BlockHash, b: BlockHash) -> Result<GetBlockHeaderResult> {
        let mut a = self.btc_client.get_block_header_info(&a).await?;
        let mut b = self.btc_client.get_block_header_info(&b).await?;

        while a != b {
            if a.height > b.height && (b.confirmations - 1) as usize == a.height - b.height {
                return Ok(b);
            } else if b.height > a.height && (a.confirmations - 1) as usize == b.height - a.height {
                return Ok(a);
            } else if a.height > b.height {
                let prev = a.previous_block_hash.unwrap();
                a = self.btc_client.get_block_header_info(&prev).await?;
            } else {
                let prev = b.previous_block_hash.unwrap();
                b = self.btc_client.get_block_header_info(&prev).await?;
            }
        }

        Ok(a)
    }
}

pub struct OutputMatch {
    sigset_index: u32,
    vout: u32,
    dest: Address,
}

fn time_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

async fn sleep(seconds: u64) {
    let duration = std::time::Duration::from_secs(seconds);
    tokio::time::sleep(duration).await;
}

/// A collection which stores all watched addresses and signatory sets, for
/// efficiently detecting deposit output scripts.
#[derive(Default)]
pub struct WatchedScripts {
    scripts: HashMap<::bitcoin::Script, (Address, u32)>,
    sigsets: BTreeMap<u32, (SignatorySet, Vec<Address>)>,
}

impl WatchedScripts {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn get(&self, script: &::bitcoin::Script) -> Option<(Address, u32)> {
        self.scripts.get(script).copied()
    }

    pub fn has(&self, script: &::bitcoin::Script) -> bool {
        self.scripts.contains_key(script)
    }

    pub fn len(&self) -> usize {
        self.scripts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.scripts.is_empty()
    }

    pub fn insert(&mut self, addr: Address, sigset: &SignatorySet) -> Result<bool> {
        let script = self.derive_script(addr, sigset)?;

        if self.scripts.contains_key(&script) {
            return Ok(false);
        }

        self.scripts.insert(script, (addr, sigset.index()));

        let (_, addrs) = self
            .sigsets
            .entry(sigset.index())
            .or_insert((sigset.clone(), vec![]));
        addrs.push(addr);

        Ok(true)
    }

    pub fn remove_expired(&mut self) -> Result<()> {
        let now = time_now();

        for (i, (sigset, addrs)) in self.sigsets.iter() {
            if now < sigset.deposit_timeout() {
                break;
            }

            for addr in addrs {
                let script = self.derive_script(*addr, sigset)?;
                self.scripts.remove(&script);
            }
        }

        Ok(())
    }

    fn derive_script(&self, addr: Address, sigset: &SignatorySet) -> Result<::bitcoin::Script> {
        sigset.output_script(addr)
    }
}

use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;

pub struct WatchedScriptStore {
    scripts: WatchedScripts,
    file: File,
}

impl WatchedScriptStore {
    pub async fn open<P: AsRef<Path>, T: Clone + Send>(
        path: P,
        checkpoint_client: &CheckpointQueueClient<T>,
    ) -> Result<Self>
    where
        T: AsyncQuery<Query = <CheckpointQueue as Query>::Query>,
        T: for<'a> AsyncQuery<Response<'a> = &'a CheckpointQueue>,
        T: AsyncCall<Call = <CheckpointQueue as Call>::Call>,
    {
        let mut scripts = WatchedScripts::new();
        Self::maybe_load(&path, &mut scripts, checkpoint_client).await?;

        let mut file = File::create(path)?;
        for (addr, sigset_index) in scripts.scripts.values() {
            Self::write(&mut file, *addr, *sigset_index)?;
        }

        Ok(WatchedScriptStore { scripts, file })
    }

    async fn maybe_load<P: AsRef<Path>, T: Clone + Send>(
        path: P,
        scripts: &mut WatchedScripts,
        client: &CheckpointQueueClient<T>,
    ) -> Result<()>
    where
        T: AsyncQuery<Query = <CheckpointQueue as Query>::Query>,
        T: for<'a> AsyncQuery<Response<'a> = &'a CheckpointQueue>,
        T: AsyncCall<Call = <CheckpointQueue as Call>::Call>,
    {
        let file = match File::open(&path) {
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
            Ok(file) => file,
        };

        let mut sigsets = BTreeMap::new();
        for (index, checkpoint) in client.all().await?? {
            sigsets.insert(index, checkpoint.sigset.clone());
        }

        let lines = BufReader::new(file).lines();
        for line in lines {
            let line = line?;
            let items: Vec<_> = line.split(',').collect();

            let sigset_index: u32 = items[1]
                .parse()
                .map_err(|e| orga::Error::App("Could not parse sigset index".to_string()))?;
            let sigset = match sigsets.get(&sigset_index) {
                Some(sigset) => sigset,
                None => continue,
            };

            let address: Address = items[0]
                .parse()
                .map_err(|e| orga::Error::App("Could not parse address".to_string()))?;

            scripts.insert(address, sigset)?;
        }

        scripts.remove_expired()?;

        Ok(())
    }

    pub fn insert(&mut self, addr: Address, sigset: &SignatorySet) -> Result<()> {
        if self.scripts.insert(addr, sigset)? {
            Self::write(&mut self.file, addr, sigset.index())?;
        }

        Ok(())
    }

    fn write(file: &mut File, addr: Address, sigset_index: u32) -> Result<()> {
        writeln!(file, "{},{}", addr, sigset_index)?;
        Ok(())
    }
}

#[cfg(todo)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitcoin::adapter::Adapter;
    use crate::bitcoin::header_queue::{Config, HeaderQueue};
    use bitcoincore_rpc::Auth;
    use bitcoind::BitcoinD;
    use orga::encoding::Encode;
    use orga::store::{MapStore, Shared, Store};

    #[test]
    fn relayer_seek() {
        let bitcoind = BitcoinD::new(bitcoind::downloaded_exe_path().unwrap()).unwrap();

        let address = bitcoind.client.get_new_address(None, None).unwrap();
        bitcoind.client.generate_to_address(30, &address).unwrap();
        let trusted_hash = bitcoind.client.get_block_hash(30).unwrap();
        let trusted_header = bitcoind.client.get_block_header(&trusted_hash).unwrap();

        let bitcoind_url = bitcoind.rpc_url();
        let bitcoin_cookie_file = bitcoind.params.cookie_file.clone();
        let rpc_client =
            BitcoinRpcClient::new(&bitcoind_url, Auth::CookieFile(bitcoin_cookie_file)).unwrap();

        let encoded_header = Encode::encode(&Adapter::new(trusted_header)).unwrap();
        let mut config: Config = Default::default();
        config.encoded_trusted_header = encoded_header;
        config.trusted_height = 30;
        config.retargeting = false;

        bitcoind.client.generate_to_address(100, &address).unwrap();

        let store = Store::new(Shared::new(MapStore::new()).into());
        let mut header_queue = HeaderQueue::with_conf(store, Default::default(), config).unwrap();
        let relayer = Relayer::new(rpc_client);
        relayer.seek_to_tip(&mut header_queue).unwrap();
        let height = header_queue.height().unwrap();

        assert_eq!(height, 130);
    }

    #[test]
    fn relayer_seek_uneven_batch() {
        let bitcoind = BitcoinD::new(bitcoind::downloaded_exe_path().unwrap()).unwrap();

        let address = bitcoind.client.get_new_address(None, None).unwrap();
        bitcoind.client.generate_to_address(30, &address).unwrap();
        let trusted_hash = bitcoind.client.get_block_hash(30).unwrap();
        let trusted_header = bitcoind.client.get_block_header(&trusted_hash).unwrap();

        let bitcoind_url = bitcoind.rpc_url();
        let bitcoin_cookie_file = bitcoind.params.cookie_file.clone();
        let rpc_client =
            BitcoinRpcClient::new(&bitcoind_url, Auth::CookieFile(bitcoin_cookie_file)).unwrap();

        let encoded_header = Encode::encode(&Adapter::new(trusted_header)).unwrap();
        let mut config: Config = Default::default();
        config.encoded_trusted_header = encoded_header;
        config.trusted_height = 30;
        config.retargeting = false;

        bitcoind
            .client
            .generate_to_address(42 as u64, &address)
            .unwrap();

        let store = Store::new(Shared::new(MapStore::new()));

        let mut header_queue = HeaderQueue::with_conf(store, Default::default(), config).unwrap();
        let relayer = Relayer::new(rpc_client);
        relayer.seek_to_tip(&mut header_queue).unwrap();
        let height = header_queue.height().unwrap();

        assert_eq!(height, 72);
    }
}
