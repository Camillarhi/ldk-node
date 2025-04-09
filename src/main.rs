use bitcoin::address::{NetworkUnchecked, ParseError};
use bitcoin::Address;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::Network;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning_invoice::Bolt11Invoice;
use ldk_node::Builder;
use std::str::FromStr;
mod error;

pub use error::Error as NodeError;
use error::Error;

fn main() {
	let mut builder = Builder::new();
	builder.set_network(Network::Testnet);
	builder.set_chain_source_esplora("https://blockstream.info/testnet/api".to_string(), None);
	builder.set_gossip_source_rgs(
		"https://rapidsync.lightningdevkit.org/testnet/snapshot".to_string(),
	);

	let node = builder.build().unwrap();

	node.start().unwrap();

	let funding_address = node.onchain_payment().new_address();
	// let funding_address = "tb1q0d40e5rta4fty63z64gztf8c3v20cvet6v2jdh"; // Testnet address
	println!("Funding address: {:?}", funding_address);
	// .. fund address ..
	// Fund the wallet using the printed address before proceeding
	println!("Please fund the wallet with at least 11000 sats (including fees).");
	println!("Press Enter to continue after funding...");
	// let mut input = String::new();
	// std::io::stdin().read_line(&mut input).unwrap();

	// Wait for synchronization
	println!("Waiting for wallet to sync with the blockchain...");
	// std::thread::sleep(std::time::Duration::from_secs(600)); // Wait for 10 minutes (600 seconds) for the wallet to sync

	// let funding_address = funding_address.unwrap();
	// node.onchain_payment().send_to_address(&funding_address, 10000, None).unwrap();
	// println!("Sent 10000 sat to funding address");

	let balance = node.list_balances();
	println!("Wallet balance: {:?}", balance);

	let config = node.config();
	println!("Node config: {:?}", config);

	let node_id = node.node_id();
	println!("Node ID: {:?}", node_id);

	let listnening_address = node.listening_addresses();
	println!("Listening address: {:?}", listnening_address);

	//todo: delay for 10 min or more to allow real funding of a generated address

	// let node_id = PublicKey::from_str("026fa208407265cddd6f8184803e71480c3ba1d8885d9e322dba1c57bb414d1ed1").unwrap();
	// let node_addr = SocketAddress::from_str("127.0.0.1:9735").unwrap();
	let channels = node.list_channels();
	for channel in channels {
		println!("Channel: {:?}", channel);
	}

	let send_to_address = get_sendto_address().unwrap();

	const ADDR: &str = "tb1q0d40e5rta4fty63z64gztf8c3v20cvet6v2jdh";
	// let val = parse_and_validate_address(node.config().network, &send_to_address.to_string()).unwrap();
	// println!("Parsed and validated address: {:?}", val);

	// let validated_address =
	// 	unchecked_address.require_network(node.config().network).map_err(|_| Error::InvalidNetwork);

	// let tx_id = node.onchain_payment().send_to_address(&send_to_address, 300, None)?;
	// println!("Transaction ID: {:?}", tx_id);
	match node.onchain_payment().send_to_address(&send_to_address, 300, None) {
		Ok(txid) => {
			println!("Successfully sent transaction: {}", txid);
			// Continue with success case
		},
		// Err(Error::InvalidNetworkAddress { expected, received }) => {
		// 	eprintln!("Error: Please use a {} address (this was a {} address)", expected, actual);
		// 	// Handle network mismatch case
		// },
		// Err(Error::InvalidAddressFormat(e)) => {
		// 	eprintln!("Error: Invalid address format: {}", e);
		// 	// Handle bad address case
		// },
		Err(e) => {
			eprintln!("Error: {}", e);
			node.stop().unwrap();
			// Handle other errors
		},
	}
	// node.open_channel(node_id, node_addr, 10000, None, None).unwrap();
	node.stop().unwrap();

	let event = node.wait_next_event();
	println!("EVENT: {:?}", event);
	node.event_handled();

	let invoice = Bolt11Invoice::from_str("lnbcrt1m1pn7cpr8pp57czqam62fenlx7dq9gd0zj53kymydl6kgmd8j76qeawszeagcvjqdqqcqzzsxqyz5vqsp57kzl3xlsvkx68jeegrt548tp02vrkcu7ku7x5eszwx36825kgshs9qxpqysgqlyfja29vh9m3jsu6uxrzz7htg84qygc3kgahxw2njzrmfh8htaqs0sscr86wfywq934w8sxzqwhq6hpemg7c4ufnmqz559kzr0sswfgq7cs35c").unwrap();
	node.bolt11_payment().send(&invoice, None).unwrap();

	node.stop().unwrap();
}

fn get_sendto_address() -> Result<Address, ParseError> {
	let static_address = "bc1qmhwps3pz9ff0ms8gvte66l59g0zx7cvx3fw0nn";
	// let static_address = "tb1q0d40e5rta4fty63z64gztf8c3v20cvet6v2jdh";
	let unchecked_address = Address::<NetworkUnchecked>::from_str(static_address)?;
	let send_to_address = unchecked_address.assume_checked();

	Ok(send_to_address)
}

fn parse_and_validate_address(network: Network, addr: &str) -> Result<Address, ParseError> {
	// Parse the address into an unchecked state
	let unchecked_address = Address::<NetworkUnchecked>::from_str(addr)?;

	// Validate the network and convert to a checked address
	let validated_address = unchecked_address.require_network(network)?;

	Ok(validated_address)

	// let address = ADDR.parse::<Address<_>>()?.require_network(network)?;
	// Ok(address)
}
