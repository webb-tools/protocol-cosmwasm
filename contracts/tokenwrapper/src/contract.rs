#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    attr, coins, from_binary, to_binary, Addr, BankMsg, Binary, CosmosMsg, Decimal, Deps, DepsMut,
    Env, Fraction, MessageInfo, Response, StdError, StdResult, Uint128, WasmMsg,
};
use cw2::set_contract_version;

use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg};
use cw20_base::allowances::{
    execute_burn_from, execute_decrease_allowance, execute_increase_allowance, execute_send_from,
    execute_transfer_from, query_allowance,
};
use cw20_base::contract::{
    execute_burn, execute_mint, execute_send, execute_transfer, query_balance, query_token_info,
};

use cw20_base::state::{MinterData, TokenInfo, TOKEN_INFO};

use protocol_cosmwasm::error::ContractError;
use protocol_cosmwasm::token_wrapper::{
    ConfigResponse, Cw20HookMsg, ExecuteMsg, FeeFromAmountResponse, GetAmountToWrapResponse,
    InstantiateMsg, QueryMsg,
};

use crate::state::{Config, Supply, CONFIG, HISTORICAL_TOKENS, TOKENS, TOTAL_SUPPLY};
use crate::utils::{
    get_amount_to_wrap, get_fee_from_amount, is_valid_address, is_valid_unwrap_amount,
    is_valid_wrap_amount,
};

