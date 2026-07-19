//! MCP server, compiled into the binary. `nuthatch mcp` speaks the Model Context Protocol over
//! stdio (newline-delimited JSON-RPC), so a coding agent (Claude Code, Cursor, …) can query a
//! running index directly. It is a thin, fully-offline bridge to the local HTTP API of a running
//! `nuthatch dev` — no external calls, no telemetry, no gated data service. Nothing phones home.
//!
//! Bridging (rather than reopening the redb/segments) means the MCP server never contends with the
//! indexer for the single-writer store, and it automatically reflects the live IVM views.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PROTOCOL_VERSION: &str = "2025-06-18";

/// The MCP server is meant to be launched by an MCP *client* (Claude Code, Cursor, …) as a stdio
/// subprocess. When a human runs `nuthatch mcp` in a terminal, stdin is a TTY and no client is
/// driving it — so instead of silently blocking on a read that never comes, show how to wire it up.
/// Returns true if we short-circuited (printed guidance and should exit).
fn guide_if_interactive(base: &str) -> bool {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return false; // a client is piping JSON-RPC in — run the server.
    }
    eprintln!("`nuthatch mcp` is a stdio server for an AI client to launch, not to run by hand.\n");
    print_client_config(base);
    true
}

/// Emit a copy-paste MCP client configuration for this binary bridging to `base`. One documented
/// command wires a coding agent to a running nest (RFC-0015 slice 5): print it, paste it, ask your
/// contract's data in plain English — fully offline, nothing phones home.
pub fn print_client_config(base: &str) {
    // Prefer this binary's absolute path so the snippet works even off `PATH`; fall back to the bare
    // command name.
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_else(|| "nuthatch".to_string());

    let snippet = client_config_json(&exe, base);

    println!("First run your index:   nuthatch dev");
    println!("Then point an AI client at it — either way is one step:\n");
    println!("  Claude Code (one-liner):");
    println!("    claude mcp add nuthatch -- {exe} mcp --url {base}\n");
    println!("  Or add to .mcp.json / your client's MCP config:");
    println!(
        "{}",
        serde_json::to_string_pretty(&snippet).unwrap_or_default()
    );
    println!(
        "\nThen ask your agent: \"what are the top USDC holders?\" — it queries the nest over MCP."
    );
}

/// The MCP-client server entry for this binary bridging to `base` — the value that goes under
/// `mcpServers.nuthatch` in a client's config.
fn client_config_json(exe: &str, base: &str) -> Value {
    json!({
        "mcpServers": {
            "nuthatch": {
                "command": exe,
                "args": ["mcp", "--url", base]
            }
        }
    })
}

/// Run the stdio MCP loop, bridging tool calls to `base` (a running `nuthatch dev` HTTP API).
pub async fn serve(base: String) -> Result<()> {
    if guide_if_interactive(&base) {
        return Ok(());
    }
    let client = reqwest::Client::new();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(resp) = handle(&req, &client, &base).await {
            stdout
                .write_all(serde_json::to_string(&resp)?.as_bytes())
                .await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

/// Dispatch one JSON-RPC message. Returns None for notifications (no response expected).
async fn handle(req: &Value, client: &reqwest::Client, base: &str) -> Option<Value> {
    let id = req.get("id").cloned();
    match req.get("method").and_then(Value::as_str).unwrap_or("") {
        "initialize" => Some(ok(id?, initialize_result(req))),
        "notifications/initialized" => None,
        "ping" => Some(ok(id?, json!({}))),
        "tools/list" => Some(ok(id?, json!({ "tools": tool_specs() }))),
        "tools/call" => {
            let id = id?;
            let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
            match call_tool(&params, client, base).await {
                Ok(text) => Some(ok(id, content(&text, false))),
                Err(e) => Some(ok(id, content(&format!("{e:#}"), true))),
            }
        }
        _ => Some(err(id?, -32601, "method not found")),
    }
}

fn initialize_result(req: &Value) -> Value {
    // Echo the client's requested protocol version when present.
    let pv = req
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "protocolVersion": pv,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "nuthatch", "version": env!("CARGO_PKG_VERSION") },
    })
}

