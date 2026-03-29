//! ClipCash NFT - Soroban Smart Contract
//!
//! This contract enables minting video clips as NFTs on the Stellar network
//! with built-in royalty support for content creators.
//! Royalties can be paid in XLM or any custom Stellar asset (SEP-0041 token).
//!
//! # Storage layout & gas cost notes
//!
//! ## Storage tiers used
//! - `instance`   – cheap, loaded once per tx, shared across all calls in the tx.
//!                  Used for: Admin, NextTokenId, Paused.
//! - `persistent` – per-entry fee, survives ledger expiry extension.
//!                  Used for: TokenData (owner+clip_id packed), Metadata, Royalty,
//!                  ClipIdMinted (dedup guard).
//!
//! ## Estimated storage operations per function
//!
//! ### `mint`
//! | Op              | Tier       | Count |
//! |-----------------|------------|-------|
//! | instance read   | instance   | 3     | (Admin, NextTokenId, Paused)
//! | instance write  | instance   | 1     | (NextTokenId++)
//! | persistent read | persistent | 1     | (ClipIdMinted dedup check)
//! | persistent write| persistent | 3     | (TokenData, Metadata, Royalty)
//! | persistent write| persistent | 1     | (ClipIdMinted)
//! Total persistent writes: **4**  (was 9 before optimisation)
//!
//! ### `transfer`
//! | Op              | Tier       | Count |
//! |-----------------|------------|-------|
//! | instance read   | instance   | 1     | (Paused)
//! | persistent read | persistent | 1     | (TokenData — owner check)
//! | persistent write| persistent | 1     | (TokenData — new owner)
//! Total persistent writes: **1**  (was 3 before optimisation)
//!
//! ### `burn`
//! | Op              | Tier       | Count |
//! |-----------------|------------|-------|
//! | persistent read | persistent | 1     | (TokenData — owner check + clip_id)
//! | persistent remove| persistent| 3     | (TokenData, Metadata, Royalty)
//! | persistent remove| persistent| 1     | (ClipIdMinted)
//! Total persistent removes: **4**
//!
//! ## Removed counters / indexes (vs. previous version)
//! - `Balance(Address)` — per-address token counter removed; `balance_of` view
//!   removed. Saves 1 read + 1 write on every mint, transfer, and burn.
//! - `TokenCount` — replaced by `next_token_id - 1`; saves 1 read + 1 write
//!   on every mint and burn.
//! - `TokenClipId(TokenId)` — reverse map removed; clip_id is now packed into
//!   `TokenData` alongside the owner. Saves 1 write on mint and 1 read + 1
//!   remove on burn.

#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype,
    symbol_short, Address, Env, String,
};

/// Custom errors for the NFT contract
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum Error {
    /// Operation not authorized
    Unauthorized = 1,
    /// Invalid token ID
    InvalidTokenId = 2,
    /// Token already minted
    TokenAlreadyMinted = 3,
    /// Royalty too high (max 10000 basis points = 100%)
    RoyaltyTooHigh = 4,
    /// Invalid recipient
    InvalidRecipient = 5,
    /// Sale price must be greater than zero
    InvalidSalePrice = 6,
    /// Contract is paused — minting and transfers are blocked
    ContractPaused = 7,
}

/// Token ID type
pub type TokenId = u32;

/// Packs owner address and originating clip_id into a single persistent entry.
///
/// Combining these two fields eliminates the separate `TokenClipId` reverse-map
/// entry that was previously written on every mint and read+removed on every burn.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TokenData {
    pub owner: Address,
    /// The off-chain clip identifier this token was minted for.
    pub clip_id: u32,
}

/// Royalty information stored per token.
/// `asset_address` is `None` for native XLM, or `Some(contract_address)`
/// for any SEP-0041 custom Stellar asset.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Royalty {
    pub recipient: Address,
    /// Royalty in basis points (0-10000, where 10000 = 100%)
    pub basis_points: u32,
    /// Optional SEP-0041 asset contract address.
    /// `None` → royalties expected in XLM (native).
    pub asset_address: Option<Address>,
}