// version info for migration info
const CONTRACT_NAME: &str = "crates.io:cosmwasm-tokenwrapper";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    // store token info using cw20-base format
    let data = TokenInfo {
        name: msg.name,
        symbol: msg.symbol,
        decimals: msg.decimals,
        total_supply: Uint128::zero(),
        // set self as minter, so we can properly execute mint and burn
        mint: Some(MinterData {
            minter: env.contract.address,
            cap: None,
        }),
    };
    TOKEN_INFO.save(deps.storage, &data)?;

    // set supply to 0
    let supply = Supply::default();
    TOTAL_SUPPLY.save(deps.storage, &supply)?;

    // set config
    let governer = match msg.governer {
        Some(v) => deps.api.addr_validate(v.as_str())?,
        None => info.sender,
    };
    let fee_recipient = deps.api.addr_validate(msg.fee_recipient.as_str())?;
    let fee_perc = match msg.fee_percentage.parse::<u64>() {
        Ok(v) => {
            if v > 100 {
                return Err(ContractError::Std(StdError::GenericErr {
                    msg: "Percentage should be in range [0, 100]".to_string(),
                }));
            } else {
                v
            }
        }
        Err(e) => {
            return Err(ContractError::Std(StdError::GenericErr {
                msg: e.to_string(),
            }))
        }
    };
    let fee_percentage = Decimal::percent(fee_perc);
    let wrapping_limit = match msg.wrapping_limit.parse::<u128>() {
        Ok(v) => Uint128::from(v),
        Err(e) => {
            return Err(ContractError::Std(StdError::GenericErr {
                msg: e.to_string(),
            }))
        }
    };
    CONFIG.save(
        deps.storage,
        &Config {
            governer,
            fee_recipient,
            fee_percentage,
            native_token_denom: msg.native_token_denom,
            is_native_allowed: msg.is_native_allowed != 0,
            wrapping_limit,
            proposal_nonce: 0_u64,
        },
    )?;

    Ok(Response::default())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        // Used to wrap native tokens on behalf of a sender.
        ExecuteMsg::Wrap {} => wrap_native(deps, env, info),

        // Used to unwrap native/cw20 tokens on behalf of a sender.
        ExecuteMsg::Unwrap { token, amount } => match token {
            // Unwrap the cw20 tokens.
            Some(token) => unwrap_cw20(deps, env, info, token, amount),
            // Unwrap the native token.
            None => unwrap_native(deps, env, info, amount),
        },

        // Used to wrap cw20 tokens on behalf of a sender.
        ExecuteMsg::Receive(msg) => wrap_cw20(deps, env, info, msg),

        // // Governing functionality
        // Sets a new governer. Only the governer can execute this entry.
        ExecuteMsg::SetGoverner { new_governer } => set_governer(deps, env, info, new_governer),

        // Sets the "is_native_allowed" of config
        ExecuteMsg::SetNativeAllowed { is_native_allowed } => {
            set_native_allowed(deps, info, is_native_allowed)
        }

        // Updates the "wrapping_limit"
        ExecuteMsg::UpdateLimit { new_limit } => update_wrapping_limit(deps, info, new_limit),

        // Sets a new "fee_percentage"
        ExecuteMsg::SetFee { new_fee_perc } => update_fee_perc(deps, info, new_fee_perc),

        // Sets a new "fee_recipient"
        ExecuteMsg::SetFeeRecipient { new_recipient } => {
            update_fee_recipient(deps, info, new_recipient)
        }

        // Add a cw20 token address to wrapping list
        ExecuteMsg::AddCw20TokenAddr { token, nonce } => add_token_addr(deps, info, token, nonce),

        // Removes a cw20 token address from wrapping list (disallow wrapping)
        ExecuteMsg::RemoveCw20TokenAddr { token, nonce } => {
            remove_token_addr(deps, info, token, nonce)
        }

        // these all come from cw20-base to implement the cw20 standard
        ExecuteMsg::Transfer { recipient, amount } => {
            Ok(execute_transfer(deps, env, info, recipient, amount)?)
        }
        ExecuteMsg::Burn { amount } => Ok(execute_burn(deps, env, info, amount)?),
        ExecuteMsg::Send {
            contract,
            amount,
            msg,
        } => Ok(execute_send(deps, env, info, contract, amount, msg)?),
        ExecuteMsg::IncreaseAllowance {
            spender,
            amount,
            expires,
        } => Ok(execute_increase_allowance(
            deps, env, info, spender, amount, expires,
        )?),
        ExecuteMsg::DecreaseAllowance {
            spender,
            amount,
            expires,
        } => Ok(execute_decrease_allowance(
            deps, env, info, spender, amount, expires,
        )?),
        ExecuteMsg::TransferFrom {
            owner,
            recipient,
            amount,
        } => Ok(execute_transfer_from(
            deps, env, info, owner, recipient, amount,
        )?),
        ExecuteMsg::BurnFrom { owner, amount } => {
            Ok(execute_burn_from(deps, env, info, owner, amount)?)
        }
        ExecuteMsg::SendFrom {
            owner,
            contract,
            amount,
            msg,
        } => Ok(execute_send_from(
            deps, env, info, owner, contract, amount, msg,
        )?),
    }
}

fn wrap_native(mut deps: DepsMut, env: Env, info: MessageInfo) -> Result<Response, ContractError> {
    // Check if the wrapping native token is allowed
    let config = CONFIG.load(deps.storage)?;
    if !config.is_native_allowed {
        return Err(ContractError::Std(StdError::GenericErr {
            msg: "Wrapping native token is not allowed in this token wrapper".to_string(),
        }));
    }

    // Validate the wrapping amount
    let sent_native_token = info
        .funds
        .iter()
        .find(|token| token.denom == *config.native_token_denom)
        .ok_or(ContractError::InsufficientFunds {})?;
    let wrapping_amount = sent_native_token.amount;
    if wrapping_amount.is_zero() || !is_valid_wrap_amount(deps.branch(), wrapping_amount) {
        return Err(ContractError::Std(StdError::GenericErr {
            msg: "Invalid native token amount".to_string(),
        }));
    }

    // Calculate the "fee" & "amount_to_wrap".
    let cost_to_wrap = get_fee_from_amount(wrapping_amount, config.fee_percentage.numerator());
    let left_over = wrapping_amount - cost_to_wrap;

    // Save the wrapped token amount.
    let mut supply = TOTAL_SUPPLY.load(deps.storage)?;
    supply.issued += left_over;
    TOTAL_SUPPLY.save(deps.storage, &supply)?;

    // call into cw20-base to mint the token, call as self as no one else is allowed
    let sub_info = MessageInfo {
        sender: env.contract.address.clone(),
        funds: vec![],
    };
    execute_mint(deps, env, sub_info, info.sender.to_string(), left_over)?;

    // send "fee" to fee_recipient
    let msgs: Vec<CosmosMsg> = vec![CosmosMsg::Bank(BankMsg::Send {
        to_address: config.fee_recipient.to_string(),
        amount: coins(cost_to_wrap.u128(), config.native_token_denom),
    })];

    Ok(Response::new().add_messages(msgs).add_attributes(vec![
        attr("action", "wrap_native"),
        attr("from", info.sender),
        attr("minted", left_over),
        attr("fee", cost_to_wrap),
    ]))
}

