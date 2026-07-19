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
        // Resources (RFC-0016 §6): a client can preload the schema/tables/status context without
        // burning a tool call. Each maps to an HTTP GET on the running nest.
        "resources/list" => Some(ok(id?, json!({ "resources": resource_specs() }))),
        "resources/read" => {
            let id = id?;
            let uri = req
                .pointer("/params/uri")
                .and_then(Value::as_str)
                .unwrap_or("");
            match read_resource(uri, client, base).await {
                Ok(text) => Some(ok(
                    id,
                    json!({ "contents": [{ "uri": uri, "mimeType": "text/plain", "text": text }] }),
                )),
                Err(e) => Some(err(id, -32602, &format!("{e:#}"))),
            }
        }
        // Prompts (RFC-0016 §6): canned, argument-taking analysis flows that name real tools.
        "prompts/list" => Some(ok(id?, json!({ "prompts": prompt_specs() }))),
        "prompts/get" => {
            let id = id?;
            let name = req
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let args = req
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match render_prompt(name, &args) {
                Some(result) => Some(ok(id, result)),
                None => Some(err(id, -32602, &format!("unknown prompt `{name}`"))),
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
        // Advertise exactly what we implement — tools, resources, prompts; nothing else. When a future
        // standing-queries RFC lands, `notifications` slots in here without breaking a client (§6).
        "capabilities": { "tools": {}, "resources": {}, "prompts": {} },
        "serverInfo": { "name": "nuthatch", "version": env!("CARGO_PKG_VERSION") },
    })
}

/// The resources a client may preload (RFC-0016 §6). Stable `nuthatch://…` URIs backed by the running
/// nest's HTTP surface — reading one is a GET, so a client gets the context without a tool round-trip.
fn resource_specs() -> Value {
    json!([
        { "uri": "nuthatch://schema", "name": "schema", "mimeType": "text/plain",
          "description": "The enriched data model: tables, meaning, footguns, and the hot/cold coverage seam." },
        { "uri": "nuthatch://tables", "name": "tables", "mimeType": "application/json",
          "description": "Every decoded table with its columns, Solidity types, and topic0." },
        { "uri": "nuthatch://status", "name": "status", "mimeType": "application/json",
          "description": "Index status: chain, contracts, last & sealed block." },
    ])
}

/// Resolve a `nuthatch://…` resource URI to its HTTP-backed content.
async fn read_resource(uri: &str, client: &reqwest::Client, base: &str) -> Result<String> {
    let path = match uri {
        "nuthatch://schema" => "/schema",
        "nuthatch://tables" => "/tables",
        "nuthatch://status" => "/",
        other => bail!("unknown resource `{other}`"),
    };
    get(client, &format!("{base}{path}")).await
}

/// The argument-taking prompts (RFC-0016 §6) — canned analysis flows that name real tools. Rendered
/// entirely client-side (no network), so they work the instant a client lists them.
fn prompt_specs() -> Value {
    json!([
        { "name": "profile-contract", "description": "An activity overview of the indexed contract(s).",
          "arguments": [] },
        { "name": "investigate-address", "description": "Balances, exposure, flags, and screening for one address.",
          "arguments": [ { "name": "address", "description": "The 0x address to investigate.", "required": true } ] },
        { "name": "verify-a-number", "description": "Re-derive a figure from scratch with provenance.",
          "arguments": [ { "name": "claim", "description": "The number/claim to verify.", "required": true } ] },
    ])
}

