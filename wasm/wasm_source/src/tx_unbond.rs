//! A tx for a PoS unbond that removes staked tokens from a self-bond or a
//! delegation to be withdrawn in or after unbonding epoch.

use anoma_tx_prelude::proof_of_stake::unbond_tokens;
use anoma_tx_prelude::*;

#[transaction]
fn apply_tx(tx_data: Vec<u8>) {
    let signed = SignedTxData::try_from_slice(&tx_data[..]).unwrap();
    let unbond =
        transaction::pos::Unbond::try_from_slice(&signed.data.unwrap()[..])
            .unwrap();

    if let Err(err) =
        unbond_tokens(unbond.source.as_ref(), &unbond.validator, unbond.amount)
    {
        debug_log!("Unbonding failed with: {}", err);
        panic!()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use anoma::ledger::pos::PosParams;
    use anoma::proto::Tx;
    use anoma::types::storage::Epoch;
    use anoma_tests::log::test;
    use anoma_tests::native_vp::pos::init_pos;
    use anoma_tests::native_vp::TestNativeVpEnv;
    use anoma_tests::tx::*;
    use anoma_tx_prelude::address::testing::{
        arb_established_address, arb_non_internal_address,
    };
    use anoma_tx_prelude::address::InternalAddress;
    use anoma_tx_prelude::key::testing::arb_common_keypair;
    use anoma_tx_prelude::key::RefTo;
    use anoma_tx_prelude::proof_of_stake::parameters::testing::arb_pos_params;
    use anoma_tx_prelude::token;
    use anoma_vp_prelude::proof_of_stake::types::{
        Bond, Unbond, VotingPower, VotingPowerDelta,
    };
    use anoma_vp_prelude::proof_of_stake::{
        staking_token_address, BondId, GenesisValidator, PosVP,
    };
    use proptest::prelude::*;

    use super::*;

    fn test_tx_unbond_aux(
        initial_stake: token::Amount,
        unbond: transaction::pos::Unbond,
        key: key::common::SecretKey,
        pos_params: PosParams,
    ) {
        let staking_reward_address = address::testing::established_address_1();
        let consensus_key = key::testing::keypair_1().ref_to();
        let staking_reward_key = key::testing::keypair_2().ref_to();

        let genesis_validators = [GenesisValidator {
            address: unbond.validator.clone(),
            staking_reward_address,
            tokens: if unbond.source.is_some() {
                // If we're unbonding a delegation, we'll give the initial stake
                // to the delegation instead of the validator
                token::Amount::default()
            } else {
                initial_stake
            },
            consensus_key,
            staking_reward_key,
        }];

        init_pos(&genesis_validators[..], &pos_params, Epoch(0));

        tx_host_env::with(|tx_env| {
            if let Some(source) = &unbond.source {
                tx_env.spawn_accounts([source]);

                // To allow to unbond delegation, there must be a delegation
                // bond first.
                // First, credit the bond's source with the initial stake,
                // before we initialize the bond below
                tx_env.credit_tokens(
                    &source,
                    &staking_token_address(),
                    initial_stake,
                );
            }
        });

        if let Some(source) = &unbond.source {
            // Initialize the delegation - unlike genesis validator's self-bond,
            // this happens at pipeline offset
            anoma_tx_prelude::proof_of_stake::bond_tokens(
                unbond.source.as_ref(),
                &unbond.validator,
                initial_stake,
            )
            .unwrap();
        }
        tx_host_env::commit_tx_and_block();

        let tx_code = vec![];
        let tx_data = unbond.try_to_vec().unwrap();
        let tx = Tx::new(tx_code, Some(tx_data));
        let signed_tx = tx.sign(&key);
        let tx_data = signed_tx.data.unwrap();

        let unbond_src = unbond
            .source
            .clone()
            .unwrap_or_else(|| unbond.validator.clone());
        let unbond_id = BondId {
            validator: unbond.validator.clone(),
            source: unbond_src,
        };

        let pos_balance_key = token::balance_key(
            &staking_token_address(),
            &Address::Internal(InternalAddress::PoS),
        );
        let pos_balance_pre: token::Amount =
            read(&pos_balance_key.to_string()).expect("PoS must have balance");
        assert_eq!(pos_balance_pre, initial_stake);
        let total_voting_powers_pre = PoS.read_total_voting_power();
        let validator_sets_pre = PoS.read_validator_set();
        let validator_voting_powers_pre =
            PoS.read_validator_voting_power(&unbond.validator).unwrap();
        let bonds_pre = PoS.read_bond(&unbond_id).unwrap();
        dbg!(&bonds_pre);

        apply_tx(tx_data);

        // Read the data after the tx is executed

        // The following storage keys should be updated:

        //     - `#{PoS}/validator/#{validator}/total_deltas`
        let total_delta_post =
            PoS.read_validator_total_deltas(&unbond.validator);

        let expected_deltas_at_pipeline = if unbond.source.is_some() {
            // When this is a delegation, there will be no bond until pipeline
            0.into()
        } else {
            // Before pipeline offset, there can only be self-bond
            initial_stake
        };

        // Before pipeline offset, there can only be self-bond for genesis
        // validator. In case of a delegation the state is setup so that there
        // is no bond until pipeline offset.
        for epoch in 0..pos_params.pipeline_len {
            assert_eq!(
                total_delta_post.as_ref().unwrap().get(epoch),
                Some(expected_deltas_at_pipeline.into()),
                "The total deltas before the pipeline offset must not change \
                 - checking in epoch: {epoch}"
            );
        }

        // At and after pipeline offset, there can be either delegation or
        // self-bond, both of which are initialized to the same `initial_stake`
        for epoch in pos_params.pipeline_len..pos_params.unbonding_len {
            assert_eq!(
                total_delta_post.as_ref().unwrap().get(epoch),
                Some(initial_stake.into()),
                "The total deltas before the unbonding offset must not change \
                 - checking in epoch: {epoch}"
            );
        }

        {
            let epoch = pos_params.unbonding_len + 1;
            let expected_stake =
                i128::from(initial_stake) - i128::from(unbond.amount);
            assert_eq!(
                total_delta_post.as_ref().unwrap().get(epoch),
                Some(expected_stake),
                "The total deltas after the unbonding offset epoch must be \
                 decremented by the unbonded amount - checking in epoch: \
                 {epoch}"
            );
        }

        //     - `#{staking_token}/balance/#{PoS}`
        let pos_balance_post: token::Amount =
            read(&pos_balance_key.to_string()).unwrap();
        assert_eq!(
            pos_balance_pre, pos_balance_post,
            "Unbonding doesn't affect PoS system balance"
        );

        //     - `#{PoS}/unbond/#{owner}/#{validator}`
        let unbonds_post = PoS.read_unbond(&unbond_id).unwrap();
        let bonds_post = PoS.read_bond(&unbond_id).unwrap();
        for epoch in 0..pos_params.unbonding_len {
            let unbond: Option<Unbond<token::Amount>> = unbonds_post.get(epoch);

            assert!(
                unbond.is_none(),
                "There should be no unbond until unbonding offset - checking \
                 epoch {epoch}"
            );
        }
        match &unbond.source {
            Some(_) => {
                // This bond was a delegation
                let start_epoch =
                    anoma_tx_prelude::proof_of_stake::types::Epoch::from(
                        pos_params.pipeline_len,
                    );
                // We're unbonding the delegation in the same epoch as it was
                // initialized
                let end_epoch =
                    anoma_tx_prelude::proof_of_stake::types::Epoch::from(
                        pos_params.unbonding_len - 1,
                    );

                let expected_unbond = HashMap::from_iter([(
                    (start_epoch, end_epoch),
                    unbond.amount,
                )]);
                let actual_unbond: Unbond<token::Amount> =
                    unbonds_post.get(pos_params.unbonding_len).unwrap();
                assert_eq!(
                    actual_unbond.deltas, expected_unbond,
                    "Delegation at unbonding offset should be equal to the \
                     unbonded amount"
                );

                dbg!(&bonds_post);
                for epoch in 0..pos_params.pipeline_len {
                    let bond: Option<Bond<token::Amount>> =
                        bonds_post.get(epoch);
                    assert!(
                        bond.is_none(),
                        "There is no delegation before pipeline offset, got \
                         {bond:?}, checking epoch {epoch}"
                    );
                }
                for epoch in pos_params.pipeline_len..pos_params.unbonding_len {
                    let bond: Bond<token::Amount> =
                        bonds_post.get(epoch).unwrap();
                    let expected_bond =
                        HashMap::from_iter([(start_epoch, initial_stake)]);
                    assert_eq!(
                        bond.deltas, expected_bond,
                        "Before unbonding offset, the bond should be \
                         untouched, checking epoch {epoch}"
                    );
                }
                {
                    let epoch = pos_params.unbonding_len + 1;
                    let bond: Bond<token::Amount> =
                        bonds_post.get(epoch).unwrap();
                    let expected_bond = HashMap::from_iter([(
                        start_epoch,
                        initial_stake - unbond.amount,
                    )]);
                    assert_eq!(
                        bond.deltas, expected_bond,
                        "At unbonding offset, the unbonded amount should have \
                         been deducted, checking epoch {epoch}"
                    );
                }
            }
            None => {
                // This bond was a genesis validator self-bond
                let start_epoch =
                    anoma_tx_prelude::proof_of_stake::types::Epoch::default();
                let end_epoch =
                    anoma_tx_prelude::proof_of_stake::types::Epoch::from(
                        pos_params.unbonding_len - 1,
                    );

                let expected_unbond = HashMap::from_iter([(
                    (start_epoch, end_epoch),
                    unbond.amount,
                )]);
                let unbond: Unbond<token::Amount> =
                    unbonds_post.get(pos_params.unbonding_len).unwrap();
                assert_eq!(
                    unbond.deltas, expected_unbond,
                    "Unbonded self-bond at unbonding offset should be equal \
                     to the unbonded amount"
                );

                for epoch in 0..pos_params.unbonding_len {}
                {
                    let epoch = pos_params.unbonding_len + 1;
                }
            }
        }
    }

    proptest! {
        #[test]
        fn test_tx_unbond(
        (initial_stake, unbond) in arb_initial_stake_and_unbond(),
        key in arb_common_keypair(),
        pos_params in arb_pos_params()) {
            test_tx_unbond_aux(initial_stake, unbond, key, pos_params)
        }
    }

    pub fn arb_bond(
        max_amount: u64,
    ) -> impl Strategy<Value = transaction::pos::Bond> {
        (
            address::testing::arb_established_address(),
            prop::option::of(address::testing::arb_non_internal_address()),
            token::testing::arb_amount_ceiled(max_amount),
        )
            .prop_map(|(validator, source, amount)| {
                let validator = Address::Established(validator);
                // If the source is the same as validator, remove it to
                let source = match source {
                    Some(source) if source == validator => None,
                    _ => source,
                };
                transaction::pos::Bond {
                    validator,
                    amount,
                    source,
                }
            })
    }
    fn arb_initial_stake_and_unbond()
    -> impl Strategy<Value = (token::Amount, transaction::pos::Unbond)> {
        // Generate initial stake
        token::testing::arb_amount().prop_flat_map(|initial_stake| {
            // Use the initial stake to limit the bond amount
            let unbond = arb_bond(u64::from(initial_stake));
            // Use the generated initial stake too too
            (Just(initial_stake), unbond)
        })
    }
}