fn unwrap_native(
    mut deps: DepsMut,
    env: Env,
    info: MessageInfo,
    amount: Uint128,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    // Validate the "is_native_allowed"
    if !config.is_native_allowed {
        return Err(ContractError::Std(StdError::GenericErr {
            msg: "Unwrapping native token is not allowed in this token wrapper".to_string(),
        }));
    }

    // Validate the "amount"
    if !is_valid_unwrap_amount(deps.branch(), info.clone(), amount) {
        return Err(ContractError::Std(StdError::GenericErr {
            msg: "Insufficient native token balance".to_string(),
        }));
    }

    // Calculate the remainder
    let total_supply = TOTAL_SUPPLY.load(deps.storage)?;
    let remainder = total_supply.issued - amount;

    // burn from the original caller
    execute_burn(deps.branch(), env, info.clone(), amount)?;

    // Save the "total_supply"
    TOTAL_SUPPLY.save(deps.storage, &Supply { issued: remainder })?;

    // Refund the native token.
    let msgs: Vec<CosmosMsg> = vec![CosmosMsg::Bank(BankMsg::Send {
        to_address: info.sender.to_string(),
        amount: coins(amount.u128(), config.native_token_denom),
    })];

    Ok(Response::new().add_messages(msgs).add_attributes(vec![
        attr("action", "unwrap_native"),
        attr("from", info.sender),
        attr("unwrap", amount),
        attr("refund", amount),
    ]))
}

fn unwrap_cw20(
    mut deps: DepsMut,
    env: Env,
    info: MessageInfo,
    token: Addr,
    amount: Uint128,
) -> Result<Response, ContractError> {
    // Validate the "token" address
    if !is_valid_address(deps.branch(), token.clone()) {
        return Err(ContractError::Std(StdError::GenericErr {
            msg: "Invalid Cw20 token address".to_string(),
        }));
    }

    // Validate the "token" amount
    if !is_valid_unwrap_amount(deps.branch(), info.clone(), amount) {
        return Err(ContractError::Std(StdError::GenericErr {
            msg: "Insufficient cw20 token amount".to_string(),
        }));
    }

    // Calculate the remainder
    let total_supply = TOTAL_SUPPLY.load(deps.storage)?;
    let remainder = total_supply.issued - amount;

    // burn from the original caller
    execute_burn(deps.branch(), env, info.clone(), amount)?;

    // Save the "total_supply"
    TOTAL_SUPPLY.save(deps.storage, &Supply { issued: remainder })?;

    // Refund the Cw20 token
    let msgs: Vec<CosmosMsg> = vec![CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: token.to_string(),
        funds: vec![],
        msg: to_binary(&Cw20ExecuteMsg::Transfer {
            recipient: info.sender.to_string(),
            amount,
        })?,
    })];

    Ok(Response::new().add_messages(msgs).add_attributes(vec![
        attr("action", "unwrap_cw20"),
        attr("from", info.sender),
        attr("unwrap", amount),
        attr("refund", amount),
    ]))
}

