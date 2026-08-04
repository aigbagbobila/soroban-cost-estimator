use clap::Parser;
use soroban_cost_estimator::cache;
use soroban_cost_estimator::cli;
use soroban_cost_estimator::config_snapshot;
use soroban_cost_estimator::error;
use soroban_cost_estimator::report;
use soroban_cost_estimator::rpc;
use soroban_cost_estimator::wasm;
use soroban_cost_estimator::xdr_helper;

#[tokio::main]
async fn main() {
    let args = cli::Cli::parse();

    if let Err(err) = run(args).await {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
}

async fn run(args: cli::Cli) -> error::AppResult<()> {
    match args.command {
        cli::Command::Estimate {
            wasm,
            network,
            rpc_url,
            r#fn,
            id,
            args,
            json,
        } => {
            cmd_estimate(
                &wasm,
                &network,
                rpc_url.as_deref(),
                id.as_deref(),
                r#fn.as_deref(),
                &args,
                json,
            )
            .await
        }
        cli::Command::EstimateAll {
            wasm,
            network,
            id,
            json,
        } => cmd_estimate_all(&wasm, &network, id.as_deref(), json).await,
        cli::Command::Config { action } => match action {
            cli::ConfigAction::Snapshot { network, out, json } => {
                cmd_config_snapshot(&network, out.as_deref(), json).await
            }
            cli::ConfigAction::Diff { network, against } => {
                cmd_config_diff(&network, against.as_deref()).await
            }
        },
        cli::Command::Watch { network, interval } => cmd_watch(&network, &interval).await,
    }
}

/// True when a simulation response carried neither cost data, nor
/// transaction data, nor a latest ledger — the signature of a misconfigured
/// request (bad `--id`, wrong network, or RPC schema drift), not a free
/// transaction.
fn missing_simulation_data(resp: &rpc::simulate::SimulateTransactionResponse) -> bool {
    resp.cost.is_none() && resp.transaction_data.is_none() && resp.latest_ledger.is_none()
}

/// Extract resource usage from a simulation response.
///
/// Legacy RPC versions report CPU/memory in a `cost` object; modern versions
/// dropped `cost` and carry the resources (CPU instructions, footprint entry
/// counts, disk read/write bytes) inside `transactionData` XDR. Memory bytes
/// are only reported by the legacy `cost` object — modern RPC responses do
/// not expose them, so they report 0.
///
/// Returns (cpu_insns, mem_bytes, read_entries, write_entries, read_bytes,
/// write_bytes). Entry/byte counts are u32 to match `CostReport`.
fn response_resources(
    response: &rpc::simulate::SimulateTransactionResponse,
) -> error::AppResult<(u64, u64, u32, u32, u32, u32)> {
    let resources = rpc::simulate::parse_transaction_data_resources(&response.transaction_data)?;
    let (cpu, mem) = match &response.cost {
        Some(cost) => (cost.cpu_insns, cost.mem_bytes),
        None => (resources.map(|r| r.cpu_insns).unwrap_or(0), 0),
    };
    let entries = |count: usize| u32::try_from(count).unwrap_or(u32::MAX);
    let bytes = |count: u64| u32::try_from(count).unwrap_or(u32::MAX);
    Ok((
        cpu,
        mem,
        resources.map(|r| entries(r.read_entries)).unwrap_or(0),
        resources.map(|r| entries(r.write_entries)).unwrap_or(0),
        resources.map(|r| bytes(r.read_bytes)).unwrap_or(0),
        resources.map(|r| bytes(r.write_bytes)).unwrap_or(0),
    ))
}

/// Fetch fee rates from the network config (compute + ledger cost + bandwidth).
///
/// Returns a `FeeRates` struct with raw config rates. These are passed to
/// `compute_fee_breakdown` which does the proper `(units * rate) / scale`
/// math to preserve precision.
///
/// If any of the three `ConfigSetting*` sources cannot be fetched or
/// decoded, its rate(s) fall back to 0 and a warning is printed to stderr
/// — a silent zero rate would understate the non-refundable fee, so it must
/// never pass unannounced.
async fn fetch_fee_rates(client: &rpc::client::RpcClient) -> report::fee_calc::FeeRates {
    let mut degraded: Vec<&'static str> = Vec::new();

    let raw_compute =
        rpc::config::fetch_config_setting(client, rpc::config::ConfigSettingId::ContractComputeV0)
            .await;

    let raw_ledger_cost = rpc::config::fetch_config_setting(
        client,
        rpc::config::ConfigSettingId::ContractLedgerCostV0,
    )
    .await;

    let raw_bandwidth = rpc::config::fetch_config_setting(
        client,
        rpc::config::ConfigSettingId::ContractBandwidthV0,
    )
    .await;

    // ConfigSettingContractComputeV0.fee_rate_per_instructions_increment
    // is stroops per 10,000 instructions (not per instruction).
    let compute_per_10k = match raw_compute {
        Ok(raw) => match xdr_helper::decode_config_entry_xdr(&raw.config_xdr) {
            Ok(stellar_xdr::ConfigSettingEntry::ContractComputeV0(s)) => {
                s.fee_rate_per_instructions_increment
            }
            _ => {
                degraded.push("ContractComputeV0");
                0
            }
        },
        Err(_) => {
            degraded.push("ContractComputeV0");
            0
        }
    };

    // ConfigSettingContractLedgerCostV0: per-entry read/write fees and the
    // per-KB disk read fee — all part of the non-refundable fee in
    // stellar-core's resource fee model.
    let (read_entry, write_entry, read_1kb) = match raw_ledger_cost {
        Ok(raw) => match xdr_helper::decode_config_entry_xdr(&raw.config_xdr) {
            Ok(stellar_xdr::ConfigSettingEntry::ContractLedgerCostV0(s)) => (
                s.fee_disk_read_ledger_entry,
                s.fee_write_ledger_entry,
                s.fee_disk_read1_kb,
            ),
            _ => {
                degraded.push("ContractLedgerCostV0");
                (0, 0, 0)
            }
        },
        Err(_) => {
            degraded.push("ContractLedgerCostV0");
            (0, 0, 0)
        }
    };

    // ConfigSettingContractBandwidthV0.fee_tx_size1_kb
    // is stroops per 1KB of tx size (not per byte).
    let bandwidth_per_kb = match raw_bandwidth {
        Ok(raw) => match xdr_helper::decode_config_entry_xdr(&raw.config_xdr) {
            Ok(stellar_xdr::ConfigSettingEntry::ContractBandwidthV0(s)) => s.fee_tx_size1_kb,
            _ => {
                degraded.push("ContractBandwidthV0");
                0
            }
        },
        Err(_) => {
            degraded.push("ContractBandwidthV0");
            0
        }
    };

    if !degraded.is_empty() {
        eprintln!(
            "Warning: fee rate source(s) {} unavailable — affected rate(s) set to 0 (non-refundable fee understated)",
            degraded.join(", ")
        );
    }

    report::fee_calc::FeeRates {
        fee_per_10k_insns: compute_per_10k,
        fee_per_read_entry: read_entry,
        fee_per_write_entry: write_entry,
        fee_per_read_1kb: read_1kb,
        fee_per_1kb: bandwidth_per_kb,
    }
}

/// `estimate` command: simulate a single invocation and print cost report.
async fn cmd_estimate(
    wasm_path: &str,
    network: &str,
    rpc_url: Option<&str>,
    contract_id: Option<&str>,
    fn_name: Option<&str>,
    args: &[String],
    json_flag: bool,
) -> error::AppResult<()> {
    use sha2::Digest;

    let wasm_info = wasm::parser::load_wasm(std::path::Path::new(wasm_path))?;
    let endpoint = rpc::client::resolve_endpoint(network, rpc_url)?;
    let client = rpc::client::RpcClient::new(&endpoint);

    let sc_vals: Vec<stellar_xdr::ScVal> = args
        .iter()
        .map(|a| xdr_helper::parse_arg_scval(a))
        .collect();

    // Raw XDR bytes: the transaction size for the bandwidth fee must be the
    // XDR byte count, not the base64 length (base64 inflates it by ~33%).
    let tx_xdr =
        xdr_helper::build_simulation_tx_envelope(&wasm_info.bytes, contract_id, fn_name, &sc_vals)?;
    let tx_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &tx_xdr);

    let response = rpc::simulate::simulate_transaction(&client, &tx_b64).await?;

    let wasm_hash = hex::encode(sha2::Sha256::digest(&wasm_info.bytes));

    // Guard: a simulation that returns no cost data and no ledger is almost
    // certainly a misconfigured request (bad --id, wrong network, or an RPC
    // schema drift), not a free transaction. Fail loudly instead of silently
    // printing an all-zero report.
    if missing_simulation_data(&response) {
        return Err(error::AppError::SimulationFailed(
            "simulation returned no cost data and no latest ledger — check --id, --fn, and the RPC endpoint".to_string(),
        ));
    }

    // Resource usage: legacy RPCs report it in `cost`, modern ones carry it
    // in `transactionData` XDR. See `response_resources`.
    let (cpu_instructions, memory_bytes, read_entries, write_entries, read_bytes, write_bytes) =
        response_resources(&response)?;

    let latest_ledger: u32 = response
        .latest_ledger
        .and_then(|l| u32::try_from(l).ok())
        .unwrap_or(0);

    // Authoritative total resource fee: prefer the RPC's `minResourceFee`
    // (the minimum the network charges), fall back to the XDR-derived
    // `transaction_data.resource_fee` when it is omitted. Both carry the
    // same value on a successful simulation. A malformed source is ignored
    // in favor of the other, since the total only feeds the breakdown display.
    let total_fee_stroops = rpc::simulate::parse_resource_fee(&response.min_resource_fee)
        .unwrap_or(None)
        .or(rpc::simulate::parse_transaction_data_resource_fee(
            &response.transaction_data,
        )?)
        .unwrap_or(0);

    // Fetch the real fee rates from the network
    let fee_rates = fetch_fee_rates(&client).await;

    let fee = report::fee_calc::compute_fee_breakdown(
        total_fee_stroops,
        cpu_instructions,
        read_entries,
        write_entries,
        read_bytes,
        tx_xdr.len() as u32,
        fee_rates,
    );

    let function_name = fn_name.unwrap_or("(wasm upload)");

    let report = report::cost_report::CostReport {
        function: function_name.to_string(),
        wasm_hash: wasm_hash.clone(),
        cpu_instructions,
        memory_bytes,
        tx_size: tx_xdr.len() as u32,
        read_entries,
        write_entries,
        read_bytes,
        write_bytes,
        fee: fee.clone(),
        ledger: latest_ledger,
        network: network.to_string(),
    };

    // Save to cache
    let _ = cache::save_estimate(
        &wasm_hash,
        function_name,
        args,
        network,
        latest_ledger,
        fee.total_stroops,
        cpu_instructions,
        memory_bytes,
    );

    if json_flag {
        println!("{}", report::cost_report::format_report_json(&report));
    } else {
        println!("{}", report::cost_report::format_report_table(&report));
    }

    Ok(())
}

