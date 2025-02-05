use std::{
    fmt::Display,
    ops::{Deref, DerefMut},
    str::FromStr,
};

use bitcoin::{consensus::encode, network};
use ed::{Decode, Encode, Terminated};
use helios_consensus_core::{
    apply_bootstrap, apply_finality_update, apply_update, expected_current_slot,
    types::{
        bls::{PublicKey as HeliosPublicKey, Signature as HeliosSignature},
        Bootstrap as HeliosBootstrap, FinalityUpdate as HeliosFinalityUpdate, Forks, GenericUpdate,
        Header as HeliosHeader, LightClientStore, SyncAggregate as HeliosSyncAggregate,
        SyncCommittee as HeliosSyncCommittee, Update as HeliosUpdate,
    },
    verify_bootstrap, verify_finality_update, verify_update,
};
use orga::{
    call::FieldCall, encoding::LengthVec, migrate::Migrate, orga, query::FieldQuery, state::State,
};
use serde::{Deserialize, Serialize};
use serde_hex::{SerHex, StrictPfx};
use ssz::{Decode as SszDecode, Encode as SszEncode};
use ssz_types::{Bitfield, FixedVector};
use tree_hash::TreeHash;

use crate::error::Result;

#[cfg(feature = "ethereum-full")]
pub mod relayer;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LightClient {
    lcs: LightClientStore,
    network: Network,
}

impl LightClient {
    pub fn new(bootstrap: Bootstrap, network: Network) -> Result<Self> {
        let bootstrap = bootstrap.into();

        verify_bootstrap(&bootstrap, bootstrap.header.tree_hash_root())
            .map_err(|e| orga::Error::App(format!("Invalid bootstrap: {}", e.to_string())))?;

        let mut lcs = LightClientStore::default();
        apply_bootstrap(&mut lcs, &bootstrap);

        Ok(LightClient { lcs, network })
    }

    pub fn update(&mut self, update: Update, now_seconds: u64) -> Result<()> {
        let expected_slot = (now_seconds - self.network.genesis_time) / 12;
        let mut forks = Forks::default();
        forks.deneb.fork_version = (&self.network.deneb_fork_version.to_le_bytes()).into();
        let genesis_root = (&self.network.genesis_vals_root.0).into();

        if update.next_sync_committee.is_some() {
            let update: HeliosUpdate = update.try_into().unwrap();
            verify_update(&update, expected_slot, &self.lcs, genesis_root, &forks)
                .map_err(|e| orga::Error::App(format!("Invalid update: {}", e.to_string())))?;
            apply_update(&mut self.lcs, &update);
        } else {
            let update: HeliosFinalityUpdate = update.into();
            verify_finality_update(&update, expected_slot, &self.lcs, genesis_root, &forks)
                .map_err(|e| orga::Error::App(format!("Invalid update: {}", e.to_string())))?;
            apply_finality_update(&mut self.lcs, &update);
        }

        Ok(())
    }

    pub fn slot(&self) -> u64 {
        self.lcs.finalized_header.slot
    }

    pub fn state_root(&self) -> Bytes32 {
        self.lcs.finalized_header.state_root.0.into()
    }

    pub fn light_client_store(&self) -> &LightClientStore {
        &self.lcs
    }
}

impl State for LightClient {
    fn attach(&mut self, _store: orga::prelude::Store) -> orga::Result<()> {
        Ok(())
    }

    fn field_keyop(_field_name: &str) -> Option<orga::describe::KeyOp> {
        // TODO
        None
    }

    fn flush<W: std::io::Write>(self, out: &mut W) -> orga::Result<()> {
        Ok(self.encode_into(out)?)
    }

    fn load(store: orga::prelude::Store, bytes: &mut &[u8]) -> orga::Result<Self> {
        Ok(Self::decode(bytes)?)
    }
}

impl Migrate for LightClient {
    fn migrate(
        _src: orga::prelude::Store,
        _dest: orga::prelude::Store,
        bytes: &mut &[u8],
    ) -> orga::Result<Self> {
        Ok(Self::decode(bytes)?)
    }
}

impl FieldCall for LightClient {
    type FieldCall = ();

    fn field_call(&mut self, call: ()) -> orga::Result<()> {
        Err(orga::Error::App("FieldCall not supported".to_string()))
    }
}

impl FieldQuery for LightClient {
    type FieldQuery = ();

