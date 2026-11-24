use flate2::read::GzDecoder;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Read;

const SP_PER_BP: f64 = 65536.0 * (72.27 / 72.0);

#[derive(Debug, Clone)]
pub struct LineMapping {
    pub orig_start: usize,
    pub orig_end: usize,
    pub gen_start: usize,
    pub gen_end: usize,
    pub is_independent: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SourceMap {
    pub mappings: Vec<LineMapping>,
}

impl SourceMap {
    /// 生ソース直接コンパイル用（1対1の恒等マッピング）
    pub fn identity(total_lines: usize) -> Self {
        Self {
            mappings: vec![LineMapping {
                orig_start: 1,
                orig_end: total_lines.max(1),
                gen_start: 1,
                gen_end: total_lines.max(1),
                is_independent: false,
            }],
        }
    }

    pub fn to_generated_line(&self, orig_line: usize) -> Option<usize> {
        for m in &self.mappings {
            if orig_line >= m.orig_start && orig_line <= m.orig_end {
                if m.is_independent {
                    return Some(m.gen_start);
                }
                let offset = orig_line - m.orig_start;
                return Some((m.gen_start + offset).min(m.gen_end));
            }
        }
        self.mappings
            .iter()
            .rev()
            .find(|m| m.orig_start <= orig_line)
            .map(|m| m.gen_end)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PdfSyncTarget {
    pub page: usize,
    pub y_pt: f64,
}

#[derive(Debug, Clone, Default)]
pub struct SyncTexIndex {
    line_to_pos: BTreeMap<usize, PdfSyncTarget>,
    source_map: SourceMap,
}

impl SyncTexIndex {
    pub fn parse_gz(gz_bytes: &[u8], source_map: SourceMap) -> Result<Self, String> {
        let mut decoder = GzDecoder::new(gz_bytes);
        let mut content = String::new();
        decoder
            .read_to_string(&mut content)
            .map_err(|e| format!("Failed to decompress synctex.gz: {}", e))?;

        let mut job_tag: Option<usize> = None;
        let mut current_page: usize = 0;
        let mut line_to_pos: BTreeMap<usize, PdfSyncTarget> = BTreeMap::new();

        for raw_line in content.lines() {
            if let Some(rest) = raw_line.strip_prefix("Input:") {
                if let Some((tag_str, path_str)) = rest.split_once(':') {
                    if path_str.ends_with("job.tex") {
                        if let Ok(tag) = tag_str.parse::<usize>() {
                            job_tag = Some(tag);
                        }
                    }
                }
                continue;
            }

            if let Some(page_str) = raw_line.strip_prefix('{') {
                if let Ok(page) = page_str.parse::<usize>() {
                    current_page = page;
                }
                continue;
            }

            if current_page == 0 {
                continue;
            }
            let Some(target_tag) = job_tag else {
                continue;
            };

            let Some(first_ch) = raw_line.chars().next() else {
                continue;
            };
            if matches!(first_ch, '[' | '(' | 'v' | 'h' | 'x' | 'k' | 'g' | '$') {
                if let Some((tag, gen_line, v_sp)) = parse_node_record(&raw_line[1..]) {
                    if tag == target_tag && gen_line > 0 {
                        let y_pt = (v_sp as f64) / SP_PER_BP;
                        line_to_pos
                            .entry(gen_line)
                            .and_modify(|existing| {
                                if existing.page == current_page && y_pt < existing.y_pt {
                                    existing.y_pt = y_pt;
                                }
                            })
                            .or_insert(PdfSyncTarget {
                                page: current_page,
                                y_pt,
                            });
                    }
                }
            }
        }

        Ok(Self {
            line_to_pos,
            source_map,
        })
    }

    pub fn lookup_editor_line(&self, editor_line: usize) -> Option<PdfSyncTarget> {
        let gen_line = self.source_map.to_generated_line(editor_line)?;

        if let Some(target) = self.line_to_pos.get(&gen_line) {
            return Some(target.clone());
        }

        let prev = self.line_to_pos.range(..=gen_line).next_back();
        let next = self.line_to_pos.range(gen_line..).next();

        match (prev, next) {
            (Some((&pl, pt)), Some((&nl, nt))) => {
                if gen_line - pl <= nl - gen_line {
                    Some(pt.clone())
                } else {
                    Some(nt.clone())
                }
            }
            (Some((_, pt)), None) => Some(pt.clone()),
            (None, Some((_, nt))) => Some(nt.clone()),
            (None, None) => None,
        }
    }
}

fn parse_node_record(rest: &str) -> Option<(usize, usize, i64)> {
    let (link_part, coords_and_more) = rest.split_once(':')?;
    let mut link_iter = link_part.split(',');
    let tag: usize = link_iter.next()?.parse().ok()?;
    let line: usize = link_iter.next()?.parse().ok()?;

    let coords_part = coords_and_more.split(':').next()?;
    let (_, v_str) = coords_part.split_once(',')?;
    let v_sp: i64 = v_str.parse().ok()?;

    Some((tag, line, v_sp))
}
