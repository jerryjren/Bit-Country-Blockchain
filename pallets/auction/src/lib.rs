// This file is part of Bit.Country.

// Copyright (C) 2020-2021 Bit.Country.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// This pallet use The Open Runtime Module Library (ORML) which is a community maintained collection
// of Substrate runtime modules. Thanks to all contributors of orml.
// Ref: https://github.com/open-web3-stack/open-runtime-module-library

#![cfg_attr(not(feature = "std"), no_std)]
#![allow(clippy::string_lit_as_bytes)]
#![allow(clippy::unnecessary_cast)]
#![allow(clippy::unused_unit)]
#![allow(clippy::upper_case_acronyms)]

use frame_support::traits::{Currency, ExistenceRequirement, LockableCurrency, ReservableCurrency};
use frame_support::{ensure, pallet_prelude::*, transactional};
use frame_system::{self as system, ensure_signed};
use sp_core::sp_std::convert::TryInto;
use sp_runtime::SaturatedConversion;
use sp_runtime::{
	traits::{CheckedDiv, One, Saturating, Zero},
	DispatchError, DispatchResult,
};

use auction_manager::{Auction, AuctionHandler, AuctionInfo, AuctionItem, AuctionType, Change, OnNewBidResult};
pub use pallet::*;
use pallet_nft::Pallet as NFTModule;
use primitives::{continuum::Continuum, estate::Estate, AuctionId, ItemId};
pub use weights::WeightInfo;

#[cfg(feature = "runtime-benchmarks")]
pub mod benchmarking;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

pub mod weights;

pub struct AuctionLogicHandler;

pub mod migration_v2 {
	use codec::FullCodec;
	use codec::{Decode, Encode};
	use scale_info::TypeInfo;
	#[cfg(feature = "std")]
	use serde::{Deserialize, Serialize};
	use sp_runtime::{traits::AtLeast32BitUnsigned, DispatchError, RuntimeDebug};
	use sp_std::{
		cmp::{Eq, PartialEq},
		fmt::Debug,
		vec::Vec,
	};

	use auction_manager::{AuctionType, ListingLevel};
	use primitives::{AssetId, EstateId, FungibleTokenId, MetaverseId};

	#[derive(Encode, Decode, Copy, Clone, PartialEq, Eq, RuntimeDebug, TypeInfo)]
	#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
	pub enum V1ItemId {
		NFT(AssetId),
		Spot(u64, MetaverseId),
		Country(MetaverseId),
		Block(u64),
		Estate(EstateId),
		LandUnit((i32, i32), MetaverseId),
	}

	#[cfg_attr(feature = "std", derive(PartialEq, Eq))]
	#[derive(Encode, Decode, Clone, RuntimeDebug, TypeInfo)]
	pub struct AuctionItem<AccountId, BlockNumber, Balance> {
		pub item_id: V1ItemId,
		pub recipient: AccountId,
		pub initial_amount: Balance,
		/// Current amount for sale
		pub amount: Balance,
		/// Auction start time
		pub start_time: BlockNumber,
		pub end_time: BlockNumber,
		pub auction_type: AuctionType,
		pub listing_level: ListingLevel<AccountId>,
		pub currency_id: FungibleTokenId,
	}
}

#[frame_support::pallet]
pub mod pallet {
	use frame_support::dispatch::DispatchResultWithPostInfo;
	use frame_support::log;
	use frame_support::sp_runtime::traits::CheckedSub;
	use frame_system::pallet_prelude::OriginFor;
	use orml_traits::{MultiCurrency, MultiReservableCurrency};

	use auction_manager::{CheckAuctionItemHandler, ListingLevel};
	use core_primitives::{MetaverseTrait, NFTTrait};
	use primitives::{AssetId, Balance, ClassId, FungibleTokenId, MetaverseId, TokenId};

	use crate::migration_v2::V1ItemId;

	use super::*;

	#[pallet::pallet]
	#[pallet::generate_store(pub (super) trait Store)]
	#[pallet::without_storage_info]
	pub struct Pallet<T>(PhantomData<T>);

	pub(super) type BalanceOf<T> =
		<<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;