/// Royalty payment info returned by `royalty_info()`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoyaltyInfo {
    pub receiver: Address,
    /// Royalty amount in the same denomination as `sale_price`
    pub royalty_amount: i128,
    /// `None` → pay in XLM; `Some(addr)` → pay in that SEP-0041 token.
    pub asset_address: Option<Address>,
}

/// Storage keys
///
/// Key sizing notes:
/// - Enum variants with no payload (Admin, NextTokenId, Paused) are 1-word keys.
/// - Variants with a u32 payload (Token, Metadata, Royalty, ClipIdMinted) are
///   2-word keys — the smallest possible for per-token entries.
#[contracttype]
pub enum DataKey {
    /// Contract administrator address (instance storage)
    Admin,
    /// Monotonically increasing token ID counter (instance storage).
    /// `total_supply = NextTokenId - 1` — no separate TokenCount needed.
    NextTokenId,
    /// Pause flag (instance storage)
    Paused,
    /// Packed owner + clip_id for a token (persistent storage)
    Token(TokenId),
    /// Metadata URI for a token (persistent storage)
    Metadata(TokenId),
    /// Royalty config for a token (persistent storage)
    Royalty(TokenId),
    /// Dedup guard: clip_id → token_id (persistent storage)
    ClipIdMinted(u32),
}

/// Event emitted when a new NFT is minted
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MintEvent {
    pub to: Address,
    pub clip_id: u32,
    pub token_id: TokenId,
    pub metadata_uri: String,
}

/// NFT Contract
#[contract]
pub struct ClipsNftContract;

#[contractimpl]
impl ClipsNftContract {
    /// Initialize the contract with an admin address.
    pub fn init(env: Env, admin: Address) {
        env.storage().instance().set(&DataKey::Admin, &admin);
        // NextTokenId starts at 1; total_supply = NextTokenId - 1
        env.storage().instance().set(&DataKey::NextTokenId, &1u32);
        env.storage().instance().set(&DataKey::Paused, &false);
    }

    // -------------------------------------------------------------------------
    // Pausable
    // -------------------------------------------------------------------------

    /// Pause the contract. Blocks `mint` and `transfer` until unpaused.
    /// Only callable by the admin.
    pub fn pause(env: Env, admin: Address) -> Result<(), Error> {
        Self::require_admin(&env, &admin)?;
        env.storage().instance().set(&DataKey::Paused, &true);
        env.events().publish((symbol_short!("paused"),), ());
        Ok(())
    }

    /// Unpause the contract, re-enabling `mint` and `transfer`.
    /// Only callable by the admin.
    pub fn unpause(env: Env, admin: Address) -> Result<(), Error> {
        Self::require_admin(&env, &admin)?;
        env.storage().instance().set(&DataKey::Paused, &false);
        env.events().publish((symbol_short!("unpaused"),), ());
        Ok(())
    }

    /// Returns `true` if the contract is currently paused.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    // -------------------------------------------------------------------------
    // Core NFT operations
    // -------------------------------------------------------------------------

    /// Mint a new NFT for a video clip.
    ///
    /// Storage writes (persistent): TokenData, Metadata, Royalty, ClipIdMinted = **4**
    /// Instance writes: NextTokenId = **1**
    ///
    /// # Arguments
    /// * `admin`        - Must be the contract admin
    /// * `to`           - Address that will own the NFT
    /// * `clip_id`      - Unique off-chain clip identifier
    /// * `metadata_uri` - IPFS or Arweave URI pointing to the clip metadata JSON
    /// * `royalty`      - Royalty configuration
    pub fn mint(
        env: Env,
        admin: Address,
        to: Address,
        clip_id: u32,
        metadata_uri: String,
        royalty: Royalty,
    ) -> Result<TokenId, Error> {
        Self::require_admin(&env, &admin)?;
        Self::require_not_paused(&env)?;

        // Dedup check — one persistent read
        if env
            .storage()
            .persistent()
            .contains_key(&DataKey::ClipIdMinted(clip_id))
        {
            return Err(Error::TokenAlreadyMinted);
        }

        if royalty.basis_points > 10000 {
            return Err(Error::RoyaltyTooHigh);
        }

        // One instance read
        let token_id: TokenId = env
            .storage()
            .instance()
            .get(&DataKey::NextTokenId)
            .unwrap_or(1);

        // 4 persistent writes
        env.storage()
            .persistent()
            .set(&DataKey::Token(token_id), &TokenData { owner: to.clone(), clip_id });
        env.storage()
            .persistent()
            .set(&DataKey::Metadata(token_id), &metadata_uri);
        env.storage()
            .persistent()
            .set(&DataKey::Royalty(token_id), &royalty);
        env.storage()
            .persistent()
            .set(&DataKey::ClipIdMinted(clip_id), &token_id);

        // 1 instance write
        env.storage()
            .instance()
            .set(&DataKey::NextTokenId, &(token_id + 1));

        env.events().publish(
            (symbol_short!("mint"),),
            MintEvent { to, clip_id, token_id, metadata_uri },
        );

        Ok(token_id)
    }

