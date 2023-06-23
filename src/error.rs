#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("{0}")]
    Account(String),
    #[error("{0}")]
    Address(String),
    #[error(transparent)]
    Bitcoin(#[from] bitcoin::Error),
    #[error(transparent)]
    BitcoinAddress(#[from] bitcoin::util::address::Error),
    #[error(transparent)]
    BitcoinHash(#[from] bitcoin::hashes::Error),
    #[error(transparent)]
    BitcoinLockTime(#[from] bitcoin::locktime::Error),
    #[error("{0}")]
    BitcoinPubkeyHash(String),
    #[error(transparent)]
    BitcoinEncode(#[from] bitcoin::consensus::encode::Error),
    #[error("Unable to deduct fee: {0}")]
    BitcoinFee(u64),
    #[error("{0}")]
    BitcoinRecoveryScript(String),
    #[error(transparent)]
    Bip32(#[from] bitcoin::util::bip32::Error),
    #[error("{0}")]
    Checkpoint(String),
    #[error(transparent)]
    Sighash(#[from] bitcoin::util::sighash::Error),
    #[error(transparent)]
    TryFrom(#[from] std::num::TryFromIntError),
    #[error("{0}")]
    Test(String),
    #[error(transparent)]
    Secp(#[from] bitcoin::secp256k1::Error),
    #[error("Could not verify merkle proof")]
    BitcoinMerkleBlockError,
    #[cfg(feature = "full")]
    #[error(transparent)]
    BitcoinRpc(#[from] bitcoincore_rpc_async::Error),
    #[error("{0}")]
    Header(String),
    #[error("{0}")]
    Ibc(String),
    #[error("Input index: {0} out of bounds")]
    InputIndexOutOfBounds(usize),
    #[error("Invalid Deposit Address")]
    InvalidDepositAddress,
    #[error(transparent)]
    Orga(#[from] orga::Error),
    #[error(transparent)]
    Ed(#[from] ed::Error),
    #[error("{0}")]
    Relayer(String),
    #[error("Warp Rejection")]
    WarpRejection(),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Unknown Error")]
    Unknown,
}

impl From<warp::Rejection> for Error {
    fn from(_: warp::Rejection) -> Self {
        Error::WarpRejection()
    }
}

impl warp::reject::Reject for Error {}

impl From<Error> for orga::Error {
    fn from(err: Error) -> Self {
        orga::Error::App(err.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
