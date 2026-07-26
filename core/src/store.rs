use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow_array::{
    cast::AsArray, types::Float32Type, FixedSizeListArray, Int32Array, RecordBatch,
    RecordBatchIterator, StringArray,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use lancedb::{connect, Connection, Table};

use crate::embedding::EMBEDDING_DIM;
use crate::synthesize::is_question;

const TABLE_NAME: &str = "files";

// Hybrid ranking blends three signals: semantic similarity (embedding cosine,
// since stored/query embeddings are already L2-normalized), a fuzzy filename
// match (so a literal/typo'd filename match can surface a file even when its
// embedding similarity is mediocre), and literal term overlap against the
// chunk's own body text (embedding similarity alone can rank a chunk that
// merely discusses a similar topic above one that verbatim contains the
// words the query used — this catches that case without needing an extra
// model). Weights are re-split (not just fuzzy zeroed) for question-shaped
// queries, per the same reasoning as `is_question` below: filename fuzzy
// matching stops making sense against a full sentence, but content overlap
// still does, so its share of the total stays fixed across both branches.
const SEMANTIC_WEIGHT: f32 = 0.55;
const FUZZY_WEIGHT: f32 = 0.30;
const CONTENT_MATCH_WEIGHT: f32 = 0.15;
// Soft-normalizes fuzzy-matcher's unbounded integer score into [0, 1):
// score / (score + K), so it saturates smoothly instead of clipping.
const FUZZY_SATURATION: f32 = 50.0;
// Query tokens shorter than this are dropped before content-overlap scoring
// — short words ("a", "is", "to") are too common to be a meaningful signal
// and would inflate overlap fraction for nearly any chunk.
const CONTENT_MATCH_MIN_TOKEN_LEN: usize = 3;

pub struct HybridHit {
    pub path: String,
    pub content: String,
    pub score: f32,
    pub start_line: i32,
    pub end_line: i32,
    pub chunk_kind: String,
    pub name: String,
}

/// One chunk of a file (a function, an impl block, or the whole file as a
/// fallback), already embedded and ready to write. `name` is the chunk's own
/// identifier when `chunk::extract_name` found one (empty string otherwise —
/// stored as a plain, non-nullable column like every other field here rather
/// than `Option`, matching this schema's existing convention), and is what
/// `Store::find_by_name` matches on for exact-symbol lookup.
pub struct ChunkRecord {
    pub start_line: i32,
    pub end_line: i32,
    pub kind: String,
    pub content: String,
    pub embedding: Vec<f32>,
    pub name: String,
}

pub struct Store {
    table: Table,
}

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("path", DataType::Utf8, false),
        Field::new("start_line", DataType::Int32, false),
        Field::new("end_line", DataType::Int32, false),
        Field::new("chunk_kind", DataType::Utf8, false),
        Field::new("content", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new(
            "embedding",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                EMBEDDING_DIM as i32,
            ),
            false,
        ),
    ]))
}

impl Store {
    pub async fn open(db_uri: &str) -> Result<Self> {
        let db: Connection = connect(db_uri).execute().await?;

        let existing = db.table_names().execute().await?;
        let table = if existing.iter().any(|n| n == TABLE_NAME) {
            db.open_table(TABLE_NAME).execute().await?
        } else {
            db.create_empty_table(TABLE_NAME, schema()).execute().await?
        };

        Ok(Self { table })
    }

