// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bitcoin::{psbt::Input, Amount, FeeRate, Transaction, TxIn};

use lightning::ln::channelmanager::PaymentId;
use lightning::log_warn;

use payjoin::io::fetch_ohttp_keys;
use payjoin::persist::OptionalTransitionOutcome;
use payjoin::receive::v2::{
	replay_event_log_async as replay_receiver_event_log_async, HasReplyableError, Initialized,
	MaybeInputsOwned, MaybeInputsSeen, Monitor, OutputsUnknown, PayjoinProposal,
	ProvisionalProposal, ReceiveSession, Receiver, ReceiverBuilder,
	SessionOutcome as ReceiverSessionOutcome, UncheckedOriginalPayload, WantsFeeRange, WantsInputs,
	WantsOutputs,
};
use payjoin::receive::InputPair;
use payjoin::send::v2::{
	replay_event_log_async as replay_sender_event_log_async, PollingForProposal, SendSession,
	Sender, SenderBuilder, SessionOutcome as SenderSessionOutcome, WithReplyKey,
};
use payjoin::{ImplementationError, PjParam, Uri};

use crate::chain::ChainSource;
use crate::config::{
	Config, PayjoinConfig, PAYJOIN_MAX_RESPONSE_BYTES, PAYJOIN_RELAY_TIMEOUT_SECS,
	PAYJOIN_RETRY_DELAY, PAYJOIN_SESSION_CLEANUP_AGE_SECS,
};
use crate::fee_estimator::{ConfirmationTarget, FeeEstimator, OnchainFeeEstimator};
use crate::logger::{log_debug, log_error, log_info, LdkLogger, Logger};
use crate::payment::payjoin::payjoin_session::{PayjoinSession, PayjoinSessionKind, PayjoinStatus};
use crate::payment::payjoin::persist::{
	KVStorePayjoinReceiverPersister, KVStorePayjoinSenderPersister,
};
use crate::payment::payjoin::seen_inputs::check_and_record_seen_input;

use crate::types::{Broadcaster, PayjoinSeenInputsStore, PayjoinSessionStore};
use crate::wallet::Wallet;
use crate::Error;

/// The result of opening a Payjoin receive session.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PayjoinReceiveSession {
	/// The BIP 21 URI to share with the sender.
	pub uri: String,
	/// Identifier for this session
	pub session_id: PaymentId,
}

#[derive(Clone)]
pub(crate) struct PayjoinManager {
	payjoin_session_store: Arc<PayjoinSessionStore>,
	payjoin_seen_inputs_store: Arc<PayjoinSeenInputsStore>,
	logger: Arc<Logger>,
	config: Arc<Config>,
	wallet: Arc<Wallet>,
	fee_estimator: Arc<OnchainFeeEstimator>,
	chain_source: Arc<ChainSource>,
	broadcaster: Arc<Broadcaster>,
	session_retry_at: Arc<Mutex<HashMap<PaymentId, Instant>>>,
}

impl PayjoinManager {
	pub(crate) fn new(
		payjoin_session_store: Arc<PayjoinSessionStore>,
		payjoin_seen_inputs_store: Arc<PayjoinSeenInputsStore>, logger: Arc<Logger>,
		config: Arc<Config>, wallet: Arc<Wallet>, fee_estimator: Arc<OnchainFeeEstimator>,
		chain_source: Arc<ChainSource>, broadcaster: Arc<Broadcaster>,
	) -> Self {
		Self {
			payjoin_session_store,
			payjoin_seen_inputs_store,
			logger,
			config,
			wallet,
			fee_estimator,
			chain_source,
			broadcaster,
			session_retry_at: Arc::new(Mutex::new(HashMap::new())),
		}
	}

	pub(crate) async fn receive_payjoin(
		&self, amount_sats: u64, fee_rate: Option<FeeRate>,
	) -> Result<PayjoinReceiveSession, Error> {
		let payjoin_config =
			self.config.payjoin_config.as_ref().ok_or(Error::PayjoinNotConfigured)?;

		if payjoin_config.ohttp_relays.is_empty() {
			log_error!(self.logger, "No OHTTP relays configured.");
			return Err(Error::PayjoinNotConfigured);
		}

		// Generate a new session ID
		let mut random_bytes = [0u8; 32];
		getrandom::fill(&mut random_bytes).map_err(|e| {
			log_error!(self.logger, "Failed to generate random session ID: {}", e);
			Error::PayjoinSessionCreationFailed
		})?;
		let session_id = PaymentId(random_bytes);

		let confirmation_target = ConfirmationTarget::OnchainPayment;
		let fee_rate =
			fee_rate.unwrap_or_else(|| self.fee_estimator.estimate_fee_rate(confirmation_target));

		let address = self.wallet.get_new_address().await?;
		let ohttp_keys = {
			let mut result = Err(Error::ConnectionFailed);
			for relay in self.relay_order(payjoin_config)? {
				match fetch_ohttp_keys(relay, payjoin_config.payjoin_directory.as_str()).await {
					Ok(keys) => {
						result = Ok(keys);
						break;
					},
					Err(e) => {
						log_error!(
							self.logger,
							"Failed to fetch OHTTP keys via {}: {}. Trying next relay.",
							relay,
							e
						);
					},
				}
			}
			result
		}?;
		log_debug!(self.logger, "Fetched OHTTP keys: {:?}", ohttp_keys);

		let amount = Amount::from_sat(amount_sats);

		// Create a new persister for this session
		let persister = KVStorePayjoinReceiverPersister::new(
			session_id,
			Arc::clone(&self.payjoin_session_store),
			PayjoinSessionKind::Receive,
		);

		let session =
			ReceiverBuilder::new(address, payjoin_config.payjoin_directory.as_str(), ohttp_keys)
				.map_err(|e| {
					log_error!(self.logger, "Failed to create receiver builder: {}", e);
					Error::PayjoinSessionCreationFailed
				})?
				.with_amount(amount)
				.with_max_fee_rate(fee_rate)
				.build()
				.save_async(&persister)
				.await
				.map_err(|e| {
					log_error!(self.logger, "Failed to persist the new payjoin session: {:?}", e);
					Error::PersistenceFailed
				})?;

		log_info!(self.logger, "Receive session established");
		let pj_uri = session.pj_uri();

		Ok(PayjoinReceiveSession { uri: pj_uri.to_string(), session_id })
	}

