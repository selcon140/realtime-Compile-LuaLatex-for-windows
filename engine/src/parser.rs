use serde::Serialize;
use sha2::{Digest, Sha256};
use tree_sitter::{Node, Parser};

const INDEPENDENT_ENVS: &[&str] = &[
    "tikzpicture",
    "circuitikz",
    "pgfplots",
    "tcolorbox",
    "equation",
    "equation*",
    "align",
    "align*",
    "figure",
    "figure*",
    "table",
    "table*",
    "axiom",
    "definition",
    "theorem",
];

#[derive(Debug, Clone, Serialize, PartialEq)]
pub enum BlockKind {
    Independent { env_name: String },
    InlineText,
}

#[derive(Debug, Clone, Serialize)]
pub struct TexBlock {
    pub id: usize,
    pub kind: BlockKind,
    pub content: String,
    pub cache_key: String,
    pub has_error: bool,
    pub start_line: usize,
    pub end_line: usize,
}

pub struct ParseResult {
    pub preamble: String,
    #[allow(dead_code)]
    pub preamble_hash: String,
    pub blocks: Vec<TexBlock>,
    pub has_fatal_error: bool,
}

pub fn parse_and_split(source: &str) -> ParseResult {
    let mut parser = Parser::new();
    parser
        .set_language(&codebook_tree_sitter_latex::LANGUAGE.into())
        .expect("Failed to load LaTeX grammar");

    let tree = parser.parse(source, None).unwrap();
    let root = tree.root_node();

    if root.has_error() {
        let mut error_blocks = Vec::new();
        collect_error_nodes(root, source, &mut error_blocks);
        return ParseResult {
            preamble: String::new(),
            preamble_hash: String::new(),
            blocks: error_blocks,
            has_fatal_error: true,
        };
    }

    let Some((preamble_end, body_start, body_end, doc_node)) = find_document_bounds(root, source)
    else {
        return ParseResult {
            preamble: String::new(),
            preamble_hash: String::new(),
            blocks: vec![TexBlock {
                id: 0,
                kind: BlockKind::InlineText,
                content: String::new(),
                cache_key: String::new(),
                has_error: true,
                start_line: 1,
                end_line: 1,
            }],
            has_fatal_error: true,
        };
    };

    let preamble = source[..preamble_end].to_string();
    let preamble_hash = compute_hash(&preamble);

    let mut indep_spans: Vec<(usize, usize, String)> = Vec::new();
    collect_independent_envs(doc_node, source, body_start, body_end, &mut indep_spans);
    indep_spans.sort_by_key(|(start, _, _)| *start);

    let mut blocks = Vec::new();
    let mut cursor_byte = body_start;
    let mut block_id = 0usize;

    for (start, end, env_name) in indep_spans {
        if start > cursor_byte {
            let gap = &source[cursor_byte..start];
            if !gap.trim().is_empty() {
                blocks.push(make_block(
                    block_id,
                    BlockKind::InlineText,
                    gap,
                    &preamble_hash,
                    byte_to_line(source, cursor_byte),
                    byte_to_line(source, start.saturating_sub(1)),
                ));
                block_id += 1;
            }
        }

        let env_text = &source[start..end];
        blocks.push(make_block(
            block_id,
            BlockKind::Independent { env_name },
            env_text,
            &preamble_hash,
            byte_to_line(source, start),
            byte_to_line(source, end.saturating_sub(1)),
        ));
        block_id += 1;
        cursor_byte = end;
    }

    if cursor_byte < body_end {
        let tail = &source[cursor_byte..body_end];
        if !tail.trim().is_empty() {
            blocks.push(make_block(
                block_id,
                BlockKind::InlineText,
                tail,
                &preamble_hash,
                byte_to_line(source, cursor_byte),
                byte_to_line(source, body_end.saturating_sub(1)),
            ));
        }
    }

    ParseResult {
        preamble,
        preamble_hash,
        blocks,
        has_fatal_error: false,
    }
}

fn find_document_bounds<'a>(
    root: Node<'a>,
    source: &str,
) -> Option<(usize, usize, usize, Node<'a>)> {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        let text = &source[child.byte_range()];
        if text.starts_with("\\begin{document}") {
            let mut inner_cursor = child.walk();
            let mut begin_node = None;
            let mut end_node = None;

            for sub in child.children(&mut inner_cursor) {
                let sub_text = &source[sub.byte_range()];
                if sub_text == "\\begin{document}" {
                    begin_node = Some(sub);
                } else if sub_text == "\\end{document}" {
                    end_node = Some(sub);
                }
            }

            if let (Some(b), Some(e)) = (begin_node, end_node) {
                return Some((b.start_byte(), b.end_byte(), e.start_byte(), child));
            }
        }
    }
    None
}

fn collect_independent_envs(
    node: Node,
    source: &str,
    body_start: usize,
    body_end: usize,
    out: &mut Vec<(usize, usize, String)>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let start = child.start_byte();
        let end = child.end_byte();
        if end <= body_start || start >= body_end {
            continue;
        }

        let text = source[start..end].trim_start();
        if let Some(env_name) = match_independent_env(text) {
            out.push((start, end, env_name));
            continue;
        }

        collect_independent_envs(child, source, body_start, body_end, out);
    }
}

fn match_independent_env(trimmed: &str) -> Option<String> {
    for &env in INDEPENDENT_ENVS {
        let prefix = format!("\\begin{{{}}}", env);
        if trimmed.starts_with(&prefix) {
            return Some(env.to_string());
        }
    }
    None
}

fn collect_error_nodes(node: Node, source: &str, out: &mut Vec<TexBlock>) {
    if node.is_error() || node.is_missing() {
        let id = out.len();
        out.push(TexBlock {
            id,
            kind: BlockKind::InlineText,
            content: source[node.byte_range()].to_string(),
            cache_key: String::new(),
            has_error: true,
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
        });
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.has_error() || child.is_error() || child.is_missing() {
            collect_error_nodes(child, source, out);
        }
    }
}

fn make_block(
    id: usize,
    kind: BlockKind,
    content: &str,
    preamble_hash: &str,
    start_line: usize,
    end_line: usize,
) -> TexBlock {
    let cache_key = compute_hash(&format!("{}:{}", preamble_hash, content));
    TexBlock {
        id,
        kind,
        content: content.to_string(),
        cache_key,
        has_error: false,
        start_line,
        end_line,
    }
}

fn byte_to_line(source: &str, byte_idx: usize) -> usize {
    let clamped = byte_idx.min(source.len());
    source[..clamped].bytes().filter(|&b| b == b'\n').count() + 1
}

fn compute_hash(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    hex::encode(hasher.finalize())
}