/// The tool surface. Not a thin single-endpoint wrapper — schema discovery, SQL, point-reads, and
/// the IVM views, each with an LLM-friendly description.
pub fn tool_specs() -> Value {
    json!([
        { "name": "status", "description": "Index status: contract, chain, transfers indexed, holders, last & sealed block.",
          "inputSchema": { "type": "object", "properties": {} } },
        { "name": "schema", "description": "The data model — how tables/views are named and queried. Read this first, then `tables` for the exact tables and columns.",
          "inputSchema": { "type": "object", "properties": {} } },
        { "name": "tables", "description": "List every decoded table (`{alias}__{event}`) with its columns, Solidity types, and topic0.",
          "inputSchema": { "type": "object", "properties": {} } },
        { "name": "table", "description": "Recent rows of one table, merged across the hot tip and sealed segments.",
          "inputSchema": { "type": "object", "properties": { "name": { "type": "string" }, "limit": { "type": "integer", "default": 50 } }, "required": ["name"] } },
        { "name": "sql", "description": "Run a read-only SQL query over sealed (finalized) data. Each event is a DuckDB view named `{alias}__{event}` (e.g. \"usdc__transfer\") with block_number, log_index, tx_hash, address + the event's params. Call `schema` first. SELECT/WITH only.",
          "inputSchema": { "type": "object", "properties": { "query": { "type": "string", "description": "A SELECT or WITH query." } }, "required": ["query"] } },
        { "name": "explain", "description": "Validate a SQL query WITHOUT executing it — binds tables/columns/types and returns {valid:true} or an error with a fix hint. Cheaper than `sql`; use it to check a query before running it.",
          "inputSchema": { "type": "object", "properties": { "query": { "type": "string", "description": "A SELECT or WITH query to validate." } }, "required": ["query"] } },
        { "name": "entity", "description": "Look up one transfer by its id, formatted `{block:012}-{logindex:06}`.",
          "inputSchema": { "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"] } },
        { "name": "balance", "description": "Derived token balance for an address (IVM view; i128 base units, returned as a decimal string).",
          "inputSchema": { "type": "object", "properties": { "address": { "type": "string" } }, "required": ["address"] } },
        { "name": "top_balances", "description": "Top holder balances, descending (IVM view; i128 base units as decimal strings).",
          "inputSchema": { "type": "object", "properties": { "limit": { "type": "integer", "default": 20 } } } },
        { "name": "flags", "description": "Compliance flags (RFC-0008 C3): `kind=threshold` (single transfers over the configured amount) or `kind=velocity` (addresses over the windowed-volume threshold). Amounts are i128 base units as decimal strings.",
          "inputSchema": { "type": "object", "properties": { "kind": { "type": "string", "enum": ["threshold", "velocity"] }, "limit": { "type": "integer", "default": 50 } } } },
        { "name": "exposure", "description": "Direct counterparty-exposure of an address to the labeled set (RFC-0008 C1): inbound/outbound count + summed amount per label.",
          "inputSchema": { "type": "object", "properties": { "address": { "type": "string" } }, "required": ["address"] } },
        { "name": "screen_status", "description": "Sanctions-screening result for an address (RFC-0008 C2): the `sanction_hit` annotations against it, with the list-snapshot version each was screened against. Answers 'was X flagged, and against which list version?'",
          "inputSchema": { "type": "object", "properties": { "address": { "type": "string" } }, "required": ["address"] } }
    ])
}

