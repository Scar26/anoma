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
        Unbond, VotingPower, VotingPowerDelta,
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
            tokens: initial_stake,
            consensus_key,
            staking_reward_key,
        }];

        init_pos(&genesis_validators[..], &pos_params, Epoch(0));

        tx_host_env::with(|tx_env| {
            if let Some(source) = &unbond.source {
                tx_env.spawn_accounts([source]);
            }
	});

	
        let tx_code = vec![];
        let tx_data = unbond.try_to_vec().unwrap();
        let tx = Tx::new(tx_code, Some(tx_data));
        let signed_tx = tx.sign(&key);
        let tx_data = signed_tx.data.unwrap();
   
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

        apply_tx(tx_data);

	// Read the data after the tx is executed

        // The following storage keys should be updated:

        //     - `#{PoS}/validator/#{validator}/total_deltas`
	let total_delta_post = PoS.read_validator_total_deltas(&unbond.validator);
        for epoch in 0..pos_params.unbonding_len {
            assert_eq!(
                total_delta_post.as_ref().unwrap().get(epoch),
                Some(initial_stake.into()),
                "The total deltas before the unbonding offset must not change \
                 - checking in epoch: {epoch}"
            );
        }
	
	for epoch in pos_params.unbonding_len..(pos_params.unbonding_len+pos_params.pipeline_len) {
	    // the choice of interval  (+pipeline_len) is arbitrary
            let expected_stake =
                i128::from(initial_stake) - i128::from(unbond.amount);
            assert_eq!(
                total_delta_post.as_ref().unwrap().get(epoch),
                Some(expected_stake),
                "The total deltas at and after the pipeline offset epoch must \
                 be incremented by the bonded amount - checking in epoch: \
                 {epoch}"
            );
        }

	  //     - `#{staking_token}/balance/#{PoS}`
        let pos_balance_post: token::Amount =
            read(&pos_balance_key.to_string()).unwrap();
        assert_eq!(pos_balance_pre - unbond.amount, pos_balance_post);

	   //     - `#{PoS}/unbond/#{owner}/#{validator}`
        let unbond_src = unbond
            .source
            .clone()
            .unwrap_or_else(|| unbond.validator.clone());
        let unbond_id = BondId {
            validator: unbond.validator.clone(),
            source: unbond_src,
        };
        let bonds_post = PoS.read_unbond(&unbond_id).unwrap();
        match &unbond.source {
            Some (_) => {
                // This bond was a delegation
                for epoch in 0..pos_params.unbonding_len {
                    let bond: Option<Unbond<token::Amount>> =
                        bonds_post.get(epoch);
		    
                    assert!(
                        bond.is_none(),
                        "Delegation until unbonding offset should be unchanged - \
                         checking epoch {epoch}"
                    );
                }
                for epoch in pos_params.unbonding_len..=(pos_params.unbonding_len + pos_params.pipeline_len)
                {
		    // the choice of interval (+pipeline_len) is arbitrary
		    let genesis_epoch =
                    anoma_tx_prelude::proof_of_stake::types::Epoch::from(0);
		    let start_epoch =
                        anoma_tx_prelude::proof_of_stake::types::Epoch::from(
                            pos_params.unbonding_len
                        );
		    
		    
                    let expected_bond =
                        HashMap::from_iter([((genesis_epoch, start_epoch),  initial_stake - unbond.amount)]);
                    let bond: Unbond<token::Amount> =
                        bonds_post.get(epoch).unwrap();
                    assert_eq!(
                        bond.deltas, expected_bond,
                        "Delegation at and after pipeline offset should be \
                         equal to the bonded amount - checking epoch {epoch}"
                    );
                }
            }
            None => { //TODO: review this case
                // It was a self-bond
                let genesis_epoch =
                    anoma_tx_prelude::proof_of_stake::types::Epoch::from(0);
		 let start_epoch =
                        anoma_tx_prelude::proof_of_stake::types::Epoch::from(
                            pos_params.unbonding_len
                        );
                for epoch in 0..pos_params.unbonding_len {
                    let expected_bond =
                        HashMap::from_iter([((genesis_epoch, start_epoch), initial_stake)]);
                    let bond: Unbond<token::Amount> =
                        bonds_post.get(epoch).expect(
                            "Genesis validator should already have self-bond",
                        );
                    assert_eq!(
                        bond.deltas, expected_bond,
                        "Delegation before unbonding offset should be equal to \
                         the genesis initial stake - checking epoch {epoch}"
                    );
                }
                for epoch in pos_params.unbonding_len..=(pos_params.unbonding_len+pos_params.pipeline_len)
                {
                    let start_epoch =
                        anoma_tx_prelude::proof_of_stake::types::Epoch::from(
                            pos_params.pipeline_len,
                        );
                    let expected_bond =  HashMap::from_iter([((genesis_epoch, start_epoch), initial_stake - unbond.amount)]);
                    let bond: Unbond<token::Amount> =
                        bonds_post.get(epoch).unwrap();
                    assert_eq!(
                        bond.deltas, expected_bond,
                        "Delegation at and after pipeline offset should \
                         contain genesis stake and the bonded amount - \
                         checking epoch {epoch}"
                    );
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
                transaction::pos::Bond {
                    validator: Address::Established(validator),
                    amount,
                    source,
                }
            })
    }
     fn arb_initial_stake_and_unbond(
    ) -> impl Strategy<Value = (token::Amount, transaction::pos::Unbond)> {
        // Generate initial stake
        token::testing::arb_amount().prop_flat_map(|initial_stake| {
            // Use the initial stake to limit the bond amount
            let unbond = arb_bond(u64::from(initial_stake));
            // Use the generated initial stake too too
            (Just(initial_stake), unbond)
        })
    }
}