    fn field_query(&self, query: ()) -> orga::Result<()> {
        Err(orga::Error::App("FieldQuery not supported".to_string()))
    }
}

impl Deref for LightClient {
    type Target = LightClientStore;

    fn deref(&self) -> &Self::Target {
        &self.lcs
    }
}

impl DerefMut for LightClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.lcs
    }
}

impl Encode for LightClient {
    fn encode_into<W: std::io::Write>(&self, dest: &mut W) -> ed::Result<()> {
        encode_header(&self.finalized_header, dest)?;
        encode_sync_committee(&self.current_sync_committee, dest)?;
        self.next_sync_committee
            .as_ref()
            .map(|_| ())
            .encode_into(dest)?;
        if let Some(sc) = &self.next_sync_committee {
            encode_sync_committee(sc, dest)?;
        }
        encode_header(&self.optimistic_header, dest)?;
        self.lcs
            .previous_max_active_participants
            .encode_into(dest)?;
        self.lcs.current_max_active_participants.encode_into(dest)?;
        self.network.encode_into(dest)
    }

    fn encoding_length(&self) -> ed::Result<usize> {
        // TODO: remove need for copying
        Ok(Header(self.lcs.finalized_header.clone()).encoding_length()?
            + SyncCommittee(self.lcs.current_sync_committee.clone()).encoding_length()?
            + self
                .lcs
                .next_sync_committee
                .clone()
                .map(SyncCommittee)
                .encoding_length()?
            + Header(self.lcs.optimistic_header.clone()).encoding_length()?
            + self
                .lcs
                .previous_max_active_participants
                .encoding_length()?
            + self.lcs.current_max_active_participants.encoding_length()?
            + self.network.encoding_length()?)
    }
}

impl Decode for LightClient {
    fn decode<R: std::io::Read>(mut input: R) -> ed::Result<Self> {
        let finalized_header = Header::decode(&mut input)?;
        let current_sync_committee = SyncCommittee::decode(&mut input)?;
        let next_sync_committee = Option::<SyncCommittee>::decode(&mut input)?;
        let optimistic_header = Header::decode(&mut input)?;
        let previous_max_active_participants = u64::decode(&mut input)?;
        let current_max_active_participants = u64::decode(&mut input)?;
        let network = Network::decode(&mut input)?;

        Ok(LightClient {
            lcs: LightClientStore {
                finalized_header: finalized_header.into_inner(),
                current_sync_committee: current_sync_committee.into_inner(),
                next_sync_committee: next_sync_committee.map(|sc| sc.into_inner()),
                optimistic_header: optimistic_header.into_inner(),
                previous_max_active_participants,
                current_max_active_participants,
            },
            network,
        })
    }
}

impl Terminated for LightClient {}

#[derive(Clone, Debug, Default, Encode, Decode, Serialize, Deserialize)]
pub struct Network {
    pub genesis_vals_root: Bytes32,
    pub deneb_fork_version: u32,
    pub genesis_time: u64,
}

impl Network {
    pub fn ethereum_mainnet() -> Self {
        Network {
            genesis_vals_root: "0x4b363db94e286120d76eb905340fdd4e54bfe9f06bf33ff6cf5ad27f511bfe95"
                .parse()
                .unwrap(),
            deneb_fork_version: 4,
            genesis_time: 1606824023,
        }
    }
}

#[derive(Clone, Debug, Encode, Decode, Serialize, Deserialize)]
pub struct Update {
    #[serde(deserialize_with = "wrapped_header::deserialize")]
    pub attested_header: Header,
    pub next_sync_committee: Option<SyncCommittee>,
    pub next_sync_committee_branch: Option<LengthVec<u8, Bytes32>>,
    #[serde(deserialize_with = "wrapped_header::deserialize")]
    pub finalized_header: Header,
    pub finality_branch: LengthVec<u8, Bytes32>,
    pub sync_aggregate: SyncAggregate,
    #[serde(with = "u64_string")]
    pub signature_slot: u64,
}

impl TryFrom<Update> for HeliosUpdate {
    type Error = crate::error::Error;

