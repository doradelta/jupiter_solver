use std::{env, fmt, str::FromStr};

use solana_sdk::transaction::VersionedTransaction;

use {
    serde::{Deserialize, Serialize},
    solana_client::nonblocking::rpc_client::RpcClient,
    solana_sdk::{
        hash::Hash,
        instruction::Instruction,
        pubkey::{ParsePubkeyError, Pubkey},
        signature::Signer,
        transaction::Transaction,
    },
    std::collections::HashMap,
};

use serde_json::Value;
use solana_sdk::pubkey;
use solana_sdk::signer::keypair::Keypair;
use spl_associated_token_account::instruction;

mod field_as_string;
mod field_instruction;
mod field_prioritization_fee;
mod field_pubkey;

/// A `Result` alias where the `Err` case is `jup_ag::Error`.
pub type Result<T> = std::result::Result<T, Error>;

// Reference: https://quote-api.jup.ag/v4/docs/static/index.html
fn quote_api_url() -> String {
    env::var("QUOTE_API_URL").unwrap_or_else(|_| "https://quote-api.jup.ag/v6".to_string())
}

// Reference: https://quote-api.jup.ag/docs/static/index.html
fn price_api_url() -> String {
    env::var("PRICE_API_URL").unwrap_or_else(|_| "https://price.jup.ag/v1".to_string())
}

