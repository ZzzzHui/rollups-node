// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use crate::{FoldableError, UserData};

use eth_state_fold::{
    utils as fold_utils, FoldMiddleware, Foldable, StateFoldEnvironment,
    SyncMiddleware,
};
use eth_state_fold_types::{
    ethers::{
        contract::LogMeta,
        core::k256::elliptic_curve::rand_core::block,
        prelude::EthEvent,
        providers::Middleware,
        types::{Address, TxHash},
    },
    Block,
};

use anyhow::{ensure, Context};
use async_trait::async_trait;
use im::{HashMap, Vector};
use reqwest::header;
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct InputBoxInitialState {
    pub dapp_address: Arc<Address>,
    pub input_box_address: Arc<Address>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Input {
    pub sender: Arc<Address>,
    pub payload: Vec<u8>,
    pub block_added: Arc<Block>,
    pub dapp: Arc<Address>,
    pub tx_hash: Arc<TxHash>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DAppInputBox {
    pub inputs: Vector<Arc<Input>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InputBox {
    pub dapp_address: Arc<Address>,
    pub input_box_address: Arc<Address>,
    pub dapp_input_boxes: Arc<HashMap<Arc<Address>, Arc<DAppInputBox>>>,
}

#[async_trait]
impl Foldable for InputBox {
    type InitialState = InputBoxInitialState;
    type Error = FoldableError;
    type UserData = Mutex<UserData>;

    async fn sync<M: Middleware + 'static>(
        initial_state: &Self::InitialState,
        _block: &Block,
        env: &StateFoldEnvironment<M, Self::UserData>,
        access: Arc<SyncMiddleware<M>>,
    ) -> Result<Self, Self::Error> {
        let dapp_address = Arc::clone(&initial_state.dapp_address);
        let input_box_address = Arc::clone(&initial_state.input_box_address);

        Ok(Self {
            dapp_input_boxes: updated_inputs(
                None,
                access,
                env,
                &input_box_address,
                &dapp_address,
                None,
            )
            .await?,
            dapp_address,
            input_box_address,
        })
    }

    async fn fold<M: Middleware + 'static>(
        previous_state: &Self,
        block: &Block, // TODO: when new version of state-fold gets released, change this to Arc
        // and save on cloning.
        env: &StateFoldEnvironment<M, Self::UserData>,
        access: Arc<FoldMiddleware<M>>,
    ) -> Result<Self, Self::Error> {
        let dapp_address = Arc::clone(&previous_state.dapp_address);
        let input_box_address = Arc::clone(&previous_state.input_box_address);

        if !fold_utils::contains_address(&block.logs_bloom, &input_box_address)
            || !fold_utils::contains_topic(&block.logs_bloom, &*dapp_address)
            || !fold_utils::contains_topic(
                &block.logs_bloom,
                &contracts::input_box::InputAddedFilter::signature(),
            )
        {
            return Ok(previous_state.clone());
        }

        Ok(Self {
            dapp_input_boxes: updated_inputs(
                Some(&previous_state.dapp_input_boxes),
                access,
                env,
                &input_box_address,
                &dapp_address,
                None,
            )
            .await?,
            dapp_address,
            input_box_address,
        })
    }
}

async fn updated_inputs<M1: Middleware + 'static, M2: Middleware + 'static>(
    previous_input_boxes: Option<&HashMap<Arc<Address>, Arc<DAppInputBox>>>,
    provider: Arc<M1>,
    env: &StateFoldEnvironment<M2, <InputBox as Foldable>::UserData>,
    contract_address: &Address,
    dapp_address: &Address,
    block_opt: Option<Block>, // TODO: Option<Arc<Block>>,
) -> Result<Arc<HashMap<Arc<Address>, Arc<DAppInputBox>>>, FoldableError> {
    let mut input_boxes =
        previous_input_boxes.cloned().unwrap_or(HashMap::new());

    let new_inputs = fetch_all_new_inputs(
        provider,
        env,
        contract_address,
        dapp_address,
        block_opt,
    )
    .await?;

    for input in new_inputs {
        let dapp = input.dapp.clone();
        let input = Arc::new(input);

        input_boxes
            .entry(dapp)
            .and_modify(|i| {
                let mut new_input_box = (**i).clone();
                new_input_box.inputs.push_back(input.clone());
                *i = Arc::new(new_input_box);
            })
            .or_insert_with(|| {
                Arc::new(DAppInputBox {
                    inputs: im::vector![input],
                })
            });
    }

    Ok(Arc::new(input_boxes))
}

async fn fetch_all_new_inputs<
    M1: Middleware + 'static,
    M2: Middleware + 'static,
>(
    provider: Arc<M1>,
    env: &StateFoldEnvironment<M2, <InputBox as Foldable>::UserData>,
    contract_address: &Address,
    dapp_address: &Address,
    block_opt: Option<Block>, // TODO: Option<Arc<Block>>,
) -> Result<Vec<Input>, FoldableError> {
    use contracts::input_box::*;
    let contract = InputBox::new(*contract_address, Arc::clone(&provider));

    // Retrieve `InputAdded` events
    let input_events = contract
        .input_added_filter()
        .topic1(*dapp_address)
        .query_with_meta()
        .await
        .context("Error querying for input added events")?;

    let mut inputs = Vec::with_capacity(input_events.len());
    for (event, meta) in input_events {
        inputs.push(Input::build_input(env, event, meta, &block_opt).await?);
    }

    Ok(inputs)
}

impl Input {
    async fn build_input<M: Middleware + 'static>(
        env: &StateFoldEnvironment<M, <InputBox as Foldable>::UserData>,
        event: contracts::input_box::InputAddedFilter,
        meta: LogMeta,
        block_opt: &Option<Block>, // TODO: &Option<Arc<Block>>
    ) -> Result<Self, FoldableError> {
        let block =
            match block_opt {
                Some(ref b) => Arc::new(b.clone()), // TODO: remove Arc::new

                None => env.block_with_hash(&meta.block_hash).await.context(
                    format!("Could not query block `{:?}`", meta.block_hash),
                )?,
            };

        meta_consistent_with_block(&meta, &block)?;

        let mut user_data = env
            .user_data()
            .lock()
            .expect("Mutex should never be poisoned");

        let sender = user_data.get(event.sender);
        let dapp = user_data.get(event.dapp);

        // from blob hash, obtain blob data
        let block_id = block.number.as_u64();
        let blob_payload = extract_blob_sidecar(block_id).unwrap();

        Ok(Self {
            sender,
            payload: blob_payload,
            dapp,
            block_added: block,
            tx_hash: Arc::new(meta.transaction_hash),
        })
    }
}