    fn try_from(value: Update) -> Result<Self> {
        let attested_header = value.attested_header.into_inner();
        let next_sync_committee = value
            .next_sync_committee
            .map(|sc| sc.into_inner())
            .ok_or_else(|| orga::Error::App("next_sync_committee is required".to_string()))?;
        let next_sync_committee_branch = value
            .next_sync_committee_branch
            .map(|branch| {
                Vec::from(branch)
                    .into_iter()
                    .map(|b| b.into_inner().into())
                    .collect()
            })
            .ok_or_else(|| {
                orga::Error::App("next_sync_committee_branch is required".to_string())
            })?;
        let finalized_header = value.finalized_header.into_inner();
        let finality_branch = Vec::from(value.finality_branch)
            .into_iter()
            .map(|b| b.into_inner().into())
            .collect();
        let sync_aggregate = value.sync_aggregate.into_inner();
        let signature_slot = value.signature_slot;

        Ok(HeliosUpdate {
            attested_header,
            next_sync_committee,
            next_sync_committee_branch,
            finalized_header,
            finality_branch,
            sync_aggregate,
            signature_slot,
        })
    }
}

impl From<Update> for HeliosFinalityUpdate {
    fn from(value: Update) -> Self {
        let attested_header = value.attested_header.into_inner();
        let finalized_header = value.finalized_header.into_inner();
        let finality_branch = Vec::from(value.finality_branch)
            .into_iter()
            .map(|b| b.into_inner().into())
            .collect();
        let sync_aggregate = value.sync_aggregate.into_inner();
        let signature_slot = value.signature_slot;

        HeliosFinalityUpdate {
            attested_header,
            finalized_header,
            finality_branch,
            sync_aggregate,
            signature_slot,
        }
    }
}

mod u64_string {
    use serde::{de::Error, Deserializer, Serializer};

    pub fn serialize<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        let val: String = serde::Deserialize::deserialize(deserializer)?;
        val.parse().map_err(D::Error::custom)
    }
}

mod wrapped_header {
    use super::Header;
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Header, D::Error>
    where
        D: Deserializer<'de>,
    {
        let header: LightClientHeader = Deserialize::deserialize(deserializer)?;

        Ok(match header {
            LightClientHeader::Unwrapped(header) => header,
            LightClientHeader::Wrapped(header) => header.beacon,
        })
    }

    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum LightClientHeader {
        Unwrapped(Header),
        Wrapped(Beacon),
    }

    #[derive(serde::Deserialize)]
    struct Beacon {
        beacon: Header,
    }
}

#[derive(Clone, Debug, Encode, Decode, Serialize, Deserialize)]
pub struct Bootstrap {
    #[serde(deserialize_with = "wrapped_header::deserialize")]
    pub header: Header,
    pub current_sync_committee: SyncCommittee,
    pub current_sync_committee_branch: LengthVec<u8, Bytes32>,
}