/// `estimate-all` command: enumerate all functions and estimate each.
async fn cmd_estimate_all(
    wasm_path: &str,
    network: &str,
    contract_id: Option<&str>,
    json_flag: bool,
) -> error::AppResult<()> {
    let wasm_info = wasm::parser::load_wasm(std::path::Path::new(wasm_path))?;
    let endpoint = rpc::client::resolve_endpoint(network, None)?;
    let client = rpc::client::RpcClient::new(&endpoint);

    if !json_flag {
        println!(
            "Enumerated {} function(s) in WASM:",
            wasm_info.functions.len()
        );
        for (i, fn_info) in wasm_info.functions.iter().enumerate() {
            println!("  {}. {}", i + 1, wasm::parser::format_function(fn_info));
        }
        println!();
        println!(
            "Contract spec: {}",
            if wasm_info.has_spec {
                "present (typed params decoded from contractspecv0)"
            } else {
                "absent (bare WASM exports only)"
            }
        );
        if contract_id.is_none() {
            println!(
                "Note: pass --id <contract-id> to simulate each function against a deployed contract."
            );
        }
    }

    use sha2::Digest;
    let wasm_hash = hex::encode(sha2::Sha256::digest(&wasm_info.bytes));

    let mut json_results: Vec<serde_json::Value> = Vec::new();
    let total = wasm_info.functions.len();
    for (i, fn_info) in wasm_info.functions.iter().enumerate() {
        if !json_flag {
            println!("[{}/{}] {}", i + 1, total, fn_info.name);
        }
        let result = estimate_all_function(
            &client,
            &wasm_info,
            fn_info,
            contract_id,
            &wasm_hash,
            network,
            json_flag,
        )
        .await?;
        if let Some(value) = result {
            json_results.push(value);
        }
    }

    if json_flag {
        println!("{}", serde_json::to_string_pretty(&json_results)?);
    }

    Ok(())
}

