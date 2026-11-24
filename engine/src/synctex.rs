use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

const SP_PER_BP: f64 = 65536.0 * (72.27 / 72.0);

#[derive(Debug, Clone, Serialize)]
pub struct PdfSyncTarget {
    pub page: usize,
    pub y_pt: f64,
}

#[derive(Debug, Clone, Default)]
pub struct SyncTexIndex {
    editor_line_to_pos: BTreeMap<usize, PdfSyncTarget>,
}

impl SyncTexIndex {
    /// 非圧縮 .synctex を直接パースし、複数の入力ファイル（prefix.tex / suffix.tex / job.tex）を元のエディタ行へ統合マッピングする
    pub fn parse_raw_with_offsets(content: &str, file_offsets: &[(&str, isize)]) -> Self {
        let mut tag_to_offset: HashMap<usize, isize> = HashMap::new();
        let mut current_page: usize = 0;
        let mut editor_line_to_pos: BTreeMap<usize, PdfSyncTarget> = BTreeMap::new();

        for raw_line in content.lines() {
            if let Some(rest) = raw_line.strip_prefix("Input:") {
                if let Some((tag_str, path_str)) = rest.split_once(':') {
                    if let Ok(tag) = tag_str.parse::<usize>() {
                        for &(suffix_name, offset) in file_offsets {
                            if path_str.ends_with(suffix_name) {
                                tag_to_offset.insert(tag, offset);
                                break;
                            }
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

            if current_page == 0 || tag_to_offset.is_empty() {
                continue;
            }

            let Some(first_ch) = raw_line.chars().next() else {
                continue;
            };
            if matches!(first_ch, '[' | '(' | 'v' | 'h' | 'x' | 'k' | 'g' | '$') {
                if let Some((tag, file_line, v_sp)) = parse_node_record(&raw_line[1..]) {
                    if let Some(&offset) = tag_to_offset.get(&tag) {
                        if file_line > 0 {
                            let editor_line = (file_line as isize + offset).max(1) as usize;
                            let y_pt = (v_sp as f64) / SP_PER_BP;
                            editor_line_to_pos
                                .entry(editor_line)
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
        }

        Self { editor_line_to_pos }
    }

    pub fn lookup_editor_line(&self, editor_line: usize) -> Option<PdfSyncTarget> {
        if let Some(target) = self.editor_line_to_pos.get(&editor_line) {
            return Some(target.clone());
        }

        let prev = self.editor_line_to_pos.range(..=editor_line).next_back();
        let next = self.editor_line_to_pos.range(editor_line..).next();

        match (prev, next) {
            (Some((&pl, pt)), Some((&nl, nt))) => {
                if editor_line - pl <= nl - editor_line {
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
