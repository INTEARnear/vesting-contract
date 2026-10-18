use anyhow::{anyhow, bail, ensure};
use near_sdk::Timestamp;
use near_sdk::json_types::{U64, U128};
use near_sdk::serde_json::json;
use near_workspaces::{
    Account, AccountId, Contract, Worker, network::Sandbox, result::ExecutionFinalResult,
    types::NearToken,
};

const TOKEN_ID: &str = "token.intear.near";
const TOKEN_SUPPLY: u128 = 21_000_000 * 10u128.pow(24);
const TOTAL: u128 = 5_000_000 * 10u128.pow(24);

const SEC: u64 = 1_000_000_000;
const START_DELAY: u64 = 10 * SEC;
const DURATION: u64 = 100 * SEC;

const ONE_YOCTO: NearToken = NearToken::from_yoctonear(1);

struct Env {
    worker: Worker<Sandbox>,
    token: Contract,
    vesting: Contract,
    owner: Account,
    minter: Account,
}

/// Token imported from mainnet, vesting contract deployed (not initialized),
/// owner + vesting registered on the token, vesting already funded with TOTAL.
async fn setup() -> anyhow::Result<Env> {
    let worker = near_workspaces::sandbox().await?;
    let mainnet = near_workspaces::mainnet().await?;
    let wasm = near_workspaces::compile_project("./").await?;

    let token_id: AccountId = TOKEN_ID.parse()?;
    let token = worker
        .import_contract(&token_id, &mainnet)
        .initial_balance(NearToken::from_near(100))
        .transact()
        .await?;

    let root = worker.root_account()?;
    let minter = create_account(&root, "minter").await?;
    let owner = create_account(&root, "owner").await?;
    let vesting = create_account(&root, "vesting")
        .await?
        .deploy(&wasm)
        .await?
        .into_result()?;

    token
        .call("new_default_meta")
        .args_json(json!({
            "owner_id": minter.id(),
            "total_supply": U128(TOKEN_SUPPLY),
        }))
        .transact()
        .await?
        .into_result()?;

    for account_id in [vesting.id(), owner.id()] {
        minter
            .call(token.id(), "storage_deposit")
            .args_json(json!({ "account_id": account_id }))
            .deposit(NearToken::from_millinear(10))
            .transact()
            .await?
            .into_result()?;
    }

    minter
        .call(token.id(), "ft_transfer")
        .args_json(json!({ "receiver_id": vesting.id(), "amount": U128(TOTAL) }))
        .deposit(ONE_YOCTO)
        .transact()
        .await?
        .into_result()?;

    Ok(Env {
        worker,
        token,
        vesting,
        owner,
        minter,
    })
}

/// `setup()` plus `new` with a schedule starting START_DELAY from now.
async fn setup_initialized() -> anyhow::Result<(Env, Timestamp, Timestamp)> {
    let env = setup().await?;
    let start = now(&env.worker).await? + START_DELAY;
    let end = start + DURATION;
    env.vesting
        .call("new")
        .args_json(init_args(&env, TOTAL, start, end))
        .transact()
        .await?
        .into_result()?;
    Ok((env, start, end))
}

async fn create_account(root: &Account, name: &str) -> anyhow::Result<Account> {
    Ok(root
        .create_subaccount(name)
        .initial_balance(NearToken::from_near(20))
        .transact()
        .await?
        .into_result()?)
}

fn init_args(
    env: &Env,
    total: u128,
    start: Timestamp,
    end: Timestamp,
) -> near_sdk::serde_json::Value {
    json!({
        "token": env.token.id(),
        "total": U128(total),
        "start": U64(start),
        "end": U64(end),
        "owner": env.owner.id(),
    })
}

async fn claim(
    caller: &Account,
    vesting: &AccountId,
    deposit: NearToken,
) -> anyhow::Result<ExecutionFinalResult> {
    Ok(caller
        .call(vesting, "claim")
        .deposit(deposit)
        .max_gas()
        .transact()
        .await?)
}

