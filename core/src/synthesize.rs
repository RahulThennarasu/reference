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
    pub start_line: usize,
    pub end_line: usize,
    pub chunk_kind: String,
    pub truncated: bool,
}

/// 1-indexed line number where `needle` first appears in `content`. Falls
/// back to line 1 (rather than failing) if it can't be found — worst case
/// the editor just opens at the top of the file instead of not opening.
fn line_of(content: &str, needle: &str) -> usize {
    match content.find(needle) {
        Some(byte_pos) => content[..byte_pos].matches('\n').count() + 1,
        None => 1,
    }
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

/// Splits `line` on '.'/'!'/'?' only when followed by whitespace or the end
/// of the line — a bare `.` inside a URL or version string (`marketplace.
/// visualstudio.com`, `v2.1`) has no trailing space, so it's left alone
/// instead of chopping the sentence off mid-token.
fn split_line_into_sentences(line: &str) -> Vec<&str> {
    let bytes = line.as_bytes();
    let mut sentences = Vec::new();
    let mut start = 0;

    for (i, &b) in bytes.iter().enumerate() {
        let is_terminator = matches!(b, b'.' | b'!' | b'?');
        let followed_by_boundary = i + 1 >= bytes.len() || bytes[i + 1] == b' ';
        if is_terminator && followed_by_boundary {
            sentences.push(line[start..=i].trim());
            start = i + 1;
        }
    }
    if start < line.len() {
        sentences.push(line[start..].trim());
    }
    sentences
}

fn split_sentences(text: &str) -> Vec<&str> {
    text.split('\n')
        .flat_map(split_line_into_sentences)
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

/// Any chunk kind other than "file" came from a language-aware chunker
/// (function, impl, class, interface, ...) and is already a syntactically
/// complete unit, so citing it verbatim doesn't have the incoherence
/// problem that citing a naively-split code "sentence" did. A "file" chunk
/// is either prose or an arbitrary whole-file blob for a language with no
/// chunker yet, so it doesn't qualify on its own.
fn is_code_chunk(kind: &str) -> bool {
    kind != "file"
}

/// Extractive answer synthesis: for each of the top-ranked chunks, either
/// cites the whole chunk verbatim (code — already a coherent unit) or picks
/// the single sentence whose embedding is closest to the query (prose). No
/// generative model involved — every word in the answer comes verbatim from
/// a source file, kept local and fast.
pub fn synthesize(embedder: &Embedder, query: &str, hits: &[HybridHit]) -> Result<Vec<Citation>> {
    let query_embedding = embedder.embed(query)?;
    let mut citations = Vec::new();

    let citable: Vec<&HybridHit> = hits
        .iter()
        .filter(|h| is_prose_file(&h.path) || is_code_chunk(&h.chunk_kind))
        .collect();
    let top_score = citable.first().map(|h| h.score).unwrap_or(0.0);
    let relevant = citable
        .into_iter()
        .take(MAX_CITED_FILES)
        .take_while(|h| h.score >= top_score * RELEVANCE_CUTOFF);

    for hit in relevant {
        if is_code_chunk(&hit.chunk_kind) {
            citations.push(Citation {
                path: hit.path.clone(),
                snippet: hit.content.trim().to_string(),
                start_line: hit.start_line as usize,
                end_line: hit.end_line as usize,
                chunk_kind: hit.chunk_kind.clone(),
                truncated: hit.truncated,
            });
            continue;
        }

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

        let snippet = owned[best_idx].clone();
        // `hit.content` is chunk-scoped, not file-scoped, so the line found
        // within it needs `start_line`'s offset to become a real file line
        // (today always 1 for prose since prose files aren't chunked yet,
        // but this stays correct once they are).
        let line = line_of(&hit.content, &snippet) + (hit.start_line as usize - 1);
        citations.push(Citation {
            path: hit.path.clone(),
            snippet,
            start_line: line,
            end_line: line,
            chunk_kind: hit.chunk_kind.clone(),
            truncated: hit.truncated,
        });
    }

    Ok(citations)
}

/// Finds the 1-indexed line in `content` whose text is closest to `query`,
/// for jumping straight to the relevant part of a file when opening it from
/// a plain search result (as opposed to a citation, which already has an
/// exact snippet to locate). Works line-by-line rather than sentence-by-
/// sentence, since that's what an editor actually jumps to and it holds up
/// for source code too, not just prose.
pub fn best_matching_line(embedder: &Embedder, query: &str, content: &str) -> Result<usize> {
    let query_embedding = embedder.embed(query)?;

    let lines: Vec<(usize, &str)> = content
        .lines()
        .enumerate()
        .map(|(i, line)| (i + 1, line.trim()))
        .filter(|(_, line)| line.len() > 3)
        .collect();
    if lines.is_empty() {
        return Ok(1);
    }

    let owned: Vec<String> = lines.iter().map(|(_, l)| l.to_string()).collect();
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

    Ok(lines[best_idx].0)
}
