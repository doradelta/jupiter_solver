use base64::Engine;
use regex::Regex;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_transaction_status::{
    EncodedConfirmedTransactionWithStatusMeta, EncodedTransaction, TransactionBinaryEncoding,
};
use solver::jupyter_swap;
use std::str::FromStr;

#[derive(Debug, Serialize, Deserialize)]
struct Param {
    commitment: String,
    max_sup_ver: u16,
}

#[derive(Debug, Serialize, Deserialize)]
struct Payload {
    jsonrpc: String,
    id: u64,
    method: String,
    params: (String, Param),
}

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    jsonrpc: String,
    id: u64,
    result: EncodedConfirmedTransactionWithStatusMeta,
}

#[tokio::main]
async fn main() {
    let rpc_url = "https://api.mainnet-beta.solana.com";
    let program_id = "CM7x9QG6ABALVcLxGNVdUpB9X6P6ZNL92VvmzBH1WPt6";
    let client = RpcClient::new_with_commitment(rpc_url.to_string(), CommitmentConfig::confirmed());
    let mut last_tx =
        EncodedTransaction::Binary(String::default(), TransactionBinaryEncoding::Base64);

    println!("running solver...");
    let mut skip = true;

    loop {
        let (transactions, _) =
            get_previous_transactions(&client, Pubkey::from_str(program_id).unwrap(), None).await;

        for tx in transactions.iter().take(10) {
            let data = format!("{:?}", tx.result.transaction.transaction);
            let start_index = data.find("account_keys: [").unwrap();
            let start = start_index + "account_keys: [".len();
            let end = data[start..].find(',').unwrap();
            let account_key = data[start..start + end].to_string();
            let account_key: String = account_key
                .chars()
                .filter(|c| ![' ', '"'].contains(c))
                .collect();
            let curr_tx = tx.result.transaction.transaction.clone();

            if account_key == "Ao2wBFe6VzG5B1kQKkNw4grnPRQZNpP4wwQW86vXGxpY" && last_tx != curr_tx {
                last_tx = curr_tx;

                let program_data: Option<Vec<String>> = tx
                    .result
                    .transaction
                    .meta
                    .clone()
                    .unwrap()
                    .log_messages
                    .into();

                let pre_token_balance = format!(
                    "{:?}",
                    tx.result
                        .transaction
                        .meta
                        .clone()
                        .unwrap()
                        .pre_token_balances
                );
                let post_token_balance = format!(
                    "{:?}",
                    tx.result
                        .transaction
                        .meta
                        .clone()
                        .unwrap()
                        .post_token_balances
                );

                let (amount_pre, _) = extract_fields(pre_token_balance.as_str()).unwrap();
                let (amount_post, token_in) = extract_fields(post_token_balance.as_str()).unwrap();

                let events = vec![format!("{}", program_data.unwrap().get(26).unwrap())];

                let (eves, _height) = get_events_from_logs(events);
                let input = format!("{:?}", eves.get(0).unwrap());

                let cleaned_input: String =
                    input.chars().filter(|&c| c != '\\' && c != '"').collect();
                let memo = extract_memo_value(&cleaned_input).unwrap();
                let memo = memo[1..89].to_string();
                let parts: Vec<&str> = memo.split(',').collect();

                // Input data Jupiter
                let user_account = parts[1];
                let token_in = token_in;
                let token_out = parts[0];
                let amount_in =
                    amount_post.parse::<u128>().unwrap() - amount_pre.parse::<u128>().unwrap();
                let slippage_bps = 500; // 5%

                let intent = format!(
                    r#"{{"user_account": "{}","token_in": "{}","token_out": "{}","amount_in": {},"slippage_bps": {}}}"#,
                    user_account, token_in, token_out, amount_in, slippage_bps
                );

                if !skip {
                    println!("{intent}");
                    let result = jupyter_swap(intent.as_str(), &client).await;
                    println!("{:#?}", result);
                }
                skip = false;
            }
        }

        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}

pub fn get_events_from_logs(logs: Vec<String>) -> (Vec<solana_ibc::events::Event<'static>>, u64) {
    let serialized_events: Vec<&str> = logs
        .iter()
        .filter_map(|log| {
            if log.starts_with("Program data: ") {
                Some(log.strip_prefix("Program data: ").unwrap())
            } else {
                None
            }
        })
        .collect();
    let height_str = logs
        .iter()
        .find_map(|log| {
            if log.starts_with("Program log: Current Block height ") {
                Some(
                    log.strip_prefix("Program log: Current Block height ")
                        .unwrap(),
                )
            } else {
                None
            }
        })
        .map_or("0", |height| height);
    let height = height_str.parse::<u64>().unwrap();
    let events: Vec<solana_ibc::events::Event> = serialized_events
        .iter()
        .map(|event| {
            let decoded_event = base64::prelude::BASE64_STANDARD.decode(event).unwrap();
            let decoded_event: solana_ibc::events::Event =
                borsh::BorshDeserialize::try_from_slice(&decoded_event).unwrap();
            decoded_event
        })
        .collect();
    (events, height)
}

async fn get_previous_transactions(
    rpc: &RpcClient,
    program_id: Pubkey,
    before_hash: Option<Signature>,
) -> (Vec<Response>, String) {
    let transaction_signatures = rpc
        .get_signatures_for_address_with_config(
            &program_id,
            GetConfirmedSignaturesForAddress2Config {
                limit: Some(10),
                before: before_hash,
                commitment: Some(CommitmentConfig::confirmed()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    if transaction_signatures.is_empty() {
        return (
            vec![],
            before_hash.map_or("".to_string(), |sig| sig.to_string()),
        );
    }

    let last_searched_hash = transaction_signatures
        .last()
        .map_or("".to_string(), |sig| sig.signature.clone());

    let mut body = vec![];
    for sig in transaction_signatures {
        let signature = sig.signature.clone();
        let payload = Payload {
            jsonrpc: "2.0".to_string(),
            id: 1,
            method: "getTransaction".to_string(),
            params: (
                signature,
                Param {
                    commitment: "confirmed".to_string(),
                    max_sup_ver: 0,
                },
            ),
        };
        body.push(payload);
    }

    let url = rpc.url();
    let transactions: Vec<Response> = tokio::task::spawn_blocking(move || {
        for _ in 0..5 {
            let response = Client::new().post(&url).json(&body).send();
            if let Ok(response) = response {
                if let Ok(transactions) = response.json() {
                    return transactions;
                }
            }
            log::error!("Couldn't get transactions after 5 retries");
        }
        vec![]
    })
    .await
    .unwrap();

    (transactions, last_searched_hash)
}

fn extract_memo_value(input: &str) -> Option<String> {
    let memo_key = "key: memo, value: ";
    if let Some(start) = input.find(memo_key) {
        let start = start + memo_key.len();
        if let Some(end) = input[start..].find("}") {
            let memo_value = &input[start..start + end];
            return Some(memo_value.trim().to_string());
        }
    }
    None
}

fn extract_fields(input: &str) -> Option<(String, String)> {
    let re_amount = Regex::new(r#"amount: "(.*?)""#).unwrap();
    let re_mint = Regex::new(r#"mint: "(.*?)""#).unwrap();

    let amount = re_amount
        .captures(input)
        .and_then(|cap| cap.get(1))
        .map(|m| m.as_str().to_string());
    let mint = re_mint
        .captures(input)
        .and_then(|cap| cap.get(1))
        .map(|m| m.as_str().to_string());

    match (amount, mint) {
        (Some(amount), Some(mint)) => Some((amount, mint)),
        _ => None,
    }
}

pub async fn testing_events_final() {
    let rpc = RpcClient::new("https://api.mainnet-beta.solana.com".to_string());
    let mut last_hash = None;
    loop {
        let (events, prev) = get_previous_transactions(
            &rpc,
            Pubkey::from_str("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8").unwrap(),
            last_hash,
        )
        .await;
        if events.is_empty() {
            println!("No events found");
            break;
        }
        println!("Received events {}", events.len());
        last_hash = Some(Signature::from_str(&prev).unwrap());
    }
}

pub fn testing_events() {
    let events = vec![
	"Program data: ABQVAAAAZnVuZ2libGVfdG9rZW5fcGFja2V0BwAAAAYAAABtb2R1bGUIAAAAdHJhbnNmZXIGAAAAc2VuZGVyKwAAAHBpY2ExYzhqaGdxc2R6dzVzdTR5ZWVsOTNucnA5eG1obHBhcHlqbmNrdWQIAAAAcmVjZWl2ZXIsAAAAQ003eDlRRzZBQkFMVmNMeEdOVmRVcEI5WDZQNlpOTDkyVnZtekJIMVdQdDYFAAAAZGVub20FAAAAcHBpY2EGAAAAYW1vdW50DQAAADUwMDAwMDAwMDAwMDAEAAAAbWVtb2AAAAB7IlNvMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTEiLCJCckNqZFVqcVNMMjVESEtiSGFFNHdxMlBFRG0zVXpWRjdlWEw2VkF6VnU3beKAnX0HAAAAc3VjY2VzcwQAAAB0cnVl".to_string(),
	];
    let (eves, _height) = get_events_from_logs(events);
    eves.iter().for_each(|event| println!("{:#?}", event));
}
