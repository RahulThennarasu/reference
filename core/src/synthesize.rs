use anyhow::Result;

use crate::embedding::Embedder;
use crate::store::HybridHit;

const MAX_CITED_FILES: usize = 3;
// A hit only gets cited if its score is within this fraction of the best
// hit's score — keeps noise-level matches (present only because `take(3)`
// needs *something*) out of the synthesized answer.
const RELEVANCE_CUTOFF: f32 = 0.3;
// Sentence-splitting (on '.'/'!'/'?') only makes sense for prose. Applied to
// source code it produces garbage "sentences" like `pub mod embedding;` or a
// version string chopped mid-token — so only files that look like prose are
// eligible for citation. Code still shows up fine in plain search results;
// it just shouldn't be quoted as if it were a sentence.
const PROSE_EXTENSIONS: &[&str] = &["md", "txt", "rst", "org", "adoc"];

pub struct Citation {
    pub path: String,
    pub snippet: String,
}

pub struct Answer {
    pub summary: String,
    pub citations: Vec<Citation>,
}

/// Heuristic for whether a query looks like a question rather than a
/// filename/keyword search — only question-shaped queries get a synthesized
/// answer; everything else is just the ranked file list.
pub fn is_question(query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return false;
    }
    if q.ends_with('?') {
        return true;
    }
    const STARTERS: &[&str] = &[
        "what", "who", "where", "when", "why", "how", "which", "is", "are", "does", "do", "did",
        "can", "could", "should", "would", "will",
    ];
    STARTERS
        .iter()
        .any(|w| q == *w || q.starts_with(&format!("{w} ")))
}

fn split_sentences(text: &str) -> Vec<&str> {
    text.split('\n')
        .flat_map(|line| line.split_inclusive(['.', '!', '?']))
        .map(|s| s.trim())
        // Markdown headings ("# reference") are short, title-like, and
        // essentially never the actual answer to anything — but a small
        // bi-encoder can still occasionally score one anomalously high
        // against a short query, so they need to be excluded outright
        // rather than left to lose on ranking alone.
        .filter(|s| s.len() > 3 && !s.starts_with('#'))
        .collect()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn is_prose_file(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| PROSE_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
}

/// Extractive answer synthesis: for each of the top-ranked files, picks the
/// single sentence whose embedding is closest to the query and cites it. No
/// generative model involved — every word in the answer comes verbatim from
/// a source file, kept local and fast.
pub fn synthesize(embedder: &Embedder, query: &str, hits: &[HybridHit]) -> Result<Answer> {
    let query_embedding = embedder.embed(query)?;
    let mut citations = Vec::new();

    let prose_hits: Vec<&HybridHit> = hits.iter().filter(|h| is_prose_file(&h.path)).collect();
    let top_score = prose_hits.first().map(|h| h.score).unwrap_or(0.0);
    let relevant = prose_hits
        .into_iter()
        .take(MAX_CITED_FILES)
        .take_while(|h| h.score >= top_score * RELEVANCE_CUTOFF);

    for hit in relevant {
        let sentences = split_sentences(&hit.content);
        if sentences.is_empty() {
            continue;
        }
        let owned: Vec<String> = sentences.iter().map(|s| s.to_string()).collect();
        let embeddings = embedder.embed_batch(&owned)?;

        let mut best_idx = 0;
        let mut best_score = f32::MIN;
        for (i, e) in embeddings.iter().enumerate() {
            let score = dot(&query_embedding, e);
            if score > best_score {
                best_score = score;
                best_idx = i;
            }
        }

        citations.push(Citation {
            path: hit.path.clone(),
            snippet: owned[best_idx].clone(),
        });
    }

    let summary = citations
        .iter()
        .map(|c| c.snippet.clone())
        .collect::<Vec<_>>()
        .join(" ");

    Ok(Answer { summary, citations })
}
