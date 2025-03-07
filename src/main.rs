use anyhow::{Context, Error, Result};
use aptos_sdk::{
    coin_client::CoinClient,
    rest_client::{Client, FaucetClient},
    types::{AccountKey, LocalAccount},
};
use clap::Parser;
use csv::ReaderBuilder;
use inquire::{InquireError, Select, Text};
use std::{path::PathBuf, str::FromStr, time::Duration};
use tokio::time::sleep;
use url::Url;

/// A simple Aptos token airdropper
#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    /// Activates interactive mode
    #[clap(short, long)]
    interactive_mode: bool,

    /// Path to CSV file with recipient addresses and amounts
    #[clap(short, long, required_if_eq("interactive_mode", "false"))]
    csv_file: Option<PathBuf>,

    /// Private key for sender (hex encoded)
    #[clap(short, long, required_if_eq("interactive_mode", "false"))]
    private_key: Option<String>,

    /// Network to use (devnet, testnet, mainnet)
    #[clap(short, long, default_value = "testnet")]
    network: String,

    /// Delay between transactions in milliseconds
    #[clap(short, long, default_value = "100")]
    delay: u64,

    /// Dry run (don't actually send transactions)
    #[clap(short = 'r', long)]
    dry_run: bool,
}

struct Recipient {
    address: String,
    amount: u64,
}

struct AirdropUtils {
    pub node_url: String,
    pub faucet_url: Option<String>,
    pub sender: LocalAccount,
    pub balance: u64,
    pub recipients: Vec<Recipient>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // if true starts interactive mode
    let airdrop_utils: AirdropUtils = if args.interactive_mode {
        interactive_mode().await?
    } else {
        argument_mode(&args).await?
    };

    let rest_client = Client::new(Url::from_str(airdrop_utils.node_url.as_str())?);
    let coin_client = CoinClient::new(&rest_client);

    // Calculate total amount to be sent
    let total_amount: u64 = airdrop_utils.recipients.iter().map(|r| r.amount).sum();
    println!(
        "Total amount to be sent: {} APT",
        total_amount as f64 / 100_000_000.0
    );

    if airdrop_utils.balance < total_amount {
        return Err(anyhow::anyhow!(
            "Insufficient balance. Need at least {} APT",
            total_amount as f64 / 100_000_000.0
        ));
    }

    if args.dry_run {
        println!("DRY RUN MODE - No transactions will be submitted");
        return Ok(());
    }

    // Process airdrop
    let mut sender = airdrop_utils.sender;
    let mut successful = 0;
    let mut failed = 0;

    for (i, recipient) in airdrop_utils.recipients.iter().enumerate() {
        println!(
            "[{}/{}] Sending {} APT to {}...",
            i + 1,
            airdrop_utils.recipients.len(),
            recipient.amount as f64 / 100_000_000.0,
            &recipient.address
        );

        match process_transfer(&mut sender, &coin_client, &rest_client, &recipient).await {
            Ok(txn_hash) => {
                println!("  ✅ Success! Transaction hash: {}", txn_hash);
                successful += 1;
            }
            Err(e) => {
                println!("  ❌ Failed: {}", e);
                failed += 1;
            }
        }

        // Add delay between transactions
        if i < airdrop_utils.recipients.len() - 1 {
            sleep(Duration::from_millis(args.delay)).await;
        }
    }

    // Print summary
    println!("\nAirdrop Summary:");
    println!("  Total recipients: {}", airdrop_utils.recipients.len());
    println!("  Successful: {}", successful);
    println!("  Failed: {}", failed);

    Ok(())
}

fn create_account_from_private_key(private_key: &str) -> Result<LocalAccount> {
    let private_key = private_key.trim().trim_start_matches("0x");
    LocalAccount::from_private_key(private_key, 0)
        .context("Failed to create account from private key")
}