/// Render a prompt into MCP `prompts/get` result form (a list of user-role messages). Returns `None`
/// for an unknown prompt name.
fn render_prompt(name: &str, args: &Value) -> Option<Value> {
    let text = match name {
        "profile-contract" => "Give me an activity overview of this nest. First call `schema` to see \
            the tables and their meaning, then use `sql` to summarise: total events per table, the \
            block range covered, and the busiest addresses. Cite the provenance stamp in your answer."
            .to_string(),
        "investigate-address" => {
            let a = args.get("address").and_then(Value::as_str).unwrap_or("<address>");
            format!(
                "Investigate the address {a}. Use `balance` for its token balance, `exposure` for its \
                 exposure to labeled addresses, `flags` for threshold/velocity flags, and \
                 `screen_status` for sanctions hits. Then summarise the risk picture, citing blocks."
            )
        }
        "verify-a-number" => {
            let c = args.get("claim").and_then(Value::as_str).unwrap_or("<the claim>");
            format!(
                "Independently verify this claim: \"{c}\". Call `schema` first (mind the footguns — \
                 big-int columns need their `_dec` companion), write the SQL from scratch with `sql`, \
                 and report the result *with* its provenance stamp (as-of block, sealed_through) so it \
                 is citable. If your first query errors, use the returned hint to correct it."
            )
        }
        _ => return None,
    };
    Some(json!({
        "messages": [ { "role": "user", "content": { "type": "text", "text": text } } ]
    }))
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
        { "name": "sql", "description": "Run a read-only SQL query over the live tip ∪ sealed history. Each event is a DuckDB view named `{alias}__{event}` (e.g. \"usdc__transfer\") with block_number, log_index, tx_hash, address + the event's params. Call `schema` first. SELECT/WITH only. Returns a compact table + a provenance stamp; capped at `limit` rows (default 200).",
          "inputSchema": { "type": "object", "properties": { "query": { "type": "string", "description": "A SELECT or WITH query." }, "limit": { "type": "integer", "description": "Max rows to return (default 200).", "default": 200 } }, "required": ["query"] } },
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
            // Shape the result for a context window (RFC-0016 §4): a small default row cap and a
            // compact table + provenance stamp, instead of 50K rows of verbose JSON.
            let limit = args["limit"].as_u64().unwrap_or(200).to_string();
            let raw = get_query(
                client,
                &format!("{base}/sql"),
                &[("q", q), ("max_rows", &limit)],
            )
            .await?;
            Ok(format_sql_result(&raw))
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

/// Shape a `/sql` JSON response for an agent's context window (RFC-0016 §4): a compact aligned table
/// instead of verbose per-row JSON (measured ≥3× fewer tokens), truncation stated as *guidance* (an
/// agent told *why* it was cut adapts; one silently truncated reports wrong totals), and a provenance
/// stamp so the answer is citable back to content-addressed data. An error body is relayed verbatim
/// (it already carries the §3 fix hint).
fn format_sql_result(raw: &str) -> String {
    let v: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return raw.to_string(),
    };
    if let Some(err) = v.get("error").and_then(Value::as_str) {
        return err.to_string();
    }
    let rows = v
        .get("rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut out = String::new();
    if rows.is_empty() {
        out.push_str("(0 rows)\n");
    } else {
        let cols: Vec<String> = rows[0]
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        let mut w: Vec<usize> = cols.iter().map(String::len).collect();
        for r in &rows {
            if let Some(o) = r.as_object() {
                for (i, c) in cols.iter().enumerate() {
                    w[i] = w[i].max(o.get(c).map(cell).map(|s| s.len()).unwrap_or(0));
                }
            }
        }
        let pad = |s: &str, i: usize| format!("{s:<width$}", width = w[i]);
        out.push_str(
            &cols
                .iter()
                .enumerate()
                .map(|(i, c)| pad(c, i))
                .collect::<Vec<_>>()
                .join("  "),
        );
        out.push('\n');
        for r in &rows {
            if let Some(o) = r.as_object() {
                out.push_str(
                    &cols
                        .iter()
                        .enumerate()
                        .map(|(i, c)| pad(&o.get(c).map(cell).unwrap_or_default(), i))
                        .collect::<Vec<_>>()
                        .join("  "),
                );
                out.push('\n');
            }
        }
    }

    let count = v
        .get("count")
        .and_then(Value::as_u64)
        .unwrap_or(rows.len() as u64);
    if v.get("truncated").and_then(Value::as_bool).unwrap_or(false) {
        out.push_str(&format!(
            "\n… truncated at {count} rows — aggregate (GROUP BY), tighten the WHERE, or raise `limit`.\n"
        ));
    }
    if let Some(p) = v.get("provenance") {
        let as_of = p
            .get("as_of")
            .and_then(Value::as_u64)
            .map(|b| b.to_string())
            .unwrap_or_else(|| "?".into());
        let sealed = p.get("sealed_through").and_then(Value::as_u64).unwrap_or(0);
        let rh: String = p
            .get("registry_hash")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim_start_matches("0x")
            .chars()
            .take(8)
            .collect();
        out.push_str(&format!(
            "— as of block {as_of}, sealed_through {sealed}, source hot+sealed, registry {rh}\n"
        ));
    }
    out
}

/// One result cell as compact text: strings bare (no JSON quotes), null empty, everything else its
/// JSON scalar form. This is the density win over `[{"k":"v",…}]`.
fn cell(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
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
    async fn advertises_resources_and_prompts_and_lists_them() {
        let client = reqwest::Client::new();
        let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
        let resp = handle(&init, &client, "http://127.0.0.1:1").await.unwrap();
        let caps = &resp["result"]["capabilities"];
        assert!(
            caps.get("tools").is_some()
                && caps.get("resources").is_some()
                && caps.get("prompts").is_some()
        );

        let rl = json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/list" });
        let resp = handle(&rl, &client, "http://127.0.0.1:1").await.unwrap();
        let uris: Vec<&str> = resp["result"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["uri"].as_str())
            .collect();
        assert!(uris.contains(&"nuthatch://schema"));

        let pl = json!({ "jsonrpc": "2.0", "id": 3, "method": "prompts/list" });
        let resp = handle(&pl, &client, "http://127.0.0.1:1").await.unwrap();
        assert_eq!(resp["result"]["prompts"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn prompt_get_interpolates_its_argument() {
        let client = reqwest::Client::new();
        let req = json!({ "jsonrpc": "2.0", "id": 1, "method": "prompts/get",
            "params": { "name": "investigate-address", "arguments": { "address": "0xBEEF" } } });
        let resp = handle(&req, &client, "http://127.0.0.1:1").await.unwrap();
        let text = resp["result"]["messages"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(
            text.contains("0xBEEF"),
            "renders the address into the prompt"
        );
        assert!(text.contains("screen_status"), "names real tools");

        // Unknown prompt → a clean error, not a panic.
        let bad = json!({ "jsonrpc": "2.0", "id": 2, "method": "prompts/get", "params": { "name": "nope" } });
        let resp = handle(&bad, &client, "http://127.0.0.1:1").await.unwrap();
        assert!(resp.get("error").is_some());
    }

    #[test]
    fn sql_result_is_compact_with_provenance_and_smaller_than_json() {
        let raw = r#"{"count":2,"truncated":false,"rows":[{"n":10,"to":"0xabc"},{"n":5,"to":"0xdef"}],"provenance":{"as_of":100,"sealed_through":93,"source":"hot+sealed","registry_hash":"0x30ced74de367aa"}}"#;
        let out = format_sql_result(raw);
        assert!(out.contains("0xabc") && !out.contains("\"0xabc\""));
        assert!(out.contains("as of block 100"));
        assert!(out.contains("sealed_through 93"));
        assert!(out.contains("registry 30ced74d"));
        assert!(out.len() < raw.len(), "compact must beat verbose JSON");
    }

    #[test]
    fn sql_truncation_is_guidance_not_silence() {
        let raw = r#"{"count":200,"truncated":true,"rows":[{"n":1}],"provenance":{"as_of":9,"sealed_through":9,"source":"hot+sealed","registry_hash":"0xabcd1234"}}"#;
        let out = format_sql_result(raw);
        assert!(out.contains("truncated at 200 rows"));
        assert!(
            out.contains("GROUP BY") && out.contains("`limit`"),
            "tells the agent how to adapt"
        );
    }

    #[test]
    fn sql_error_body_is_relayed_verbatim() {
        let raw = r#"{"error":"Binder Error: …\n\nhint: use value_dec"}"#;
        let out = format_sql_result(raw);
        assert!(out.contains("hint: use value_dec"));
    }

    #[tokio::test]
    async fn notifications_get_no_response() {
        let client = reqwest::Client::new();
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle(&note, &client, "http://127.0.0.1:1").await.is_none());
    }
}
