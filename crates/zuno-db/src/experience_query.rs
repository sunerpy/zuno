//! Bounded natural-language query planning for project-scoped experience search.

const MAX_QUERY_CHARACTERS: usize = 2_048;
const MAX_TERMS: usize = 64;
const MAX_TERM_CHARACTERS: usize = 128;

/// Matching semantics chosen explicitly by an experience-search consumer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExperienceMatch {
    /// Recall relevant candidates from meaningful words and CJK fragments.
    #[default]
    Any,
    /// Require every literal word or CJK phrase supplied by the caller.
    All,
}

pub(crate) struct ExperienceQuery {
    pub words: Vec<String>,
    pub cjk: Vec<String>,
    pub short_cjk: Vec<String>,
    pub mode: ExperienceMatch,
}

impl ExperienceQuery {
    pub fn new(input: &str, mode: ExperienceMatch) -> Self {
        let mut query = Self {
            words: Vec::new(),
            cjk: Vec::new(),
            short_cjk: Vec::new(),
            mode,
        };
        let mut token = String::new();
        let mut cjk = false;
        for character in input.chars().take(MAX_QUERY_CHARACTERS) {
            let next_cjk = is_cjk(character);
            if !character.is_alphanumeric() && character != '_' {
                query.push(&mut token, cjk);
            } else {
                if !token.is_empty() && next_cjk != cjk {
                    query.push(&mut token, cjk);
                }
                cjk = next_cjk;
                if token.chars().count() < MAX_TERM_CHARACTERS {
                    token.push(character);
                }
            }
            if query.words.len() + query.cjk.len() + query.short_cjk.len() >= MAX_TERMS {
                break;
            }
        }
        query.push(&mut token, cjk);
        query
    }

    fn push(&mut self, token: &mut String, cjk: bool) {
        if token.is_empty() {
            return;
        }
        let value = std::mem::take(token).to_lowercase();
        if cjk {
            let characters = value.chars().collect::<Vec<_>>();
            if characters.len() < 3 {
                if characters.len() >= 2 {
                    push_unique(&mut self.short_cjk, value);
                }
            } else if self.mode == ExperienceMatch::All {
                push_unique(&mut self.cjk, value);
            } else {
                for gram in characters.windows(3) {
                    if self.cjk.len() + self.words.len() >= MAX_TERMS {
                        break;
                    }
                    push_unique(&mut self.cjk, gram.iter().collect());
                }
            }
        } else if self.mode == ExperienceMatch::All || !is_stop_word(&value) {
            push_unique(&mut self.words, value);
        }
    }

    pub fn lexical_fts(&self) -> Option<String> {
        expression(&self.words, self.mode)
    }

    pub fn cjk_fts(&self) -> Option<String> {
        expression(&self.cjk, self.mode)
    }

    /// Prefer coverage and title matches, while retaining a stable tie-break order.
    pub fn score(&self, title: &str, summary: &str, resolution: Option<&str>) -> Option<u32> {
        let title = title.to_lowercase();
        let body = format!("{summary}\n{}", resolution.unwrap_or_default()).to_lowercase();
        let mut score = 0_u32;
        let mut matched = 0_usize;
        let terms = self.words.iter().chain(&self.cjk).chain(&self.short_cjk);
        let mut total = 0;
        for term in terms {
            total += 1;
            if title.contains(term) {
                score = score.saturating_add(8);
                matched += 1;
            } else if body.contains(term) {
                score = score.saturating_add(3);
                matched += 1;
            }
        }
        if matched == 0 || (self.mode == ExperienceMatch::All && matched != total) {
            None
        } else {
            Some(score)
        }
    }
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if values.len() < MAX_TERMS && !values.contains(&value) {
        values.push(value);
    }
}

fn expression(terms: &[String], mode: ExperienceMatch) -> Option<String> {
    if terms.is_empty() {
        return None;
    }
    let separator = match mode {
        ExperienceMatch::Any => " OR ",
        ExperienceMatch::All => " AND ",
    };
    Some(
        terms
            .iter()
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(separator),
    )
}

fn is_cjk(character: char) -> bool {
    matches!(
        character,
        '\u{2e80}'..='\u{9fff}'
            | '\u{ac00}'..='\u{d7af}'
            | '\u{f900}'..='\u{faff}'
            | '\u{20000}'..='\u{323af}'
    )
}

fn is_stop_word(word: &str) -> bool {
    matches!(
        word,
        "a" | "an"
            | "and"
            | "are"
            | "as"
            | "at"
            | "be"
            | "by"
            | "can"
            | "check"
            | "could"
            | "do"
            | "does"
            | "for"
            | "from"
            | "help"
            | "how"
            | "i"
            | "in"
            | "inspect"
            | "is"
            | "it"
            | "me"
            | "my"
            | "of"
            | "on"
            | "or"
            | "our"
            | "please"
            | "should"
            | "that"
            | "the"
            | "their"
            | "this"
            | "to"
            | "using"
            | "was"
            | "we"
            | "what"
            | "when"
            | "where"
            | "which"
            | "with"
            | "would"
            | "you"
    )
}
