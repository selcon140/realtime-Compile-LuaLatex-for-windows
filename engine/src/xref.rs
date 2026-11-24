use crate::parser::TexBlock;
use regex::Regex;
use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CounterState {
    pub section: usize,
    pub subsection: usize,
    pub equation: usize,
    pub figure: usize,
    pub table: usize,
    pub axiom: usize,
    pub definition: usize,
    pub theorem: usize,
}

impl CounterState {
    pub fn to_latex_setcounters(&self) -> String {
        format!(
            "\\makeatletter\n\
             \\@ifundefined{{c@section}}{{}}{{\\setcounter{{section}}{{{}}}}}\n\
             \\@ifundefined{{c@subsection}}{{}}{{\\setcounter{{subsection}}{{{}}}}}\n\
             \\@ifundefined{{c@equation}}{{}}{{\\setcounter{{equation}}{{{}}}}}\n\
             \\@ifundefined{{c@figure}}{{}}{{\\setcounter{{figure}}{{{}}}}}\n\
             \\@ifundefined{{c@table}}{{}}{{\\setcounter{{table}}{{{}}}}}\n\
             \\@ifundefined{{c@axiom}}{{}}{{\\setcounter{{axiom}}{{{}}}}}\n\
             \\@ifundefined{{c@definition}}{{}}{{\\setcounter{{definition}}{{{}}}}}\n\
             \\@ifundefined{{c@theorem}}{{}}{{\\setcounter{{theorem}}{{{}}}}}\n\
             \\makeatother\n",
            self.section,
            self.subsection,
            self.equation,
            self.figure,
            self.table,
            self.axiom,
            self.definition,
            self.theorem
        )
    }

    pub fn to_cache_token(&self) -> String {
        format!(
            "s{}_ss{}_eq{}_fig{}_tab{}_ax{}_def{}_thm{}",
            self.section,
            self.subsection,
            self.equation,
            self.figure,
            self.table,
            self.axiom,
            self.definition,
            self.theorem
        )
    }
}

#[derive(Debug, Clone)]
pub struct BlockXrefPlan {
    pub start_counters: CounterState,
    pub end_counters: CounterState,
    pub referenced_labels: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct XrefTable {
    pub labels: HashMap<String, String>,
    pub block_plans: Vec<BlockXrefPlan>,
}

impl XrefTable {
    pub fn to_latex_preload(&self) -> String {
        if self.labels.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "\\makeatletter\n\
             \\@ifpackageloaded{hyperref}{\n\
               \\newcommand{\\rt@defref}[2]{\\@namedef{r@#1}{{#2}{1}{}{}{}}}\n\
             }{\n\
               \\newcommand{\\rt@defref}[2]{\\@namedef{r@#1}{{#2}{1}}}\n\
             }\n",
        );

        let sorted: BTreeMap<_, _> = self.labels.iter().collect();
        for (key, value) in sorted {
            out.push_str(&format!("\\rt@defref{{{}}}{{{}}}\n", key, value));
        }
        out.push_str("\\makeatother\n");
        out
    }

    pub fn dependency_token_for(&self, plan: &BlockXrefPlan) -> String {
        let mut parts = Vec::with_capacity(plan.referenced_labels.len());
        for label in &plan.referenced_labels {
            let val = self.labels.get(label).map(String::as_str).unwrap_or("??");
            parts.push(format!("{}={}", label, val));
        }
        parts.join(",")
    }
}

fn re_ref() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\\(?:eq|page|c|C)?ref\{([^}]+)\}").unwrap())
}

fn re_token() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"(?P<begin>\\begin\{(?P<benv>[^}]+)\})",
            r"|(?P<end>\\end\{(?P<eenv>[^}]+)\})",
            r"|(?P<sec>\\section\b(?P<secstar>\*)?)",
            r"|(?P<subsec>\\subsection\b(?P<subsecstar>\*)?)",
            r"|(?P<captioncmd>\\caption\b(?P<capstar>\*)?)",
            r"|(?P<label>\\label\{(?P<lname>[^}]+)\})",
            r"|(?P<nonumber>\\(?:nonumber|notag)\b)",
            r"|(?P<linebreak>\\\\)",
        ))
        .unwrap()
    })
}

#[derive(Debug, Clone)]
struct EnvFrame {
    name: String,
    saved_label: Option<String>,
    align_state: Option<AlignRowState>,
}

#[derive(Debug, Clone, Default)]
struct AlignRowState {
    pending_labels: Vec<String>,
    suppressed: bool,
    has_content: bool,
}

struct XrefScanner {
    counters: CounterState,
    labels: HashMap<String, String>,
    current_label: Option<String>,
    env_stack: Vec<EnvFrame>,
}

impl XrefScanner {
    fn new() -> Self {
        Self {
            counters: CounterState::default(),
            labels: HashMap::new(),
            current_label: None,
            env_stack: Vec::new(),
        }
    }

