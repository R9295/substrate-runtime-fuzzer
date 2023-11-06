use asset_hub_kusama_runtime::{
    AllPalletsWithSystem, Executive, Runtime, RuntimeCall, RuntimeOrigin, UncheckedExtrinsic,
};
use codec::{DecodeLimit, Encode};
use frame_support::{
    dispatch::GetDispatchInfo,
    pallet_prelude::Weight,
    traits::{IntegrityTest, TryState, TryStateSelect},
    weights::constants::WEIGHT_REF_TIME_PER_SECOND,
};
use parachains_common::{AccountId, BlockNumber, SLOT_DURATION};
use sp_consensus_aura::{Slot, AURA_ENGINE_ID};
use sp_runtime::{
    traits::{Dispatchable, Header},
    Digest, DigestItem, Storage,
};
use std::time::{Duration, Instant};

// We use a simple Map-based Externalities implementation
type Externalities = sp_state_machine::BasicExternalities;

// The initial timestamp at the start of an input run.
const INITIAL_TIMESTAMP: u64 = 0;

/// The maximum number of extrinsics per fuzzer input.
const MAX_EXTRINSIC_COUNT: usize = 32;

/// Max number of seconds a block should run for.
const MAX_TIME_FOR_BLOCK: u64 = 6;

// We do not skip more than DEFAULT_STORAGE_PERIOD to avoid pallet_transaction_storage from
// panicking on finalize.
// The 800 is to avoid timeouts.
const MAX_BLOCK_LAPSE: u32 = sp_transaction_storage_proof::DEFAULT_STORAGE_PERIOD / 800;

// Decode depth limit
const MAX_DECODE_LIMIT: u32 = 52;

// Extrinsic delimiter: `********`
const DELIMITER: [u8; 8] = [42; 8];

struct Data<'a> {
    data: &'a [u8],
    pointer: usize,
    size: usize,
}

impl<'a> Iterator for Data<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.data.len() <= self.pointer || self.size >= MAX_EXTRINSIC_COUNT {
            return None;
        }
        let next_delimiter = self.data[self.pointer..]
            .windows(DELIMITER.len())
            .position(|window| window == DELIMITER);
        let next_pointer = match next_delimiter {
            Some(delimiter) => self.pointer + delimiter,
            None => self.data.len(),
        };
        let res = Some(&self.data[self.pointer..next_pointer]);
        self.pointer = next_pointer + DELIMITER.len();
        self.size += 1;
        res
    }
}

