use scrypto::prelude::*;

external_component! {
    PriceOracleComponentTarget {
        fn get_price(&self, base: ResourceAddress, quote: ResourceAddress) -> Option<Decimal>;
        fn update_price(&self, base: ResourceAddress, quote: ResourceAddress, price: Decimal);
        fn admin_badge_address(&self) -> ResourceAddress;
    }
}

// Main missing features:
// - Liquidation
// - Authorization through badge

#[blueprint]
mod synthetic_pool {
    struct SyntheticPool {
        /// The price oracle
        oracle_address: ComponentAddress,
        /// The collateralization ratio one has to maintain when minting synthetics
        collateralization_threshold: Decimal,
        /// SNX resource address
        snx_resource_address: ResourceAddress,
        /// USD resource address
        usd_resource_address: ResourceAddress,

        /// Users
        users: KeyValueStore<ResourceAddress, User>,
        /// Synthetics
        synthetics: HashMap<String, SyntheticToken>,
        /// Mint badge
        synthetics_mint_badge: Vault,
        /// Global debt
        synthetics_global_debt_share_resource_address: ResourceAddress,
    }

    impl SyntheticPool {
        pub fn instantiate_pool(
            oracle_address: ComponentAddress,
            snx_token_address: ResourceAddress,
            usd_token_address: ResourceAddress,
            collateralization_threshold: Decimal,
        ) -> ComponentAddress {
            let synthetics_mint_badge = ResourceBuilder::new_fungible()
                .divisibility(DIVISIBILITY_NONE)
                .metadata("name", "Synthetics Mint Badge")
                .mint_initial_supply(1);
            let synthetics_global_debt_share_resource_address = ResourceBuilder::new_fungible()
                .divisibility(DIVISIBILITY_MAXIMUM)
                .metadata("name", "Synthetics Global Debt")
                .mintable(
                    rule!(require(synthetics_mint_badge.resource_address())),
                    LOCKED,
                )
                .burnable(
                    rule!(require(synthetics_mint_badge.resource_address())),
                    LOCKED,
                )
                .create_with_no_initial_supply();

            Self {
                oracle_address,
                collateralization_threshold,
                snx_resource_address: snx_token_address,
                usd_resource_address: usd_token_address,
                users: KeyValueStore::new(),
                synthetics: HashMap::new(),
                synthetics_mint_badge: Vault::with_bucket(synthetics_mint_badge),
                synthetics_global_debt_share_resource_address,
            }
            .instantiate()
            .globalize()
        }

        /// Add new a new synthetic token to the protocol
        pub fn add_synthetic_token(
            &mut self,
            asset_symbol: String,
            asset_address: ResourceAddress,
        ) -> ResourceAddress {
            assert!(
                !self.synthetics.contains_key(&asset_symbol),
                "Asset already exist",
            );

            let token_resource_address = ResourceBuilder::new_fungible()
                .divisibility(DIVISIBILITY_MAXIMUM)
                .metadata("name", format!("Synthetic {}", asset_symbol.clone()))
                .metadata("symbol", format!("s{}", asset_symbol.clone()))
                .mintable(
                    rule!(require(self.synthetics_mint_badge.resource_address())),
                    LOCKED,
                )
                .burnable(
                    rule!(require(self.synthetics_mint_badge.resource_address())),
                    LOCKED,
                )
                .create_with_no_initial_supply();

            self.synthetics.insert(
                asset_symbol.clone(),
                SyntheticToken::new(asset_symbol, asset_address, token_resource_address.clone()),
            );

            token_resource_address
        }

        /// Deposits SNX into my staking account
        pub fn stake(&mut self, user_auth: Proof, stake_in_snx: Bucket) {
            let user_id = Self::get_user_id(user_auth);
            let mut user = self.get_user(user_id, true);
            user.snx.put(stake_in_snx);
        }

        /// Withdraws SNX from my staking account.
        pub fn unstake(&mut self, user_auth: Proof, amount: Decimal) -> Bucket {
            let user_id = Self::get_user_id(user_auth);
            let mut user = self.get_user(user_id, false);

            let tokens = user.snx.take(amount);
            user.check_collateralization_ratio(
                self.get_snx_price(),
                self.get_total_global_debt(),
                self.synthetics_global_debt_share_resource_address.clone(),
                self.collateralization_threshold,
            );
            tokens
        }

        /// Mints synthetics tokens
        pub fn mint(&mut self, user_auth: Proof, amount: Decimal, symbol: String) -> Bucket {
            let user_id = Self::get_user_id(user_auth);
            let mut user = self.get_user(user_id, false);

            let synth = self.synthetics.get(&symbol).unwrap().clone();
            let global_debt = self.get_total_global_debt();
            let new_debt = self.get_asset_price(synth.asset_address) * amount;

            user.global_debt_share
                .put(self.synthetics_mint_badge.authorize(|| {
                    let synthetics_global_debt_share_resource_manager = borrow_resource_manager!(
                        self.synthetics_global_debt_share_resource_address
                    );
                    synthetics_global_debt_share_resource_manager.mint(if global_debt.is_zero() {
                        dec!("100")
                    } else {
                        new_debt
                            / (global_debt
                                / synthetics_global_debt_share_resource_manager.total_supply())
                    })
                }));
            let tokens = self.synthetics_mint_badge.authorize(|| {
                let token_resource_manager = borrow_resource_manager!(synth.token_resource_address);
                token_resource_manager.mint(amount)
            });
            user.check_collateralization_ratio(
                self.get_snx_price(),
                self.get_total_global_debt(),
                self.synthetics_global_debt_share_resource_address.clone(),
                self.collateralization_threshold,
            );
            tokens
        }