fn wrap_cw20(
    mut deps: DepsMut,
    env: Env,
    info: MessageInfo,
    cw20_msg: Cw20ReceiveMsg,
) -> Result<Response, ContractError> {
    let sender = cw20_msg.sender;
    let cw20_address = info.sender;

    // Validate the cw20 address
    if !is_valid_address(deps.branch(), cw20_address.clone()) {
        return Err(ContractError::Std(StdError::GenericErr {
            msg: "Invalid Cw20 token address".to_string(),
        }));
    }

    // Validate the cw20 token amount.
    let cw20_token_amount = cw20_msg.amount;
    if cw20_token_amount.is_zero() || !is_valid_wrap_amount(deps.branch(), cw20_token_amount) {
        return Err(ContractError::Std(StdError::GenericErr {
            msg: "Invalid cw20 token".to_string(),
        }));
    }

    // Calculate the "fee" & "amount_to_wrap".
    let config = CONFIG.load(deps.storage)?;
    let cost_to_wrap = get_fee_from_amount(cw20_msg.amount, config.fee_percentage.numerator());
    let left_over = cw20_msg.amount - cost_to_wrap;

    match from_binary(&cw20_msg.msg) {
        Ok(Cw20HookMsg::Wrap {}) => {
            // Save the wrapping number
            let mut supply = TOTAL_SUPPLY.load(deps.storage)?;
            supply.issued += left_over;
            TOTAL_SUPPLY.save(deps.storage, &supply)?;

            // call into cw20-base to mint the token, call as self as no one else is allowed
            let sub_info = MessageInfo {
                sender: env.contract.address.clone(),
                funds: vec![],
            };
            execute_mint(deps, env, sub_info, sender.to_string(), left_over)?;

            // Send the fee to fee_recipient.
            let msgs: Vec<CosmosMsg> = vec![CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: cw20_address.to_string(),
                funds: vec![],
                msg: to_binary(&Cw20ExecuteMsg::Transfer {
                    recipient: config.fee_recipient.to_string(),
                    amount: cost_to_wrap,
                })?,
            })];

            Ok(Response::new().add_messages(msgs).add_attributes(vec![
                attr("action", "wrap_cw20"),
                attr("from", sender),
                attr("minted", left_over),
                attr("fee", cost_to_wrap),
            ]))
        }
        Err(e) => Err(ContractError::Std(e)),
    }
}

fn set_governer(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    new_governer: String,
) -> Result<Response, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;
    // Validate the tx sender.
    if config.governer != deps.api.addr_validate(info.sender.as_str())? {
        return Err(ContractError::Unauthorized {});
    }

    // Validate & save the new governer.
    config.governer = deps.api.addr_validate(new_governer.as_str())?;
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new().add_attributes(vec![
        attr("method", "set_governer"),
        attr("new_governer", new_governer),
    ]))
}

fn set_native_allowed(
    deps: DepsMut,
    info: MessageInfo,
    is_native_allowed: u32,
) -> Result<Response, ContractError> {
    let is_native_allowed = is_native_allowed != 0;

    // Validate the tx sender.
    let mut config = CONFIG.load(deps.storage)?;
    if config.governer != deps.api.addr_validate(info.sender.as_str())? {
        return Err(ContractError::Unauthorized {});
    }

    // Save "is_native_allowed" state
    config.is_native_allowed = is_native_allowed;
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new().add_attributes(vec![
        attr("method", "set_native_allowed"),
        attr("is_native_allowed", is_native_allowed.to_string()),
    ]))
}

