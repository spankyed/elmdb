//! A text field's value as terms (`docs/search.md`, Declaring). The standard analyzer is charabia's tokenizing and
//! normalizing — words, an identifier split on its case and underscores, case and accents folded — with a code such as
//! `THR-12` kept whole beside its parts, and a stem in place of each word when the index names a language. The exact
//! analyzer is the whole value, case folded.

use charabia::Tokenize;
use rust_stemmers::{Algorithm, Stemmer};

use crate::schema::Analyzer as Kind;

/// One term of a text, where it is: its place among the text's words, and its bytes in the text
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub text: String,
    pub position: u32,
    pub start: usize,
    pub end: usize,
}

/// A text index's analyzer
pub struct Analyzer {
    kind: Kind,
    stemmer: Option<Stemmer>,
}

/// The Snowball algorithm for a language `STEM_LANGUAGES` names
fn algorithm(language: &str) -> Option<Algorithm> {
    Some(match language {
        "ar" => Algorithm::Arabic,
        "da" => Algorithm::Danish,
        "de" => Algorithm::German,
        "el" => Algorithm::Greek,
        "en" => Algorithm::English,
        "es" => Algorithm::Spanish,
        "fi" => Algorithm::Finnish,
        "fr" => Algorithm::French,
        "hu" => Algorithm::Hungarian,
        "it" => Algorithm::Italian,
        "nl" => Algorithm::Dutch,
        "no" => Algorithm::Norwegian,
        "pt" => Algorithm::Portuguese,
        "ro" => Algorithm::Romanian,
        "ru" => Algorithm::Russian,
        "sv" => Algorithm::Swedish,
        "ta" => Algorithm::Tamil,
        "tr" => Algorithm::Turkish,
        _ => return None,
    })
}

impl Analyzer {
    pub fn new(kind: Kind, stem: Option<&str>) -> Self {
        Self { kind, stemmer: stem.and_then(algorithm).map(Stemmer::create) }
    }

    /// The text's terms in order. A code's whole term shares its first part's place, so a phrase of the parts still
    /// matches
    pub fn terms(&self, text: &str) -> Vec<Term> {
        match self.kind {
            Kind::Exact => {
                let folded = text.trim().to_lowercase();
                if folded.is_empty() {
                    return Vec::new();
                }
                let start = text.len() - text.trim_start().len();
                vec![Term { text: folded, position: 0, start, end: start + text.trim().len() }]
            }
            Kind::Standard => self.standard(text),
        }
    }

    fn standard(&self, text: &str) -> Vec<Term> {
        let mut words: Vec<Term> = Vec::new();
        for token in text.tokenize() {
            if !token.is_word() || token.lemma().is_empty() {
                continue;
            }
            words.push(Term {
                text: token.lemma().to_owned(),
                position: words.len() as u32,
                start: token.byte_start,
                end: token.byte_end,
            });
        }
        let mut out = Vec::with_capacity(words.len());
        for (at, word) in words.iter().enumerate() {
            out.push(Term { text: self.stem(&word.text), ..word.clone() });
            if let Some(next) = words.get(at + 1)
                && text.get(word.end..next.start) == Some("-")
                && word.text.chars().all(char::is_alphabetic)
                && next.text.chars().all(|c| c.is_ascii_digit())
            {
                out.push(Term {
                    text: format!("{}-{}", word.text, next.text),
                    position: word.position,
                    start: word.start,
                    end: next.end,
                });
            }
        }
        out
    }

    fn stem(&self, word: &str) -> String {
        match &self.stemmer {
            Some(stemmer) => stemmer.stem(word).into_owned(),
            None => word.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(analyzer: &Analyzer, text: &str) -> Vec<String> {
        analyzer.terms(text).into_iter().map(|t| t.text).collect()
    }

    #[test]
    fn the_standard_analyzer_folds_case_and_accents_and_splits_identifiers() {
        let a = Analyzer::new(Kind::Standard, None);
        assert_eq!(texts(&a, "Café au Lait"), ["cafe", "au", "lait"]);
        assert_eq!(
            texts(&a, "openDatabaseStore and run_history"),
            ["open", "database", "store", "and", "run", "history"]
        );
    }

    #[test]
    fn a_code_is_a_term_beside_its_parts_at_the_first_parts_place() {
        let a = Analyzer::new(Kind::Standard, None);
        let terms = a.terms("see THR-12 now");
        let code = terms.iter().find(|t| t.text == "thr-12").expect("the whole code");
        let first = terms.iter().find(|t| t.text == "thr").expect("its first part");
        assert_eq!((code.position, code.start, code.end), (first.position, 4, 10));
        assert_eq!(texts(&a, "see THR-12 now"), ["see", "thr", "thr-12", "12", "now"]);
    }

    #[test]
    fn byte_offsets_are_the_original_texts() {
        let a = Analyzer::new(Kind::Standard, None);
        let text = "naïve Résumé";
        let terms = a.terms(text);
        assert_eq!(&text[terms[1].start..terms[1].end], "Résumé");
    }

    #[test]
    fn a_language_stems_each_word_and_no_code() {
        let a = Analyzer::new(Kind::Standard, Some("en"));
        assert_eq!(texts(&a, "running runs"), ["run", "run"]);
        assert_eq!(texts(&a, "THR-12"), ["thr", "thr-12", "12"]);
    }

    #[test]
    fn the_exact_analyzer_is_the_whole_value_case_folded() {
        let a = Analyzer::new(Kind::Exact, None);
        let terms = a.terms("  Hello World ");
        assert_eq!(terms, [Term { text: "hello world".into(), position: 0, start: 2, end: 13 }]);
        assert!(a.terms("   ").is_empty());
    }
}