/// Estimates one exported function against the network, printing its result
/// (non-JSON mode) or returning its JSON record (JSON mode).
async fn estimate_all_function(
    client: &rpc::client::RpcClient,
    wasm_info: &wasm::parser::WasmInfo,
    fn_info: &wasm::parser::FunctionInfo,
    contract_id: Option<&str>,
    wasm_hash: &str,
    network: &str,
    json_flag: bool,
) -> error::AppResult<Option<serde_json::Value>> {
    if fn_info.param_count > 0 {
        let reason = format!("needs --fn/--arg ({} param(s))", fn_info.param_count);
        if json_flag {
            return Ok(Some(serde_json::json!({
                "function": fn_info.name,
                "status": "skipped",
                "reason": reason,
            })));
        }
        println!("── Estimating '{}' ── Skipped: {reason}", fn_info.name);
        return Ok(None);
    }

    let tx_xdr = match xdr_helper::build_simulation_tx_envelope(
        &wasm_info.bytes,
        contract_id,
        Some(fn_info.name.as_str()),
        &[],
    ) {
        Ok(tx) => tx,
        Err(e) => {
            if json_flag {
                return Ok(Some(serde_json::json!({
                    "function": fn_info.name,
                    "status": "skipped",
                    "reason": e.to_string(),
                })));
            }
            eprintln!("── Estimating '{}' ── Skipped: {e}", fn_info.name);
            return Ok(None);
        }
    };
    let tx_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &tx_xdr);

    match rpc::simulate::simulate_transaction(client, &tx_b64).await {
        Ok(resp) => {
            // Same fail-loudly guard as `estimate`: no cost data + no ledger
            // means a misconfigured request, not a free transaction.
            if missing_simulation_data(&resp) {
                let msg = "simulation returned no cost data and no latest ledger — check --id and the RPC endpoint";
                if json_flag {
                    return Ok(Some(serde_json::json!({
                        "function": fn_info.name,
                        "status": "error",
                        "error": msg,
                    })));
                }
                eprintln!("── Estimating '{}' ── Error: {msg}", fn_info.name);
                return Ok(None);
            }

            let (cpu, mem, ..) = response_resources(&resp)?;
            let fee = rpc::simulate::parse_resource_fee(&resp.min_resource_fee)
                .unwrap_or(None)
                .or(rpc::simulate::parse_transaction_data_resource_fee(
                    &resp.transaction_data,
                )?)
                .unwrap_or(0);
            let xlm = report::fee_calc::stroops_to_xlm(fee);
            let ledger: u32 = resp
                .latest_ledger
                .and_then(|l| u32::try_from(l).ok())
                .unwrap_or(0);

            let _ = cache::save_estimate(
                wasm_hash,
                &fn_info.name,
                &[],
                network,
                ledger,
                fee,
                cpu,
                mem,
            );

            if json_flag {
                Ok(Some(serde_json::json!({
                    "function": fn_info.name,
                    "status": "ok",
                    "cpu_instructions": cpu,
                    "memory_bytes": mem,
                    "fee_stroops": fee,
                    "fee_xlm": xlm,
                    "ledger": ledger,
                })))
            } else {
                println!(
                    "CPU: {cpu} insns | Mem: {mem} bytes | Fee: {fee} stroops ({xlm} XLM) | Ledger: {ledger}"
                );
                Ok(None)
            }
        }
        Err(e) => {
            if json_flag {
                Ok(Some(serde_json::json!({
                    "function": fn_info.name,
                    "status": "error",
                    "error": e.to_string(),
                })))
            } else {
                eprintln!("Skipped — simulation failed: {e}");
                Ok(None)
            }
        }
    }
}

