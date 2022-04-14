//! Randomised property testing for [`Block`]s.

use proptest::{
    arbitrary::{any, Arbitrary},
    prelude::*,
};

use std::{collections::HashMap, sync::Arc};

use crate::{
    amount::NonNegative,
    block,
    fmt::SummaryDebug,
    history_tree::HistoryTree,
    parameters::{
        Network,
        NetworkUpgrade::{self, *},
        GENESIS_PREVIOUS_BLOCK_HASH,
    },
    serialization,
    transaction::arbitrary::MAX_ARBITRARY_ITEMS,
    transparent::{
        new_transaction_ordered_outputs, CoinbaseSpendRestriction,
        MIN_TRANSPARENT_COINBASE_MATURITY,
    },
    work::{difficulty::CompactDifficulty, equihash},
};

use super::*;

/// The chain length for most zebra-chain proptests.
///
/// Most generated chains will contain transparent spends at or before this height.
///
/// This height was chosen a tradeoff between chains with no spends,
/// and chains which spend outputs created by previous spends.
///
/// The raw probability of having no spends during a test run is:
/// ```text
/// shielded_input = shielded_pool_count / pool_count
/// expected_transactions = expected_inputs = MAX_ARBITRARY_ITEMS/2
/// shielded_input^(expected_transactions * expected_inputs * (PREVOUTS_CHAIN_HEIGHT - 1))
/// ```
///
/// This probability is approximately 3%. However, proptest generation and
/// minimisation strategies can create additional chains with no transparent spends.
///
/// To increase the proportion of test runs with proptest spends, increase `PREVOUTS_CHAIN_HEIGHT`.
pub const PREVOUTS_CHAIN_HEIGHT: usize = 4;

/// The chain length for most zebra-state proptests.
///
/// Most generated chains will contain transparent spends at or before this height.
///
/// This height was chosen as a tradeoff between chains with no transparent spends,
/// and chains which spend outputs created by previous spends.
///
/// See [`block::arbitrary::PREVOUTS_CHAIN_HEIGHT`] for details.
pub const MAX_PARTIAL_CHAIN_BLOCKS: usize =
    MIN_TRANSPARENT_COINBASE_MATURITY as usize + PREVOUTS_CHAIN_HEIGHT;

impl Arbitrary for Height {
    type Parameters = ();