fn parse_csv_file(path: &PathBuf) -> Result<Vec<Recipient>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("Failed to open CSV file: {:?}", path))?;

    let mut reader = ReaderBuilder::new()
        .trim(csv::Trim::All)
        .flexible(true)
        .from_reader(file);

    // Detect and validate headers
    let headers = reader.headers()?.clone();
    let address_idx = headers
        .iter()
        .position(|h| h.to_lowercase() == "address")
        .context("CSV file must have an 'address' column")?;

    let amount_idx = headers
        .iter()
        .position(|h| h.to_lowercase() == "amount")
        .context("CSV file must have an 'amount' column")?;

    let mut recipients = Vec::new();

    for (i, record) in reader.records().enumerate() {
        let record = record.with_context(|| format!("Error reading row {}", i + 1))?;

        let address = record
            .get(address_idx)
            .context("Missing address field")?
            .trim()
            .to_string();

        let amount_str = record
            .get(amount_idx)
            .context("Missing amount field")?
            .trim();

        // Convert amount to octas (1 APT = 100_000_000 octas)
        let amount = if amount_str.contains('.') {
            // Handle decimal amounts
            let parts: Vec<&str> = amount_str.split('.').collect();
            let whole = parts[0]
                .parse::<u64>()
                .with_context(|| format!("Invalid amount in row {}: {}", i + 1, amount_str))?;

            let fraction = if parts.len() > 1 {
                let fraction_str = format!("{:0<8}", parts[1].chars().take(8).collect::<String>());
                fraction_str.parse::<u64>().with_context(|| {
                    format!("Invalid decimal part in row {}: {}", i + 1, amount_str)
                })?
            } else {
                0
            };

            whole * 100_000_000 + fraction
        } else {
            // Handle whole number amounts
            amount_str
                .parse::<u64>()
                .with_context(|| format!("Invalid amount in row {}: {}", i + 1, amount_str))?
                * 100_000_000
        };

        recipients.push(Recipient { address, amount });
    }

    Ok(recipients)
}

async fn process_transfer(
    sender: &mut LocalAccount,
    coin_client: &CoinClient<'_>,
    rest_client: &Client,
    recipient: &Recipient,
) -> Result<String> {
    // Parse recipient address - use LocalAccount's method to get the right AccountAddress type
    let recipient_address =
        aptos_sdk::types::account_address::AccountAddress::from_hex_literal(&recipient.address)
            .with_context(|| format!("Invalid recipient address: {}", recipient.address))?;

    // Check if recipient account exists
    let recipient_exists = match rest_client.get_account(recipient_address).await {
        Ok(_) => true,
        Err(_) => false,
    };

    if !recipient_exists {
        println!("  ⚠️ Note: Recipient account doesn't exist on-chain. Transaction may fail.");
    }

    // Get the current on-chain sequence number
    let on_chain_sequence = rest_client
        .get_account_sequence_number(sender.address())
        .await?
        .into_inner();

    // Update the sender's sequence number if it's behind
    if sender.sequence_number() < on_chain_sequence {
        println!(
            "  ⚠️ Updating sequence number from {} to {}",
            sender.sequence_number(),
            on_chain_sequence
        );
        sender.set_sequence_number(on_chain_sequence);
    }

    // Submit transaction with detailed error handling
    let txn_result = coin_client
        .transfer(sender, recipient_address, recipient.amount, None)
        .await;

    match txn_result {
        Ok(txn_hash) => {
            // Wait for transaction confirmation
            match rest_client.wait_for_transaction(&txn_hash).await {
                Ok(_) => {
                    // Transaction confirmed
                    Ok(format!("{:?}", txn_hash))
                }
                Err(e) => {
                    // Transaction submitted but confirmation failed
                    Err(anyhow::anyhow!(
                        "Transaction submitted but confirmation failed: {} (Details: {:?})",
                        e,
                        e
                    ))
                }
            }
        }
        Err(e) => {
            // Failed to submit transaction
            Err(anyhow::anyhow!(
                "Failed to submit transaction: {} (Details: {:?})",
                e,
                e
            ))
        }
    }
}