	pub(crate) async fn send_payjoin(
		&self, bip21: &str, fee_rate: Option<FeeRate>,
	) -> Result<PaymentId, Error> {
		let payjoin_config =
			self.config.payjoin_config.as_ref().ok_or(Error::PayjoinNotConfigured)?;

		if payjoin_config.ohttp_relays.is_empty() {
			log_error!(self.logger, "No OHTTP relays configured.");
			return Err(Error::PayjoinNotConfigured);
		}

		let uri = Uri::try_from(bip21)
			.map_err(|e| {
				log_error!(self.logger, "Failed to parse payjoin URI: {}", e);
				Error::PayjoinUriInvalid
			})?
			.assume_checked()
			.check_pj_supported()
			.map_err(|_| {
				log_error!(self.logger, "URI does not support Payjoin.");
				Error::PayjoinUriInvalid
			})?;

		let pj_param = match uri.extras().pj_param() {
			PjParam::V2(p) => p,
			#[allow(unreachable_patterns)]
			_ => {
				log_error!(self.logger, "Only BIP 77 (v2) Payjoin URIs are supported.");
				return Err(Error::PayjoinUriInvalid);
			},
		};

		let address = uri.address().clone();
		let amount = uri.amount().ok_or_else(|| {
			log_error!(self.logger, "Payjoin URI is missing an amount.");
			Error::InvalidAmount
		})?;

		let mut random_bytes = [0u8; 32];
		getrandom::fill(&mut random_bytes).map_err(|e| {
			log_error!(self.logger, "Failed to generate random session ID: {}", e);
			Error::PayjoinSessionCreationFailed
		})?;
		let session_id = PaymentId(random_bytes);

		let confirmation_target = ConfirmationTarget::OnchainPayment;
		let fee_rate =
			fee_rate.unwrap_or_else(|| self.fee_estimator.estimate_fee_rate(confirmation_target));

		let original_psbt =
			self.wallet.build_payjoin_sender_psbt(&address, amount, fee_rate).await?;

		let fallback_tx = original_psbt.clone().extract_tx_unchecked_fee_rate();

		// Persist the fallback tx before saving the first session event so it survives a crash
		// between PSBT creation and the first relay POST.
		let mut session = PayjoinSession::new(session_id, PayjoinSessionKind::Send);
		session.set_fallback_tx(fallback_tx);
		self.payjoin_session_store.insert_or_update(session).await?;

		let persister = KVStorePayjoinSenderPersister::from_session(
			session_id,
			Arc::clone(&self.payjoin_session_store),
		)
		.await?;

		SenderBuilder::from_parts(original_psbt, pj_param, &address, Some(amount))
			.build_recommended(fee_rate)
			.map_err(|e| {
				log_error!(self.logger, "Failed to build payjoin sender: {}", e);
				Error::PayjoinSessionCreationFailed
			})?
			.save_async(&persister)
			.await
			.map_err(|e| {
				log_error!(self.logger, "Failed to persist the new payjoin send session: {:?}", e);
				Error::PersistenceFailed
			})?;

		log_info!(self.logger, "Payjoin send session established.");
		Ok(session_id)
	}