// TODO: check for the sidecar that matches the blobhash onchain
// Currently it gets the first blob from the block, in which the state server detect an event
fn extract_blob_sidecar(
    block_id: u64,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    println!("extracting....");

    // beacon chain server url
    // TODO: this url is better passed from `build/compose-local.yaml` file
    let url = "http://172.23.0.100:3500"; 

    let mut headers = header::HeaderMap::new();
    headers.insert("accept", "application/json".parse().unwrap());

    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let res = client
        .get(
            url.to_owned()
                + "/eth/v1/beacon/blob_sidecars/"
                + block_id.to_string().as_str(),
        )
        .headers(headers)
        .send()?
        .text()?;

    let res_json: Value = serde_json::from_str(res.as_str())?;
    let blob = &res_json["data"][0]["blob"].to_string(); // hex string with '0x'
    println!("blob extracted: {}", blob);

    // remove 0x and remove quotes at the beginning and end
    let blob_no_0x = &blob[3..(&blob.len()-1)];
    let blob_vec = hex::decode(blob_no_0x).unwrap();

    Ok(blob_vec)
}

fn meta_consistent_with_block(
    meta: &LogMeta,
    block: &Block,
) -> Result<(), anyhow::Error> {
    ensure!(
        meta.block_hash == block.hash,
        "Sanity check failed: meta and block `block_hash` do not match"
    );

    ensure!(
        meta.block_number == block.number,
        "Sanity check failed: meta and block `block_number` do not match"
    );

    Ok(())
}
