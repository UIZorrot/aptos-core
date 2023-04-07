module token_objects::champ {
    use std::error;
    use std::option;
    use std::string::{Self, String};
    use std::signer;

    use aptos_framework::object::{Self, Object};
    use aptos_token_objects::collection;
    use aptos_token_objects::property_map;
    use aptos_token_objects::royalty;
    use aptos_token_objects::token;

    // The token does not exist
    const ETOKEN_DOES_NOT_EXIST: u64 = 1;
    /// The provided signer is not the creator
    const ENOT_CREATOR: u64 = 2;
    /// Attempted to mutate an immutable field
    const EFIELD_NOT_MUTABLE: u64 = 3;
    /// Attempted to burn a non-burnable token
    const ETOKEN_NOT_BURNABLE: u64 = 4;
    /// Attempted to mutate a property map that is not mutable
    const EPROPERTIES_NOT_MUTABLE: u64 = 5;
    // The collection does not exist
    const ECOLLECTION_DOES_NOT_EXIST: u64 = 6;
    /// The provided signer is not an authorized worker
    const ENOT_WORKER: u64 = 7;

    struct ChampCollection has key {
        /// Used to mutate collection fields
        mutator_ref: collection::MutatorRef,
        /// Used to mutate royalties
        royalty_mutator_ref: royalty::MutatorRef,
    }

    struct ChampToken has key {
        /// Used to burn.
        burn_ref: token::BurnRef,
        /// Used to control freeze.
        transfer_ref: object::TransferRef,
        /// Used to mutate fields
        mutator_ref: token::MutatorRef,
        /// Used to mutate properties
        property_mutator_ref: property_map::MutatorRef,
    }

    public fun create_champ_collection(
        creator: &signer,
        description: String,
        name: String,
        uri: String,
        royalty_numerator: u64,
        royalty_denominator: u64,
    ) {
        let creator_addr = signer::address_of(creator);
        let royalty = royalty::create(royalty_numerator, royalty_denominator, creator_addr);
        let constructor_ref = collection::create_unlimited_collection(
            creator,
            description,
            name,
            option::some(royalty),
            uri,
        );
        let object_signer = object::generate_signer(&constructor_ref);
        let mutator_ref = collection::generate_mutator_ref(&constructor_ref);
        let royalty_mutator_ref = royalty::generate_mutator_ref(object::generate_extend_ref(&constructor_ref));
        let champ_collection = ChampCollection {
            mutator_ref,
            royalty_mutator_ref,
        };
        move_to(&object_signer, champ_collection);
    }

    public fun mint_champ_token(
        creator: &signer,
        collection: String,
        description: String,
        name: String,
        uri: String,
        property_keys: vector<String>,
        property_types: vector<String>,
        property_values: vector<vector<u8>>,
    ): Object<ChampToken> {
        let constructor_ref = token::create_named_token(
            creator,
            collection,
            description,
            name,
            option::none(),
            uri,
        );
        let object_signer = object::generate_signer(&constructor_ref);
        let mutator_ref = token::generate_mutator_ref(&constructor_ref);
        let transfer_ref = object::generate_transfer_ref(&constructor_ref);
        let burn_ref = token::generate_burn_ref(&constructor_ref);

        let champ_token = ChampToken {
            burn_ref,
            transfer_ref,
            mutator_ref,
            property_mutator_ref: property_map::generate_mutator_ref(&constructor_ref),
        };
        move_to(&object_signer, champ_token);

        let properties = property_map::prepare_input(property_keys, property_types, property_values);
        property_map::init(&constructor_ref, properties);

        object::object_from_constructor_ref<ChampToken>(&constructor_ref)
    }

    inline fun authorized_borrow<T: key>(token: &Object<T>, creator: &signer): &ChampToken {
        let token_address = object::object_address(token);
        assert!(
            exists<ChampToken>(token_address),
            error::not_found(ETOKEN_DOES_NOT_EXIST),
        );

        assert!(
            token::creator(*token) == signer::address_of(creator),
            error::permission_denied(ENOT_CREATOR),
        );
        borrow_global<ChampToken>(token_address)
    }

    const WORKER_ADDR: address = @0xabc;
    inline fun is_authorization_worker(worker: &signer): bool {
        // TODO: Add your own worker authorization logic here. This is just a placeholder.
        signer::address_of(worker) == WORKER_ADDR
    }

    inline fun authorized_worker_borrow<T: key>(token: &Object<T>, worker: &signer): &ChampToken {
        let token_address = object::object_address(token);
        assert!(
            exists<ChampToken>(token_address),
            error::not_found(ETOKEN_DOES_NOT_EXIST),
        );
        assert!(
            is_authorization_worker(worker),
            error::permission_denied(ENOT_WORKER),
        );
        borrow_global<ChampToken>(token_address)
    }

    #[test(creator = @0x123, user1 = @0x456, user2_addr = @0x789, worker = @0xabc)]
    fun test_create_and_transfer(creator: &signer, user1: &signer, user2_addr: address, worker: &signer) acquires ChampToken {
        // -------------------------------------
        // Creator creates the Champ Collection.
        // -------------------------------------
        let collection_name = string::utf8(b"Champ Collection Name");
        let collection_description = string::utf8(b"Champ Collection Description");
        let collection_uri = string::utf8(b"Champ Collection URI");
        create_champ_collection(creator, collection_description, collection_name, collection_uri, 1, 100);

        // -----------------------------
        // Creator mints a Champ token.
        // -----------------------------
        let token_name = string::utf8(b"Champ Token 1");
        let token_description = string::utf8(b"description for Champ Token 1");
        let token_uri = string::utf8(b"uri for Champ Token 1");
        let token = mint_champ_token(
            creator,
            collection_name,
            token_description,
            token_name,
            token_uri,
            vector[string::utf8(b"bool")],
            vector[string::utf8(b"bool")],
            vector[vector[0x01]],
        );
        // Assert that the token is owned by the creator.
        assert!(object::owner(token) == signer::address_of(creator), 1);

        // ------------------------------------
        // Creator transfers the token to User1.
        // ------------------------------------
        let user1_addr = signer::address_of(user1);
        object::transfer(creator, token, user1_addr);
        // Assert that the token is transferred to `user1`.
        assert!(object::owner(token) == user1_addr, 2);

        // -----------------------------------
        // User1 transfers the token to User2.
        // -----------------------------------
        object::transfer(user1, token, user2_addr);
        // Assert that the token is transferred to `user2`.
        assert!(object::owner(token) == user2_addr, 3);

        // ----------------------------------------------------
        // Creator transfers the token owned by User2 to User1.
        // ----------------------------------------------------
        let champ_token_ref = authorized_borrow<ChampToken>(&token, creator);
        let trans_ref = &champ_token_ref.transfer_ref;
        let linear_trans_ref = object::generate_linear_transfer_ref(trans_ref);
        object::transfer_with_ref(linear_trans_ref, user2_addr);
        assert!(object::owner(token) == user2_addr, 4);

        // ---------------------------------------------------
        // Worker transfers the token owned by User1 to User2.
        // ---------------------------------------------------
        let champ_token_ref = authorized_worker_borrow<ChampToken>(&token, worker);
        let trans_ref = &champ_token_ref.transfer_ref;
        let linear_trans_ref = object::generate_linear_transfer_ref(trans_ref);
        object::transfer_with_ref(linear_trans_ref, user1_addr);
        assert!(object::owner(token) == user1_addr, 5);
    }
}