async fn ft_balance(token: &Contract, account_id: &AccountId) -> anyhow::Result<u128> {
    Ok(token
        .view("ft_balance_of")
        .args_json(json!({ "account_id": account_id }))
        .await?
        .json::<U128>()?
        .0)
}

async fn now(worker: &Worker<Sandbox>) -> anyhow::Result<u64> {
    Ok(worker.view_block().await?.timestamp())
}

/// Same math as the contract.
fn unlocked_at(start: u64, end: u64, now: u64) -> u128 {
    const PRECISION: u128 = 1_000_000;
    let percent = ((now - start) as u128 * PRECISION / (end - start) as u128).min(PRECISION);
    TOTAL * percent / PRECISION
}

/// Timestamp of the block where the `claim` receipt ran on the vesting contract,
/// i.e. the exact `env::block_timestamp()` the contract saw.
async fn claim_timestamp(
    worker: &Worker<Sandbox>,
    result: &ExecutionFinalResult,
    vesting: &AccountId,
) -> anyhow::Result<Timestamp> {
    let outcome = result
        .receipt_outcomes()
        .iter()
        .find(|o| &o.executor_id == vesting)
        .ok_or_else(|| anyhow!("no receipt executed on {vesting}"))?;
    Ok(worker
        .view_block()
        .block_hash(outcome.block_hash)
        .await?
        .timestamp())
}

/// Fast-forwards until the block timestamp is at least `target`.
async fn wait_until(worker: &Worker<Sandbox>, target: Timestamp) -> anyhow::Result<()> {
    const PROBE_BLOCKS: u64 = 10;
    for _ in 0..20 {
        let before = worker.view_block().await?;
        if before.timestamp() >= target {
            return Ok(());
        }
        worker.fast_forward(PROBE_BLOCKS).await?;
        let after = worker.view_block().await?;
        let per_block =
            (after.timestamp() - before.timestamp()) / (after.height() - before.height());
        ensure!(per_block > 0, "fast_forward does not advance block time");
        let remaining = target.saturating_sub(after.timestamp());
        if remaining > 0 {
            worker
                .fast_forward(remaining.div_ceil(per_block) + 1)
                .await?;
        }
    }
    bail!("sandbox time did not reach {target}")
}

/// Claims as owner and checks the owner got exactly what the schedule unlocks
/// at the claim block, minus what was already claimed. Returns that timestamp.
async fn claim_and_check(env: &Env, start: u64, end: u64) -> anyhow::Result<u64> {
    let owner_before = ft_balance(&env.token, env.owner.id()).await?;
    let claimed_before = TOTAL - ft_balance(&env.token, env.vesting.id()).await?;

    let result = claim(&env.owner, env.vesting.id(), ONE_YOCTO).await?;
    assert!(
        result.receipt_failures().is_empty(),
        "{:#?}",
        result.receipt_failures()
    );
    let ts = claim_timestamp(&env.worker, &result, env.vesting.id()).await?;
    result.into_result()?;

    let owner_after = ft_balance(&env.token, env.owner.id()).await?;
    assert_eq!(
        owner_after - owner_before,
        unlocked_at(start, end, ts) - claimed_before
    );
    Ok(ts)
}

fn assert_fails_with(result: ExecutionFinalResult, needle: &str) {
    let err = format!(
        "{:?}",
        result.into_result().expect_err("call should have failed")
    );
    assert!(err.contains(needle), "expected `{needle}` in: {err}");
}

#[tokio::test]
async fn init_rejects_zero_total() -> anyhow::Result<()> {
    let env = setup().await?;
    let start = now(&env.worker).await?;
    let result = env
        .vesting
        .call("new")
        .args_json(init_args(&env, 0, start, start + DURATION))
        .transact()
        .await?;
    assert_fails_with(result, "total must be greater than 0");
    Ok(())
}

#[tokio::test]
async fn init_rejects_end_not_after_start() -> anyhow::Result<()> {
    let env = setup().await?;
    let start = now(&env.worker).await?;
    let result = env
        .vesting
        .call("new")
        .args_json(init_args(&env, TOTAL, start, start))
        .transact()
        .await?;
    assert_fails_with(result, "end must be greater than start");
    Ok(())
}