	#[pallet::config]
	pub trait Config: frame_system::Config {
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;
		#[pallet::constant]
		type AuctionTimeToClose: Get<Self::BlockNumber>;
		/// The `AuctionHandler` that allow custom bidding logic and handles auction result
		type Handler: AuctionHandler<Self::AccountId, BalanceOf<Self>, Self::BlockNumber, AuctionId>;
		type Currency: ReservableCurrency<Self::AccountId>
			+ LockableCurrency<Self::AccountId, Moment = Self::BlockNumber>;
		/// Continuum protocol handler
		type ContinuumHandler: Continuum<Self::AccountId>;
		/// Multi-fungible token currency
		type FungibleTokenCurrency: MultiReservableCurrency<
			Self::AccountId,
			CurrencyId = FungibleTokenId,
			Balance = Balance,
		>;
		/// Metaverse info trait
		type MetaverseInfoSource: MetaverseTrait<Self::AccountId>;
		#[pallet::constant]
		type MinimumAuctionDuration: Get<Self::BlockNumber>;
		/// Handle Estate logic
		type EstateHandler: Estate<Self::AccountId>;
		/// Loyalty fee in percentage applied NFT promotion
		#[pallet::constant]
		type RoyaltyFee: Get<u16>;
		#[pallet::constant]
		type MaxFinality: Get<u32>;
		/// NFT Handler
		type NFTHandler: NFTTrait<Self::AccountId, ClassId = ClassId, TokenId = TokenId>;
	}

	#[pallet::storage]
	#[pallet::getter(fn auctions)]
	/// Stores on-going and future auctions. Closed auction are removed.
	pub(super) type Auctions<T: Config> =
		StorageMap<_, Twox64Concat, AuctionId, AuctionInfo<T::AccountId, BalanceOf<T>, T::BlockNumber>, OptionQuery>;

	#[pallet::storage]
	#[pallet::getter(fn get_auction_item)]
	//Store asset with Auction
	pub(super) type AuctionItems<T: Config> =
		StorageMap<_, Twox64Concat, AuctionId, AuctionItem<T::AccountId, T::BlockNumber, BalanceOf<T>>, OptionQuery>;

	#[pallet::storage]
	#[pallet::getter(fn items_in_auction)]
	/// Track which Assets are in auction
	pub(super) type ItemsInAuction<T: Config> = StorageMap<_, Twox64Concat, ItemId, bool, OptionQuery>;

	#[pallet::storage]
	#[pallet::getter(fn auctions_index)]
	/// Track the next auction ID.
	pub(super) type AuctionsIndex<T: Config> = StorageValue<_, AuctionId, ValueQuery>;

	#[pallet::storage]
	#[pallet::getter(fn auction_end_time)]
	/// Index auctions by end time.
	pub(super) type AuctionEndTime<T: Config> =
		StorageDoubleMap<_, Twox64Concat, T::BlockNumber, Twox64Concat, AuctionId, (), OptionQuery>;