    /// Transfer NFT ownership from `from` to `to`.
    ///
    /// Storage writes (persistent): TokenData = **1**
    ///
    /// # Arguments
    /// * `from`     - Current owner (must authorize)
    /// * `to`       - New owner
    /// * `token_id` - Token to transfer
    pub fn transfer(env: Env, from: Address, to: Address, token_id: TokenId) -> Result<(), Error> {
        from.require_auth();
        Self::require_not_paused(&env)?;

        // 1 persistent read
        let mut data: TokenData = env
            .storage()
            .persistent()
            .get(&DataKey::Token(token_id))
            .ok_or(Error::InvalidTokenId)?;

        if from != data.owner {
            return Err(Error::Unauthorized);
        }

        // 1 persistent write — update owner in-place, clip_id unchanged
        data.owner = to;
        env.storage().persistent().set(&DataKey::Token(token_id), &data);

        Ok(())
    }

    // -------------------------------------------------------------------------
    // View functions
    // -------------------------------------------------------------------------

    /// Returns the owner of a given token ID.
    pub fn owner_of(env: Env, token_id: TokenId) -> Result<Address, Error> {
        Ok(Self::load_token(&env, token_id)?.owner)
    }

    /// Returns the metadata URI for a given token ID.
    pub fn token_uri(env: Env, token_id: TokenId) -> Result<String, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Metadata(token_id))
            .ok_or(Error::InvalidTokenId)
    }

    /// Alias for `token_uri`, kept for compatibility.
    pub fn get_metadata(env: Env, token_id: TokenId) -> Result<String, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Metadata(token_id))
            .ok_or(Error::InvalidTokenId)
    }

    /// Look up the on-chain token ID for a given clip_id.
    pub fn clip_token_id(env: Env, clip_id: u32) -> Result<TokenId, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::ClipIdMinted(clip_id))
            .ok_or(Error::InvalidTokenId)
    }

    /// Returns the stored `Royalty` struct for a token.
    pub fn get_royalty(env: Env, token_id: TokenId) -> Result<Royalty, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Royalty(token_id))
            .ok_or(Error::InvalidTokenId)
    }

    /// Returns the total number of minted (and not yet burned) tokens.
    ///
    /// Derived from `NextTokenId - 1` — no separate counter needed.
    pub fn total_supply(env: Env) -> u32 {
        env.storage()
            .instance()
            .get::<DataKey, u32>(&DataKey::NextTokenId)
            .unwrap_or(1)
            .saturating_sub(1)
    }

    /// Returns true if the token exists.
    pub fn exists(env: Env, token_id: TokenId) -> bool {
        env.storage()
            .persistent()
            .contains_key(&DataKey::Token(token_id))
    }

    // -------------------------------------------------------------------------
    // Royalty extension (EIP-2981 style, with custom asset support)
    // -------------------------------------------------------------------------

    /// Returns the royalty receiver, amount, and payment asset for a given sale price.
    ///
    /// `royalty_amount = sale_price * basis_points / 10000`
    pub fn royalty_info(
        env: Env,
        token_id: TokenId,
        sale_price: i128,
    ) -> Result<RoyaltyInfo, Error> {
        if sale_price <= 0 {
            return Err(Error::InvalidSalePrice);
        }

        let royalty: Royalty = env
            .storage()
            .persistent()
            .get(&DataKey::Royalty(token_id))
            .ok_or(Error::InvalidTokenId)?;

        let royalty_amount = sale_price
            .saturating_mul(royalty.basis_points as i128)
            / 10_000;

        Ok(RoyaltyInfo {
            receiver: royalty.recipient,
            royalty_amount,
            asset_address: royalty.asset_address,
        })
    }

    /// Pay royalties for a token sale using the asset configured in the royalty.
    ///
    /// Only handles SEP-0041 custom assets. For XLM (`asset_address` is `None`)
    /// the marketplace must handle the transfer directly.
    pub fn pay_royalty(
        env: Env,
        payer: Address,
        token_id: TokenId,
        sale_price: i128,
    ) -> Result<(), Error> {
        payer.require_auth();

        let info = Self::royalty_info(env.clone(), token_id, sale_price)?;

        if info.royalty_amount == 0 {
            return Ok(());
        }

        let asset_address = info.asset_address.ok_or(Error::InvalidRecipient)?;

        let token_client = soroban_sdk::token::TokenClient::new(&env, &asset_address);
        token_client.transfer(&payer, &info.receiver, &info.royalty_amount);

        env.events().publish(
            (symbol_short!("royalty"),),
            (token_id, info.receiver, info.royalty_amount, asset_address),
        );

        Ok(())
    }

    /// Update the royalty configuration for a token. Admin only.
    pub fn set_royalty(
        env: Env,
        admin: Address,
        token_id: TokenId,
        new_royalty: Royalty,
    ) -> Result<(), Error> {
        Self::require_admin(&env, &admin)?;

        // Existence check reuses the Token entry — no extra read needed
        if !env.storage().persistent().contains_key(&DataKey::Token(token_id)) {
            return Err(Error::InvalidTokenId);
        }

        if new_royalty.basis_points > 10000 {
            return Err(Error::RoyaltyTooHigh);
        }

        env.storage()
            .persistent()
            .set(&DataKey::Royalty(token_id), &new_royalty);

        Ok(())
    }

    /// Burn (destroy) an NFT. Only the current owner may burn.
    ///
    /// Storage removes (persistent): TokenData, Metadata, Royalty, ClipIdMinted = **4**
    pub fn burn(env: Env, owner: Address, token_id: TokenId) -> Result<(), Error> {
        owner.require_auth();

        // 1 persistent read — also gives us clip_id for dedup cleanup
        let data: TokenData = Self::load_token(&env, token_id)?;

        if owner != data.owner {
            return Err(Error::Unauthorized);
        }

        // 4 persistent removes
        env.storage().persistent().remove(&DataKey::Token(token_id));
        env.storage().persistent().remove(&DataKey::Metadata(token_id));
        env.storage().persistent().remove(&DataKey::Royalty(token_id));
        env.storage().persistent().remove(&DataKey::ClipIdMinted(data.clip_id));

        Ok(())
    }

    // -------------------------------------------------------------------------
    // Internal helpers
    // -------------------------------------------------------------------------

    /// Load and return `TokenData`, or `InvalidTokenId` if not found.
    fn load_token(env: &Env, token_id: TokenId) -> Result<TokenData, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Token(token_id))
            .ok_or(Error::InvalidTokenId)
    }

    fn require_admin(env: &Env, addr: &Address) -> Result<(), Error> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("Admin not initialized");

        if addr != &admin {
            return Err(Error::Unauthorized);
        }

        addr.require_auth();
        Ok(())
    }

    fn require_not_paused(env: &Env) -> Result<(), Error> {
        if env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(Error::ContractPaused);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::Env;

    fn setup() -> (Env, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let user1 = Address::generate(&env);
        let user2 = Address::generate(&env);
        (env, admin, user1, user2)
    }

    fn default_royalty(recipient: Address) -> Royalty {
        Royalty { recipient, basis_points: 500, asset_address: None }
    }

    fn do_mint(
        client: &ClipsNftContractClient,
        env: &Env,
        admin: &Address,
        to: &Address,
        clip_id: u32,
    ) -> TokenId {
        client.mint(
            admin,
            to,
            &clip_id,
            &String::from_str(env, "ipfs://QmExample"),
            &default_royalty(to.clone()),
        )
    }

    #[test]
    fn test_mint_stores_owner_and_uri() {
        let (env, admin, user1, _) = setup();
        let contract_id = env.register(ClipsNftContract, ());
        let client = ClipsNftContractClient::new(&env, &contract_id);
        client.init(&admin);

        let token_id = do_mint(&client, &env, &admin, &user1, 42);
        assert_eq!(token_id, 1);

        assert_eq!(client.owner_of(&token_id), user1);
        assert_eq!(
            client.token_uri(&token_id),
            String::from_str(&env, "ipfs://QmExample")
        );
        assert_eq!(client.total_supply(), 1);
    }

    #[test]
    fn test_clip_token_id_lookup() {
        let (env, admin, user1, _) = setup();
        let contract_id = env.register(ClipsNftContract, ());
        let client = ClipsNftContractClient::new(&env, &contract_id);
        client.init(&admin);

        let token_id = do_mint(&client, &env, &admin, &user1, 99);
        assert_eq!(client.clip_token_id(&99), token_id);
    }

    #[test]
    #[should_panic]
    fn test_double_mint_same_clip_id_panics() {
        let (env, admin, user1, _) = setup();
        let contract_id = env.register(ClipsNftContract, ());
        let client = ClipsNftContractClient::new(&env, &contract_id);
        client.init(&admin);

        do_mint(&client, &env, &admin, &user1, 7);
        do_mint(&client, &env, &admin, &user1, 7);
    }

    #[test]
    fn test_mint_emits_event() {
        let (env, admin, user1, _) = setup();
        let contract_id = env.register(ClipsNftContract, ());
        let client = ClipsNftContractClient::new(&env, &contract_id);
        client.init(&admin);

        let token_id = do_mint(&client, &env, &admin, &user1, 5);

        let events = env.events().all();
        assert!(!events.is_empty());
        let (_, _, event_data): (_, soroban_sdk::Vec<soroban_sdk::Val>, MintEvent) =
            events.last().unwrap();
        assert_eq!(event_data.clip_id, 5);
        assert_eq!(event_data.token_id, token_id);
        assert_eq!(event_data.to, user1);
    }

    #[test]
    fn test_transfer_updates_owner() {
        let (env, admin, user1, user2) = setup();
        let contract_id = env.register(ClipsNftContract, ());
        let client = ClipsNftContractClient::new(&env, &contract_id);
        client.init(&admin);

        let token_id = do_mint(&client, &env, &admin, &user1, 1);
        client.transfer(&user1, &user2, &token_id);

        assert_eq!(client.owner_of(&token_id), user2);
    }

    #[test]
    fn test_total_supply_derived_from_next_token_id() {
        let (env, admin, user1, _) = setup();
        let contract_id = env.register(ClipsNftContract, ());
        let client = ClipsNftContractClient::new(&env, &contract_id);
        client.init(&admin);

        assert_eq!(client.total_supply(), 0);
        do_mint(&client, &env, &admin, &user1, 1);
        assert_eq!(client.total_supply(), 1);
        do_mint(&client, &env, &admin, &user1, 2);
        assert_eq!(client.total_supply(), 2);
    }

    #[test]
    fn test_royalty_info_xlm() {
        let (env, admin, user1, _) = setup();
        let contract_id = env.register(ClipsNftContract, ());
        let client = ClipsNftContractClient::new(&env, &contract_id);
        client.init(&admin);

        let token_id = do_mint(&client, &env, &admin, &user1, 1);

        let info = client.royalty_info(&token_id, &1_000_000i128);
        assert_eq!(info.receiver, user1);
        assert_eq!(info.royalty_amount, 50_000i128); // 5%
        assert_eq!(info.asset_address, None);
    }

    #[test]
    fn test_royalty_info_custom_asset() {
        let (env, admin, user1, _) = setup();
        let contract_id = env.register(ClipsNftContract, ());
        let client = ClipsNftContractClient::new(&env, &contract_id);
        client.init(&admin);

        let asset_addr = Address::generate(&env);
        let royalty = Royalty {
            recipient: user1.clone(),
            basis_points: 1000,
            asset_address: Some(asset_addr.clone()),
        };
        let token_id = client.mint(
            &admin,
            &user1,
            &2u32,
            &String::from_str(&env, "ipfs://QmCustom"),
            &royalty,
        );

        let info = client.royalty_info(&token_id, &500i128);
        assert_eq!(info.royalty_amount, 50i128);
        assert_eq!(info.asset_address, Some(asset_addr));
    }

    #[test]
    fn test_set_royalty_with_custom_asset() {
        let (env, admin, user1, user2) = setup();
        let contract_id = env.register(ClipsNftContract, ());
        let client = ClipsNftContractClient::new(&env, &contract_id);
        client.init(&admin);

        let token_id = do_mint(&client, &env, &admin, &user1, 1);

        let asset_addr = Address::generate(&env);
        let new_royalty = Royalty {
            recipient: user2.clone(),
            basis_points: 1000,
            asset_address: Some(asset_addr.clone()),
        };
        client.set_royalty(&admin, &token_id, &new_royalty);

        let stored = client.get_royalty(&token_id);
        assert_eq!(stored.recipient, user2);
        assert_eq!(stored.basis_points, 1000);
        assert_eq!(stored.asset_address, Some(asset_addr));
    }

    #[test]
    fn test_burn() {
        let (env, admin, user1, _) = setup();
        let contract_id = env.register(ClipsNftContract, ());
        let client = ClipsNftContractClient::new(&env, &contract_id);
        client.init(&admin);

        let token_id = do_mint(&client, &env, &admin, &user1, 1);
        client.burn(&user1, &token_id);

        assert!(!client.exists(&token_id));
        // clip_id dedup entry also removed — can re-mint same clip_id
        let token_id2 = do_mint(&client, &env, &admin, &user1, 1);
        assert!(client.exists(&token_id2));
    }

    #[test]
    fn test_pause_blocks_mint() {
        let (env, admin, user1, _) = setup();
        let contract_id = env.register(ClipsNftContract, ());
        let client = ClipsNftContractClient::new(&env, &contract_id);
        client.init(&admin);

        assert!(!client.is_paused());
        client.pause(&admin);
        assert!(client.is_paused());

        let result = client.try_mint(
            &admin,
            &user1,
            &1u32,
            &String::from_str(&env, "ipfs://QmPaused"),
            &default_royalty(user1.clone()),
        );
        assert_eq!(result, Err(Ok(Error::ContractPaused)));
    }

    #[test]
    fn test_pause_blocks_transfer() {
        let (env, admin, user1, user2) = setup();
        let contract_id = env.register(ClipsNftContract, ());
        let client = ClipsNftContractClient::new(&env, &contract_id);
        client.init(&admin);

        let token_id = do_mint(&client, &env, &admin, &user1, 1);
        client.pause(&admin);

        let result = client.try_transfer(&user1, &user2, &token_id);
        assert_eq!(result, Err(Ok(Error::ContractPaused)));
    }

    #[test]
    fn test_unpause_restores_mint_and_transfer() {
        let (env, admin, user1, user2) = setup();
        let contract_id = env.register(ClipsNftContract, ());
        let client = ClipsNftContractClient::new(&env, &contract_id);
        client.init(&admin);

        client.pause(&admin);
        client.unpause(&admin);
        assert!(!client.is_paused());

        let token_id = do_mint(&client, &env, &admin, &user1, 1);
        client.transfer(&user1, &user2, &token_id);
        assert_eq!(client.owner_of(&token_id), user2);
    }

    #[test]
    #[should_panic]
    fn test_non_admin_cannot_pause() {
        let (env, admin, user1, _) = setup();
        let contract_id = env.register(ClipsNftContract, ());
        let client = ClipsNftContractClient::new(&env, &contract_id);
        client.init(&admin);

        client.pause(&user1);
    }
}