impl From<Bootstrap> for HeliosBootstrap {
    fn from(value: Bootstrap) -> Self {
        let header = value.header.into_inner();
        let current_sync_committee = value.current_sync_committee.into_inner();
        let current_sync_committee_branch = Vec::from(value.current_sync_committee_branch)
            .into_iter()
            .map(|b| b.into_inner().into())
            .collect();

        HeliosBootstrap {
            header,
            current_sync_committee,
            current_sync_committee_branch,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Header(HeliosHeader);

impl Header {
    pub fn into_inner(self) -> HeliosHeader {
        self.0
    }
}

impl Deref for Header {
    type Target = HeliosHeader;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Header {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Encode for Header {
    fn encode_into<W: std::io::Write>(&self, dest: &mut W) -> ed::Result<()> {
        encode_header(&self.0, dest)
    }

    fn encoding_length(&self) -> ed::Result<usize> {
        Ok(8 + 8 + 32 + 32 + 32)
    }
}

pub fn encode_header<W: std::io::Write>(header: &HeliosHeader, dest: &mut W) -> ed::Result<()> {
    header.slot.encode_into(dest)?;
    header.proposer_index.encode_into(dest)?;
    header.parent_root.0.encode_into(dest)?;
    header.state_root.0.encode_into(dest)?;
    header.body_root.0.encode_into(dest)
}

impl Decode for Header {
    fn decode<R: std::io::Read>(mut input: R) -> ed::Result<Self> {
        let slot = u64::decode(&mut input)?;
        let proposer_index = u64::decode(&mut input)?;
        let parent_root = <[u8; 32]>::decode(&mut input)?;
        let state_root = <[u8; 32]>::decode(&mut input)?;
        let body_root = <[u8; 32]>::decode(&mut input)?;

        Ok(Header(HeliosHeader {
            slot,
            proposer_index,
            parent_root: parent_root.into(),
            state_root: state_root.into(),
            body_root: body_root.into(),
        }))
    }
}

impl Terminated for Header {}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SyncCommittee(HeliosSyncCommittee);

impl SyncCommittee {
    pub fn into_inner(self) -> HeliosSyncCommittee {
        self.0
    }
}

impl Deref for SyncCommittee {
    type Target = HeliosSyncCommittee;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for SyncCommittee {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Encode for SyncCommittee {
    fn encode_into<W: std::io::Write>(&self, dest: &mut W) -> ed::Result<()> {
        encode_sync_committee(&self.0, dest)
    }

    fn encoding_length(&self) -> ed::Result<usize> {
        Ok(512 * 48 + 48)
    }
}

pub fn encode_sync_committee<W: std::io::Write>(
    sc: &HeliosSyncCommittee,
    dest: &mut W,
) -> ed::Result<()> {
    for i in 0..512 {
        encode_public_key(&sc.pubkeys[i], dest)?;
    }
    encode_public_key(&sc.aggregate_pubkey, dest)
}

impl Decode for SyncCommittee {
    fn decode<R: std::io::Read>(mut input: R) -> ed::Result<Self> {
        let mut pubkeys = Vec::with_capacity(512);
        for i in 0..512 {
            pubkeys.push(PublicKey::decode(&mut input)?.into_inner());
        }
        let aggregate_pubkey = PublicKey::decode(&mut input)?.into_inner();

        Ok(SyncCommittee(HeliosSyncCommittee {
            pubkeys: FixedVector::new(pubkeys).unwrap(),
            aggregate_pubkey,
        }))
    }
}

impl Terminated for SyncCommittee {}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SyncAggregate(HeliosSyncAggregate);

impl SyncAggregate {
    pub fn into_inner(self) -> HeliosSyncAggregate {
        self.0
    }
}

impl Deref for SyncAggregate {
    type Target = HeliosSyncAggregate;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for SyncAggregate {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Encode for SyncAggregate {
    fn encode_into<W: std::io::Write>(&self, dest: &mut W) -> ed::Result<()> {
        encode_sync_aggregate(&self.0, dest)
    }

    fn encoding_length(&self) -> ed::Result<usize> {
        Ok(64 + 96)
    }
}

pub fn encode_sync_aggregate<W: std::io::Write>(
    sa: &HeliosSyncAggregate,
    dest: &mut W,
) -> ed::Result<()> {
    sa.sync_committee_bits.as_slice().encode_into(dest)?;
    encode_signature(&sa.sync_committee_signature, dest)
}

impl Decode for SyncAggregate {
    fn decode<R: std::io::Read>(mut input: R) -> ed::Result<Self> {
        let sync_committee_bits = Vec::<u8>::decode(&mut input)?;
        let sync_committee_signature = Signature::decode(&mut input)?.into_inner();

        Ok(SyncAggregate(HeliosSyncAggregate {
            sync_committee_bits: Bitfield::from_ssz_bytes(&sync_committee_bits)
                // TODO: pass through error
                .map_err(|e| ed::Error::UnexpectedByte(34))?,
            sync_committee_signature,
        }))
    }
}

impl Terminated for SyncAggregate {}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PublicKey(HeliosPublicKey);

impl PublicKey {
    pub fn into_inner(self) -> HeliosPublicKey {
        self.0
    }
}

impl From<HeliosPublicKey> for PublicKey {
    fn from(value: HeliosPublicKey) -> Self {
        PublicKey(value)
    }
}

impl Deref for PublicKey {
    type Target = HeliosPublicKey;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for PublicKey {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Encode for PublicKey {
    fn encode(&self) -> ed::Result<Vec<u8>> {
        Ok(self.0.as_ssz_bytes())
    }

    fn encode_into<W: std::io::Write>(&self, dest: &mut W) -> ed::Result<()> {
        self.0.as_ssz_bytes().encode_into(dest)?;
        Ok(())
    }

    fn encoding_length(&self) -> ed::Result<usize> {
        Ok(self.0.ssz_bytes_len())
    }
}

fn encode_public_key<W: std::io::Write>(pk: &HeliosPublicKey, dest: &mut W) -> ed::Result<()> {
    pk.as_ssz_bytes().encode_into(dest)
}

impl Decode for PublicKey {
    fn decode<R: std::io::Read>(mut input: R) -> ed::Result<Self> {
        let mut bytes = [0u8; 48];
        input.read_exact(&mut bytes)?;
        // TODO: pass through error
        let value =
            HeliosPublicKey::from_ssz_bytes(&bytes).map_err(|e| ed::Error::UnexpectedByte(33))?;
        Ok(PublicKey(value))
    }
}

impl Terminated for PublicKey {}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Signature(HeliosSignature);

impl Signature {
    pub fn into_inner(self) -> HeliosSignature {
        self.0
    }
}

impl From<HeliosSignature> for Signature {
    fn from(value: HeliosSignature) -> Self {
        Signature(value)
    }
}

impl Deref for Signature {
    type Target = HeliosSignature;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Signature {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Encode for Signature {
    fn encode(&self) -> ed::Result<Vec<u8>> {
        Ok(self.0.as_ssz_bytes())
    }

    fn encode_into<W: std::io::Write>(&self, dest: &mut W) -> ed::Result<()> {
        self.0.as_ssz_bytes().encode_into(dest)?;
        Ok(())
    }

    fn encoding_length(&self) -> ed::Result<usize> {
        Ok(self.0.ssz_bytes_len())
    }
}

fn encode_signature<W: std::io::Write>(sig: &HeliosSignature, dest: &mut W) -> ed::Result<()> {
    sig.as_ssz_bytes().encode_into(dest)
}

impl Decode for Signature {
    fn decode<R: std::io::Read>(mut input: R) -> ed::Result<Self> {
        let mut bytes = [0u8; 96];
        input.read_exact(&mut bytes)?;
        // TODO: pass through error
        let value =
            HeliosSignature::from_ssz_bytes(&bytes).map_err(|e| ed::Error::UnexpectedByte(33))?;
        Ok(Signature(value))
    }
}

impl Terminated for Signature {}

#[derive(Clone, Debug, Default, Encode, Decode, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Bytes32(#[serde(with = "SerHex::<StrictPfx>")] pub [u8; 32]);

impl Bytes32 {
    pub fn into_inner(self) -> [u8; 32] {
        self.0
    }
}

impl From<[u8; 32]> for Bytes32 {
    fn from(value: [u8; 32]) -> Self {
        Bytes32(value)
    }
}

impl Display for Bytes32 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "0x{}", hex::encode(&self.0))
    }
}

impl FromStr for Bytes32 {
    type Err = crate::error::Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.strip_prefix("0x").unwrap_or(s);
        let bytes = hex::decode(s).map_err(|_| orga::Error::App("Invalid hex".to_string()))?;
        let bytes = bytes
            .as_slice()
            .try_into()
            .map_err(|_| orga::Error::App("Invalid length".to_string()))?;
        Ok(Bytes32(bytes))
    }
}

#[cfg(test)]
mod tests {
    use relayer::Response;

    use super::*;

    #[test]
    fn encode_decode() {
        let pk = PublicKey(HeliosPublicKey::default());
        let bytes = pk.encode().unwrap();
        let pk2 = PublicKey::decode(&bytes[..]).unwrap();

        let lc = LightClient::default();
        let bytes = lc.encode().unwrap();
        let lc = LightClient::decode(&bytes[..]).unwrap();
    }

    #[test]
    fn serialize_deserialize() {
        let pk = PublicKey(HeliosPublicKey::default());
        let pk_str = serde_json::to_string(&pk).unwrap();
        assert_eq!(pk_str, "\"0x000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000\"");
        let pk2: PublicKey = serde_json::from_str(&pk_str).unwrap();

        let sig = Signature(HeliosSignature::default());
        let sig_str = serde_json::to_string(&sig).unwrap();
        assert_eq!(sig_str, "\"0x000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000\"");
        let sig2: Signature = serde_json::from_str(&sig_str).unwrap();
    }

    #[tokio::test]
    async fn update() {
        let fixtures = include_str!("test_fixtures.json");
        let (bootstrap, updates, finality_update): (
            Response<Bootstrap>,
            Vec<Response<Update>>,
            Response<Update>,
        ) = serde_json::from_str(fixtures).unwrap();

        let mut client = LightClient::new(bootstrap.data, Network::ethereum_mainnet()).unwrap();
        for update in updates {
            client.update(update.data, 1727740110).unwrap();
        }
        client.update(finality_update.data, 1727740110).unwrap();

        assert_eq!(client.lcs.finalized_header.slot, 10076224);
    }
}
