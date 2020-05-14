//! The Substrate Node runtime. This can be compiled with `#[no_std]`, ready for Wasm.

#![cfg_attr(not(feature = "std"), no_std)]
#![feature(option_result_contains)]

use codec::{Decode, Encode};
use frame_support::{
    decl_event, decl_module, decl_storage, decl_error, dispatch::DispatchResult, ensure,
    weights::SimpleDispatchInfo
};
use sp_std::{prelude::*, convert::TryInto, collections::btree_map::BTreeMap};
use system::ensure_signed;
use sp_runtime::{traits::StaticLookup};

#[cfg(feature = "std")]
use serde::{Deserialize, Serialize};

// Crust runtime modules
use primitives::{
    Address, MerkleRoot, Balance, BlockNumber, Hash,
    constants::tee::REPORT_SLOT
};

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests;

#[derive(Debug, PartialEq, Eq, Clone, Encode, Decode, Default)]
#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
pub struct StorageOrder<AccountId> {
    pub file_identifier: MerkleRoot,
    pub file_size: u64,
    pub created_on: BlockNumber,
    pub expired_on: BlockNumber,
    pub provider: AccountId,
    pub client: AccountId,
    pub order_status: OrderStatus
}

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
pub enum OrderStatus {
    Success,
    Failed,
    Pending
}

impl Default for OrderStatus {
    fn default() -> Self {
        OrderStatus::Pending
    }
}

/// Preference of what happens regarding validation.
#[derive(Debug, PartialEq, Eq, Clone, Encode, Decode, Default)]
#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
pub struct Provision {
    /// Vendor's address
    pub address: Address,

    /// Mapping from `file_id` to `order_id`, this mapping only add when user place the order
    pub file_map: BTreeMap<MerkleRoot, Hash>,
}

/// An event handler for paying market order
pub trait Payment<AccountId> {
    // Pay the storage order, return an UNIQUE `transaction id`🙏🏻
    fn pay_sorder(transactor: &AccountId, dest: &AccountId, value: Balance) -> Hash;
}

impl<AId> Payment<AId> for () {
    fn pay_sorder(_: &AId, _: &AId, _: Balance) -> Hash {
        // transfer the fee and return order id
        // TODO: using random to generate non-duplicated order id
        Hash::default()
    }
}

/// A trait for checking order's legality
/// This wanyi is an outer inspector to judge if s/r order can be accepted 😵
pub trait OrderInspector<AccountId> {
    // check if the provider can take storage order
    fn check_works(provider: &AccountId, file_size: u64) -> bool;
}

/// The module's configuration trait.
pub trait Trait: system::Trait {
    /// The overarching event type.
    type Event: From<Event<Self>> + Into<<Self as system::Trait>::Event>;
    type Payment: Payment<Self::AccountId>;
    type OrderInspector: OrderInspector<Self::AccountId>;
}

// This module's storage items.
decl_storage! {
    trait Store for Module<T: Trait> as Market {
        /// A mapping from storage provider to order id
        pub Providers get(fn providers):
        map hasher(twox_64_concat) T::AccountId => Option<Provision>;

        /// A mapping from clients to order id
        pub Clients get(fn clients):
        map hasher(twox_64_concat) T::AccountId => Option<Vec<Hash>>;

        /// Order details iterated by order id
        pub StorageOrders get(fn storage_orders):
        map hasher(twox_64_concat) Hash => Option<StorageOrder<T::AccountId>>;
    }
}

decl_error! {
    /// Error for the market module.
    pub enum Error for Module<T: Trait> {
        /// Duplicate order id.
		DuplicateOrderId,
		/// No workload
		NoWorkload,
		/// Not provider
		NotProvider,
		/// File duration is too short
		DurationTooShort
    }
}