	fn relay_order<'a>(&self, payjoin_config: &'a PayjoinConfig) -> Result<Vec<&'a str>, Error> {
		let count = payjoin_config.ohttp_relays.len();
		let start = if count > 0 {
			let mut bytes = [0u8; 8];
			getrandom::fill(&mut bytes).map_err(|e| {
				log_error!(self.logger, "Failed to generate random relay index: {}", e);
				Error::PayjoinSessionFailed
			})?;
			u64::from_ne_bytes(bytes) as usize % count
		} else {
			0
		};
		Ok((0..count).map(|i| payjoin_config.ohttp_relays[(start + i) % count].as_str()).collect())
	}

	async fn process_receiver_session(
		&self, mut session: ReceiveSession, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		loop {
			session = match session {
				ReceiveSession::Initialized(proposal) => {
					match self.read_from_directory(proposal, persister).await? {
						// No sender yet. Yield so the resume task can service other sessions;
						// the next tick re-enters this session where it left off.
						ReceiveSession::Initialized(_) => return Ok(()),
						next => next,
					}
				},
				ReceiveSession::UncheckedOriginalPayload(proposal) => {
					self.check_original_payload(proposal, persister).await?
				},
				ReceiveSession::MaybeInputsOwned(proposal) => {
					self.check_inputs_not_owned(proposal, persister).await?
				},
				ReceiveSession::MaybeInputsSeen(proposal) => {
					self.check_no_inputs_seen_before(proposal, persister).await?
				},
				ReceiveSession::OutputsUnknown(proposal) => {
					self.identify_receiver_outputs(proposal, persister).await?
				},
				ReceiveSession::WantsOutputs(proposal) => {
					self.commit_outputs(proposal, persister).await?
				},
				ReceiveSession::WantsInputs(proposal) => {
					self.contribute_inputs(proposal, persister).await?
				},
				ReceiveSession::WantsFeeRange(proposal) => {
					self.apply_fee_range(proposal, persister).await?
				},
				ReceiveSession::ProvisionalProposal(proposal) => {
					self.finalize_proposal(proposal, persister).await?
				},
				ReceiveSession::PayjoinProposal(proposal) => {
					self.send_payjoin_proposal(proposal, persister).await?
				},
				ReceiveSession::HasReplyableError(error) => {
					match self.handle_error(error, persister).await? {
						// Retry on the next tick instead of hammering the relay
						// when a transient failure occurs.
						ReceiveSession::HasReplyableError(_) => return Ok(()),
						next => next,
					}
				},
				ReceiveSession::Monitor(proposal) => {
					self.monitor_payjoin_proposal(proposal, persister).await?;
					return Ok(());
				},
				ReceiveSession::PendingFallback(pending) => {
					let fallback_tx = pending.fallback_tx().clone();
					let mut payjoin_session = persister.get_session().await?;
					self.close_session_with_fallback(&mut payjoin_session, Some(&fallback_tx))
						.await;

					pending.close().save_async(persister).await?;
					return Ok(());
				},
				ReceiveSession::Closed(outcome) => {
					return self.handle_closed_session(outcome, persister).await;
				},
			}
		}
	}

	async fn process_sender_session(
		&self, mut session: SendSession, persister: &KVStorePayjoinSenderPersister,
	) -> Result<(), Error> {
		loop {
			session = match session {
				SendSession::WithReplyKey(sender) => {
					self.post_original_psbt(sender, persister).await?
				},
				SendSession::PollingForProposal(sender) => {
					match self.poll_for_proposal(sender, persister).await? {
						// No proposal yet; yield so the next background tick retries.
						SendSession::PollingForProposal(_) => return Ok(()),
						next => next,
					}
				},
				SendSession::Closed(SenderSessionOutcome::Success(proposal_psbt)) => {
					self.finalize_and_broadcast(proposal_psbt, persister).await?;
					return Ok(());
				},
				SendSession::Closed(SenderSessionOutcome::Aborted) => {
					self.broadcast_sender_fallback(persister).await;
					return Ok(());
				},
				SendSession::PendingFallback(pending) => {
					let fallback_tx = pending.fallback_tx().clone();
					let mut session = persister.get_session().await?;
					self.close_session_with_fallback(&mut session, Some(&fallback_tx)).await;
					pending.close().save_async(persister).await.map_err(|e| {
						log_error!(
							self.logger,
							"Failed to close pending fallback session: {:?}",
							e
						);
						Error::PersistenceFailed
					})?;
					return Ok(());
				},
			};
		}
	}

	async fn read_from_directory(
		&self, session: Receiver<Initialized>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<ReceiveSession, Error> {
		let Some((ohttp_response, context)) = self
			.post_via_relay(|relay| {
				session.create_poll_request(relay).map(Some).or_else(|e| {
					if e.is_expired() {
						Ok(None)
					} else {
						Err(Error::PayjoinSessionFailed)
					}
				})
			})
			.await?
		else {
			log_info!(self.logger, "Payjoin session {} expired.", persister.session_id());
			return Ok(ReceiveSession::Closed(ReceiverSessionOutcome::Aborted));
		};

		let state_transition = session
			.process_response(ohttp_response.as_bytes(), context)
			.save_async(persister)
			.await;

		match state_transition {
			Ok(OptionalTransitionOutcome::Progress(next_state)) => {
				log_info!(
					self.logger,
					"Got a request from the sender. Responding with a Payjoin proposal."
				);
				Ok(ReceiveSession::UncheckedOriginalPayload(next_state))
			},
			Ok(OptionalTransitionOutcome::Stasis(current_state)) => {
				Ok(ReceiveSession::Initialized(current_state))
			},
			Err(e) if e.is_transient() => {
				log_debug!(self.logger, "Transient error polling for request, retrying: {e:?}");
				let session = e.transient_state().expect("transient error carries current state");
				Ok(ReceiveSession::Initialized(session))
			},
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to process the sender's directory response: {:?}",
					e
				);
				Err(Error::PersistenceFailed)
			},
		}
	}

	async fn post_request(&self, req: payjoin::Request) -> Result<bitreq::Response, Error> {
		let response = bitreq::post(req.url)
			.with_header("Content-Type", req.content_type)
			.with_body(req.body)
			.with_timeout(PAYJOIN_RELAY_TIMEOUT_SECS)
			.with_max_body_size(PAYJOIN_MAX_RESPONSE_BYTES)
			.send_async()
			.await
			.map_err(|e| {
				log_error!(self.logger, "HTTP request failed: {}", e);
				Error::ConnectionFailed
			})?;

		if !(200..300).contains(&response.status_code) {
			log_error!(
				self.logger,
				"Relay returned status {} for payjoin request.",
				response.status_code
			);
			return Err(Error::ConnectionFailed);
		}

		Ok(response)
	}

	async fn check_original_payload(
		&self, proposal: Receiver<UncheckedOriginalPayload>,
		persister: &KVStorePayjoinReceiverPersister,
	) -> Result<ReceiveSession, Error> {
		let transaction = proposal.extract_tx_to_check_broadcast_suitability();
		let is_broadcast_suitable =
			self.chain_source.can_broadcast_transaction(&transaction).await.map_err(|e| {
				log_error!(
					self.logger,
					"Failed to check broadcast suitability of the sender's original transaction: {:?}",
					e
				);
				Error::PayjoinSessionFailed
			})?;

		let proposal = proposal
			.apply_broadcast_suitability(None, is_broadcast_suitable)
			.save_async(persister)
			.await
			.map_err(|e| {
				log_error!(
					self.logger,
					"Failed to check the sender's original transaction for broadcast suitability: {:?}",
					e
				);
				Error::PersistenceFailed
			})?;

		// If the payjoin fails or times out, broadcast this fallback tx to ensure the receiver still gets paid.
		let fallback_tx = proposal.extract_tx_to_schedule_broadcast();

		let mut session = persister.get_session().await?;

		session.set_fallback_tx(fallback_tx);
		self.payjoin_session_store.insert_or_update(session).await?;

		log_info!(
			self.logger,
			"Fallback transaction received. This will be broadcast if the Payjoin fails"
		);

		// Sender inputs with a non-empty script_sig change the txid once the sender signs, so we
		// couldn't match the payjoin to its payment record. Cancel and let the fallback
		// transaction pay us instead.
		if !proposal.proposal_txid_is_stable() {
			log_info!(
				self.logger,
				"Declining payjoin: sender inputs aren't native SegWit. Broadcasting the fallback."
			);
			let pending = proposal.cancel().save_async(persister).await.map_err(|e| {
				log_error!(
					self.logger,
					"Failed to cancel the payjoin session after declining: {:?}",
					e
				);
				Error::PersistenceFailed
			})?;
			return Ok(ReceiveSession::PendingFallback(pending));
		}

		Ok(ReceiveSession::MaybeInputsOwned(proposal))
	}

	async fn check_inputs_not_owned(
		&self, proposal: Receiver<MaybeInputsOwned>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<ReceiveSession, Error> {
		let proposal = proposal
			.check_inputs_not_owned(&mut |outpoint| {
				self.wallet
					.is_my_outpoint(&outpoint)
					.map_err(|e| ImplementationError::from(e.to_string().as_str()))
			})
			.save_async(persister)
			.await
			.map_err(|e| {
				log_error!(
					self.logger,
					"Failed to check whether the sender's inputs belong to us: {:?}",
					e
				);
				Error::PersistenceFailed
			})?;

		Ok(ReceiveSession::MaybeInputsSeen(proposal))
	}

	async fn check_no_inputs_seen_before(
		&self, proposal: Receiver<MaybeInputsSeen>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<ReceiveSession, Error> {
		let session_id = persister.session_id();

		let now = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or(Duration::from_secs(0))
			.as_secs();

		let mut marked_checklist = Vec::new();
		for item in proposal.inputs_seen_checklist() {
			let seen = check_and_record_seen_input(
				&self.payjoin_seen_inputs_store,
				*item.value(),
				session_id,
				now,
			)
			.await?;
			marked_checklist.push(item.mark(seen));
		}

		let transition = proposal.apply_inputs_seen_checklist(marked_checklist);
		let proposal = transition.save_async(persister).await.map_err(|e| {
			log_error!(self.logger, "Failed to check the sender's inputs for reuse: {:?}", e);
			Error::PersistenceFailed
		})?;

		Ok(ReceiveSession::OutputsUnknown(proposal))
	}

	async fn identify_receiver_outputs(
		&self, proposal: Receiver<OutputsUnknown>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<ReceiveSession, Error> {
		let proposal = proposal
			.identify_receiver_outputs(&mut |output_script| {
				self.wallet
					.is_mine(output_script.to_owned())
					.map_err(|e| ImplementationError::from(e.to_string().as_str()))
			})
			.save_async(persister)
			.await
			.map_err(|e| {
				log_error!(
					self.logger,
					"Failed to identify our outputs in the sender's proposal: {:?}",
					e
				);
				Error::PersistenceFailed
			})?;
		Ok(ReceiveSession::WantsOutputs(proposal))
	}

	async fn commit_outputs(
		&self, proposal: Receiver<WantsOutputs>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<ReceiveSession, Error> {
		let proposal = proposal.commit_outputs().save_async(persister).await.map_err(|e| {
			log_error!(self.logger, "Failed to commit the payjoin outputs: {:?}", e);
			Error::PersistenceFailed
		})?;
		Ok(ReceiveSession::WantsInputs(proposal))
	}

	async fn contribute_inputs(
		&self, proposal: Receiver<WantsInputs>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<ReceiveSession, Error> {
		let candidate_inputs = self.list_input_pairs()?;

		if candidate_inputs.is_empty() {
			log_error!(
				self.logger,
				"No spendable UTXOs available in wallet. Cannot contribute inputs to payjoin."
			);
			return Err(Error::InsufficientFunds);
		}

		let selected_input = proposal.try_preserving_privacy(candidate_inputs).map_err(|e| {
			log_error!(self.logger, "Failed to select input for payjoin contribution: {}", e);
			Error::PayjoinSessionFailed
		})?;
		let proposal = proposal
			.contribute_inputs(vec![selected_input])
			.map_err(|e| {
				log_error!(self.logger, "Failed to contribute inputs to payjoin: {}", e);
				Error::PayjoinSessionFailed
			})?
			.commit_inputs()
			.save_async(persister)
			.await
			.map_err(|e| {
				log_error!(self.logger, "Failed to commit our contributed payjoin inputs: {:?}", e);
				Error::PersistenceFailed
			})?;
		Ok(ReceiveSession::WantsFeeRange(proposal))
	}

	fn list_input_pairs(&self) -> Result<Vec<InputPair>, Error> {
		let unspent = self.wallet.list_unspent_confirmed_utxos()?;

		let mut input_pairs = Vec::with_capacity(unspent.len());

		for u in unspent {
			let txin = TxIn { previous_output: u.outpoint, ..Default::default() };
			let psbtin = Input { witness_utxo: Some(u.output.clone()), ..Default::default() };

			let input_pair = InputPair::new(txin, psbtin, None).map_err(|e| {
				log_error!(self.logger, "Failed to create InputPair: {}", e);
				Error::PayjoinSessionFailed
			})?;

			input_pairs.push(input_pair);
		}

		Ok(input_pairs)
	}

	async fn apply_fee_range(
		&self, proposal: Receiver<WantsFeeRange>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<ReceiveSession, Error> {
		let proposal =
			proposal.apply_fee_range(None, None).save_async(persister).await.map_err(|e| {
				log_error!(self.logger, "Failed to apply the payjoin fee range: {:?}", e);
				Error::PersistenceFailed
			})?;

		Ok(ReceiveSession::ProvisionalProposal(proposal))
	}

	async fn finalize_proposal(
		&self, proposal: Receiver<ProvisionalProposal>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<ReceiveSession, Error> {
		let proposal = proposal
			.finalize_proposal(|psbt| {
				self.wallet
					.process_psbt(psbt.clone())
					.map_err(|e| ImplementationError::from(e.to_string().as_str()))
			})
			.save_async(persister)
			.await
			.map_err(|e| {
				log_error!(self.logger, "Failed to finalize the payjoin proposal: {:?}", e);
				Error::PersistenceFailed
			})?;
		Ok(ReceiveSession::PayjoinProposal(proposal))
	}

	async fn send_payjoin_proposal(
		&self, proposal: Receiver<PayjoinProposal>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<ReceiveSession, Error> {
		let Some((ohttp_response, context)) = self
			.post_via_relay(|relay| {
				proposal.create_post_request(relay).map(Some).or_else(|e| {
					if e.is_expired() {
						Ok(None)
					} else {
						Err(Error::PayjoinSessionFailed)
					}
				})
			})
			.await?
		else {
			log_info!(self.logger, "Payjoin session {} expired.", persister.session_id());
			return Ok(ReceiveSession::Closed(ReceiverSessionOutcome::Aborted));
		};

		let payjoin_psbt = proposal.psbt().clone();

		match proposal
			.process_response(ohttp_response.as_bytes(), context)
			.save_async(persister)
			.await
		{
			Ok(session) => {
				// At this point we will persist the fee and txid to the session store
				let payjoin_tx = payjoin_psbt.extract_tx_unchecked_fee_rate();
				let txid = payjoin_tx.compute_txid();

				log_info!(
					self.logger,
					"Response successful. Watch mempool for successful Payjoin. TXID: {}",
					txid
				);
				Ok(ReceiveSession::Monitor(session))
			},
			Err(e) if e.is_transient() => {
				log_debug!(
					self.logger,
					"Transient error sending payjoin proposal, retrying: {e:?}"
				);
				let proposal = e.transient_state().expect("transient error carries current state");
				Ok(ReceiveSession::PayjoinProposal(proposal))
			},
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to process the sender's response to our payjoin proposal: {:?}",
					e
				);
				Err(Error::PersistenceFailed)
			},
		}
	}

	async fn monitor_payjoin_proposal(
		&self, mut proposal: Receiver<Monitor>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		// On a session resumption, the receiver will resume again in this state.
		let poll_interval = tokio::time::Duration::from_secs(2);

		let timeout_duration = tokio::time::Duration::from_secs(10);

		let mut interval = tokio::time::interval(poll_interval);
		interval.tick().await;

		log_debug!(self.logger, "Polling for payjoin transaction in the mempool...");

		let polled = tokio::time::timeout(timeout_duration, async {
			loop {
				interval.tick().await;
				let check_result = proposal
					.check_for_transaction(|txid| {
						tokio::task::block_in_place(|| {
							tokio::runtime::Handle::current()
								.block_on(self.chain_source.get_transaction(&txid))
						})
						.map_err(|e| ImplementationError::from(e.to_string().as_str()))
					})
					.save_async(persister)
					.await;

				match check_result {
					Ok(OptionalTransitionOutcome::Progress(())) => {
						return Ok(());
					},
					Ok(OptionalTransitionOutcome::Stasis(current_state)) => {
						proposal = current_state;
					},
					Err(e) if e.is_transient() => {
						log_debug!(
							self.logger,
							"Transient error checking for transaction, retrying: {e:?}"
						);
						proposal =
							e.transient_state().expect("transient error carries current state");
					},
					Err(_) => return Err(Error::PayjoinSessionFailed),
				}
			}
		})
		.await;

		match polled {
			Ok(Ok(())) => {
				log_info!(self.logger, "Payjoin transaction detected in the mempool!");
				// Replay the session to recover the outcome and record the
				// payment now rather than on the next resume tick.
				let (session, _) =
					replay_receiver_event_log_async(persister).await.map_err(|e| {
						log_error!(
							self.logger,
							"Failed to replay the closed payjoin session: {:?}",
							e
						);
						Error::PayjoinSessionFailed
					})?;
				if let ReceiveSession::Closed(outcome) = session {
					return self.handle_closed_session(outcome, persister).await;
				}
				Ok(())
			},
			Ok(Err(e)) => Err(e),
			Err(_) => {
				log_debug!(
					self.logger,
					"Payjoin transaction not yet seen after {:?}. Will retry on next background tick.",
					timeout_duration
				);
				Ok(())
			},
		}
	}

	async fn post_original_psbt(
		&self, sender: Sender<WithReplyKey>, persister: &KVStorePayjoinSenderPersister,
	) -> Result<SendSession, Error> {
		let Some((response, ctx)) = self
			.post_via_relay(|relay| {
				sender.create_v2_post_request(relay).map(Some).or_else(|e| {
					if e.is_expired() {
						Ok(None)
					} else {
						Err(Error::PayjoinSessionFailed)
					}
				})
			})
			.await?
		else {
			log_info!(self.logger, "Payjoin send session {} expired.", persister.session_id());
			return Ok(SendSession::Closed(SenderSessionOutcome::Aborted));
		};

		match sender.process_response(response.as_bytes(), ctx).save_async(persister).await {
			Ok(next) => {
				log_info!(self.logger, "Posted original PSBT. Waiting for payjoin proposal.");
				Ok(SendSession::PollingForProposal(next))
			},
			Err(e) if e.is_transient() => {
				log_debug!(self.logger, "Transient error posting original PSBT, retrying: {:?}", e);
				let sender = e.transient_state().expect("transient error carries current state");
				Ok(SendSession::WithReplyKey(sender))
			},
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to process response to original PSBT post: {:?}",
					e
				);
				Err(Error::PayjoinSessionFailed)
			},
		}
	}

	async fn poll_for_proposal(
		&self, sender: Sender<PollingForProposal>, persister: &KVStorePayjoinSenderPersister,
	) -> Result<SendSession, Error> {
		let Some((response, ctx)) = self
			.post_via_relay(|relay| {
				sender.create_poll_request(relay).map(Some).or_else(|e| {
					if e.is_expired() {
						Ok(None)
					} else {
						Err(Error::PayjoinSessionFailed)
					}
				})
			})
			.await?
		else {
			log_info!(self.logger, "Payjoin send session {} expired.", persister.session_id());
			return Ok(SendSession::Closed(SenderSessionOutcome::Aborted));
		};

		match sender.clone().process_response(response.as_bytes(), ctx).save_async(persister).await
		{
			Ok(OptionalTransitionOutcome::Progress(proposal_psbt)) => {
				log_info!(self.logger, "Received payjoin proposal from receiver. Processing.");
				Ok(SendSession::Closed(SenderSessionOutcome::Success(proposal_psbt)))
			},
			Ok(OptionalTransitionOutcome::Stasis(current)) => {
				log_debug!(self.logger, "No payjoin proposal yet.");
				Ok(SendSession::PollingForProposal(current))
			},
			Err(e) if e.is_transient() => {
				log_debug!(self.logger, "Transient error polling for proposal, retrying: {:?}", e);
				let sender = e.transient_state().expect("transient error carries current state");
				Ok(SendSession::PollingForProposal(sender))
			},
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to process payjoin proposal poll response: {:?}",
					e
				);
				Err(Error::PayjoinSessionFailed)
			},
		}
	}

	async fn finalize_and_broadcast(
		&self, proposal_psbt: bitcoin::Psbt, persister: &KVStorePayjoinSenderPersister,
	) -> Result<(), Error> {
		let signed_psbt = self.wallet.process_psbt(proposal_psbt).map_err(|e| {
			log_error!(self.logger, "Failed to sign payjoin proposal PSBT: {:?}", e);
			e
		})?;

		let fee_paid_sats = signed_psbt.fee().ok().map(|f| f.to_sat());
		let tx = signed_psbt.extract_tx_unchecked_fee_rate();
		let txid = tx.compute_txid();
		self.broadcaster.broadcast(tx.clone());

		log_info!(self.logger, "Payjoin transaction broadcast. TXID: {}", txid);

		self.wallet.classify_payjoin(txid, Some(&tx), fee_paid_sats).await?;

		let mut session = persister.get_session().await?;
		session.status = PayjoinStatus::Completed;
		session.completed_at = Some(
			SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.unwrap_or(Duration::from_secs(0))
				.as_secs(),
		);
		self.payjoin_session_store.insert_or_update(session).await?;

		Ok(())
	}

	async fn broadcast_sender_fallback(&self, persister: &KVStorePayjoinSenderPersister) {
		match persister.get_session().await {
			Ok(mut session) => self.close_session_with_fallback(&mut session, None).await,
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to load session {} to broadcast fallback: {:?}",
					persister.session_id(),
					e
				);
			},
		}
	}

	async fn post_via_relay<F>(
		&self, mut build: F,
	) -> Result<Option<(bitreq::Response, payjoin::OhttpResponse)>, Error>
	where
		F: FnMut(&str) -> Result<Option<(payjoin::Request, payjoin::OhttpResponse)>, Error>,
	{
		let payjoin_config =
			self.config.payjoin_config.as_ref().ok_or(Error::PayjoinNotConfigured)?;

		for relay in self.relay_order(payjoin_config)? {
			let (req, ctx) = match build(relay)? {
				Some(r) => r,
				// Session expired; caller handles this as a clean abort.
				None => return Ok(None),
			};

			match self.post_request(req).await {
				Ok(resp) => return Ok(Some((resp, ctx))),
				Err(e) => {
					log_debug!(
						self.logger,
						"Request via relay {} failed, trying next: {:?}",
						relay,
						e
					);
				},
			}
		}

		log_error!(self.logger, "All configured OHTTP relays failed.");
		Err(Error::ConnectionFailed)
	}

	async fn handle_error(
		&self, session: Receiver<HasReplyableError>, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<ReceiveSession, Error> {
		let Some((err_response, err_ctx)) = self
			.post_via_relay(|relay| {
				session.create_error_request(relay).map(Some).or_else(|e| {
					if e.is_expired() {
						Ok(None)
					} else {
						Err(Error::PayjoinSessionFailed)
					}
				})
			})
			.await?
		else {
			log_info!(self.logger, "Payjoin session {} expired.", persister.session_id());
			return Ok(ReceiveSession::Closed(ReceiverSessionOutcome::Aborted));
		};

		let err_bytes = err_response.as_bytes();

		match session.process_error_response(err_bytes, err_ctx).save_async(persister).await {
			Ok(Some(pending)) => {
				log_info!(
					self.logger,
					"Session delivered error reply. Broadcast the fallback transaction."
				);
				Ok(ReceiveSession::PendingFallback(pending))
			},
			Ok(None) => Ok(ReceiveSession::Closed(ReceiverSessionOutcome::Aborted)),
			Err(e) if e.is_transient() => {
				log_debug!(self.logger, "Transient error posting error response, retrying: {e:?}");
				let session = e.transient_state().expect("transient error carries current state");
				Ok(ReceiveSession::HasReplyableError(session))
			},
			Err(e) => {
				if let Some(api_err) = e.api_error_ref() {
					log_warn!(self.logger, "Failed to confirm error response delivery: {api_err}");
				}
				match e.fatal_state() {
					Some(pending) => {
						log_error!(self.logger, "Session failed to deliver error reply. Broadcast the fallback transaction");
						Ok(ReceiveSession::PendingFallback(pending))
					},
					None => Err(Error::PayjoinSessionFailed),
				}
			},
		}
	}

	async fn handle_closed_session(
		&self, outcome: ReceiverSessionOutcome, persister: &KVStorePayjoinReceiverPersister,
	) -> Result<(), Error> {
		let session_id = persister.session_id();
		let mut session = persister.get_session().await?;
		session.completed_at.get_or_insert_with(|| {
			SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::from_secs(0)).as_secs()
		});

		match outcome {
			ReceiverSessionOutcome::Success(txid) => {
				log_info!(
					self.logger,
					"Payjoin session detected in the mempool and completed successfully."
				);
				// The transaction only supplies figures if wallet sync hasn't recorded it yet.
				let tx = match self.chain_source.get_transaction(&txid).await {
					Ok(tx) => tx,
					Err(e) => {
						log_debug!(
							self.logger,
							"Could not look up payjoin transaction {}: {:?}. Wallet sync will fill in the payment's amount and fee",
							txid,
							e
						);
						None
					},
				};
				self.wallet.classify_payjoin(txid, tx.as_ref(), None).await?;

				session.status = PayjoinStatus::Completed;

				self.payjoin_session_store.insert_or_update(session).await?;
			},
			ReceiverSessionOutcome::PayjoinProposalSent => {
				log_info!(
					self.logger,
					"Payjoin proposal sent. Cannot track broadcast due to non-SegWit sender inputs."
				);
				session.status = PayjoinStatus::Completed;

				self.payjoin_session_store.insert_or_update(session).await?;
			},
			ReceiverSessionOutcome::FallbackBroadcasted => {
				log_info!(self.logger, "Payjoin failed. Fallback transaction was broadcasted.");
				session.status = PayjoinStatus::Failed;

				self.payjoin_session_store.insert_or_update(session).await?;
			},
			ReceiverSessionOutcome::Aborted => {
				log_info!(self.logger, "Payjoin session was aborted.");
				self.close_session_with_fallback(&mut session, None).await;
			},
			ReceiverSessionOutcome::Unrecognized(txid) => {
				log_error!(
					self.logger,
					"Payjoin session {} was settled by an unrecognized transaction {}.",
					session_id,
					txid
				);
				session.status = PayjoinStatus::Failed;

				self.payjoin_session_store.insert_or_update(session).await?;
			},
		}
		Ok(())
	}

	pub(crate) async fn resume_payjoin_sessions(
		&self, mut stop_receiver: tokio::sync::watch::Receiver<()>,
	) -> Result<(), Error> {
		let now = Instant::now();
		let backing_off: HashSet<PaymentId> = {
			let retry_at = self.session_retry_at.lock().expect("lock");
			retry_at.iter().filter(|(_, at)| now < **at).map(|(id, _)| *id).collect()
		};

		let active_sessions = self
			.payjoin_session_store
			.list_filter(|p| {
				p.status == PayjoinStatus::Active && !backing_off.contains(&p.session_id)
			})
			.await;

		let recv_session_ids: Vec<PaymentId> = active_sessions
			.iter()
			.filter(|p| matches!(p.kind, PayjoinSessionKind::Receive))
			.map(|s| s.session_id)
			.collect();

		let send_session_ids: Vec<PaymentId> = active_sessions
			.iter()
			.filter(|p| matches!(p.kind, PayjoinSessionKind::Send))
			.map(|s| s.session_id)
			.collect();

		if recv_session_ids.is_empty() && send_session_ids.is_empty() {
			log_debug!(self.logger, "No sessions to resume.");
			return Ok(());
		}

		let mut join_set: tokio::task::JoinSet<Result<(), Error>> = tokio::task::JoinSet::new();

		// Process sender sessions
		for session_id in send_session_ids {
			let self_clone = self.clone();
			let send_persister = match KVStorePayjoinSenderPersister::from_session(
				session_id,
				Arc::clone(&self.payjoin_session_store),
			)
			.await
			{
				Ok(p) => p,
				Err(e) => {
					log_error!(
						self.logger,
						"Failed to create persister for sender session {:?}: {:?}",
						session_id,
						e
					);
					self.session_retry_at
						.lock()
						.expect("lock")
						.insert(session_id, Instant::now() + PAYJOIN_RETRY_DELAY);
					continue;
				},
			};

			match replay_sender_event_log_async(&send_persister).await {
				Ok((sender_state, _)) => {
					join_set.spawn(async move {
						let res =
							self_clone.process_sender_session(sender_state, &send_persister).await;
						{
							let mut retry_at = self_clone.session_retry_at.lock().expect("lock");
							if res.is_err() {
								retry_at.insert(session_id, Instant::now() + PAYJOIN_RETRY_DELAY);
							} else {
								retry_at.remove(&session_id);
							}
						}
						res
					});
				},
				Err(e) if e.is_expired() => match send_persister.get_session().await {
					Ok(mut session) => {
						self.close_session_with_fallback(&mut session, e.expiry_fallback_tx()).await
					},
					Err(err) => log_error!(
						self.logger,
						"Failed to close expired sender session {session_id}: {err:?}"
					),
				},
				Err(e) => {
					log_error!(
						self.logger,
						"An error {:?} occurred while replaying sender session",
						e
					);
					match self.payjoin_session_store.get(&session_id).await {
						Ok(Some(mut session)) => {
							self.close_session_with_fallback(&mut session, None).await;
						},
						Ok(None) => {
							log_error!(
								self.logger,
								"Payjoin sender session {} disappeared before it could be closed.",
								session_id
							);
						},
						Err(e) => {
							log_error!(
								self.logger,
								"Failed to read sender session {} while closing it: {:?}",
								session_id,
								e
							);
						},
					}
				},
			}
		}

		// Process receiver sessions
		for session_id in recv_session_ids {
			let self_clone = self.clone();
			// Create a persister for this session
			let recv_persister = match KVStorePayjoinReceiverPersister::from_session(
				session_id,
				Arc::clone(&self.payjoin_session_store),
			)
			.await
			{
				Ok(p) => p,
				Err(e) => {
					log_error!(
						self.logger,
						"Failed to create persister for session {:?}: {:?}",
						session_id,
						e
					);
					self.session_retry_at
						.lock()
						.expect("lock")
						.insert(session_id, Instant::now() + PAYJOIN_RETRY_DELAY);
					continue;
				},
			};

			match replay_receiver_event_log_async(&recv_persister).await {
				Ok((receiver_state, _)) => {
					join_set.spawn(async move {
						let res = self_clone
							.process_receiver_session(receiver_state, &recv_persister)
							.await;
						{
							let mut retry_at = self_clone.session_retry_at.lock().expect("lock");
							if res.is_err() {
								retry_at.insert(session_id, Instant::now() + PAYJOIN_RETRY_DELAY);
							} else {
								retry_at.remove(&session_id);
							}
						}
						res
					});
				},
				Err(e) if e.is_expired() => match recv_persister.get_session().await {
					Ok(mut session) => {
						self.close_session_with_fallback(&mut session, e.expiry_fallback_tx()).await
					},
					Err(err) => log_error!(
						self.logger,
						"Failed to close expired receiver session {session_id}: {err:?}"
					),
				},
				Err(e) => {
					log_error!(
						self.logger,
						"An error {:?} occurred while replaying receiver session",
						e
					);
					match self.payjoin_session_store.get(&session_id).await {
						Ok(Some(mut session)) => {
							self.close_session_with_fallback(&mut session, None).await;
						},
						Ok(None) => {
							log_error!(
								self.logger,
								"Payjoin session {} disappeared before it could be closed.",
								session_id
							);
						},
						Err(e) => {
							log_error!(
								self.logger,
								"Failed to read payjoin session {} while closing it: {:?}",
								session_id,
								e
							);
						},
					}
				},
			}
		}

		tokio::select! {
			_ = async {
				while let Some(result) = join_set.join_next().await {
					match result {
						Ok(Ok(())) => log_debug!(self.logger, "A payjoin session task finished."),
						Ok(Err(e)) => log_error!(self.logger, "A payjoin session failed: {:?}", e),
						Err(e) => log_error!(self.logger, "A payjoin session task panicked: {:?}", e),
					}
				}
			} => {
				log_debug!(self.logger, "All payjoin resumed sessions completed.");
			}
			_ = stop_receiver.changed() => {
				join_set.abort_all();
				log_info!(self.logger, "Resumed payjoin sessions were interrupted.");
			}
		}
		Ok(())
	}

	async fn close_session_with_fallback(
		&self, session: &mut PayjoinSession, fallback_tx: Option<&Transaction>,
	) {
		session.status = PayjoinStatus::Failed;
		// Keep an earlier timestamp if payjoin's `close()` already ran
		session.completed_at.get_or_insert_with(|| {
			SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::from_secs(0)).as_secs()
		});

		let fallback_tx = fallback_tx.or_else(|| session.fallback_tx());

		if let Some(fallback_tx) = fallback_tx {
			self.broadcaster.broadcast(fallback_tx.clone());
		} else {
			log_warn!(
				self.logger,
				"Payjoin session {} missing fallback transaction; closing as Failed without broadcast.",
				session.session_id
			);
		}

		if let Err(close_err) = self.payjoin_session_store.insert_or_update(session.clone()).await {
			log_error!(
				self.logger,
				"Failed to close receiver session {}: {:?}",
				session.session_id,
				close_err
			);
		} else {
			log_info!(self.logger, "Closed failed payjoin session: {}", session.session_id);
		}
	}

	/// Cleans up old payjoin sessions that are completed or failed.
	/// Sessions older than `PAYJOIN_SESSION_CLEANUP_AGE_SECS` will be removed.
	pub(crate) async fn cleanup_old_sessions(&self) -> Result<(), Error> {
		let now = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.unwrap_or(std::time::Duration::from_secs(0))
			.as_secs();

		let sessions_to_remove: Vec<PaymentId> = self
			.payjoin_session_store
			.list_filter(|s| {
				let is_terminal =
					s.status == PayjoinStatus::Completed || s.status == PayjoinStatus::Failed;
				let completed_at = s.completed_at.unwrap_or(s.latest_update_timestamp);
				is_terminal && now.saturating_sub(completed_at) > PAYJOIN_SESSION_CLEANUP_AGE_SECS
			})
			.await
			.into_iter()
			.map(|s| s.session_id)
			.collect();

		if sessions_to_remove.is_empty() {
			return Ok(());
		}

		log_info!(self.logger, "Cleaning up {} old payjoin sessions", sessions_to_remove.len());

		for session_id in sessions_to_remove {
			if let Err(e) = self.payjoin_session_store.remove(&session_id).await {
				log_error!(
					self.logger,
					"Failed to remove old payjoin session {:?}: {:?}",
					session_id,
					e
				);
			}
			self.session_retry_at.lock().expect("lock").remove(&session_id);
		}

		Ok(())
	}
}