    fn scan_block(&mut self, raw_text: &str) {
        let text = strip_latex_comments(raw_text);
        let mut last_pos = 0usize;

        for cap in re_token().captures_iter(&text) {
            let m = cap.get(0).unwrap();

            let between = &text[last_pos..m.start()];
            if !between.trim().is_empty() {
                if let Some(frame) = self.env_stack.last_mut() {
                    if let Some(align) = frame.align_state.as_mut() {
                        align.has_content = true;
                    }
                }
            }
            last_pos = m.end();

            if let Some(env_match) = cap.name("benv") {
                self.on_begin_env(env_match.as_str());
            } else if let Some(env_match) = cap.name("eenv") {
                self.on_end_env(env_match.as_str());
            } else if cap.name("sec").is_some() {
                if cap.name("secstar").is_none() {
                    self.counters.section += 1;
                    self.counters.subsection = 0;
                    self.counters.axiom = 0;
                    self.counters.definition = 0;
                    self.counters.theorem = 0;
                    self.current_label = Some(self.counters.section.to_string());
                }
            } else if cap.name("subsec").is_some() {
                if cap.name("subsecstar").is_none() {
                    self.counters.subsection += 1;
                    self.current_label =
                        Some(format!("{}.{}", self.counters.section, self.counters.subsection));
                }
            } else if cap.name("captioncmd").is_some() {
                if cap.name("capstar").is_none() {
                    self.on_caption();
                }
            } else if let Some(lname) = cap.name("lname") {
                self.on_label(lname.as_str());
            } else if cap.name("nonumber").is_some() {
                if let Some(frame) = self.env_stack.last_mut() {
                    if let Some(align) = frame.align_state.as_mut() {
                        align.suppressed = true;
                    }
                }
            } else if cap.name("linebreak").is_some() {
                if matches!(self.env_stack.last(), Some(f) if f.align_state.is_some()) {
                    self.commit_align_row();
                }
            }
        }
    }

    fn on_begin_env(&mut self, env: &str) {
        let saved_label = self.current_label.clone();
        let mut align_state = None;

        match env {
            "equation" => {
                self.counters.equation += 1;
                self.current_label = Some(self.counters.equation.to_string());
            }
            "align" => {
                align_state = Some(AlignRowState::default());
            }
            "axiom" => {
                self.counters.axiom += 1;
                self.current_label =
                    Some(format!("{}.{}", self.counters.section, self.counters.axiom));
            }
            "definition" => {
                self.counters.definition += 1;
                self.current_label =
                    Some(format!("{}.{}", self.counters.section, self.counters.definition));
            }
            "theorem" => {
                self.counters.theorem += 1;
                self.current_label =
                    Some(format!("{}.{}", self.counters.section, self.counters.theorem));
            }
            _ => {}
        }

        if let Some(parent) = self.env_stack.last_mut() {
            if let Some(align) = parent.align_state.as_mut() {
                align.has_content = true;
            }
        }

        self.env_stack.push(EnvFrame {
            name: env.to_string(),
            saved_label,
            align_state,
        });
    }

    fn on_end_env(&mut self, env: &str) {
        if matches!(self.env_stack.last(), Some(f) if f.name == env && f.align_state.is_some()) {
            self.commit_align_row();
        }

        if let Some(pos) = self.env_stack.iter().rposition(|f| f.name == env) {
            let popped = self.env_stack.drain(pos..).next().unwrap();
            self.current_label = popped.saved_label;
        }
    }

    fn on_caption(&mut self) {
        for frame in self.env_stack.iter().rev() {
            match frame.name.as_str() {
                "figure" | "figure*" => {
                    self.counters.figure += 1;
                    self.current_label = Some(self.counters.figure.to_string());
                    return;
                }
                "table" | "table*" => {
                    self.counters.table += 1;
                    self.current_label = Some(self.counters.table.to_string());
                    return;
                }
                _ => {}
            }
        }
    }

    fn on_label(&mut self, label_name: &str) {
        if let Some(frame) = self.env_stack.last_mut() {
            if let Some(align) = frame.align_state.as_mut() {
                align.pending_labels.push(label_name.to_string());
                align.has_content = true;
                return;
            }
        }

        if let Some(ref current) = self.current_label {
            self.labels.insert(label_name.to_string(), current.clone());
        }
    }

    fn commit_align_row(&mut self) {
        let Some(frame) = self.env_stack.last_mut() else {
            return;
        };
        let Some(align) = frame.align_state.as_mut() else {
            return;
        };

        let should_number = align.has_content && !align.suppressed;
        let pending = std::mem::take(&mut align.pending_labels);
        align.suppressed = false;
        align.has_content = false;

        if should_number {
            self.counters.equation += 1;
            let num_str = self.counters.equation.to_string();
            for lbl in pending {
                self.labels.insert(lbl, num_str.clone());
            }
        }
    }
}

fn strip_latex_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.lines() {
        let bytes = line.as_bytes();
        let mut cut = bytes.len();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] == b'%' {
                cut = i;
                break;
            }
            i += 1;
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

pub fn resolve_document_xref(blocks: &[TexBlock]) -> XrefTable {
    let mut scanner = XrefScanner::new();
    let mut block_plans = Vec::with_capacity(blocks.len());

    for block in blocks {
        let start_counters = scanner.counters.clone();

        let mut referenced_labels: Vec<String> = re_ref()
            .captures_iter(&block.content)
            .map(|c| c[1].to_string())
            .collect();
        referenced_labels.sort();
        referenced_labels.dedup();

        scanner.scan_block(&block.content);

        block_plans.push(BlockXrefPlan {
            start_counters,
            end_counters: scanner.counters.clone(),
            referenced_labels,
        });
    }

    XrefTable {
        labels: scanner.labels,
        block_plans,
    }
}