fn update_wrapping_limit(
    deps: DepsMut,
    info: MessageInfo,
    new_limit: String,
) -> Result<Response, ContractError> {
    let new_wrapping_limit = match new_limit.parse::<u128>() {
        Ok(v) => Uint128::from(v),
        Err(e) => {
            return Err(ContractError::Std(StdError::GenericErr {
                msg: e.to_string(),
            }))
        }
    };

    // Validate the tx sender.
    let mut config = CONFIG.load(deps.storage)?;
    if config.governer != deps.api.addr_validate(info.sender.as_str())? {
        return Err(ContractError::Unauthorized {});
    }

    // Save a new "wrapping_limit" state
    config.wrapping_limit = new_wrapping_limit;
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new().add_attributes(vec![
        attr("method", "set_wrapping_limit"),
        attr("new_limit", new_wrapping_limit.to_string()),
    ]))
}

fn update_fee_perc(
    deps: DepsMut,
    info: MessageInfo,
    new_fee_perc: String,
) -> Result<Response, ContractError> {
    let new_fee_perc = match new_fee_perc.parse::<u64>() {
        Ok(v) => {
            if v > 100 {
                return Err(ContractError::Std(StdError::GenericErr {
                    msg: "Percentage should be in range [0, 100]".to_string(),
                }));
            } else {
                v
            }
        }
        Err(e) => {
            return Err(ContractError::Std(StdError::GenericErr {
                msg: e.to_string(),
            }))
        }
    };

    // Validate the tx sender.
    let mut config = CONFIG.load(deps.storage)?;
    if config.governer != deps.api.addr_validate(info.sender.as_str())? {
        return Err(ContractError::Unauthorized {});
    }

    // Save a new "new_fee_perc" state
    config.fee_percentage = Decimal::percent(new_fee_perc);
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new().add_attributes(vec![
        attr("method", "set_fee"),
        attr("new_fee_perc", config.fee_percentage.to_string()),
    ]))
}

fn update_fee_recipient(
    deps: DepsMut,
    info: MessageInfo,
    new_recipient: String,
) -> Result<Response, ContractError> {
    let new_recipient = deps.api.addr_validate(new_recipient.as_str())?;

    // Validate the tx sender.
    let mut config = CONFIG.load(deps.storage)?;
    if config.governer != deps.api.addr_validate(info.sender.as_str())? {
        return Err(ContractError::Unauthorized {});
    }

    // Save a new "fee_recipient" state
    config.fee_recipient = new_recipient;
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new().add_attributes(vec![
        attr("method", "set_fee_recipient"),
        attr("new_recipient", config.fee_recipient.to_string()),
    ]))
}

fn add_token_addr(
    deps: DepsMut,
    info: MessageInfo,
    token: String,
    nonce: u64,
) -> Result<Response, ContractError> {
    let token_addr = deps.api.addr_validate(token.as_str())?;
    if TOKENS.has(deps.storage, token_addr.clone()) {
        return Err(ContractError::Std(StdError::GenericErr {
            msg: "Token must not be valid".to_string(),
        }));
    }

    // Validate the tx sender.
    let mut config = CONFIG.load(deps.storage)?;
    if config.governer != deps.api.addr_validate(info.sender.as_str())? {
        return Err(ContractError::Unauthorized {});
    }

    // Validate the "nonce" value
    if nonce != config.proposal_nonce + 1 {
        return Err(ContractError::Std(StdError::GenericErr {
            msg: "Nonce must increment by 1".to_string(),
        }));
    }

    // Add the "token" to wrapping list
    TOKENS.save(deps.storage, token_addr.clone(), &true)?;

    HISTORICAL_TOKENS.save(deps.storage, token_addr.clone(), &true)?;

    // Save the "proposal_nonce"
    config.proposal_nonce = nonce;
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new().add_attributes(vec![
        attr("method", "add_token"),
        attr("token", token_addr.to_string()),
    ]))
}

