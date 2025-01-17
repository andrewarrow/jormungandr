use crate::{
    blockcfg::{ApplyBlockLedger, LedgerParameters},
    fragment::{
        selection::{FragmentSelectionAlgorithm, FragmentSelectionAlgorithmParams, OldestFirst},
        Fragment, FragmentId, Logs,
    },
    intercom::{NetworkMsg, PropagateMsg},
    utils::async_msg::MessageBox,
};
use chain_core::property::Fragment as _;
use chain_impl_mockchain::{fragment::Contents, transaction::Transaction};
use futures::channel::mpsc::SendError;
use futures::sink::SinkExt;
use jormungandr_lib::interfaces::{FragmentLog, FragmentOrigin, FragmentStatus};
use thiserror::Error;

pub struct Pools {
    logs: Logs,
    pools: Vec<internal::Pool>,
    network_msg_box: MessageBox<NetworkMsg>,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("cannot propagate a fragment to the network")]
    CannotPropagate(#[source] SendError),
}

impl Pools {
    pub fn new(
        max_entries: usize,
        n_pools: usize,
        logs: Logs,
        network_msg_box: MessageBox<NetworkMsg>,
    ) -> Self {
        let pools = (0..=n_pools)
            .map(|_| internal::Pool::new(max_entries))
            .collect();
        Pools {
            logs,
            pools,
            network_msg_box,
        }
    }

    pub fn logs(&mut self) -> &mut Logs {
        &mut self.logs
    }

    /// Returns number of registered fragments
    pub async fn insert_and_propagate_all(
        &mut self,
        origin: FragmentOrigin,
        mut fragments: Vec<Fragment>,
    ) -> Result<usize, Error> {
        tracing::debug!(origin = ?origin, "received {} fragments", fragments.len());
        fragments.retain(is_fragment_valid);
        if fragments.is_empty() {
            tracing::debug!("none of the received fragments are valid");
            return Ok(0);
        }
        let mut network_msg_box = self.network_msg_box.clone();
        let fragment_ids = fragments.iter().map(Fragment::id).collect::<Vec<_>>();
        let fragments_exist_in_logs = self.logs.exist_all(fragment_ids);
        let new_fragments = fragments
            .into_iter()
            .zip(fragments_exist_in_logs)
            .filter(|(_, exists_in_logs)| !exists_in_logs)
            .map(|(fragment, _)| fragment);

        let mut max_added = 0;

        for (i, pool) in self.pools.iter_mut().enumerate() {
            let new_fragments = pool.insert_all(new_fragments.clone());
            let count = new_fragments.len();
            tracing::debug!(
                "{} of the received fragments were added to the pool number {}",
                count,
                i
            );
            let fragment_logs = new_fragments
                .iter()
                .map(move |fragment| FragmentLog::new(fragment.id(), origin))
                .collect::<Vec<_>>();
            self.logs.insert_all(fragment_logs);
            if count > max_added {
                max_added = count;
            }
        }

        for fragment in new_fragments.into_iter() {
            let fragment_msg = NetworkMsg::Propagate(PropagateMsg::Fragment(fragment));
            network_msg_box
                .send(fragment_msg)
                .await
                .map_err(Error::CannotPropagate)?;
        }

        Ok(max_added)
    }

    pub fn remove_added_to_block(&mut self, fragment_ids: Vec<FragmentId>, status: FragmentStatus) {
        for pool in &mut self.pools {
            pool.remove_all(fragment_ids.iter());
        }
        self.logs.modify_all(fragment_ids, status);
    }

    pub async fn select(
        &mut self,
        pool_idx: usize,
        ledger: ApplyBlockLedger,
        ledger_params: LedgerParameters,
        selection_alg: FragmentSelectionAlgorithmParams,
        soft_deadline_future: futures::channel::oneshot::Receiver<()>,
        hard_deadline_future: futures::channel::oneshot::Receiver<()>,
    ) -> (Contents, ApplyBlockLedger) {
        let Pools { logs, pools, .. } = self;
        let pool = &mut pools[pool_idx];
        match selection_alg {
            FragmentSelectionAlgorithmParams::OldestFirst => {
                let mut selection_alg = OldestFirst::new();
                selection_alg
                    .select(
                        ledger,
                        &ledger_params,
                        logs,
                        pool,
                        soft_deadline_future,
                        hard_deadline_future,
                    )
                    .await
            }
        }
    }
}