async fn interactive_mode() -> Result<AirdropUtils, Error> {
    let network_options: Vec<&str> = vec!["mainnet", "devnet", "testnet"];
    let network_selected: Result<&str, InquireError> =
        Select::new("Select Network?", network_options).prompt();
    let network = network_selected.unwrap_or_else(|e| {
        eprintln!("Failed to select a network. Defaulting to 'mainnet'.");
        "devnet"
    });

    // Configure network URLs
    let (node_url, faucet_url) = match network {
        "devnet" => (
            std::string::String::from("https://fullnode.devnet.aptoslabs.com"),
            Some(std::string::String::from(
                "https://faucet.devnet.aptoslabs.com",
            )),
        ),
        "testnet" => (
            std::string::String::from("https://fullnode.testnet.aptoslabs.com"),
            Some(std::string::String::from(
                "https://faucet.testnet.aptoslabs.com",
            )),
        ),
        "mainnet" => (
            std::string::String::from("https://fullnode.mainnet.aptoslabs.com"),
            None,
        ),
        _ => return Err(anyhow::anyhow!("Invalid network: {}", network)),
    };

    // Create REST clients
    let rest_client = Client::new(Url::from_str(node_url.as_str())?);
    let coin_client = CoinClient::new(&rest_client);

    let private_key_input = Text::new("Enter Private key?").prompt();
    let private_key_result: Result<(LocalAccount, u64), anyhow::Error> = match private_key_input {
        Ok(private_key) => {
            let sender = create_account_from_private_key(&private_key)?;
            println!("Sender address: {}", sender.address().to_hex_literal());
            let balance = coin_client.get_account_balance(&sender.address()).await?;
            println!("Sender balance: {} APT", balance as f64 / 100_000_000.0);

            Ok((sender, balance))
        }
        Err(e) => Err(anyhow::anyhow!(
            "Invalid private or account not found on chain {:?}",
            e
        )),
    };
    let (sender, balance) = private_key_result?;

    let csv_path = Text::new("Enter path for recipient file?").prompt();
    let csv_path_result: Result<Vec<Recipient>, anyhow::Error> = match csv_path {
        Ok(path) => Ok(parse_csv_file(&PathBuf::from(path))?),
        Err(e) => Err(anyhow::anyhow!(
            "Invalid private or account not found on chain {:?}",
            e
        )),
    };
    let recipients = csv_path_result?;
    println!("Loaded {} recipients from CSV file", recipients.len());

    Ok(AirdropUtils {
        node_url,
        faucet_url,
        sender,
        balance,
        recipients,
    })
}

async fn argument_mode(args: &Args) -> Result<AirdropUtils, Error> {
    let (node_url, faucet_url) = match args.network.to_lowercase().as_str() {
        "devnet" => (
            std::string::String::from("https://fullnode.devnet.aptoslabs.com"),
            Some(std::string::String::from(
                "https://faucet.devnet.aptoslabs.com",
            )),
        ),
        "testnet" => (
            std::string::String::from("https://fullnode.testnet.aptoslabs.com"),
            Some(std::string::String::from(
                "https://faucet.testnet.aptoslabs.com",
            )),
        ),
        "mainnet" => (
            std::string::String::from("https://fullnode.mainnet.aptoslabs.com"),
            None,
        ),
        _ => return Err(anyhow::anyhow!("Invalid network: {}", args.network)),
    };

    // Create REST clients
    let rest_client = Client::new(Url::from_str(node_url.as_str())?);
    let coin_client = CoinClient::new(&rest_client);

    // Create sender account from private key
    let sender = create_account_from_private_key(
        args.clone()
            .private_key
            .clone()
            .expect("private key is expected in non interactive mode")
            .as_str(),
    )?;
    println!("Sender address: {}", sender.address().to_hex_literal());

    // Check sender balance
    let balance = coin_client.get_account_balance(&sender.address()).await?;
    println!("Sender balance: {} APT", balance as f64 / 100_000_000.0);

    // Parse CSV file
    let recipients = parse_csv_file(&PathBuf::from(
        args.clone()
            .csv_file
            .clone()
            .expect("csv file path is expected in non interactive mode")

    ))?;
    println!("Loaded {} recipients from CSV file", recipients.len());

    Ok(AirdropUtils {
        node_url,
        faucet_url,
        sender,
        balance,
        recipients,
    })
}