/// The Errors that may occur while using this crate
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("reqwest: {0}")]
    Reqwest(#[from] reqwest::Error),

    #[error("invalid pubkey in response data: {0}")]
    ParsePubkey(#[from] ParsePubkeyError),

    #[error("base64: {0}")]
    Base64Decode(#[from] base64::DecodeError),

    #[error("bincode: {0}")]
    Bincode(#[from] bincode::Error),

    #[error("Jupiter API: {0}")]
    JupiterApi(String),

    #[error("serde_json: {0}")]
    SerdeJson(#[from] serde_json::Error),

    #[error("parse SwapMode: Invalid value `{value}`")]
    ParseSwapMode { value: String },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Price {
    #[serde(with = "field_as_string", rename = "id")]
    pub input_mint: Pubkey,
    #[serde(rename = "mintSymbol")]
    pub input_symbol: String,
    #[serde(with = "field_as_string", rename = "vsToken")]
    pub output_mint: Pubkey,
    #[serde(rename = "vsTokenSymbol")]
    pub output_symbol: String,
    pub price: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Quote {
    #[serde(with = "field_as_string")]
    pub input_mint: Pubkey,
    #[serde(with = "field_as_string")]
    pub in_amount: u64,
    #[serde(with = "field_as_string")]
    pub output_mint: Pubkey,
    #[serde(with = "field_as_string")]
    pub out_amount: u64,
    #[serde(with = "field_as_string")]
    pub other_amount_threshold: u64,
    pub swap_mode: String,
    pub slippage_bps: u64,
    pub platform_fee: Option<PlatformFee>,
    #[serde(with = "field_as_string")]
    pub price_impact_pct: f64,
    pub route_plan: Vec<RoutePlan>,
    pub context_slot: Option<u64>,
    pub time_taken: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformFee {
    #[serde(with = "field_as_string")]
    pub amount: u64,
    pub fee_bps: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutePlan {
    pub swap_info: SwapInfo,
    pub percent: u8,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapInfo {
    #[serde(with = "field_as_string")]
    pub amm_key: Pubkey,
    pub label: Option<String>,
    #[serde(with = "field_as_string")]
    pub input_mint: Pubkey,
    #[serde(with = "field_as_string")]
    pub output_mint: Pubkey,
    #[serde(with = "field_as_string")]
    pub in_amount: u64,
    #[serde(with = "field_as_string")]
    pub out_amount: u64,
    #[serde(with = "field_as_string")]
    pub fee_amount: u64,
    #[serde(with = "field_as_string")]
    pub fee_mint: Pubkey,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeInfo {
    #[serde(with = "field_as_string")]
    pub amount: u64,
    #[serde(with = "field_as_string")]
    pub mint: Pubkey,
    pub pct: f64,
}

/// Partially signed transactions required to execute a swap
#[derive(Clone, Debug)]
pub struct Swap {
    pub swap_transaction: VersionedTransaction,
    pub last_valid_block_height: u64,
}

/// Swap instructions
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapInstructions {
    #[serde(with = "field_instruction::option_instruction")]
    pub token_ledger_instruction: Option<Instruction>,
    #[serde(with = "field_instruction::vec_instruction")]
    pub compute_budget_instructions: Vec<Instruction>,
    #[serde(with = "field_instruction::vec_instruction")]
    pub setup_instructions: Vec<Instruction>,
    #[serde(with = "field_instruction::instruction")]
    pub swap_instruction: Instruction,
    #[serde(with = "field_instruction::option_instruction")]
    pub cleanup_instruction: Option<Instruction>,
    #[serde(with = "field_pubkey::vec")]
    pub address_lookup_table_addresses: Vec<Pubkey>,
    pub prioritization_fee_lamports: u64,
}

/// Hashmap of possible swap routes from input mint to an array of output mints
pub type RouteMap = HashMap<Pubkey, Vec<Pubkey>>;

fn maybe_jupiter_api_error<T>(value: serde_json::Value) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    #[derive(Deserialize)]
    struct ErrorResponse {
        error: String,
    }
    if let Ok(ErrorResponse { error }) = serde_json::from_value::<ErrorResponse>(value.clone()) {
        Err(Error::JupiterApi(error))
    } else {
        serde_json::from_value(value).map_err(|err| err.into())
    }
}

/// Get simple price for a given input mint, output mint, and amount
pub async fn price(input_mint: Pubkey, output_mint: Pubkey, ui_amount: f64) -> Result<Price> {
    let url = format!(
        "{base_url}/price?id={input_mint}&vsToken={output_mint}&amount={ui_amount}",
        base_url = price_api_url(),
    );
    maybe_jupiter_api_error(reqwest::get(url).await?.json().await?)
}

#[derive(Serialize, Deserialize, Default, PartialEq, Clone, Debug)]
pub enum SwapMode {
    #[default]
    ExactIn,
    ExactOut,
}

impl FromStr for SwapMode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "ExactIn" => Ok(Self::ExactIn),
            "ExactOut" => Ok(Self::ExactOut),
            _ => Err(Error::ParseSwapMode { value: s.into() }),
        }
    }
}

impl fmt::Display for SwapMode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Self::ExactIn => write!(f, "ExactIn"),
            Self::ExactOut => write!(f, "ExactOut"),
        }
    }
}

#[derive(Default)]
pub struct QuoteConfig {
    pub slippage_bps: Option<u64>,
    pub swap_mode: Option<SwapMode>,
    pub dexes: Option<Vec<Pubkey>>,
    pub exclude_dexes: Option<Vec<Pubkey>>,
    pub only_direct_routes: bool,
    pub as_legacy_transaction: Option<bool>,
    pub platform_fee_bps: Option<u64>,
    pub max_accounts: Option<u64>,
}

/// Get quote for a given input mint, output mint, and amount
pub async fn quote(
    input_mint: Pubkey,
    output_mint: Pubkey,
    amount: u64,
    quote_config: QuoteConfig,
) -> Result<Quote> {
    let url = format!(
        "{base_url}/quote?inputMint={input_mint}&outputMint={output_mint}&amount={amount}&onlyDirectRoutes={}&{}{}{}{}{}{}{}",
        quote_config.only_direct_routes,
        quote_config
            .as_legacy_transaction
            .map(|as_legacy_transaction| format!("&asLegacyTransaction={as_legacy_transaction}"))
            .unwrap_or_default(),
        quote_config
            .swap_mode
            .map(|swap_mode| format!("&swapMode={swap_mode}"))
            .unwrap_or_default(),
        quote_config
            .slippage_bps
            .map(|slippage_bps| format!("&slippageBps={slippage_bps}"))
            .unwrap_or_default(),
        quote_config
            .platform_fee_bps
            .map(|platform_fee_bps| format!("&feeBps={platform_fee_bps}"))
            .unwrap_or_default(),
        quote_config
            .dexes
            .map(|dexes| format!("&dexes={:?}", dexes))
            .unwrap_or_default(),
        quote_config
            .exclude_dexes
            .map(|exclude_dexes| format!("&excludeDexes={:?}", exclude_dexes))
            .unwrap_or_default(),
        quote_config
            .max_accounts
            .map(|max_accounts| format!("&maxAccounts={max_accounts}"))
            .unwrap_or_default(),
        base_url=quote_api_url(),
    );

    maybe_jupiter_api_error(reqwest::get(url).await?.json().await?)
}

#[derive(Debug)]
pub enum PrioritizationFeeLamports {
    Auto,
    Exact { lamports: u64 },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(non_snake_case)]
pub struct SwapRequest {
    #[serde(with = "field_as_string")]
    pub user_public_key: Pubkey,
    pub wrap_and_unwrap_sol: Option<bool>,
    pub use_shared_accounts: Option<bool>,
    #[serde(with = "field_pubkey::option")]
    pub fee_account: Option<Pubkey>,
    #[deprecated = "please use SwapRequest::prioritization_fee_lamports instead"]
    pub compute_unit_price_micro_lamports: Option<u64>,
    #[serde(with = "field_prioritization_fee")]
    pub prioritization_fee_lamports: PrioritizationFeeLamports,
    pub as_legacy_transaction: Option<bool>,
    pub use_token_ledger: Option<bool>,
    #[serde(with = "field_pubkey::option")]
    pub destination_token_account: Option<Pubkey>,
    pub quote_response: Quote,
}

impl SwapRequest {
    /// Creates new SwapRequest with the given and default values
    pub fn new(
        user_public_key: Pubkey,
        quote_response: Quote,
        destination_account: Pubkey,
    ) -> Self {
        #[allow(deprecated)]
        SwapRequest {
            user_public_key,
            wrap_and_unwrap_sol: Some(true),
            use_shared_accounts: Some(true),
            fee_account: None,
            compute_unit_price_micro_lamports: None,
            prioritization_fee_lamports: PrioritizationFeeLamports::Auto,
            as_legacy_transaction: Some(false),
            use_token_ledger: Some(false),
            destination_token_account: Some(destination_account),
            quote_response,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwapResponse {
    pub swap_transaction: String,
    pub last_valid_block_height: u64,
}

/// Get swap serialized transactions for a quote
pub async fn swap(swap_request: SwapRequest) -> Result<Swap> {
    let url = format!("{}/swap", quote_api_url());

    let response = maybe_jupiter_api_error::<SwapResponse>(
        reqwest::Client::builder()
            .build()?
            .post(url)
            .header("Accept", "application/json")
            .json(&swap_request)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?,
    )?;

    fn decode(base64_transaction: String) -> Result<VersionedTransaction> {
        #[allow(deprecated)]
        bincode::deserialize(&base64::decode(base64_transaction)?).map_err(|err| err.into())
    }

    Ok(Swap {
        swap_transaction: decode(response.swap_transaction)?,
        last_valid_block_height: response.last_valid_block_height,
    })
}

/// Get swap serialized transaction instructions for a quote
pub async fn swap_instructions(swap_request: SwapRequest) -> Result<SwapInstructions> {
    let url = format!("{}/swap-instructions", quote_api_url());

    let response = reqwest::Client::builder()
        .build()?
        .post(url)
        .header("Accept", "application/json")
        .json(&swap_request)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(Error::JupiterApi(response.text().await?));
    }

    Ok(response.json::<SwapInstructions>().await?)
}

/// Returns a hash map, input mint as key and an array of valid output mint as values
pub async fn route_map() -> Result<RouteMap> {
    let url = format!(
        "{}/indexed-route-map?onlyDirectRoutes=false",
        quote_api_url()
    );

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct IndexedRouteMap {
        mint_keys: Vec<String>,
        indexed_route_map: HashMap<usize, Vec<usize>>,
    }

    let response = reqwest::get(url).await?.json::<IndexedRouteMap>().await?;

    let mint_keys = response
        .mint_keys
        .into_iter()
        .map(|x| x.parse::<Pubkey>().map_err(|err| err.into()))
        .collect::<Result<Vec<Pubkey>>>()?;

    let mut route_map = HashMap::new();
    for (from_index, to_indices) in response.indexed_route_map {
        route_map.insert(
            mint_keys[from_index],
            to_indices.into_iter().map(|i| mint_keys[i]).collect(),
        );
    }

    Ok(route_map)
}

#[derive(PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct Memo {
    pub user_account: pubkey::Pubkey,
    pub token_in: pubkey::Pubkey,
    pub token_out: pubkey::Pubkey,
    pub amount_in: u64,
    pub slippage_bps: u64,
}

impl Memo {
    pub fn from_json(json_str: &str) -> Result<Memo> {
        let parsed_json: Value = serde_json::from_str(json_str).unwrap();
        let mut memo = Memo::default();

        memo.user_account =
            Pubkey::from_str(&parsed_json["user_account"].to_string().trim_matches('"')).unwrap();
        memo.token_in =
            Pubkey::from_str(&parsed_json["token_in"].to_string().trim_matches('"')).unwrap();
        memo.token_out =
            Pubkey::from_str(&parsed_json["token_out"].to_string().trim_matches('"')).unwrap();
        memo.amount_in = parsed_json["amount_in"].as_u64().unwrap_or_default();
        memo.slippage_bps = parsed_json["slippage_bps"].as_u64().unwrap_or_default();

        Ok(memo)
    }
}

pub async fn jupyter_swap(intent: &str, rpc_client: &RpcClient) -> Result<()> {
    let memo = Memo::from_json(&intent)?;

    let keypair = Keypair::from_base58_string("");

    let only_direct_routes = false;
    let quotes = quote(
        memo.token_in,
        memo.token_out,
        memo.amount_in,
        QuoteConfig {
            only_direct_routes,
            slippage_bps: Some(memo.slippage_bps),
            ..QuoteConfig::default()
        },
    )
    .await?;

    let route = quotes.route_plan[0]
        .swap_info
        .label
        .clone()
        .unwrap_or_else(|| "Unknown DEX".to_string());
    println!(
        "Quote: token_in_amount {} for token_out_amount {} via {} (worst case with slippage: {}). Impact: {:.2}%",
        quotes.in_amount,
        quotes.out_amount,
        route,
        quotes.other_amount_threshold,
        quotes.price_impact_pct * 100.
    );

    let user_token_out = spl_associated_token_account::get_associated_token_address(
        &memo.user_account,
        &memo.token_out,
    );

    if rpc_client
        .get_token_account_balance(&user_token_out)
        .await
        .is_err()
    {
        create_token_account(memo, keypair.insecure_clone(), &rpc_client).await?;
    }

    let request: SwapRequest = SwapRequest::new(keypair.pubkey(), quotes.clone(), user_token_out);

    let Swap {
        mut swap_transaction,
        last_valid_block_height: _,
    } = swap(request).await?;

    let recent_blockhash_for_swap: Hash = rpc_client.get_latest_blockhash().await.unwrap();
    swap_transaction
        .message
        .set_recent_blockhash(recent_blockhash_for_swap);

    let swap_transaction =
        VersionedTransaction::try_new(swap_transaction.message, &[&keypair]).unwrap();

    rpc_client
        .simulate_transaction(&swap_transaction)
        .await
        .unwrap();

    rpc_client
        .send_and_confirm_transaction_with_spinner(&swap_transaction)
        .await
        .unwrap();

    Ok(())
}

pub async fn create_token_account(
    memo: Memo,
    keypair: Keypair,
    rpc_client: &RpcClient,
) -> Result<()> {
    let create_account_ix = instruction::create_associated_token_account(
        &keypair.pubkey(),
        &memo.user_account,
        &memo.token_out,
        &pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
    );

    let mut transaction =
        Transaction::new_with_payer(&[create_account_ix], Some(&keypair.pubkey()));

    let recent_blockhash: Hash = rpc_client.get_latest_blockhash().await.unwrap();
    transaction.sign(&[&keypair], recent_blockhash);

    rpc_client.simulate_transaction(&transaction).await.unwrap();

    rpc_client
        .send_and_confirm_transaction(&transaction)
        .await
        .unwrap();

    Ok(())
}