#[tokio::test]
async fn init_is_private() -> anyhow::Result<()> {
    let env = setup().await?;
    let start = now(&env.worker).await?;
    let result = env
        .owner
        .call(env.vesting.id(), "new")
        .args_json(init_args(&env, TOTAL, start, start + DURATION))
        .transact()
        .await?;
    assert_fails_with(result, "Method new is private");
    Ok(())
}

#[tokio::test]
async fn claim_requires_exactly_one_yocto() -> anyhow::Result<()> {
    let (env, start, _) = setup_initialized().await?;
    wait_until(&env.worker, start + DURATION / 2).await?;

    for deposit in [NearToken::from_yoctonear(0), NearToken::from_yoctonear(2)] {
        let result = claim(&env.owner, env.vesting.id(), deposit).await?;
        assert_fails_with(result, "Requires attached deposit of exactly 1 yoctoNEAR");
    }
    assert_eq!(ft_balance(&env.token, env.owner.id()).await?, 0);
    Ok(())
}

#[tokio::test]
async fn claim_only_by_owner() -> anyhow::Result<()> {
    let (env, start, _) = setup_initialized().await?;
    wait_until(&env.worker, start + DURATION / 2).await?;

    let result = claim(&env.minter, env.vesting.id(), ONE_YOCTO).await?;
    assert_fails_with(result, "Only the owner can claim");
    assert_eq!(ft_balance(&env.token, env.vesting.id()).await?, TOTAL);
    Ok(())
}

#[tokio::test]
async fn claim_before_start_fails() -> anyhow::Result<()> {
    let (env, start, _) = setup_initialized().await?;
    ensure!(now(&env.worker).await? < start, "schedule already started");

    // `now - start` underflows, so the contract panics instead of claiming 0.
    let result = claim(&env.owner, env.vesting.id(), ONE_YOCTO).await?;
    assert_fails_with(result, "Option::unwrap()");
    assert_eq!(ft_balance(&env.token, env.vesting.id()).await?, TOTAL);
    Ok(())
}

#[tokio::test]
async fn claims_follow_linear_schedule() -> anyhow::Result<()> {
    let (env, start, end) = setup_initialized().await?;

    wait_until(&env.worker, start + DURATION / 4).await?;
    let ts = claim_and_check(&env, start, end).await?;
    assert!(ts < end, "first claim should be mid-vesting");
    let first = ft_balance(&env.token, env.owner.id()).await?;
    assert!(first > 0 && first < TOTAL);

    wait_until(&env.worker, start + DURATION * 3 / 5).await?;
    let ts = claim_and_check(&env, start, end).await?;
    assert!(ts < end, "second claim should be mid-vesting");
    let second = ft_balance(&env.token, env.owner.id()).await?;
    assert!(second > first && second < TOTAL);

    wait_until(&env.worker, end).await?;
    claim_and_check(&env, start, end).await?;
    assert_eq!(ft_balance(&env.token, env.owner.id()).await?, TOTAL);
    assert_eq!(ft_balance(&env.token, env.vesting.id()).await?, 0);
    Ok(())
}

#[tokio::test]
async fn claim_after_full_unlock_sends_nothing() -> anyhow::Result<()> {
    let (env, start, end) = setup_initialized().await?;
    wait_until(&env.worker, end + DURATION).await?;

    claim_and_check(&env, start, end).await?;
    assert_eq!(ft_balance(&env.token, env.owner.id()).await?, TOTAL);

    let result = claim(&env.owner, env.vesting.id(), ONE_YOCTO).await?;
    // NEP-141 rejects a 0-amount ft_transfer; the transfer is detached,
    // so `claim` itself still succeeds.
    assert!(!result.receipt_failures().is_empty());
    result.into_result()?;

    assert_eq!(ft_balance(&env.token, env.owner.id()).await?, TOTAL);
    assert_eq!(ft_balance(&env.token, env.vesting.id()).await?, 0);
    Ok(())
}
