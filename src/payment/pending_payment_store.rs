// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use bitcoin::{Transaction, Txid};
use lightning::{impl_writeable_tlv_based, ln::channelmanager::PaymentId};

use crate::{
	data_store::{StorableObject, StorableObjectUpdate},
	payment::{store::PaymentDetailsUpdate, PaymentDetails},
};

/// Represents a pending payment
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPaymentDetails {
	/// The full payment details
	pub details: PaymentDetails,
	/// Transaction IDs that have replaced or conflict with this payment.
	pub conflicting_txids: Vec<Txid>,
	/// The raw transaction for rebroadcasting
	pub raw_tx: Option<Transaction>,
	/// Last broadcast attempt timestamp (UNIX seconds)
	pub last_broadcast_time: Option<u64>,
	/// Number of broadcast attempts
	pub broadcast_attempts: Option<u32>,
}

impl PendingPaymentDetails {
	pub(crate) fn new(
		details: PaymentDetails, conflicting_txids: Vec<Txid>, raw_tx: Option<Transaction>,
		last_broadcast_time: Option<u64>, broadcast_attempts: Option<u32>,
	) -> Self {
		Self { details, conflicting_txids, raw_tx, last_broadcast_time, broadcast_attempts }
	}

	/// Convert to finalized payment for the main payment store
	pub fn into_payment_details(self) -> PaymentDetails {
		self.details
	}
}

impl_writeable_tlv_based!(PendingPaymentDetails, {
	(0, details, required),
	(2, conflicting_txids, optional_vec),
	(3, raw_tx, option),
	(5, last_broadcast_time, option),
	(7, broadcast_attempts, option),
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingPaymentDetailsUpdate {
	pub id: PaymentId,
	pub payment_update: Option<PaymentDetailsUpdate>,
	pub conflicting_txids: Option<Vec<Txid>>,
	pub raw_tx: Option<Option<Transaction>>,
	pub last_broadcast_time: Option<Option<u64>>,
	pub broadcast_attempts: Option<Option<u32>>,
}

impl StorableObject for PendingPaymentDetails {
	type Id = PaymentId;
	type Update = PendingPaymentDetailsUpdate;

	fn id(&self) -> Self::Id {
		self.details.id
	}

	fn update(&mut self, update: &Self::Update) -> bool {
		let mut updated = false;

		macro_rules! update_if_necessary {
			($val:expr, $update:expr) => {
				if $val != $update {
					$val = $update;
					updated = true;
				}
			};
		}

		// Update the underlying payment details if present
		if let Some(payment_update) = &update.payment_update {
			updated |= self.details.update(payment_update);
		}

		if let Some(new_conflicting_txids) = &update.conflicting_txids {
			update_if_necessary!(self.conflicting_txids, new_conflicting_txids.clone());
		}

		if let Some(new_raw_tx) = &update.raw_tx {
			update_if_necessary!(self.raw_tx, new_raw_tx.clone());
		}

		if let Some(new_last_broadcast_time) = update.last_broadcast_time {
			update_if_necessary!(self.last_broadcast_time, new_last_broadcast_time);
		}

		if let Some(new_broadcast_attempts) = update.broadcast_attempts {
			update_if_necessary!(self.broadcast_attempts, new_broadcast_attempts);
		}

		updated
	}

	fn to_update(&self) -> Self::Update {
		self.into()
	}
}

impl StorableObjectUpdate<PendingPaymentDetails> for PendingPaymentDetailsUpdate {
	fn id(&self) -> <PendingPaymentDetails as StorableObject>::Id {
		self.id
	}
}

impl From<&PendingPaymentDetails> for PendingPaymentDetailsUpdate {
	fn from(value: &PendingPaymentDetails) -> Self {
		Self {
			id: value.id(),
			payment_update: Some(value.details.to_update()),
			conflicting_txids: Some(value.conflicting_txids.clone()),
			raw_tx: Some(value.raw_tx.clone()),
			last_broadcast_time: Some(value.last_broadcast_time),
			broadcast_attempts: Some(value.broadcast_attempts),
		}
	}
}
