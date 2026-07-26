// MCP server: the agent-facing successor to the removed `reference-cli`.
// Exposes one read-only `search` tool over the same index the Tauri app
// watches and writes (`~/.reference/index`) — no tool to add/remove watched
// folders, by design, since folder opt-in is a deliberate human action taken
// through the app's picker, never something an agent should be able to do
// on its own (see CLAUDE.md's "opt-in, never automatic" principle).
//
// Unlike the CLI, this process stays alive for the duration of an agent
// session instead of reloading the embedding model on every invocation, so
// repeated searches mid-task are fast after the first one.
use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo,
};
use rmcp::{tool, tool_handler, tool_router, transport::stdio, ErrorData as McpError, ServerHandler, ServiceExt};
use serde::{Deserialize, Serialize};
use serde_json::json;

use reference_core::embedding::Embedder;
use reference_core::paths;
use reference_core::store::Store;
use reference_core::synthesize;

// Mirrors the Tauri app's `SYNTHESIS_CANDIDATE_POOL` (app/src-tauri/src/lib.rs):
// `synthesize()` needs a wider candidate pool to pick citations from than
// whatever small `top_k` the caller asked for, otherwise a low `top_k` (the
// MCP default is 5) starves it down to exactly the results already shown,
// same failure mode the app avoids by expanding its own search pool first.
const SYNTHESIS_CANDIDATE_POOL: usize = 50;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SearchParams {
    /// Natural language query describing behavior or intent, not a grep
    /// pattern (e.g. "where do we retry failed api calls").
    query: String,
    /// How many results to return. Defaults to 5.
    top_k: Option<usize>,
}

#[derive(Serialize)]
struct JsonHit {
    path: String,
    start_line: i32,
    end_line: i32,
    chunk_kind: String,
    score: f32,
    // `hybrid_search` already has this in hand; an MCP caller is an agent
    // that almost always needs the actual text next, not just a pointer to
    // it, unlike the app's UI where a human just clicks through. Returning
    // it here saves a follow-up Read call on every single search.
    content: String,
}

#[derive(Serialize)]
struct JsonCitation {
    path: String,
    snippet: String,
    start_line: usize,
    end_line: usize,
    chunk_kind: String,
}

#[derive(Clone)]
struct ReferenceServer {
    embedder: std::sync::Arc<Embedder>,
    store: std::sync::Arc<Store>,
    tool_router: ToolRouter<ReferenceServer>,
}

#[tool_router]
impl ReferenceServer {
    async fn new() -> Result<Self> {
        let db_uri = paths::default_db_uri();
        std::fs::create_dir_all(paths::default_app_data_dir())?;

        tracing::info!("loading embedding model (all-MiniLM-L6-v2)...");
        let embedder = Embedder::load().await?;
        let store = Store::open(&db_uri).await?;

        Ok(Self {
            embedder: std::sync::Arc::new(embedder),
            store: std::sync::Arc::new(store),
            tool_router: Self::tool_router(),
        })
    }

    #[tool(
        description = "Semantic search over the locally indexed files/code this machine has opted into watching. Prefer this over grep whenever the question describes behavior or intent rather than naming a literal string/identifier to search for."
    )]
    async fn search(
        &self,
        Parameters(SearchParams { query, top_k }): Parameters<SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let k = top_k.unwrap_or(5);

        let embedding = self
            .embedder
            .embed(&query)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let hits = self
            .store
            .hybrid_search(&query, &embedding, k.max(SYNTHESIS_CANDIDATE_POOL))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let citations = if synthesize::is_question(&query) && !hits.is_empty() {
            synthesize::synthesize(&self.embedder, &query, &hits)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
        } else {
            Vec::new()
        };

        let results: Vec<JsonHit> = hits
            .iter()
            .take(k)
            .map(|h| JsonHit {
                path: h.path.clone(),
                start_line: h.start_line,
                end_line: h.end_line,
                chunk_kind: h.chunk_kind.clone(),
                score: h.score,
                content: h.content.clone(),
            })
            .collect();
        let citations: Vec<JsonCitation> = citations
            .into_iter()
            .map(|c| JsonCitation {
                path: c.path,
                snippet: c.snippet,
                start_line: c.start_line,
                end_line: c.end_line,
                chunk_kind: c.chunk_kind,
            })
            .collect();

        let body = json!({ "results": results, "citations": citations });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            body.to_string(),
        )]))
    }
}

#[tool_handler]
impl ServerHandler for ReferenceServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder().enable_tools().build(),
        )
        .with_server_info(Implementation::from_build_env())
        .with_instructions(
            "Read-only semantic search over the reference index (~/.reference). \
             Only covers folders a human has already opted into watching via \
             the reference desktop app; this server cannot add folders."
                .to_string(),
        )
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let server = ReferenceServer::new().await?;
    let service = server.serve(stdio()).await.inspect_err(|e| {
        tracing::error!("serving error: {:?}", e);
    })?;

    service.waiting().await?;
    Ok(())
}