	#[pallet::storage]
	#[pallet::getter(fn authorised_collection_local)]
	/// Local marketplace collection authorisation
	pub(super) type MetaverseAuthorizedCollection<T: Config> =
		StorageMap<_, Twox64Concat, (MetaverseId, ClassId), (), OptionQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub (crate) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A bid is placed. [auction_id, bidder, bidding_amount]
		Bid(AuctionId, T::AccountId, BalanceOf<T>),
		NewAuctionItem(
			AuctionId,
			T::AccountId,
			ListingLevel<T::AccountId>,
			BalanceOf<T>,
			BalanceOf<T>,
			T::BlockNumber,
		),
		AuctionFinalized(AuctionId, T::AccountId, BalanceOf<T>),
		BuyNowFinalised(AuctionId, T::AccountId, BalanceOf<T>),
		AuctionFinalizedNoBid(AuctionId),
		CollectionAuthorizedInMetaverse(ClassId, MetaverseId),
		CollectionAuthorizationRemoveInMetaverse(ClassId, MetaverseId),
	}

	/// Errors inform users that something went wrong.
	#[pallet::error]
	pub enum Error<T> {
		AuctionNotExist,
		AssetIsNotExist,
		AuctionNotStarted,
		AuctionIsExpired,
		AuctionTypeIsNotSupported,
		BidNotAccepted,
		InsufficientFreeBalance,
		InvalidBidPrice,
		NoAvailableAuctionId,
		NoPermissionToCreateAuction,
		SelfBidNotAccepted,
		CannotBidOnOwnAuction,
		InvalidBuyItNowPrice,
		InsufficientFunds,
		/// Invalid auction type
		InvalidAuctionType,
		/// Asset already in Auction
		ItemAlreadyInAuction,
		/// Wrong Listing Level
		WrongListingLevel,
		/// Social Token Currency is not exist
		FungibleTokenCurrencyNotFound,
		/// Minimum Duration Is Too Low
		AuctionEndIsLessThanMinimumDuration,
		/// Overflow
		Overflow,
		EstateDoesNotExist,
		LandUnitDoesNotExist,
		/// User has no permission to authorise collection
		NoPermissionToAuthoriseCollection,
		/// Collection has already authorised
		CollectionAlreadyAuthorised,
		/// Collection is not authorised
		CollectionIsNotAuthorised,
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// User can bid on listing
		#[pallet::weight(10_000 + T::DbWeight::get().writes(1))]
		#[transactional]
		pub fn bid(origin: OriginFor<T>, id: AuctionId, value: BalanceOf<T>) -> DispatchResultWithPostInfo {
			let from = ensure_signed(origin)?;

			let auction_item: AuctionItem<T::AccountId, T::BlockNumber, BalanceOf<T>> =
				Self::get_auction_item(id.clone()).ok_or(Error::<T>::AuctionNotExist)?;
			ensure!(
				auction_item.auction_type == AuctionType::Auction,
				Error::<T>::InvalidAuctionType
			);
			ensure!(auction_item.recipient != from, Error::<T>::SelfBidNotAccepted);

			<Auctions<T>>::try_mutate_exists(id, |auction| -> DispatchResult {
				let mut auction = auction.as_mut().ok_or(Error::<T>::AuctionNotExist)?;

				let block_number = <system::Pallet<T>>::block_number();

				// make sure auction is started
				ensure!(block_number >= auction.start, Error::<T>::AuctionNotStarted);

				let auction_end: Option<T::BlockNumber> = auction.end;

				ensure!(block_number < auction_end.unwrap(), Error::<T>::AuctionIsExpired);

				if let Some(ref current_bid) = auction.bid {
					ensure!(value > current_bid.1, Error::<T>::InvalidBidPrice);
				} else {
					ensure!(!value.is_zero(), Error::<T>::InvalidBidPrice);
				}
				// implement hooks for future event
				let bid_result = T::Handler::on_new_bid(block_number, id, (from.clone(), value), auction.bid.clone());

				ensure!(bid_result.accept_bid, Error::<T>::BidNotAccepted);

				ensure!(
					<T as Config>::Currency::free_balance(&from) >= value,
					Error::<T>::InsufficientFreeBalance
				);

				Self::auction_bid_handler(block_number, id, (from.clone(), value), auction.bid.clone())?;

				auction.bid = Some((from.clone(), value));
				Self::deposit_event(Event::Bid(id, from, value));

				Ok(())
			})?;

			Ok(().into())
		}

		/// User can buy now on listing
		#[pallet::weight(10_000 + T::DbWeight::get().writes(1))]
		pub fn buy_now(origin: OriginFor<T>, auction_id: AuctionId, value: BalanceOf<T>) -> DispatchResultWithPostInfo {
			let from = ensure_signed(origin)?;

			let auction = Self::auctions(auction_id.clone()).ok_or(Error::<T>::AuctionNotExist)?;
			let auction_item = Self::get_auction_item(auction_id.clone()).ok_or(Error::<T>::AuctionNotExist)?;

			ensure!(
				auction_item.auction_type == AuctionType::BuyNow,
				Error::<T>::InvalidAuctionType
			);

			ensure!(auction_item.recipient != from, Error::<T>::CannotBidOnOwnAuction);

			let block_number = <system::Pallet<T>>::block_number();
			ensure!(block_number >= auction.start, Error::<T>::AuctionNotStarted);
			if !(auction.end.is_none()) {
				let auction_end: T::BlockNumber = auction.end.unwrap();
				ensure!(block_number < auction_end, Error::<T>::AuctionIsExpired);
			}

			ensure!(value == auction_item.amount, Error::<T>::InvalidBuyItNowPrice);
			ensure!(
				<T as Config>::Currency::free_balance(&from) >= value,
				Error::<T>::InsufficientFunds
			);

			Self::remove_auction(auction_id.clone(), auction_item.item_id);

			// Transfer balance from buy it now user to asset owner
			let currency_transfer = <T as Config>::Currency::transfer(
				&from,
				&auction_item.recipient,
				value,
				ExistenceRequirement::KeepAlive,
			);
			match currency_transfer {
				Err(_e) => {}
				Ok(_v) => {
					// Transfer asset from asset owner to buy it now user
					<ItemsInAuction<T>>::remove(auction_item.item_id);
					match auction_item.item_id {
						ItemId::NFT(class_id, token_id) => {
							Self::collect_royalty_fee(
								&value,
								&auction_item.recipient,
								&(class_id, token_id),
								FungibleTokenId::NativeToken(0),
							);

							let asset_transfer =
								T::NFTHandler::transfer_nft(&auction_item.recipient, &from, &(class_id, token_id));
							match asset_transfer {
								Err(_) => (),
								Ok(_) => {
									Self::deposit_event(Event::BuyNowFinalised(auction_id, from, value));
								}
							}
						}
						ItemId::Spot(spot_id, metaverse_id) => {
							let continuum_spot = T::ContinuumHandler::transfer_spot(
								spot_id,
								&auction_item.recipient,
								&(from.clone(), metaverse_id),
							);
							match continuum_spot {
								Err(_) => (),
								Ok(_) => {
									Self::deposit_event(Event::BuyNowFinalised(auction_id, from, value));
								}
							}
						}
						ItemId::Estate(estate_id) => {
							let estate =
								T::EstateHandler::transfer_estate(estate_id, &auction_item.recipient, &from.clone());
							match estate {
								Err(_) => (),
								Ok(_) => {
									Self::deposit_event(Event::BuyNowFinalised(auction_id, from, value));
								}
							}
						}
						ItemId::LandUnit(coordinate, metaverse_id) => {
							let land_unit = T::EstateHandler::transfer_landunit(
								coordinate,
								&auction_item.recipient,
								&(from.clone(), metaverse_id),
							);
							match land_unit {
								Err(_) => (),
								Ok(_) => {
									Self::deposit_event(Event::BuyNowFinalised(auction_id, from, value));
								}
							}
						}
						_ => {} // Future implementation for Land, Metaverse
					}
				}
			}
			Ok(().into())
		}

		#[pallet::weight(10_000 + T::DbWeight::get().writes(1))]
		pub fn create_new_auction(
			origin: OriginFor<T>,
			item_id: ItemId,
			value: BalanceOf<T>,
			end_time: T::BlockNumber,
			listing_level: ListingLevel<T::AccountId>,
		) -> DispatchResultWithPostInfo {
			let from = ensure_signed(origin)?;

			ensure!(
				matches!(item_id, ItemId::NFT(_, _)),
				Error::<T>::NoPermissionToCreateAuction
			);

			match listing_level {
				ListingLevel::Local(metaverse_id) => {
					ensure!(
						T::MetaverseInfoSource::check_ownership(&from, &metaverse_id),
						Error::<T>::NoPermissionToCreateAuction
					);
				}
				_ => {}
			}

			let start_time: T::BlockNumber = <system::Pallet<T>>::block_number();

			let remaining_time: T::BlockNumber = end_time.checked_sub(&start_time).ok_or(Error::<T>::Overflow)?;

			ensure!(
				remaining_time >= T::MinimumAuctionDuration::get(),
				Error::<T>::AuctionEndIsLessThanMinimumDuration
			);

			Self::create_auction(
				AuctionType::Auction,
				item_id,
				Some(end_time),
				from.clone(),
				value.clone(),
				start_time,
				listing_level.clone(),
			)?;
			Ok(().into())
		}

		#[pallet::weight(10_000 + T::DbWeight::get().writes(1))]
		pub fn create_new_buy_now(
			origin: OriginFor<T>,
			item_id: ItemId,
			value: BalanceOf<T>,
			end_time: T::BlockNumber,
			listing_level: ListingLevel<T::AccountId>,
		) -> DispatchResultWithPostInfo {
			let from = ensure_signed(origin)?;
			ensure!(
				matches!(item_id, ItemId::NFT(_, _)),
				Error::<T>::NoPermissionToCreateAuction
			);

			match listing_level {
				ListingLevel::Local(metaverse_id) => {
					ensure!(
						T::MetaverseInfoSource::check_ownership(&from, &metaverse_id),
						Error::<T>::NoPermissionToCreateAuction
					);
				}
				_ => {}
			}

			let start_time: T::BlockNumber = <system::Pallet<T>>::block_number();
			let remaining_time: T::BlockNumber = end_time.checked_sub(&start_time).ok_or(Error::<T>::Overflow)?;

			ensure!(
				remaining_time >= T::MinimumAuctionDuration::get(),
				Error::<T>::AuctionEndIsLessThanMinimumDuration
			);

			Self::create_auction(
				AuctionType::BuyNow,
				item_id,
				Some(end_time),
				from.clone(),
				value.clone(),
				start_time,
				listing_level.clone(),
			)?;

			Ok(().into())
		}

		#[pallet::weight(10_000 + T::DbWeight::get().writes(1))]
		pub fn authorise_metaverse_collection(
			origin: OriginFor<T>,
			class_id: ClassId,
			metaverse_id: MetaverseId,
		) -> DispatchResultWithPostInfo {
			let from = ensure_signed(origin)?;
			ensure!(
				T::MetaverseInfoSource::check_ownership(&from, &metaverse_id),
				Error::<T>::NoPermissionToAuthoriseCollection
			);

			ensure!(
				!MetaverseAuthorizedCollection::<T>::contains_key((metaverse_id, class_id)),
				Error::<T>::CollectionAlreadyAuthorised
			);

			MetaverseAuthorizedCollection::<T>::insert((metaverse_id.clone(), class_id.clone()), ());

			Self::deposit_event(Event::<T>::CollectionAuthorizedInMetaverse(class_id, metaverse_id));

			Ok(().into())
		}

		#[pallet::weight(10_000 + T::DbWeight::get().writes(1))]
		pub fn remove_authorise_metaverse_collection(
			origin: OriginFor<T>,
			class_id: ClassId,
			metaverse_id: MetaverseId,
		) -> DispatchResultWithPostInfo {
			let from = ensure_signed(origin)?;
			ensure!(
				T::MetaverseInfoSource::check_ownership(&from, &metaverse_id),
				Error::<T>::NoPermissionToAuthoriseCollection
			);

			ensure!(
				MetaverseAuthorizedCollection::<T>::contains_key((metaverse_id, class_id)),
				Error::<T>::CollectionIsNotAuthorised
			);

			MetaverseAuthorizedCollection::<T>::remove((metaverse_id.clone(), class_id.clone()));
			Self::deposit_event(Event::<T>::CollectionAuthorizationRemoveInMetaverse(
				class_id,
				metaverse_id,
			));
			Ok(().into())
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<T::BlockNumber> for Pallet<T> {
		fn on_finalize(now: T::BlockNumber) {
			let max_finality = T::MaxFinality::get();
			let mut proceeded_item: u32 = 0;
			for (auction_id, _) in <AuctionEndTime<T>>::drain_prefix(&now) {
				if proceeded_item == max_finality {
					break;
				};
				if let Some(auction) = <Auctions<T>>::get(&auction_id) {
					if let Some(auction_item) = <AuctionItems<T>>::get(&auction_id) {
						proceeded_item.checked_add(One::one()).ok_or("Overflow");
						Self::remove_auction(auction_id.clone(), auction_item.item_id);
						// Transfer balance from high bidder to asset owner
						if let Some(current_bid) = auction.bid {
							let (high_bidder, high_bid_price): (T::AccountId, BalanceOf<T>) = current_bid;
							// Handle listing
							<T as Config>::Currency::unreserve(&high_bidder, high_bid_price);

							// Handle balance transfer
							let currency_transfer = <T as Config>::Currency::transfer(
								&high_bidder,
								&auction_item.recipient,
								high_bid_price,
								ExistenceRequirement::KeepAlive,
							);

							match currency_transfer {
								Err(_e) => continue,
								Ok(_v) => {
									// Transfer asset from asset owner to high bidder
									// Check asset type and handle internal logic

									match auction_item.item_id {
										ItemId::NFT(class_id, token_id) => {
											Self::collect_royalty_fee(
												&high_bid_price,
												&auction_item.recipient,
												&(class_id, token_id),
												FungibleTokenId::NativeToken(0),
											);
											let asset_transfer = T::NFTHandler::transfer_nft(
												&auction_item.recipient,
												&high_bidder,
												&(class_id, token_id),
											);

											match asset_transfer {
												Err(_) => continue,
												Ok(_) => {
													Self::deposit_event(Event::AuctionFinalized(
														auction_id,
														high_bidder,
														high_bid_price,
													));
												}
											}
										}
										ItemId::Spot(spot_id, metaverse_id) => {
											let continuum_spot = T::ContinuumHandler::transfer_spot(
												spot_id,
												&auction_item.recipient,
												&(high_bidder.clone(), metaverse_id),
											);
											match continuum_spot {
												Err(_) => continue,
												Ok(_) => {
													Self::deposit_event(Event::AuctionFinalized(
														auction_id,
														high_bidder,
														high_bid_price,
													));
												}
											}
										}
										ItemId::Estate(estate_id) => {
											let estate = T::EstateHandler::transfer_estate(
												estate_id,
												&auction_item.recipient,
												&high_bidder.clone(),
											);
											match estate {
												Err(_) => (),
												Ok(_) => {
													Self::deposit_event(Event::AuctionFinalized(
														auction_id,
														high_bidder,
														high_bid_price,
													));
												}
											}
										}
										ItemId::LandUnit(coordinate, metaverse_id) => {
											let land_unit = T::EstateHandler::transfer_landunit(
												coordinate,
												&auction_item.recipient,
												&(high_bidder.clone(), metaverse_id),
											);
											match land_unit {
												Err(_) => (),
												Ok(_) => {
													Self::deposit_event(Event::AuctionFinalized(
														auction_id,
														high_bidder,
														high_bid_price,
													));
												}
											}
										}
										_ => {} // Future implementation for Spot, Metaverse
									}
									<ItemsInAuction<T>>::remove(auction_item.item_id);
								}
							}
						} else {
							Self::deposit_event(Event::AuctionFinalizedNoBid(auction_id));
						}
					}
				};
			}
		}
		fn on_runtime_upgrade() -> Weight {
			Self::upgrade_asset_auction_data_v2();

			0
		}
	}

	impl<T: Config> Auction<T::AccountId, T::BlockNumber> for Pallet<T> {
		type Balance = BalanceOf<T>;

		fn update_auction(
			id: AuctionId,
			info: AuctionInfo<T::AccountId, Self::Balance, T::BlockNumber>,
		) -> DispatchResult {
			let auction = <Auctions<T>>::get(id).ok_or(Error::<T>::AuctionNotExist)?;
			if let Some(old_end) = auction.end {
				<AuctionEndTime<T>>::remove(&old_end, id);
			}
			if let Some(new_end) = info.end {
				<AuctionEndTime<T>>::insert(&new_end, id, ());
			}
			<Auctions<T>>::insert(id, info);
			Ok(())
		}

		fn new_auction(
			_recipient: T::AccountId,
			_initial_amount: Self::Balance,
			start: T::BlockNumber,
			end: Option<T::BlockNumber>,
		) -> Result<AuctionId, DispatchError> {
			let auction: AuctionInfo<T::AccountId, Self::Balance, T::BlockNumber> =
				AuctionInfo { bid: None, start, end };

			let auction_id: AuctionId = AuctionsIndex::<T>::try_mutate(|n| -> Result<AuctionId, DispatchError> {
				let id = *n;
				ensure!(id != AuctionId::max_value(), Error::<T>::NoAvailableAuctionId);
				*n = n.checked_add(One::one()).ok_or(Error::<T>::NoAvailableAuctionId)?;
				Ok(id)
			})?;

			<Auctions<T>>::insert(auction_id, auction);

			if let Some(end_block) = end {
				<AuctionEndTime<T>>::insert(&end_block, auction_id, ());
			}

			Ok(auction_id)
		}

		fn create_auction(
			auction_type: AuctionType,
			item_id: ItemId,
			_end: Option<T::BlockNumber>,
			recipient: T::AccountId,
			initial_amount: Self::Balance,
			_start: T::BlockNumber,
			listing_level: ListingLevel<T::AccountId>,
		) -> Result<AuctionId, DispatchError> {
			ensure!(
				Self::items_in_auction(item_id) == None,
				Error::<T>::ItemAlreadyInAuction
			);

			match item_id {
				ItemId::NFT(class_id, token_id) => {
					// Check ownership
					let is_owner = T::NFTHandler::check_ownership(&recipient, &(class_id, token_id))?;

					ensure!(is_owner == true, Error::<T>::NoPermissionToCreateAuction);

					let is_transferable = T::NFTHandler::is_transferable(&(class_id, token_id))?;

					ensure!(is_transferable == true, Error::<T>::NoPermissionToCreateAuction);

					// Ensure NFT authorised to sell
					match listing_level {
						ListingLevel::Local(metaverse_id) => {
							ensure!(
								MetaverseAuthorizedCollection::<T>::contains_key((metaverse_id, class_id))
									|| T::MetaverseInfoSource::check_ownership(&recipient, &metaverse_id),
								Error::<T>::NoPermissionToCreateAuction
							);
						}
						_ => {}
					}

					let start_time = <system::Pallet<T>>::block_number();

					let mut end_time = start_time + T::AuctionTimeToClose::get();
					if let Some(_end_block) = _end {
						end_time = _end_block
					}
					let auction_id = Self::new_auction(recipient.clone(), initial_amount, start_time, Some(end_time))?;
					let mut currency_id: FungibleTokenId = FungibleTokenId::NativeToken(0);

					let new_auction_item = AuctionItem {
						item_id,
						recipient: recipient.clone(),
						initial_amount: initial_amount,
						amount: initial_amount,
						start_time,
						end_time,
						auction_type,
						listing_level: listing_level.clone(),
						currency_id,
					};

					<AuctionItems<T>>::insert(auction_id, new_auction_item);

					Self::deposit_event(Event::NewAuctionItem(
						auction_id,
						recipient,
						listing_level,
						initial_amount,
						initial_amount,
						end_time,
					));
					<ItemsInAuction<T>>::insert(item_id, true);
					Ok(auction_id)
				}
				ItemId::Spot(_spot_id, _metaverse_id) => {
					let start_time = <system::Pallet<T>>::block_number();
					let end_time: T::BlockNumber = start_time + T::AuctionTimeToClose::get();
					let auction_id = Self::new_auction(recipient.clone(), initial_amount, start_time, Some(end_time))?;

					let new_auction_item = AuctionItem {
						item_id,
						recipient: recipient.clone(),
						initial_amount,
						amount: initial_amount,
						start_time,
						end_time,
						auction_type,
						listing_level: listing_level.clone(),
						currency_id: FungibleTokenId::NativeToken(0),
					};

					<AuctionItems<T>>::insert(auction_id, new_auction_item);

					Self::deposit_event(Event::NewAuctionItem(
						auction_id,
						recipient,
						listing_level,
						initial_amount,
						initial_amount,
						end_time,
					));
					<ItemsInAuction<T>>::insert(item_id, true);
					Ok(auction_id)
				}
				ItemId::Estate(_estate_id_) => {
					// Ensure the _estate_id_ exist/minted
					ensure!(
						T::EstateHandler::check_estate(_estate_id_)?,
						Error::<T>::EstateDoesNotExist
					);

					let start_time = <system::Pallet<T>>::block_number();
					let end_time: T::BlockNumber = start_time + T::AuctionTimeToClose::get(); // add 7 days block for default auction
					let auction_id = Self::new_auction(recipient.clone(), initial_amount, start_time, Some(end_time))?;

					let new_auction_item = AuctionItem {
						item_id,
						recipient: recipient.clone(),
						initial_amount,
						amount: initial_amount,
						start_time,
						end_time,
						auction_type,
						listing_level: ListingLevel::Global,
						currency_id: FungibleTokenId::NativeToken(0),
					};

					<AuctionItems<T>>::insert(auction_id, new_auction_item);

					Self::deposit_event(Event::NewAuctionItem(
						auction_id,
						recipient,
						listing_level,
						initial_amount,
						initial_amount,
						end_time,
					));
					<ItemsInAuction<T>>::insert(item_id, true);
					Ok(auction_id)
				}
				ItemId::LandUnit(_coordinate_, _metaverse_id_) => {
					// Ensure the _coordinate_ exist/minted
					ensure!(
						T::EstateHandler::check_landunit(_metaverse_id_, _coordinate_)?,
						Error::<T>::LandUnitDoesNotExist
					);

					let start_time = <system::Pallet<T>>::block_number();
					let end_time: T::BlockNumber = start_time + T::AuctionTimeToClose::get(); // add 7 days block for default auction
					let auction_id = Self::new_auction(recipient.clone(), initial_amount, start_time, Some(end_time))?;

					let new_auction_item = AuctionItem {
						item_id,
						recipient: recipient.clone(),
						initial_amount,
						amount: initial_amount,
						start_time,
						end_time,
						auction_type,
						listing_level: ListingLevel::Global,
						currency_id: FungibleTokenId::NativeToken(0),
					};

					<AuctionItems<T>>::insert(auction_id, new_auction_item);

					Self::deposit_event(Event::NewAuctionItem(
						auction_id,
						recipient,
						listing_level,
						initial_amount,
						initial_amount,
						end_time,
					));
					<ItemsInAuction<T>>::insert(item_id, true);
					Ok(auction_id)
				}
				_ => Err(Error::<T>::AuctionTypeIsNotSupported.into()),
			}
		}

		fn remove_auction(id: AuctionId, item_id: ItemId) {
			if let Some(auction) = <Auctions<T>>::get(&id) {
				if let Some(end_block) = auction.end {
					<AuctionEndTime<T>>::remove(end_block, id);
					<Auctions<T>>::remove(&id);
					<ItemsInAuction<T>>::remove(item_id);
				}
			}
		}

		fn auction_bid_handler(
			_now: T::BlockNumber,
			id: AuctionId,
			new_bid: (T::AccountId, Self::Balance),
			last_bid: Option<(T::AccountId, Self::Balance)>,
		) -> DispatchResult {
			let (new_bidder, new_bid_price) = new_bid;
			ensure!(!new_bid_price.is_zero(), Error::<T>::InvalidBidPrice);

			<AuctionItems<T>>::try_mutate_exists(id, |auction_item| -> DispatchResult {
				let mut auction_item = auction_item.as_mut().ok_or(Error::<T>::AuctionNotExist)?;

				match auction_item.clone().listing_level {
					ListingLevel::NetworkSpot(allowed_bidders) => {
						ensure!(allowed_bidders.contains(&new_bidder), Error::<T>::BidNotAccepted);
					}
					_ => {}
				}

				let last_bid_price = last_bid.clone().map_or(Zero::zero(), |(_, price)| price); // get last bid price
				let last_bidder = last_bid.as_ref().map(|(who, _)| who);

				if let Some(last_bidder) = last_bidder {
					//unlock reserve amount
					if !last_bid_price.is_zero() {
						//Unreserve balance of last bidder
						<T as Config>::Currency::unreserve(&last_bidder, last_bid_price);
					}
				}

				// Lock fund of new bidder
				// Reserve balance
				<T as Config>::Currency::reserve(&new_bidder, new_bid_price)?;
				auction_item.amount = new_bid_price.clone();

				Ok(())
			})
		}

		fn local_auction_bid_handler(
			_now: T::BlockNumber,
			id: AuctionId,
			new_bid: (T::AccountId, Self::Balance),
			last_bid: Option<(T::AccountId, Self::Balance)>,
			social_currency_id: FungibleTokenId,
		) -> DispatchResult {
			let (new_bidder, new_bid_price) = new_bid;
			ensure!(!new_bid_price.is_zero(), Error::<T>::InvalidBidPrice);

			<AuctionItems<T>>::try_mutate_exists(id, |auction_item| -> DispatchResult {
				let mut auction_item = auction_item.as_mut().ok_or(Error::<T>::AuctionNotExist)?;

				let last_bid_price = last_bid.clone().map_or(Zero::zero(), |(_, price)| price); // get last bid price
				let last_bidder = last_bid.as_ref().map(|(who, _)| who);

				if let Some(last_bidder) = last_bidder {
					// unlock reserve amount
					if !last_bid_price.is_zero() {
						// Un-reserve balance of last bidder
						T::FungibleTokenCurrency::unreserve(
							social_currency_id,
							&last_bidder,
							last_bid_price.saturated_into(),
						);
					}
				}

				// Lock fund of new bidder
				// Reserve balance
				T::FungibleTokenCurrency::reserve(social_currency_id, &new_bidder, new_bid_price.saturated_into())?;
				auction_item.amount = new_bid_price.clone();

				Ok(())
			})
		}

		fn auction_info(id: AuctionId) -> Option<AuctionInfo<T::AccountId, Self::Balance, T::BlockNumber>> {
			Self::auctions(id)
		}

		fn collect_royalty_fee(
			high_bid_price: &Self::Balance,
			high_bidder: &T::AccountId,
			asset_id: &(ClassId, TokenId),
			social_currency_id: FungibleTokenId,
		) -> DispatchResult {
			let fee_scale = T::RoyaltyFee::get();
			// Calculate loyalty fee and deposit to pot fund
			let royalty_fee = high_bid_price
				.saturating_mul(fee_scale.into())
				.checked_div(&10000u128.saturated_into())
				.ok_or("Overflow")?;

			// Collect loyalty fee
			// and deposit to class fund
			let class_fund = T::NFTHandler::get_class_fund(&asset_id.0);
			// Transfer loyalty fee from winner to class fund pot
			if social_currency_id == FungibleTokenId::NativeToken(0) {
				<T as Config>::Currency::transfer(
					&high_bidder,
					&class_fund,
					royalty_fee,
					ExistenceRequirement::KeepAlive,
				)?;
				// Reserve class fund pot
				<T as Config>::Currency::reserve(&class_fund, royalty_fee)?;
			} else {
				T::FungibleTokenCurrency::transfer(
					social_currency_id.clone(),
					&high_bidder,
					&class_fund,
					royalty_fee.saturated_into(),
				)?;
				// Reserve class fund pot
				T::FungibleTokenCurrency::reserve(social_currency_id, &class_fund, royalty_fee.saturated_into())?;
			}
			Ok(())
		}
	}

	impl<T: Config> CheckAuctionItemHandler for Pallet<T> {
		fn check_item_in_auction(item_id: ItemId) -> bool {
			Self::items_in_auction(item_id) == Some(true)
		}
	}

	impl<T: Config> AuctionHandler<T::AccountId, BalanceOf<T>, T::BlockNumber, AuctionId> for Pallet<T> {
		fn on_new_bid(
			_now: T::BlockNumber,
			_id: AuctionId,
			_new_bid: (T::AccountId, BalanceOf<T>),
			_last_bid: Option<(T::AccountId, BalanceOf<T>)>,
		) -> OnNewBidResult<T::BlockNumber> {
			OnNewBidResult {
				accept_bid: true,
				auction_end_change: Change::NoChange,
			}
		}

		fn on_auction_ended(_id: AuctionId, _winner: Option<(T::AccountId, BalanceOf<T>)>) {}
	}

	impl<T: Config> Pallet<T> {
		pub fn upgrade_asset_auction_data_v2() -> Weight {
			log::info!("Start upgrading nft class data v2");
			let mut num_auction_item = 0;

			AuctionItems::<T>::translate(
				|_k, auction_v1: migration_v2::AuctionItem<T::AccountId, T::BlockNumber, BalanceOf<T>>| {
					num_auction_item += 1;

					log::info!("Upgrading auction items data");

					let asset_id = auction_v1.item_id;

					match asset_id {
						V1ItemId::NFT(asset_id) => {
							num_auction_item += 1;
							let token = T::NFTHandler::get_asset_id(asset_id).unwrap();
							let v2_item_id = ItemId::NFT(token.0, token.1);

							let v: AuctionItem<T::AccountId, T::BlockNumber, BalanceOf<T>> = AuctionItem {
								item_id: v2_item_id,
								recipient: auction_v1.recipient,
								initial_amount: auction_v1.initial_amount,
								amount: auction_v1.amount,
								start_time: auction_v1.start_time,
								end_time: auction_v1.end_time,
								auction_type: auction_v1.auction_type,
								listing_level: auction_v1.listing_level,
								currency_id: auction_v1.currency_id,
							};
							Some(v)
						}
						_ => None,
					}
				},
			);

			log::info!("Asset Item in Auction upgraded: {}", num_auction_item);
			0
		}
	}
}