    fn arbitrary_with(_args: ()) -> Self::Strategy {
        (Height::MIN.0..=Height::MAX.0).prop_map(Height).boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
/// The configuration data for proptest when generating arbitrary chains
pub struct LedgerState {
    /// The height of the generated block, or the start height of the generated chain.
    ///
    /// To get the network upgrade, use the `network_upgrade` method.
    ///
    /// If `network_upgrade_override` is not set, the network upgrade is derived
    /// from the `height` and `network`.
    pub height: Height,

    /// The network to generate fake blocks for.
    pub network: Network,

    /// Overrides the network upgrade calculated from `height` and `network`.
    ///
    /// To get the network upgrade, use the `network_upgrade` method.
    network_upgrade_override: Option<NetworkUpgrade>,

    /// Overrides the previous block hashes in blocks generated by this ledger.
    previous_block_hash_override: Option<block::Hash>,

    /// Regardless of tip height and network, every transaction is this version.
    transaction_version_override: Option<u32>,

    /// Every V5 and later transaction has a valid `network_upgrade` field.
    ///
    /// If `false`, zero or more transactions may have invalid network upgrades.
    transaction_has_valid_network_upgrade: bool,

    /// Generate coinbase transactions.
    ///
    /// In a block or transaction vector, make the first transaction a coinbase
    /// transaction.
    ///
    /// For an individual transaction, make the transaction a coinbase
    /// transaction.
    pub(crate) has_coinbase: bool,
}

/// Overrides for arbitrary [`LedgerState`]s.
#[derive(Debug, Clone, Copy)]
pub struct LedgerStateOverride {
    /// Every chain starts at this block. Single blocks have this height.
    pub height_override: Option<Height>,

    /// Every chain starts with a block with this previous block hash.
    /// Single blocks have this previous block hash.
    pub previous_block_hash_override: Option<block::Hash>,

    /// Regardless of tip height and network, every block has features from this
    /// network upgrade.
    pub network_upgrade_override: Option<NetworkUpgrade>,

    /// Regardless of tip height and network, every transaction is this version.
    pub transaction_version_override: Option<u32>,

    /// Every V5 and later transaction has a valid `network_upgrade` field.
    ///
    /// If `false`, zero or more transactions may have invalid network upgrades.
    pub transaction_has_valid_network_upgrade: bool,

    /// Every block has exactly one coinbase transaction.
    /// Transactions are always coinbase transactions.
    pub always_has_coinbase: bool,
}

impl LedgerState {
    /// Returns the default strategy for creating arbitrary `LedgerState`s.
    pub fn default_strategy() -> BoxedStrategy<Self> {
        Self::arbitrary_with(LedgerStateOverride::default())
    }

    /// Returns a strategy for creating arbitrary `LedgerState`s, without any
    /// overrides.
    pub fn no_override_strategy() -> BoxedStrategy<Self> {
        Self::arbitrary_with(LedgerStateOverride {
            height_override: None,
            previous_block_hash_override: None,
            network_upgrade_override: None,
            transaction_version_override: None,
            transaction_has_valid_network_upgrade: false,
            always_has_coinbase: false,
        })
    }

    /// Returns a strategy for creating `LedgerState`s with features from
    /// `network_upgrade_override`.
    ///
    /// These features ignore the actual tip height and network.
    pub fn network_upgrade_strategy(
        network_upgrade_override: NetworkUpgrade,
        transaction_version_override: impl Into<Option<u32>>,
        transaction_has_valid_network_upgrade: bool,
    ) -> BoxedStrategy<Self> {
        Self::arbitrary_with(LedgerStateOverride {
            height_override: None,
            previous_block_hash_override: None,
            network_upgrade_override: Some(network_upgrade_override),
            transaction_version_override: transaction_version_override.into(),
            transaction_has_valid_network_upgrade,
            always_has_coinbase: false,
        })
    }

    /// Returns a strategy for creating `LedgerState`s that always have coinbase
    /// transactions.
    ///
    /// Also applies `network_upgrade_override`, if present.
    pub fn coinbase_strategy(
        network_upgrade_override: impl Into<Option<NetworkUpgrade>>,
        transaction_version_override: impl Into<Option<u32>>,
        transaction_has_valid_network_upgrade: bool,
    ) -> BoxedStrategy<Self> {
        Self::arbitrary_with(LedgerStateOverride {
            height_override: None,
            previous_block_hash_override: None,
            network_upgrade_override: network_upgrade_override.into(),
            transaction_version_override: transaction_version_override.into(),
            transaction_has_valid_network_upgrade,
            always_has_coinbase: true,
        })
    }

    /// Returns a strategy for creating `LedgerState`s that start with a genesis
    /// block.
    ///
    /// These strategies also have coinbase transactions, and an optional network
    /// upgrade override.
    ///
    /// Use the `Genesis` network upgrade to get a random genesis block, with
    /// Zcash genesis features.
    pub fn genesis_strategy(
        network_upgrade_override: impl Into<Option<NetworkUpgrade>>,
        transaction_version_override: impl Into<Option<u32>>,
        transaction_has_valid_network_upgrade: bool,
    ) -> BoxedStrategy<Self> {
        Self::arbitrary_with(LedgerStateOverride {
            height_override: Some(Height(0)),
            previous_block_hash_override: Some(GENESIS_PREVIOUS_BLOCK_HASH),
            network_upgrade_override: network_upgrade_override.into(),
            transaction_version_override: transaction_version_override.into(),
            transaction_has_valid_network_upgrade,
            always_has_coinbase: true,
        })
    }

    /// Returns a strategy for creating `LedgerState`s that start at `height`.
    ///
    /// These strategies also have coinbase transactions, and an optional network
    /// upgrade override.
    pub fn height_strategy(
        height: Height,
        network_upgrade_override: impl Into<Option<NetworkUpgrade>>,
        transaction_version_override: impl Into<Option<u32>>,
        transaction_has_valid_network_upgrade: bool,
    ) -> BoxedStrategy<Self> {
        Self::arbitrary_with(LedgerStateOverride {
            height_override: Some(height),
            previous_block_hash_override: None,
            network_upgrade_override: network_upgrade_override.into(),
            transaction_version_override: transaction_version_override.into(),
            transaction_has_valid_network_upgrade,
            always_has_coinbase: true,
        })
    }

    /// Returns the network upgrade for this ledger state.
    ///
    /// If `network_upgrade_override` is set, it replaces the upgrade calculated
    /// using `height` and `network`.
    pub fn network_upgrade(&self) -> NetworkUpgrade {
        if let Some(network_upgrade_override) = self.network_upgrade_override {
            network_upgrade_override
        } else {
            NetworkUpgrade::current(self.network, self.height)
        }
    }

    /// Returns the transaction version override.
    pub fn transaction_version_override(&self) -> Option<u32> {
        self.transaction_version_override
    }

    /// Returns `true` if all transactions have valid network upgrade fields.
    ///
    /// If `false`, some transactions have invalid network upgrades.
    pub fn transaction_has_valid_network_upgrade(&self) -> bool {
        self.transaction_has_valid_network_upgrade
    }
}

impl Default for LedgerState {
    fn default() -> Self {
        // TODO: stop having a default network
        let default_network = Network::default();
        let default_override = LedgerStateOverride::default();

        let most_recent_nu = NetworkUpgrade::current(default_network, Height::MAX);
        let most_recent_activation_height =
            most_recent_nu.activation_height(default_network).unwrap();

        LedgerState {
            height: most_recent_activation_height,
            network: default_network,
            network_upgrade_override: default_override.network_upgrade_override,
            previous_block_hash_override: default_override.previous_block_hash_override,
            transaction_version_override: default_override.transaction_version_override,
            transaction_has_valid_network_upgrade: default_override
                .transaction_has_valid_network_upgrade,
            has_coinbase: default_override.always_has_coinbase,
        }
    }
}

impl Default for LedgerStateOverride {
    fn default() -> Self {
        let default_network = Network::default();

        // TODO: dynamically select any future network upgrade (#1974)
        let nu5_activation_height = Nu5.activation_height(default_network);
        let nu5_override = if nu5_activation_height.is_some() {
            None
        } else {
            Some(Nu5)
        };

        LedgerStateOverride {
            height_override: None,
            previous_block_hash_override: None,
            network_upgrade_override: nu5_override,
            transaction_version_override: None,
            transaction_has_valid_network_upgrade: false,
            always_has_coinbase: true,
        }
    }
}

impl Arbitrary for LedgerState {
    type Parameters = LedgerStateOverride;

    /// Generate an arbitrary [`LedgerState`].
    ///
    /// The default strategy arbitrarily skips some coinbase transactions, and
    /// has an arbitrary start height. To override, use a specific [`LegderState`]
    /// strategy method.
    fn arbitrary_with(ledger_override: Self::Parameters) -> Self::Strategy {
        (
            any::<Height>(),
            any::<Network>(),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(
                move |(height, network, transaction_has_valid_network_upgrade, has_coinbase)| {
                    LedgerState {
                        height: ledger_override.height_override.unwrap_or(height),
                        network,
                        network_upgrade_override: ledger_override.network_upgrade_override,
                        previous_block_hash_override: ledger_override.previous_block_hash_override,
                        transaction_version_override: ledger_override.transaction_version_override,
                        transaction_has_valid_network_upgrade: ledger_override
                            .transaction_has_valid_network_upgrade
                            || transaction_has_valid_network_upgrade,
                        has_coinbase: ledger_override.always_has_coinbase || has_coinbase,
                    }
                },
            )
            .boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

impl Arbitrary for Block {
    type Parameters = LedgerState;

    fn arbitrary_with(ledger_state: Self::Parameters) -> Self::Strategy {
        let transactions_strategy =
            (1..MAX_ARBITRARY_ITEMS).prop_flat_map(move |transaction_count| {
                Transaction::vec_strategy(ledger_state, transaction_count)
            });

        // TODO: if needed, fixup:
        // - history and authorizing data commitments
        // - the transaction merkle root

        (Header::arbitrary_with(ledger_state), transactions_strategy)
            .prop_map(move |(header, transactions)| Self {
                header,
                transactions,
            })
            .boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

/// Skip checking transparent coinbase spends in [`Block::partial_chain_strategy`].
#[allow(clippy::result_unit_err)]
pub fn allow_all_transparent_coinbase_spends(
    _: transparent::OutPoint,
    _: transparent::CoinbaseSpendRestriction,
    _: transparent::OrderedUtxo,
) -> Result<(), ()> {
    Ok(())
}

impl Block {
    /// Returns a strategy for creating vectors of blocks with increasing height.
    ///
    /// Each vector is `count` blocks long.
    ///
    /// `check_transparent_coinbase_spend` is used to check if
    /// transparent coinbase UTXOs are valid, before using them in blocks.
    /// Use [`allow_all_transparent_coinbase_spends`] to disable this check.
    ///
    /// `generate_valid_commitments` specifies if the generated blocks
    /// should have valid commitments. This makes it much slower so it's better
    /// to enable only when needed.
    pub fn partial_chain_strategy<F, T, E>(
        mut current: LedgerState,
        count: usize,
        check_transparent_coinbase_spend: F,
        generate_valid_commitments: bool,
    ) -> BoxedStrategy<SummaryDebug<Vec<Arc<Self>>>>
    where
        F: Fn(
                transparent::OutPoint,
                transparent::CoinbaseSpendRestriction,
                transparent::OrderedUtxo,
            ) -> Result<T, E>
            + Copy
            + 'static,
    {
        let mut vec = Vec::with_capacity(count);

        // generate block strategies with the correct heights
        for _ in 0..count {
            vec.push((Just(current.height), Block::arbitrary_with(current)));
            current.height.0 += 1;
        }

        // after the vec strategy generates blocks, fixup invalid parts of the blocks
        vec.prop_map(move |mut vec| {
            let mut previous_block_hash = None;
            let mut utxos = HashMap::new();
            let mut chain_value_pools = ValueBalance::zero();
            let mut sapling_tree = sapling::tree::NoteCommitmentTree::default();
            let mut orchard_tree = orchard::tree::NoteCommitmentTree::default();
            // The history tree usually takes care of "creating itself". But this
            // only works when blocks are pushed into it starting from genesis
            // (or at least pre-Heartwood, where the tree is not required).
            // However, this strategy can generate blocks from an arbitrary height,
            // so we must wait for the first block to create the history tree from it.
            // This is why `Option` is used here.
            let mut history_tree: Option<HistoryTree> = None;

            for (height, block) in vec.iter_mut() {
                // fixup the previous block hash
                if let Some(previous_block_hash) = previous_block_hash {
                    block.header.previous_block_hash = previous_block_hash;
                }

                let mut new_transactions = Vec::new();
                for (tx_index_in_block, transaction) in block.transactions.drain(..).enumerate() {
                    if let Some(transaction) = fix_generated_transaction(
                        (*transaction).clone(),
                        tx_index_in_block,
                        *height,
                        &mut chain_value_pools,
                        &mut utxos,
                        check_transparent_coinbase_spend,
                    ) {
                        // The FinalizedState does not update the note commitment trees with the genesis block,
                        // because it doesn't need to (the trees are not used at that point) and updating them
                        // would be awkward since the genesis block is handled separatedly there.
                        // This forces us to skip the genesis block here too in order to able to use
                        // this to test the finalized state.
                        if generate_valid_commitments && *height != Height(0) {
                            for sapling_note_commitment in transaction.sapling_note_commitments() {
                                sapling_tree.append(*sapling_note_commitment).unwrap();
                            }
                            for orchard_note_commitment in transaction.orchard_note_commitments() {
                                orchard_tree.append(*orchard_note_commitment).unwrap();
                            }
                        }
                        new_transactions.push(Arc::new(transaction));
                    }
                }

                // delete invalid transactions
                block.transactions = new_transactions;

                // fix commitment (must be done after finishing changing the block)
                if generate_valid_commitments {
                    let current_height = block.coinbase_height().unwrap();
                    let heartwood_height = NetworkUpgrade::Heartwood
                        .activation_height(current.network)
                        .unwrap();
                    let nu5_height = NetworkUpgrade::Nu5.activation_height(current.network);
                    match current_height.cmp(&heartwood_height) {
                        std::cmp::Ordering::Less => {
                            // In pre-Heartwood blocks this is the Sapling note commitment tree root.
                            // We don't validate it since we checkpoint on Canopy, but it
                            // needs to be well-formed, i.e. smaller than 𝑞_J, so we
                            // arbitrarily set it to 1.
                            block.header.commitment_bytes = [0u8; 32];
                            block.header.commitment_bytes[0] = 1;
                        }
                        std::cmp::Ordering::Equal => {
                            // The Heartwood activation block has a hardcoded all-zeroes commitment.
                            block.header.commitment_bytes = [0u8; 32];
                        }
                        std::cmp::Ordering::Greater => {
                            // Set the correct commitment bytes according to the network upgrade.
                            let history_tree_root = match &history_tree {
                                Some(tree) => tree.hash().unwrap_or_else(|| [0u8; 32].into()),
                                None => [0u8; 32].into(),
                            };
                            if nu5_height.is_some() && current_height >= nu5_height.unwrap() {
                                // From zebra-state/src/service/check.rs
                                let auth_data_root = block.auth_data_root();
                                let hash_block_commitments =
                                    ChainHistoryBlockTxAuthCommitmentHash::from_commitments(
                                        &history_tree_root,
                                        &auth_data_root,
                                    );
                                block.header.commitment_bytes = hash_block_commitments.into();
                            } else {
                                block.header.commitment_bytes = history_tree_root.into();
                            }
                        }
                    }
                    // update history tree for the next block
                    if history_tree.is_none() {
                        history_tree = Some(
                            HistoryTree::from_block(
                                current.network,
                                Arc::new(block.clone()),
                                &sapling_tree.root(),
                                &orchard_tree.root(),
                            )
                            .unwrap(),
                        );
                    } else {
                        history_tree
                            .as_mut()
                            .unwrap()
                            .push(
                                current.network,
                                Arc::new(block.clone()),
                                sapling_tree.root(),
                                orchard_tree.root(),
                            )
                            .unwrap();
                    }
                }

                // now that we've made all the changes, calculate our block hash,
                // so the next block can use it
                previous_block_hash = Some(block.hash());
            }
            SummaryDebug(
                vec.into_iter()
                    .map(|(_height, block)| Arc::new(block))
                    .collect(),
            )
        })
        .boxed()
    }
}

/// Fix `transaction` so it obeys more consensus rules.
///
/// Spends [`OutPoint`]s from `utxos`, and adds newly created outputs.
///
/// If the transaction can't be fixed, returns `None`.
pub fn fix_generated_transaction<F, T, E>(
    mut transaction: Transaction,
    tx_index_in_block: usize,
    height: Height,
    chain_value_pools: &mut ValueBalance<NonNegative>,
    utxos: &mut HashMap<transparent::OutPoint, transparent::OrderedUtxo>,
    check_transparent_coinbase_spend: F,
) -> Option<Transaction>
where
    F: Fn(
            transparent::OutPoint,
            transparent::CoinbaseSpendRestriction,
            transparent::OrderedUtxo,
        ) -> Result<T, E>
        + Copy
        + 'static,
{
    let mut spend_restriction = transaction.coinbase_spend_restriction(height);
    let mut new_inputs = Vec::new();
    let mut spent_outputs = HashMap::new();

    // fixup the transparent spends
    let original_inputs = transaction.inputs().to_vec();
    for mut input in original_inputs.into_iter() {
        if input.outpoint().is_some() {
            // the transparent chain value pool is the sum of unspent UTXOs,
            // so we don't need to check it separately, because we only spend unspent UTXOs
            if let Some(selected_outpoint) = find_valid_utxo_for_spend(
                &mut transaction,
                &mut spend_restriction,
                height,
                utxos,
                check_transparent_coinbase_spend,
            ) {
                input.set_outpoint(selected_outpoint);
                new_inputs.push(input);

                let spent_utxo = utxos
                    .remove(&selected_outpoint)
                    .expect("selected outpoint must have a UTXO");
                spent_outputs.insert(selected_outpoint, spent_utxo.utxo.output);
            }
            // otherwise, drop the invalid input, because it has no valid UTXOs to spend
        } else {
            // preserve coinbase inputs
            new_inputs.push(input.clone());
        }
    }

    // delete invalid inputs
    *transaction.inputs_mut() = new_inputs;

    let (_remaining_transaction_value, new_chain_value_pools) = transaction
        .fix_chain_value_pools(*chain_value_pools, &spent_outputs)
        .expect("value fixes produce valid chain value pools and remaining transaction values");

    // TODO: if needed, check output count here as well
    if transaction.has_transparent_or_shielded_inputs() {
        // consensus rule: skip genesis created UTXOs
        // Zebra implementation: also skip shielded chain value pool changes
        if height > Height(0) {
            *chain_value_pools = new_chain_value_pools;

            utxos.extend(new_transaction_ordered_outputs(
                &transaction,
                transaction.hash(),
                tx_index_in_block,
                height,
            ));
        }

        Some(transaction)
    } else {
        None
    }
}

/// Find a valid [`OutPoint`] in `utxos` to spend in `transaction`.
///
/// Modifies `transaction` and updates `spend_restriction` if needed.
///
/// If there is no valid output, or many search attempts have failed, returns `None`.
pub fn find_valid_utxo_for_spend<F, T, E>(
    transaction: &mut Transaction,
    spend_restriction: &mut CoinbaseSpendRestriction,
    spend_height: Height,
    utxos: &HashMap<transparent::OutPoint, transparent::OrderedUtxo>,
    check_transparent_coinbase_spend: F,
) -> Option<transparent::OutPoint>
where
    F: Fn(
            transparent::OutPoint,
            transparent::CoinbaseSpendRestriction,
            transparent::OrderedUtxo,
        ) -> Result<T, E>
        + Copy
        + 'static,
{
    let has_shielded_outputs = transaction.has_shielded_outputs();
    let delete_transparent_outputs = CoinbaseSpendRestriction::OnlyShieldedOutputs { spend_height };
    let mut attempts: usize = 0;

    // choose an arbitrary spendable UTXO, in hash set order
    while let Some((candidate_outpoint, candidate_utxo)) = utxos.iter().next() {
        attempts += 1;

        // Avoid O(n^2) algorithmic complexity by giving up early,
        // rather than exhausively checking the entire UTXO set
        if attempts > 100 {
            return None;
        }

        // try the utxo as-is, then try it with deleted transparent outputs
        if check_transparent_coinbase_spend(
            *candidate_outpoint,
            *spend_restriction,
            candidate_utxo.clone(),
        )
        .is_ok()
        {
            return Some(*candidate_outpoint);
        } else if has_shielded_outputs
            && check_transparent_coinbase_spend(
                *candidate_outpoint,
                delete_transparent_outputs,
                candidate_utxo.clone(),
            )
            .is_ok()
        {
            *transaction.outputs_mut() = Vec::new();
            *spend_restriction = delete_transparent_outputs;

            return Some(*candidate_outpoint);
        }
    }

    None
}

impl Arbitrary for Commitment {
    type Parameters = ();

    fn arbitrary_with(_args: ()) -> Self::Strategy {
        (any::<[u8; 32]>(), any::<Network>(), any::<Height>())
            .prop_map(|(commitment_bytes, network, block_height)| {
                if block_height == Heartwood.activation_height(network).unwrap() {
                    Commitment::ChainHistoryActivationReserved
                } else {
                    Commitment::from_bytes(commitment_bytes, network, block_height)
                        .expect("unexpected failure in from_bytes parsing")
                }
            })
            .boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

impl Arbitrary for Header {
    type Parameters = LedgerState;

    fn arbitrary_with(ledger_state: Self::Parameters) -> Self::Strategy {
        (
            // version is interpreted as i32 in the spec, so we are limited to i32::MAX here
            (4u32..(i32::MAX as u32)),
            any::<Hash>(),
            any::<merkle::Root>(),
            any::<[u8; 32]>(),
            serialization::arbitrary::datetime_u32(),
            any::<CompactDifficulty>(),
            any::<[u8; 32]>(),
            any::<equihash::Solution>(),
        )
            .prop_map(
                move |(
                    version,
                    mut previous_block_hash,
                    merkle_root,
                    commitment_bytes,
                    time,
                    difficulty_threshold,
                    nonce,
                    solution,
                )| {
                    if let Some(previous_block_hash_override) =
                        ledger_state.previous_block_hash_override
                    {
                        previous_block_hash = previous_block_hash_override;
                    } else if ledger_state.height == Height(0) {
                        previous_block_hash = GENESIS_PREVIOUS_BLOCK_HASH;
                    }

                    Header {
                        version,
                        previous_block_hash,
                        merkle_root,
                        commitment_bytes,
                        time,
                        difficulty_threshold,
                        nonce,
                        solution,
                    }
                },
            )
            .boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}