        /// Burns synthetic tokens
        pub fn burn(&mut self, user_auth: Proof, bucket: Bucket) {
            let user_id = Self::get_user_id(user_auth);
            let mut user = self.get_user(user_id, false);

            let synth = self
                .synthetics
                .iter()
                .find(|(_, v)| v.token_resource_address == bucket.resource_address())
                .unwrap()
                .1;
            let global_debt = self.get_total_global_debt();
            let debt_to_remove = self.get_asset_price(synth.asset_address) * bucket.amount();
            let shares_to_burn = user.global_debt_share.take(
                borrow_resource_manager!(self.synthetics_global_debt_share_resource_address)
                    .total_supply()
                    * debt_to_remove
                    / global_debt,
            );

            self.synthetics_mint_badge.authorize(|| {
                shares_to_burn.burn();
            });
            self.synthetics_mint_badge.authorize(|| {
                bucket.burn();
            });
        }

        /// Returns the total global debt.
        pub fn get_total_global_debt(&self) -> Decimal {
            let mut total = Decimal::zero();
            for (_, synth) in &self.synthetics {
                total += self.get_asset_price(synth.asset_address)
                    * borrow_resource_manager!(synth.token_resource_address).total_supply();
            }
            total
        }

        /// Retrieves the price of pair SNX/USD
        pub fn get_snx_price(&self) -> Decimal {
            self.get_asset_price(self.snx_resource_address)
        }

        /// Retrieves the prices of pair XYZ/USD
        pub fn get_asset_price(&self, asset_address: ResourceAddress) -> Decimal {
            let oracle: PriceOracleComponentTarget = self.oracle_address.into();
            if let Some(oracle_price) = oracle.get_price(asset_address, self.usd_resource_address) {
                oracle_price
            } else {
                panic!(
                    "Failed to obtain price of {:?}/{:?}",
                    asset_address, self.usd_resource_address
                );
            }
        }

        /// Retrieves user summary.
        pub fn get_user_summary(&mut self, user_id: ResourceAddress) -> String {
            let user = self.get_user(user_id, false);
            format!(
                "SNX balance: {}, SNX price: {}, Debt: {} * {} / {}",
                user.snx.amount(),
                self.get_snx_price(),
                self.get_total_global_debt(),
                user.global_debt_share.amount(),
                borrow_resource_manager!(self.synthetics_global_debt_share_resource_address)
                    .total_supply()
            )
        }

        /// Registers a new user
        pub fn new_user(&self) -> Bucket {
            ResourceBuilder::new_fungible()
                .divisibility(DIVISIBILITY_NONE)
                .metadata("name", "Synthetic Pool User Badge")
                .mint_initial_supply(1)
        }

        /// Parse user id from proof.
        fn get_user_id(user_auth: Proof) -> ResourceAddress {
            user_auth.unsafe_skip_proof_validation().resource_address()
        }

        /// Retrieves user state.
        fn get_user(
            &mut self,
            user_id: ResourceAddress,
            create_if_missing: bool,
        ) -> scrypto::runtime::DataRefMut<User> {
            if let Some(user) = self.users.get_mut(&user_id) {
                user
            } else if create_if_missing {
                self.users.insert(
                    user_id,
                    User::new(
                        self.snx_resource_address,
                        self.synthetics_global_debt_share_resource_address,
                    ),
                );
                self.users.get_mut(&user_id).unwrap()
            } else {
                panic!("User not found");
            }
        }
    }
}

#[derive(
    Debug, ScryptoCategorize, ScryptoEncode, ScryptoDecode, Clone, LegacyDescribe, PartialEq, Eq,
)]
pub struct SyntheticToken {
    /// The symbol of the asset
    asset_symbol: String,
    /// The resource address of the asset
    asset_address: ResourceAddress,
    /// The synth (sXYZ) resource address
    token_resource_address: ResourceAddress,
}

impl SyntheticToken {
    pub fn new(
        asset_symbol: String,
        asset_address: ResourceAddress,
        token_resource_address: ResourceAddress,
    ) -> Self {
        Self {
            asset_symbol,
            asset_address,
            token_resource_address,
        }
    }
}

#[derive(Debug, ScryptoCategorize, LegacyDescribe, ScryptoEncode, ScryptoDecode)]
pub struct User {
    snx: Vault,
    global_debt_share: Vault,
}

impl User {
    pub fn new(snx_address: ResourceAddress, global_debt_share_address: ResourceAddress) -> Self {
        Self {
            snx: Vault::new(snx_address),
            global_debt_share: Vault::new(global_debt_share_address),
        }
    }

    // Checks the collateralization ratio of this user
    pub fn check_collateralization_ratio(
        &self,
        snx_price: Decimal,
        global_debt: Decimal,
        global_debt_resource_address: ResourceAddress,
        threshold: Decimal,
    ) {
        let resource_manager = borrow_resource_manager!(global_debt_resource_address);
        if !resource_manager.total_supply().is_zero() && !self.global_debt_share.amount().is_zero()
        {
            assert!(
                self.snx.amount() * snx_price
                    / (global_debt / resource_manager.total_supply()
                        * self.global_debt_share.amount())
                    >= threshold,
                "Under collateralized!",
            );
        }
    }
}