/// `config snapshot` command: fetch config settings and save snapshot.
async fn cmd_config_snapshot(
    network: &str,
    out_path: Option<&str>,
    json_flag: bool,
) -> error::AppResult<()> {
    let endpoint = rpc::client::resolve_endpoint(network, None)?;
    let client = rpc::client::RpcClient::new(&endpoint);
    let raw_entries = rpc::config::fetch_all_config_settings(&client).await?;

    let mut snapshot = xdr_helper::begin_snapshot(network, 0);
    for raw in &raw_entries {
        let config_entry = xdr_helper::decode_config_entry_xdr(&raw.config_xdr)?;
        xdr_helper::apply_config_entry(&mut snapshot, config_entry);
    }
    if let Some(latest) = raw_entries.iter().map(|e| e.last_modified_ledger).max() {
        snapshot.ledger = latest;
    }

    let path = config_snapshot::store::save_snapshot(&snapshot, out_path)?;

    if json_flag {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(());
    }
    println!("Config snapshot saved to: {}", path.display());
    println!("Network: {}", snapshot.network);
    println!("Ledger:  {}", snapshot.ledger);
    println!("Time:    {}", snapshot.timestamp);
    Ok(())
}

/// `config diff` command: compare current config against a snapshot.
async fn cmd_config_diff(network: &str, against_path: Option<&str>) -> error::AppResult<()> {
    let old_snapshot = match against_path {
        Some(path) => config_snapshot::store::load_snapshot_from_path(path)?,
        None => config_snapshot::store::load_latest_snapshot(network)?,
    };

    let endpoint = rpc::client::resolve_endpoint(network, None)?;
    let client = rpc::client::RpcClient::new(&endpoint);
    let raw_entries = rpc::config::fetch_all_config_settings(&client).await?;

    let mut new_snapshot = xdr_helper::begin_snapshot(network, 0);
    for raw in &raw_entries {
        let config_entry = xdr_helper::decode_config_entry_xdr(&raw.config_xdr)?;
        xdr_helper::apply_config_entry(&mut new_snapshot, config_entry);
    }
    if let Some(latest) = raw_entries.iter().map(|e| e.last_modified_ledger).max() {
        new_snapshot.ledger = latest;
    }

    let diff = config_snapshot::diff::diff_snapshots(&old_snapshot, &new_snapshot);
    println!("{}", config_snapshot::diff::format_diff(&diff));

    // Always check for stale cached estimates, regardless of pricing changes
    match cache::list_cached_estimates(network) {
        Ok(estimates) => {
            if !estimates.is_empty() {
                let stale = cache::find_stale_estimates(&estimates, new_snapshot.ledger);
                if stale.is_empty() {
                    println!(
                        "  All cached estimates are current (ledger {}).",
                        new_snapshot.ledger
                    );
                } else {
                    println!(
                        "  {} cached estimate(s) from earlier ledger(s) — may be stale:",
                        stale.len()
                    );
                    for est in &stale {
                        println!(
                            "    - {} @ ledger {} (current: {})",
                            est.function, est.ledger, new_snapshot.ledger
                        );
                    }
                }
            }
        }
        Err(e) => {
            println!("  Warning: could not check cache: {e}");
        }
    }

    if diff.has_pricing_changes {
        std::process::exit(1);
    }
    Ok(())
}