fn remove_token_addr(
    deps: DepsMut,
    info: MessageInfo,
    token: String,
    nonce: u64,
) -> Result<Response, ContractError> {
    let token_addr = deps.api.addr_validate(token.as_str())?;
    let is_token_already_invalid = match TOKENS.load(deps.storage, token_addr.clone()) {
        Ok(v) => !v,
        Err(_) => return Err(ContractError::NotInitialized),
    };
    if is_token_already_invalid {
        return Err(ContractError::Std(StdError::GenericErr {
            msg: "Token must be valid".to_string(),
        }));
    }

    // Validate the tx sender.
    let mut config = CONFIG.load(deps.storage)?;
    if config.governer != deps.api.addr_validate(info.sender.as_str())? {
        return Err(ContractError::Unauthorized {});
    }

    // Validate the "nonce" value
    if nonce != config.proposal_nonce + 1 {
        return Err(ContractError::Std(StdError::GenericErr {
            msg: "Nonce must increment by 1".to_string(),
        }));
    }

    // Remove the "token" from wrapping list
    TOKENS.save(deps.storage, token_addr.clone(), &false)?;

    // Save the "proposal_nonce"
    config.proposal_nonce = nonce;
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new().add_attributes(vec![
        attr("method", "remove_token"),
        attr("token", token_addr.to_string()),
    ]))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        // Custom queries.
        QueryMsg::Config {} => to_binary(&query_config(deps)?),
        QueryMsg::FeeFromAmount { amount_to_wrap } => {
            to_binary(&query_fee_from_amount(deps, amount_to_wrap)?)
        }
        QueryMsg::GetAmountToWrap { target_amount } => {
            to_binary(&query_amount_to_wrap(deps, target_amount)?)
        }

        // inherited from cw20-base
        QueryMsg::TokenInfo {} => to_binary(&query_token_info(deps)?),
        QueryMsg::Balance { address } => to_binary(&query_balance(deps, address)?),
        QueryMsg::Allowance { owner, spender } => {
            to_binary(&query_allowance(deps, owner, spender)?)
        }
    }
}

fn query_config(deps: Deps) -> StdResult<ConfigResponse> {
    println!("querying");
    let config = CONFIG.load(deps.storage)?;
    Ok(ConfigResponse {
        governer: config.governer.to_string(),
        native_token_denom: config.native_token_denom,
        fee_recipient: config.fee_recipient.to_string(),
        fee_percentage: config.fee_percentage.to_string(),
        is_native_allowed: config.is_native_allowed.to_string(),
        wrapping_limit: config.wrapping_limit.to_string(),
        proposal_nonce: config.proposal_nonce.to_string(),
    })
}

fn query_fee_from_amount(deps: Deps, amount_to_wrap: String) -> StdResult<FeeFromAmountResponse> {
    let config = CONFIG.load(deps.storage)?;
    let amount_to_wrap = match amount_to_wrap.parse::<u128>() {
        Ok(v) => Uint128::from(v),
        Err(e) => return Err(StdError::GenericErr { msg: e.to_string() }),
    };
    let fee_perc = config.fee_percentage.numerator();
    let fee_amt = get_fee_from_amount(amount_to_wrap, fee_perc);
    Ok(FeeFromAmountResponse {
        amount_to_wrap: amount_to_wrap.to_string(),
        fee_amt: fee_amt.to_string(),
    })
}

fn query_amount_to_wrap(deps: Deps, target_amount: String) -> StdResult<GetAmountToWrapResponse> {
    let config = CONFIG.load(deps.storage)?;
    let target_amount = match target_amount.parse::<u128>() {
        Ok(v) => Uint128::from(v),
        Err(e) => return Err(StdError::GenericErr { msg: e.to_string() }),
    };
    let fee_perc = config.fee_percentage.numerator();
    let amount_to_wrap = get_amount_to_wrap(target_amount, fee_perc);
    Ok(GetAmountToWrapResponse {
        target_amount: target_amount.to_string(),
        amount_to_wrap: amount_to_wrap.to_string(),
    })
}