async fn call_tool(params: &Value, client: &reqwest::Client, base: &str) -> Result<String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing tool name"))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match name {
        "status" => get(client, &format!("{base}/")).await,
        // The enriched, per-nest schema (RFC-0016 §2): structure + meaning + footguns + coverage,
        // composed server-side from the running nest and `semantic.toml`. No longer a static string.
        "schema" => get(client, &format!("{base}/schema")).await,
        "tables" => get(client, &format!("{base}/tables")).await,
        "table" => {
            let name = args["name"]
                .as_str()
                .ok_or_else(|| anyhow!("`name` is required"))?;
            let n = args["limit"].as_u64().unwrap_or(50);
            get(client, &format!("{base}/table/{name}?limit={n}")).await
        }
        "sql" => {
            let q = args["query"]
                .as_str()
                .ok_or_else(|| anyhow!("`query` is required"))?;
            get_query(client, &format!("{base}/sql"), &[("q", q)]).await
        }
        "explain" => {
            let q = args["query"]
                .as_str()
                .ok_or_else(|| anyhow!("`query` is required"))?;
            get_query(client, &format!("{base}/explain"), &[("q", q)]).await
        }
        "entity" => {
            let id = args["id"]
                .as_str()
                .ok_or_else(|| anyhow!("`id` is required"))?;
            get(client, &format!("{base}/entity/{id}")).await
        }
        "balance" => {
            let a = args["address"]
                .as_str()
                .ok_or_else(|| anyhow!("`address` is required"))?;
            get(client, &format!("{base}/balance/{a}")).await
        }
        "top_balances" => {
            let n = args["limit"].as_u64().unwrap_or(20);
            get(client, &format!("{base}/balances?limit={n}")).await
        }
        "flags" => {
            let kind = args["kind"].as_str().unwrap_or("threshold");
            let n = args["limit"].as_u64().unwrap_or(50);
            get(client, &format!("{base}/flags?kind={kind}&limit={n}")).await
        }
        "exposure" => {
            let a = args["address"]
                .as_str()
                .ok_or_else(|| anyhow!("`address` is required"))?;
            get(client, &format!("{base}/exposure/{a}")).await
        }
        "screen_status" => {
            // Query the sealed sanction_hit annotations for this address, with the list version.
            let a = args["address"]
                .as_str()
                .ok_or_else(|| anyhow!("`address` is required"))?
                .to_ascii_lowercase();
            let q = format!(
                "SELECT block_number, side, counterparty, value, list_snapshot FROM sanction_hit \
                 WHERE lower(address) = '{a}' ORDER BY block_number LIMIT 100"
            );
            get_query(client, &format!("{base}/sql"), &[("q", &q)]).await
        }
        other => bail!("unknown tool `{other}`"),
    }
}

async fn get(client: &reqwest::Client, url: &str) -> Result<String> {
    fetch(client.get(url), url).await
}

async fn get_query(client: &reqwest::Client, url: &str, q: &[(&str, &str)]) -> Result<String> {
    fetch(client.get(url).query(q), url).await
}

async fn fetch(req: reqwest::RequestBuilder, url: &str) -> Result<String> {
    let resp = req.send().await.map_err(|e| {
        anyhow!("cannot reach nuthatch at {url} — is `nuthatch dev` running? ({e})")
    })?;
    Ok(resp.text().await?)
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn content(text: &str, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_config_launches_this_binary_and_bridges_to_base() {
        let cfg = client_config_json("/usr/local/bin/nuthatch", "http://127.0.0.1:8288");
        let srv = &cfg["mcpServers"]["nuthatch"];
        assert_eq!(srv["command"], "/usr/local/bin/nuthatch");
        // The client launches `nuthatch mcp --url <base>` as a stdio subprocess.
        assert_eq!(
            srv["args"],
            json!(["mcp", "--url", "http://127.0.0.1:8288"])
        );
    }

    #[tokio::test]
    async fn initialize_and_tools_list_need_no_network() {
        let client = reqwest::Client::new();
        let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-03-26" } });
        let resp = handle(&init, &client, "http://127.0.0.1:1").await.unwrap();
        assert_eq!(resp["result"]["serverInfo"]["name"], "nuthatch");
        assert_eq!(
            resp["result"]["protocolVersion"], "2025-03-26",
            "echoes client version"
        );

        let list = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" });
        let resp = handle(&list, &client, "http://127.0.0.1:1").await.unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 12);
        assert!(tools.iter().any(|t| t["name"] == "sql"));
        assert!(tools.iter().any(|t| t["name"] == "explain"));
        assert!(tools.iter().any(|t| t["name"] == "tables"));
        // The compliance tools (RFC-0008 C6).
        assert!(tools.iter().any(|t| t["name"] == "flags"));
        assert!(tools.iter().any(|t| t["name"] == "exposure"));
        assert!(tools.iter().any(|t| t["name"] == "screen_status"));
    }

    #[tokio::test]
    async fn notifications_get_no_response() {
        let client = reqwest::Client::new();
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle(&note, &client, "http://127.0.0.1:1").await.is_none());
    }
}