/// Parse an interval like `3600`, `3600s`, `30m`, `1h`, or `1d` into seconds.
///
/// Defaults to one hour on unparseable input.
fn parse_interval_secs(interval: &str) -> u64 {
    let trimmed = interval.trim().to_ascii_lowercase();
    let (num_part, mult) = match trimmed.chars().last() {
        Some('s') => (&trimmed[..trimmed.len() - 1], 1u64),
        Some('m') => (&trimmed[..trimmed.len() - 1], 60u64),
        Some('h') => (&trimmed[..trimmed.len() - 1], 3600u64),
        Some('d') => (&trimmed[..trimmed.len() - 1], 86_400u64),
        _ => (&trimmed[..], 1u64),
    };
    num_part.parse::<u64>().unwrap_or(3600).saturating_mul(mult)
}

/// Resolves when the process receives SIGINT (Ctrl-C) or SIGTERM, so a
/// long-running command can stop gracefully.
///
/// # Network calls
/// None — waits on OS signals.
async fn shutdown_signal() -> error::AppResult<()> {
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    Ok(())
}

/// Runs one `watch` poll cycle: fetch the network config, diff it against
/// the previous snapshot, print changes and stale-estimate info, then save
/// the new snapshot.
///
/// # Network calls
/// Makes one batched `getLedgerEntries` RPC call.
async fn watch_poll_once(network: &str, first: &mut bool) -> error::AppResult<()> {
    let endpoint = rpc::client::resolve_endpoint(network, None)?;
    let client = rpc::client::RpcClient::new(&endpoint);

    match rpc::config::fetch_all_config_settings(&client).await {
        Ok(raw_entries) => {
            let mut snapshot = xdr_helper::begin_snapshot(network, 0);
            for raw in &raw_entries {
                if let Ok(config_entry) = xdr_helper::decode_config_entry_xdr(&raw.config_xdr) {
                    xdr_helper::apply_config_entry(&mut snapshot, config_entry);
                }
            }
            if let Some(latest) = raw_entries.iter().map(|e| e.last_modified_ledger).max() {
                snapshot.ledger = latest;
            }

            if !*first {
                if let Ok(old_snapshot) = config_snapshot::store::load_latest_snapshot(network) {
                    let diff = config_snapshot::diff::diff_snapshots(&old_snapshot, &snapshot);
                    if !diff.changes.is_empty() {
                        println!("{}", config_snapshot::diff::format_diff(&diff));
                    }

                    // Always check for stale cached estimates, regardless of pricing changes
                    if let Ok(estimates) = cache::list_cached_estimates(network) {
                        if !estimates.is_empty() {
                            let stale = cache::find_stale_estimates(&estimates, snapshot.ledger);
                            if stale.is_empty() {
                                println!(
                                    "  All cached estimates are current (ledger {}).",
                                    snapshot.ledger
                                );
                            } else {
                                println!(
                                    "  {} cached estimate(s) from earlier ledger(s) — may be stale:",
                                    stale.len()
                                );
                                for est in &stale {
                                    println!(
                                        "    - {} @ ledger {} (current: {})",
                                        est.function, est.ledger, snapshot.ledger
                                    );
                                }
                            }
                        }
                    }
                }
            }

            let _ = config_snapshot::store::save_snapshot(&snapshot, None);
            *first = false;
        }
        Err(e) => {
            eprintln!("Warning: failed to fetch config: {e}");
        }
    }
    Ok(())
}