fn is_fragment_valid(fragment: &Fragment) -> bool {
    match fragment {
        // never valid in the pool, only acceptable in genesis
        Fragment::Initial(_) => false,
        Fragment::OldUtxoDeclaration(_) => false,
        // general transactions stuff
        Fragment::Transaction(ref tx) => is_transaction_valid(tx),
        Fragment::StakeDelegation(ref tx) => is_transaction_valid(tx),
        Fragment::OwnerStakeDelegation(ref tx) => is_transaction_valid(tx),
        Fragment::PoolRegistration(ref tx) => is_transaction_valid(tx),
        Fragment::PoolRetirement(ref tx) => is_transaction_valid(tx),
        Fragment::PoolUpdate(ref tx) => is_transaction_valid(tx),
        // vote stuff
        Fragment::UpdateProposal(_) => false, // TODO: enable when ready
        Fragment::UpdateVote(_) => false,     // TODO: enable when ready
        Fragment::VotePlan(ref tx) => is_transaction_valid(tx),
        Fragment::VoteCast(ref tx) => is_transaction_valid(tx),
        Fragment::VoteTally(ref tx) => is_transaction_valid(tx),
        Fragment::EncryptedVoteTally(ref tx) => is_transaction_valid(tx),
    }
}

fn is_transaction_valid<E>(tx: &Transaction<E>) -> bool {
    tx.verify_possibly_balanced().is_ok()
}

pub(super) mod internal {
    use super::*;

    use std::{
        collections::HashMap,
        hash::{Hash, Hasher},
        ptr,
    };

    /// Doubly-linked queue with the possibility to remove elements from the middle of the list by
    /// their keys.
    struct IndexedDeqeue<K, V> {
        head: *mut IndexedDequeueEntry<K, V>,
        tail: *mut IndexedDequeueEntry<K, V>,

        index: HashMap<IndexedDequeueKeyRef<K>, Box<IndexedDequeueEntry<K, V>>>,
    }

    struct IndexedDequeueEntry<K, V> {
        key: K,
        value: V,

        prev: *mut IndexedDequeueEntry<K, V>,
        next: *mut IndexedDequeueEntry<K, V>,
    }

    /// A wrapper around the pointer to the key of the queue element. This wrapper forwards the
    /// implementations of `Eq` and `Hash` to `K`. This is required becuase by default the
    /// implementations of `Eq` and `Hash` from the pointer itself will be used.
    struct IndexedDequeueKeyRef<K>(*const K);

    impl<K, V> IndexedDeqeue<K, V>
    where
        K: Eq + Hash,
    {
        fn new() -> Self {
            Self {
                head: ptr::null_mut(),
                tail: ptr::null_mut(),

                index: HashMap::new(),
            }
        }

        fn push_front(&mut self, key: K, value: V) {
            let mut entry = Box::new(IndexedDequeueEntry {
                key,
                value,
                prev: ptr::null_mut(),
                next: self.head,
            });
            if let Some(head) = unsafe { self.head.as_mut() } {
                head.prev = &mut *entry;
            } else {
                self.tail = &mut *entry;
            }
            self.head = &mut *entry;
            if self
                .index
                .insert(IndexedDequeueKeyRef(&entry.key), entry)
                .is_some()
            {
                panic!("inserted an already existing key");
            }
        }

        fn push_back(&mut self, key: K, value: V) {
            let mut entry = Box::new(IndexedDequeueEntry {
                key,
                value,
                prev: self.tail,
                next: ptr::null_mut(),
            });
            if let Some(tail) = unsafe { self.tail.as_mut() } {
                tail.next = &mut *entry;
            } else {
                self.head = &mut *entry;
            }
            self.tail = &mut *entry;
            if self
                .index
                .insert(IndexedDequeueKeyRef(&entry.key), entry)
                .is_some()
            {
                panic!("inserted an already existing key");
            }
        }

        fn pop_back(&mut self) -> Option<(K, V)> {
            let tail = unsafe { self.tail.as_mut() }?;
            self.tail = tail.prev;
            if let Some(prev) = unsafe { tail.prev.as_mut() } {
                prev.next = ptr::null_mut();
            } else {
                self.head = ptr::null_mut();
            }
            let entry = self.index.remove(&IndexedDequeueKeyRef(&tail.key)).unwrap();
            Some((entry.key, entry.value))
        }

        fn remove(&mut self, key: &K) -> Option<V> {
            let entry = self.index.remove(&IndexedDequeueKeyRef(key))?;
            if let Some(prev) = unsafe { entry.prev.as_mut() } {
                prev.next = entry.next;
            } else {
                self.head = entry.next;
            }
            if let Some(next) = unsafe { entry.next.as_mut() } {
                next.prev = entry.prev;
            } else {
                self.tail = entry.prev;
            }
            Some(entry.value)
        }

        fn len(&self) -> usize {
            self.index.len()
        }

        fn contains(&self, key: &K) -> bool {
            self.index.contains_key(&IndexedDequeueKeyRef(key))
        }
    }

