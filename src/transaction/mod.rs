#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransactionId(pub u64);

pub struct TransactionContext {
    pub tx_id: TransactionId,
    pub read_ts: TransactionId,
}

impl TransactionContext {
    pub fn new() -> Self {
        // Global atomic should be used
        TransactionContext {
            tx_id: TransactionId(0),
            read_ts: TransactionId(0),
        }
    }
}
