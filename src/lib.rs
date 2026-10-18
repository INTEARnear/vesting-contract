use near_contract_standards::fungible_token::{Balance, core::ext_ft_core};
use near_sdk::{
    AccountId, Gas, NearToken, PanicOnDefault, Timestamp, assert_one_yocto, env,
    json_types::{U64, U128},
    near,
};

#[near(contract_state)]
#[derive(PanicOnDefault)]
struct Contract {
    token: AccountId,
    owner: AccountId,
    total: Balance,
    claimed: Balance,
    start: Timestamp,
    end: Timestamp,
}

#[near]
impl Contract {
    // The account should already have tokens in it before calling this
    #[init]
    #[private]
    pub fn new(token: AccountId, total: U128, start: U64, end: U64, owner: AccountId) -> Self {
        assert!(total.0 > 0, "total must be greater than 0");
        assert!(end > start, "end must be greater than start");
        assert!(end.0 > env::block_timestamp());

        Self {
            token,
            total: total.0,
            claimed: 0,
            start: start.0,
            end: end.0,
            owner,
        }
    }

    #[payable]
    pub fn claim(&mut self) {
        assert_one_yocto();
        assert_eq!(
            env::predecessor_account_id(),
            self.owner,
            "Only the owner can claim"
        );
        let now = env::block_timestamp() as u128;
        let period_length = self.end.checked_sub(self.start).unwrap() as u128;

        const PRECISION: u128 = 1_000_000;
        let percent_now = now
            .checked_sub(self.start as u128)
            .unwrap()
            .checked_mul(PRECISION)
            .unwrap()
            .checked_div(period_length)
            .unwrap()
            .min(PRECISION);
        let unlocked_now = self
            .total
            .checked_mul(percent_now as u128)
            .unwrap()
            .checked_div(PRECISION)
            .unwrap();
        let claimable_now = unlocked_now.checked_sub(self.claimed).unwrap();
        self.claimed = self.claimed.checked_add(claimable_now).unwrap();
        // assuming owner is registered and transfer can't fail
        ext_ft_core::ext(self.token.clone())
            .with_attached_deposit(NearToken::from_yoctonear(1))
            .with_static_gas(Gas::from_tgas(88))
            .ft_transfer(
                self.owner.clone(),
                claimable_now.into(),
                Some("Vesting claim".to_string()),
            )
            .detach();
    }
}