/// `watch` command: poll network config and print diffs.
///
/// Polls immediately, then on `interval`, until SIGINT (Ctrl-C) or SIGTERM
/// is received — then exits cleanly with code 0. The in-flight poll is
/// cancelled rather than writing a partial snapshot.
async fn cmd_watch(network: &str, interval: &str) -> error::AppResult<()> {
    let interval_secs: u64 = parse_interval_secs(interval);

    println!(
        "Watching {} for config changes every {}s... (Ctrl-C to stop)",
        network, interval_secs
    );

    let mut first = true;
    let mut shutdown_triggered = false;
    loop {
        tokio::select! {
            signal = shutdown_signal() => {
                signal?;
                if shutdown_triggered {
                    eprintln!("Second signal received — forcing exit (code 130).");
                    std::process::exit(130);
                }
                shutdown_triggered = true;
                println!("Received stop signal — finishing current poll then exiting cleanly. Send signal again to force-exit.");
                // Continue the loop to finish the current poll, then exit.
                // The next iteration will hit the check below and exit.
            }
            () = async {
                let _ = watch_poll_once(network, &mut first).await;
                tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
            } => {}
        }
        if shutdown_triggered {
            println!("Shutdown complete — exiting cleanly.");
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_interval_secs;

    #[test]
    fn test_parse_interval_secs() {
        assert_eq!(parse_interval_secs("3600"), 3600);
        assert_eq!(parse_interval_secs("3600s"), 3600);
        assert_eq!(parse_interval_secs("30m"), 1800);
        assert_eq!(parse_interval_secs("1h"), 3600);
        assert_eq!(parse_interval_secs("1d"), 86_400);
        assert_eq!(parse_interval_secs(" 5M "), 300);
        // Unparseable input falls back to the one-hour default.
        assert_eq!(parse_interval_secs(""), 3600);
        assert_eq!(parse_interval_secs("s"), 3600);
        assert_eq!(parse_interval_secs("10ss"), 3600);
        assert_eq!(parse_interval_secs("garbage"), 3600);
    }
}