    /// Replaces every chunk row for `path` with `chunks`. Chunking is a
    /// one-to-many relationship (one file -> N chunk rows), which
    /// `merge_insert`'s upsert semantics can't express on their own: it can
    /// add/update rows but has no way to notice that a function was deleted
    /// and its row should disappear too. So every re-index deletes all
    /// existing rows for the exact path first, then inserts the freshly
    /// parsed chunks via `merge_insert` (keyed on path + start_line) — still
    /// going through `merge_insert` per this project's write rule, just
    /// preceded by an explicit delete so stale chunks can't linger.
    pub async fn replace_chunks(&self, path: &str, chunks: Vec<ChunkRecord>) -> Result<()> {
        let escaped = path.replace('\'', "''");
        self.table.delete(&format!("path = '{escaped}'")).await?;

        if chunks.is_empty() {
            return Ok(());
        }

        let schema = schema();
        let n = chunks.len();
        let mut paths = Vec::with_capacity(n);
        let mut start_lines = Vec::with_capacity(n);
        let mut end_lines = Vec::with_capacity(n);
        let mut kinds = Vec::with_capacity(n);
        let mut contents = Vec::with_capacity(n);
        let mut names = Vec::with_capacity(n);
        let mut embeddings = Vec::with_capacity(n);
        for c in chunks {
            paths.push(path.to_string());
            start_lines.push(c.start_line);
            end_lines.push(c.end_line);
            kinds.push(c.kind);
            contents.push(c.content);
            names.push(c.name);
            embeddings.push(Some(c.embedding.into_iter().map(Some).collect::<Vec<_>>()));
        }

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(paths)),
                Arc::new(Int32Array::from(start_lines)),
                Arc::new(Int32Array::from(end_lines)),
                Arc::new(StringArray::from(kinds)),
                Arc::new(StringArray::from(contents)),
                Arc::new(StringArray::from(names)),
                Arc::new(FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
                    embeddings,
                    EMBEDDING_DIM as i32,
                )),
            ],
        )?;
        let reader = RecordBatchIterator::new(vec![Ok(batch)], schema.clone());

        let mut merge_insert = self.table.merge_insert(&["path", "start_line"]);
        merge_insert
            .when_matched_update_all(None)
            .when_not_matched_insert_all();
        merge_insert
            .execute(Box::new(reader))
            .await
            .context("merge_insert chunk upsert failed")?;

        Ok(())
    }

    /// Ranks every indexed chunk by a blend of semantic similarity (against
    /// `query_embedding`) and fuzzy filename match (against `query_text`),
    /// returning the top `k`. Scores every row in the table directly rather
    /// than going through LanceDB's ANN index, which is simple and exact at
    /// the scale of a personal file index; revisit if that stops being true
    /// now that chunking means several rows per file instead of one.
    ///
    /// `folder`, when set, scopes the scan to rows under that path — the
    /// same `path LIKE '<folder>/%'` predicate `delete_under` uses, applied
    /// at the query level via `only_if` so out-of-scope rows are never
    /// fetched at all, not filtered out after the fact. Without this, a
    /// query about one watched project can be outranked by an unrelated
    /// file from a completely different watched folder that merely shares
    /// some vocabulary — an actual observed failure mode, not a
    /// hypothetical one (see docs/mcp-agent-usage.md).
    pub async fn hybrid_search(
        &self,
        query_text: &str,
        query_embedding: &[f32],
        k: usize,
        folder: Option<&str>,
    ) -> Result<Vec<HybridHit>> {
        let mut query = self.table.query().select(Select::Columns(vec![
            "path".to_string(),
            "start_line".to_string(),
            "end_line".to_string(),
            "chunk_kind".to_string(),
            "content".to_string(),
            "name".to_string(),
            "embedding".to_string(),
        ]));
        if let Some(folder) = folder {
            let folder = folder.trim_end_matches('/').replace('\'', "''");
            query = query.only_if(format!("path LIKE '{folder}/%'"));
        }
        let batches = query.execute().await?.try_collect::<Vec<_>>().await?;

        let matcher = SkimMatcherV2::default();
        let mut hits = Vec::new();

        // Fuzzy filename matching assumes a short, filename-shaped query
        // ("store.rs", "auth"). Run it against a full natural-language
        // question instead and it degenerates into a coincidental
        // character-subsequence match against whatever filename happens to
        // be long enough to contain the query's letters in order, which can
        // outrank the file that's actually semantically relevant. Question-
        // shaped queries (per the same heuristic `synthesize` uses to decide
        // whether to answer at all) skip the fuzzy component entirely and
        // rank on semantic similarity alone.
        let fuzzy_weight = if is_question(query_text) { 0.0 } else { FUZZY_WEIGHT };
        let semantic_weight = if is_question(query_text) {
            SEMANTIC_WEIGHT + FUZZY_WEIGHT
        } else {
            SEMANTIC_WEIGHT
        };
        let query_terms = content_match_terms(query_text);

        for batch in &batches {
            let paths = batch
                .column_by_name("path")
                .context("missing path column")?
                .as_string::<i32>();
            let start_lines = batch
                .column_by_name("start_line")
                .context("missing start_line column")?
                .as_primitive::<arrow_array::types::Int32Type>();
            let end_lines = batch
                .column_by_name("end_line")
                .context("missing end_line column")?
                .as_primitive::<arrow_array::types::Int32Type>();
            let chunk_kinds = batch
                .column_by_name("chunk_kind")
                .context("missing chunk_kind column")?
                .as_string::<i32>();
            let contents = batch
                .column_by_name("content")
                .context("missing content column")?
                .as_string::<i32>();
            let names = batch
                .column_by_name("name")
                .context("missing name column")?
                .as_string::<i32>();
            let embeddings = batch
                .column_by_name("embedding")
                .context("missing embedding column")?
                .as_fixed_size_list();

            for i in 0..batch.num_rows() {
                let path = paths.value(i).to_string();
                let content = contents.value(i).to_string();
                let row_embedding = embeddings.value(i);
                let row_embedding = row_embedding.as_primitive::<Float32Type>().values();

                let semantic_sim = dot(query_embedding, row_embedding).max(0.0);

                let fuzzy_sim = if fuzzy_weight == 0.0 {
                    0.0
                } else {
                    let filename = Path::new(&path)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&path);
                    let fuzzy_raw = matcher.fuzzy_match(filename, query_text).unwrap_or(0) as f32;
                    fuzzy_raw / (fuzzy_raw + FUZZY_SATURATION)
                };

                let content_match_sim = content_match_score(&query_terms, &content);

                let score = semantic_weight * semantic_sim
                    + fuzzy_weight * fuzzy_sim
                    + CONTENT_MATCH_WEIGHT * content_match_sim;
                hits.push(HybridHit {
                    path,
                    content,
                    score,
                    start_line: start_lines.value(i),
                    end_line: end_lines.value(i),
                    chunk_kind: chunk_kinds.value(i).to_string(),
                    name: names.value(i).to_string(),
                });
            }
        }

        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(k);
        Ok(hits)
    }

    /// Exact, case-sensitive lookup by a chunk's own identifier (function,
    /// method, class, ... name — see `chunk::extract_name`) — the "find this
    /// exact function by name" path `hybrid_search`'s fuzzy/semantic/overlap
    /// blend can't guarantee on its own, since it always ranks by
    /// similarity, never by literal identity. An equality filter pushed down
    /// via `only_if` rather than a table scan, so this stays fast regardless
    /// of index size — no embedding or per-row scoring involved. Every hit
    /// gets `score = 1.0`: there's no ranking question once a match is
    /// exact, they're all equally "the thing you asked for" (e.g. same
    /// method name implemented on several types).
    pub async fn find_by_name(&self, name: &str, folder: Option<&str>) -> Result<Vec<HybridHit>> {
        let escaped = name.replace('\'', "''");
        let mut predicate = format!("name = '{escaped}'");
        if let Some(folder) = folder {
            let folder = folder.trim_end_matches('/').replace('\'', "''");
            predicate = format!("{predicate} AND path LIKE '{folder}/%'");
        }

        let batches = self
            .table
            .query()
            .select(Select::Columns(vec![
                "path".to_string(),
                "start_line".to_string(),
                "end_line".to_string(),
                "chunk_kind".to_string(),
                "content".to_string(),
                "name".to_string(),
            ]))
            .only_if(predicate)
            .execute()
            .await?
            .try_collect::<Vec<_>>()
            .await?;

        let mut hits = Vec::new();
        for batch in &batches {
            let paths = batch.column_by_name("path").context("missing path column")?.as_string::<i32>();
            let start_lines = batch
                .column_by_name("start_line")
                .context("missing start_line column")?
                .as_primitive::<arrow_array::types::Int32Type>();
            let end_lines = batch
                .column_by_name("end_line")
                .context("missing end_line column")?
                .as_primitive::<arrow_array::types::Int32Type>();
            let chunk_kinds = batch
                .column_by_name("chunk_kind")
                .context("missing chunk_kind column")?
                .as_string::<i32>();
            let contents = batch
                .column_by_name("content")
                .context("missing content column")?
                .as_string::<i32>();
            let names = batch.column_by_name("name").context("missing name column")?.as_string::<i32>();

            for i in 0..batch.num_rows() {
                hits.push(HybridHit {
                    path: paths.value(i).to_string(),
                    content: contents.value(i).to_string(),
                    score: 1.0,
                    start_line: start_lines.value(i),
                    end_line: end_lines.value(i),
                    chunk_kind: chunk_kinds.value(i).to_string(),
                    name: names.value(i).to_string(),
                });
            }
        }

        Ok(hits)
    }

    /// Deletes every indexed row whose path is under `folder`. `merge_insert`
    /// only ever adds/updates rows, so without this, un-watching a folder
    /// would stop it from being *updated* but leave everything already
    /// indexed permanently searchable — not what "un-watch" should mean.
    pub async fn delete_under(&self, folder: &str) -> Result<()> {
        let folder = folder.trim_end_matches('/').replace('\'', "''");
        let predicate = format!("path LIKE '{folder}/%'");
        self.table.delete(&predicate).await?;
        Ok(())
    }

    /// Total row count across the whole table. Chunking turned this from
    /// "one row per file" into "one row per function/class", so this is the
    /// number to watch if `hybrid_search`'s full-table scan ever needs
    /// revisiting per the note in docs/code-aware-chunking.md.
    pub async fn row_count(&self) -> Result<usize> {
        Ok(self.table.count_rows(None).await?)
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Lowercased, deduplicated query words worth checking for literal presence
/// in a chunk's content — short/common tokens are dropped since they'd match
/// almost any chunk and add noise rather than signal.
fn content_match_terms(query_text: &str) -> Vec<String> {
    let mut terms: Vec<String> = query_text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| s.len() >= CONTENT_MATCH_MIN_TOKEN_LEN)
        .map(|s| s.to_string())
        .collect();
    terms.sort_unstable();
    terms.dedup();
    terms
}

/// Fraction of `query_terms` that appear as whole words (case-insensitive)
/// in `content` — a lightweight complement to embedding similarity, which
/// can rank a chunk that merely discusses a similar topic above one that
/// verbatim contains the words actually searched for.
///
/// Word-boundary-aware, not a raw substring check: a naive `content.
/// contains("build")` matches inside "rebuild", "builds", "building" too,
/// which turned this into noise rather than signal for any query
/// containing a short, common word — a query for "how does X build Y"
/// coincidentally boosted files whose only connection was talking about
/// `cargo build`/"rebuild" in an unrelated comment.
fn content_match_score(query_terms: &[String], content: &str) -> f32 {
    if query_terms.is_empty() {
        return 0.0;
    }
    let content_lower = content.to_lowercase();
    let content_words: std::collections::HashSet<&str> = content_lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();
    let matched = query_terms
        .iter()
        .filter(|t| content_words.contains(t.as_str()))
        .count();
    matched as f32 / query_terms.len() as f32
}