fn main() {
    let endowed_accounts: Vec<AccountId> = (0..5).map(|i| [i; 32].into()).collect();

    let genesis_storage: Storage = {
        use asset_hub_kusama_runtime::{
            BalancesConfig, CollatorSelectionConfig, RuntimeGenesisConfig, SessionConfig,
            SessionKeys,
        };
        use sp_consensus_aura::sr25519::AuthorityId as AuraId;
        use sp_runtime::app_crypto::ByteArray;
        use sp_runtime::BuildStorage;

        let initial_authorities: Vec<(AccountId, AuraId)> =
            vec![([0; 32].into(), AuraId::from_slice(&[0; 32]).unwrap())];

        RuntimeGenesisConfig {
            system: Default::default(),
            balances: BalancesConfig {
                // Configure endowed accounts with initial balance of 1 << 60.
                balances: endowed_accounts
                    .iter()
                    .cloned()
                    .map(|k| (k, 1 << 60))
                    .collect(),
            },
            aura: Default::default(),
            session: SessionConfig {
                keys: initial_authorities
                    .iter()
                    .map(|x| (x.0.clone(), x.0.clone(), SessionKeys { aura: x.1.clone() }))
                    .collect::<Vec<_>>(),
            },
            collator_selection: CollatorSelectionConfig {
                invulnerables: initial_authorities.iter().map(|x| (x.0.clone())).collect(),
                candidacy_bond: 1 << 57,
                desired_candidates: 1,
            },
            aura_ext: Default::default(),
            parachain_info: Default::default(),
            parachain_system: Default::default(),
            polkadot_xcm: Default::default(),
        }
        .build_storage()
        .unwrap()
    };

    ziggy::fuzz!(|data: &[u8]| {
        let iteratable = Data {
            data,
            pointer: 0,
            size: 0,
        };

        // Max weight for a block.
        let max_weight: Weight = Weight::from_parts(WEIGHT_REF_TIME_PER_SECOND * 2, 0);

        let extrinsics: Vec<(u32, usize, RuntimeCall)> = iteratable
            .filter_map(|data| {
                // lapse is u32 (4 bytes), origin is u16 (2 bytes) -> 6 bytes minimum
                let min_data_len = 4 + 2;
                if data.len() <= min_data_len {
                    return None;
                }
                let lapse: u32 = u32::from_ne_bytes(data[0..4].try_into().unwrap());
                let origin: usize = u16::from_ne_bytes(data[4..6].try_into().unwrap()) as usize;
                let mut encoded_extrinsic: &[u8] = &data[6..];

                match DecodeLimit::decode_with_depth_limit(MAX_DECODE_LIMIT, &mut encoded_extrinsic)
                {
                    Ok(decoded_extrinsic) => Some((lapse, origin, decoded_extrinsic)),
                    Err(_) => None,
                }
            })
            .collect();

        if extrinsics.is_empty() {
            return;
        }

        // `externalities` represents the state of our mock chain.
        let mut externalities = Externalities::new(genesis_storage.clone());

        let mut current_block: u32 = 1;
        let mut current_weight: Weight = Weight::zero();
        // let mut already_seen = 0; // This must be uncommented if you want to print events
        let mut elapsed: Duration = Duration::ZERO;

        let start_block = |block: u32, lapse: u32| {
            #[cfg(not(fuzzing))]
            println!("\ninitializing block {}", block + lapse);

            for b in (block)..(block + lapse) {
                let current_timestamp = INITIAL_TIMESTAMP + u64::from(b) * SLOT_DURATION;
                let pre_digest = match current_timestamp {
                    INITIAL_TIMESTAMP => Default::default(),
                    _ => Digest {
                        logs: vec![DigestItem::PreRuntime(
                            AURA_ENGINE_ID,
                            Slot::from(current_timestamp / SLOT_DURATION).encode(),
                        )],
                    },
                };

                let prev_header = match block {
                    1 => None,
                    _ => Some(Executive::finalize_block()),
                };

                let parent_header = &Header::new(
                    b + 1,
                    Default::default(),
                    Default::default(),
                    prev_header.clone().map(|x| x.hash()).unwrap_or_default(),
                    pre_digest,
                );
                Executive::initialize_block(parent_header);

                // We apply the timestamp extrinsic for the current block.
                Executive::apply_extrinsic(UncheckedExtrinsic::new_unsigned(
                    RuntimeCall::Timestamp(pallet_timestamp::Call::set {
                        now: current_timestamp,
                    }),
                ))
                .unwrap()
                .unwrap();

                let parachain_validation_data = {
                    use cumulus_primitives_core::relay_chain::HeadData;
                    use cumulus_primitives_core::PersistedValidationData;
                    use cumulus_primitives_parachain_inherent::ParachainInherentData;
                    use cumulus_test_relay_sproof_builder::RelayStateSproofBuilder;

                    let parent_head =
                        HeadData(prev_header.unwrap_or(parent_header.clone()).encode()); // prev_header.encode());//
                    let sproof_builder = RelayStateSproofBuilder {
                        para_id: 100.into(),
                        current_slot: Slot::from(2 * current_timestamp / SLOT_DURATION),
                        included_para_head: Some(parent_head.clone()),
                        ..Default::default()
                    };

                    let (relay_parent_storage_root, relay_chain_state) =
                        sproof_builder.into_state_root_and_proof();
                    let data = ParachainInherentData {
                        validation_data: PersistedValidationData {
                            parent_head,
                            relay_parent_number: b,
                            relay_parent_storage_root,
                            max_pov_size: 1000,
                        },
                        relay_chain_state,
                        downward_messages: Default::default(),
                        horizontal_messages: Default::default(),
                    };
                    cumulus_pallet_parachain_system::Call::set_validation_data { data }
                };

                Executive::apply_extrinsic(UncheckedExtrinsic::new_unsigned(
                    RuntimeCall::ParachainSystem(parachain_validation_data),
                ))
                .unwrap()
                .unwrap();
            }

            // Calls that need to be called before each block starts (init_calls) go here
        };

        externalities.execute_with(|| start_block(current_block, 1));
        current_block += 1;

        for (lapse, origin, extrinsic) in extrinsics {
            // If the lapse is in the range [0, MAX_BLOCK_LAPSE] we finalize the block and initialize
            // a new one.
            if lapse > 0 && lapse < MAX_BLOCK_LAPSE {
                // We update our state variables
                current_weight = Weight::zero();
                elapsed = Duration::ZERO;

                // We start the next block
                externalities.execute_with(|| start_block(current_block, lapse));
                current_block += lapse;
            }

            // We get the current time for timing purposes.
            let now = Instant::now();

            let mut call_weight = Weight::zero();
            // We compute the weight to avoid overweight blocks.
            externalities.execute_with(|| {
                call_weight = extrinsic.get_dispatch_info().weight;
            });

            current_weight = current_weight.saturating_add(call_weight);
            if current_weight.ref_time() >= max_weight.ref_time() {
                #[cfg(not(fuzzing))]
                println!("Skipping because of max weight {max_weight}");
                continue;
            }

            externalities.execute_with(|| {
                let origin_account = endowed_accounts[origin % endowed_accounts.len()].clone();
                #[cfg(not(fuzzing))]
                {
                    println!("\n    origin:     {origin_account:?}");
                    println!("    call:       {extrinsic:?}");
                }
                let _res = extrinsic
                    .clone()
                    .dispatch(RuntimeOrigin::signed(origin_account));
                #[cfg(not(fuzzing))]
                println!("    result:     {_res:?}");

                // Uncomment to print events for debugging purposes
                /*
                #[cfg(not(fuzzing))]
                {
                    let all_events = statemine_runtime::System::events();
                    let events: Vec<_> = all_events.clone().into_iter().skip(already_seen).collect();
                    already_seen = all_events.len();
                    println!("  events:     {:?}\n", events);
                }
                */
            });

            elapsed += now.elapsed();
        }

        #[cfg(not(fuzzing))]
        println!("\n  time spent: {elapsed:?}");
        assert!(
            elapsed.as_secs() <= MAX_TIME_FOR_BLOCK,
            "block execution took too much time"
        );

        // We end the final block
        externalities.execute_with(|| {
            // Finilization
            Executive::finalize_block();
            // Invariants
            #[cfg(not(fuzzing))]
            println!("\ntesting invariants for block {current_block}");
            <AllPalletsWithSystem as TryState<BlockNumber>>::try_state(
                current_block,
                TryStateSelect::All,
            )
            .unwrap();
        });

        // After execution of all blocks.
        externalities.execute_with(|| {
            // We keep track of the sum of balance of accounts
            let mut counted_free = 0;
            let mut counted_reserved = 0;

            for acc in frame_system::Account::<Runtime>::iter() {
                // Check that the consumer/provider state is valid.
                let acc_consumers = acc.1.consumers;
                let acc_providers = acc.1.providers;
                assert!(!(acc_consumers > 0 && acc_providers == 0), "Invalid state");

                // Increment our balance counts
                counted_free += acc.1.data.free;
                counted_reserved += acc.1.data.reserved;
            }

            let total_issuance = pallet_balances::TotalIssuance::<Runtime>::get();
            let counted_issuance = counted_free + counted_reserved;
            // The reason we do not simply use `!=` here is that some balance might be transfered to another chain via XCM.
            // If we find some kind of workaround for this, we could replace `<` by `!=` here and make the check stronger.
            assert!(
                total_issuance <= counted_issuance,
                "Inconsistent total issuance: {total_issuance} but counted {counted_issuance}"
            );

            #[cfg(not(fuzzing))]
            println!("running integrity tests");
            // We run all developer-defined integrity tests
            <AllPalletsWithSystem as IntegrityTest>::integrity_test();
        });
    });
}