    unsafe impl<K: Send, V: Send> Send for IndexedDeqeue<K, V> {}

    impl<K: PartialEq> PartialEq for IndexedDequeueKeyRef<K> {
        fn eq(&self, other: &IndexedDequeueKeyRef<K>) -> bool {
            unsafe { (*self.0).eq(&*other.0) }
        }
    }

    impl<K: PartialEq> Eq for IndexedDequeueKeyRef<K> {}

    impl<K: Hash> Hash for IndexedDequeueKeyRef<K> {
        fn hash<H: Hasher>(&self, state: &mut H) {
            unsafe { (*self.0).hash(state) }
        }
    }

    pub struct Pool {
        entries: IndexedDeqeue<FragmentId, Fragment>,
        max_entries: usize,
    }

    impl Pool {
        pub fn new(max_entries: usize) -> Self {
            Pool {
                entries: IndexedDeqeue::new(),
                max_entries,
            }
        }

        /// Returns clones of registered fragments
        pub fn insert_all(
            &mut self,
            fragments: impl IntoIterator<Item = Fragment>,
        ) -> Vec<Fragment> {
            let max_fragments = self.max_entries - self.entries.len();
            fragments
                .into_iter()
                .filter(|fragment| {
                    let fragment_id = fragment.id();
                    if self.entries.contains(&fragment_id) {
                        false
                    } else {
                        self.entries.push_front(fragment_id, fragment.clone());
                        true
                    }
                })
                .take(max_fragments)
                .collect()
        }

        pub fn remove_all<'a>(&mut self, fragment_ids: impl IntoIterator<Item = &'a FragmentId>) {
            for fragment_id in fragment_ids {
                self.entries.remove(fragment_id);
            }
        }

        pub fn remove_oldest(&mut self) -> Option<Fragment> {
            self.entries.pop_back().map(|(_, value)| value)
        }

        pub fn return_to_pool(&mut self, fragments: impl IntoIterator<Item = Fragment>) {
            for fragment in fragments.into_iter() {
                self.entries.push_back(fragment.id(), fragment);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use quickcheck_macros::quickcheck;

        #[quickcheck]
        fn overflowing_pool_should_reject_new_fragments(
            fragments1_in: (Fragment, Fragment, Fragment),
            fragments2_in: (Fragment, Fragment),
        ) {
            let fragments1 = vec![
                fragments1_in.0.clone(),
                fragments1_in.1.clone(),
                fragments1_in.2.clone(),
            ];
            let fragments2 = vec![
                fragments1_in.2.clone(),
                fragments2_in.0.clone(),
                fragments2_in.1.clone(),
            ];
            let fragments2_expected = vec![fragments2_in.0.clone()];
            let final_expected = vec![
                fragments1_in.0,
                fragments1_in.1,
                fragments1_in.2,
                fragments2_in.0,
            ];
            let mut pool = Pool::new(4);
            assert_eq!(fragments1, pool.insert_all(fragments1.clone()));
            assert_eq!(fragments2_expected, pool.insert_all(fragments2));
            for expected in final_expected.into_iter() {
                assert_eq!(expected, pool.remove_oldest().unwrap());
            }
            assert!(pool.remove_oldest().is_none());
        }
    }
}