// The module's dispatchable functions.
decl_module! {
    /// The module declaration.
    pub struct Module<T: Trait> for enum Call where origin: T::Origin {
        // Initializing events
        // this is needed only if you are using events in your module
        fn deposit_event() = default;

        type Error = Error<T>;

        /// Register to be a provider, you should provide your Karst's address{ip, port}
        #[weight = SimpleDispatchInfo::default()]
        fn register(origin, address: Address) -> DispatchResult {
            let who = ensure_signed(origin)?;

            // 1. Make sure you have works
            ensure!(T::OrderInspector::check_works(&who, 0), Error::<T>::NoWorkload);

            // 2. Insert provision
            <Providers<T>>::insert(who.clone(), Provision {
                address,
                file_map: BTreeMap::new()
            });

            // 3. Emit success
            Self::deposit_event(RawEvent::RegisterSuccess(who));

            Ok(())
        }

        /// TODO: organize these parameters into a struct.
        #[weight = SimpleDispatchInfo::default()]
        fn place_storage_order(
            origin,
            dest: <T::Lookup as StaticLookup>::Source,
            #[compact] value: Balance,
            file_identifier: MerkleRoot,
            file_size: u64,
            duration: u32
        ) -> DispatchResult
            {
                let who = ensure_signed(origin)?;
                let provider = T::Lookup::lookup(dest)?;

                // 1. Expired should be greater than created
                ensure!(duration > REPORT_SLOT.try_into().unwrap(), Error::<T>::DurationTooShort);

                // 2. Check if provider is registered
                ensure!(<Providers<T>>::contains_key(&provider), Error::<T>::NotProvider);

                // 3. Check provider has capacity to store file
                ensure!(T::OrderInspector::check_works(&provider, file_size), Error::<T>::NoWorkload);

                // 4. Construct storage order
                let created_on = TryInto::<u32>::try_into(<system::Module<T>>::block_number()).ok().unwrap();
                let storage_order = StorageOrder::<T::AccountId> {
                    file_identifier,
                    file_size,
                    created_on,
                    expired_on: created_on + duration, // this will changed, when `order_status` become `Success`
                    provider: provider.clone(),
                    client: who.clone(),
                    order_status: OrderStatus::Pending
                };

                // 5. Pay the order and (maybe) add storage order
                if Self::maybe_insert_sorder(&who, &provider, value, &storage_order) {
                    // a. emit storage order event
                    Self::deposit_event(RawEvent::StorageOrderSuccess(who, storage_order));
                } else {
                    // b. emit error
                    Err(Error::<T>::DuplicateOrderId)?
                }

                Ok(())
            }
    }
}

impl<T: Trait> Module<T> {
    // MUTABLE PRIVATE
    // sorder is equal to storage order
    fn maybe_insert_sorder(client: &T::AccountId,
                           provider: &T::AccountId,
                           value: Balance,
                           so: &StorageOrder<T::AccountId>) -> bool {
        let order_id = T::Payment::pay_sorder(&client, &provider, value);

        // This should be false, cause we don't allow duplicated `order_id`
        if <StorageOrders<T>>::contains_key(&order_id) {
            false
        } else {
            // 1. Add new storage order
            <StorageOrders<T>>::insert(order_id, so);

            // 2. Add `order_id` to client orders
            <Clients<T>>::mutate(client, |maybe_client_orders| {
                if let Some(mut client_order) = maybe_client_orders.clone() {
                    client_order.push(order_id.clone());
                    *maybe_client_orders = Some(client_order)
                } else {
                    *maybe_client_orders = Some(vec![order_id.clone()])
                }
            });

            // 3. Add `file_identifier` -> `order_id` to provider's file_map
            <Providers<T>>::mutate(provider, |maybe_provision| {
                // `provision` cannot be None
                if let Some(mut provision) = maybe_provision.clone() {
                    provision.file_map.insert(so.file_identifier.clone(), order_id.clone());
                    *maybe_provision = Some(provision)
                }
            });
            true
        }
    }
}

decl_event!(
    pub enum Event<T>
    where
        AccountId = <T as system::Trait>::AccountId,
    {
        StorageOrderSuccess(AccountId, StorageOrder<AccountId>),
        RegisterSuccess(AccountId),
    }
);