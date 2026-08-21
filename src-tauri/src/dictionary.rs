//! The personal dictionary: names and jargon the recogniser keeps getting wrong.
//!
//! Originally this was going to use whisper-server's `--prompt` to bias
//! decoding toward supplied vocabulary. Parakeet, now the default backend, has
//! no equivalent - so the dictionary lives in the cleanup prompt instead.
//!
//! That turned out to be the better place for it. Token-level biasing nudges
//! the acoustic decode and can only help with words it half-heard; an
//! instruction model sees the whole sentence and can fix "the kuber netes
//! cluster" or "email Prea" from context. It also works whichever ASR backend
//! is selected.

use std::path::PathBuf;

/// Terms the user has told us about, in the order they appear in the file.
#[derive(Debug, Clone, Default)]
pub struct Dictionary {
    terms: Vec<String>,
}

impl Dictionary {
    /// Read the dictionary, creating it with a comment header if absent.
    ///
    /// Never fails: a missing or unreadable dictionary just means no terms.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            tracing::warn!("no LOCALAPPDATA; personal dictionary disabled");
            return Self::default();
        };

        if !path.exists() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, TEMPLATE);
            tracing::info!(path = %path.display(), "created empty personal dictionary");
            return Self::default();
        }

        match std::fs::read_to_string(&path) {
            Ok(body) => {
                let d = Self::parse(&body);
                tracing::info!(
                    path = %path.display(), terms = d.terms.len(), "loaded personal dictionary"
                );
                d
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "could not read dictionary");
                Self::default()
            }
        }
    }

    /// Build a dictionary from the contents of a dictionary file, without
    /// touching disk. Used by the cleanup harness so its results do not depend
    /// on whatever the machine's real dictionary happens to contain.
    pub fn from_text(body: &str) -> Self {
        Self::parse(body)
    }

    pub fn path() -> Option<PathBuf> {
        let base = std::env::var_os("LOCALAPPDATA")?;
        Some(PathBuf::from(base).join("FlowState").join("dictionary.txt"))
    }

    fn parse(body: &str) -> Self {
        let mut terms = Vec::new();
        for line in body.lines() {
            let t = line.split('#').next().unwrap_or("").trim();
            if t.is_empty() {
                continue;
            }
            // De-duplicate case-insensitively but keep the user's own casing,
            // which is the whole point - "Kubernetes", not "kubernetes".
            if !terms.iter().any(|e: &String| e.eq_ignore_ascii_case(t)) {
                terms.push(t.to_string());
            }
        }
        Self { terms }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    pub fn len(&self) -> usize {
        self.terms.len()
    }

    /// The line appended to the cleanup system prompt, or empty when there is
    /// nothing to say.
    ///
    /// This goes in the *system* prompt deliberately. It is stable for the
    /// lifetime of the process, so it stays inside llama.cpp's cached prefix
    /// and costs nothing per request. Anything that varies per utterance has to
    /// go in the final turn instead, or it would invalidate that cache.
    pub fn prompt_line(&self) -> String {
        if self.terms.is_empty() {
            return String::new();
        }
        format!(
            "\n\nThe speaker uses these names and terms. If a word in the transcript is \
             clearly a mishearing of one of them, correct it to this exact spelling. Do not \
             insert these words where the speaker did not say something like them: {}.",
            self.terms.join(", ")
        )
    }
}

const TEMPLATE: &str = "\
# FlowState personal dictionary
#
# One name or term per line. These are given to the cleanup model, which will
# correct mishearings to the exact spelling written here - so capitalise them
# the way you want them typed.
#
# Good candidates: colleagues' names, product and project names, acronyms,
# library names, anything the recogniser keeps mangling.
#
# Lines starting with # are ignored, as is anything after a # on a line.
#
# Examples, delete these:
# Kubernetes
# FlowState
# Anthropic
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let d = Dictionary::parse("# a comment\n\n  Kubernetes  \n\nFlowState # trailing\n");
        assert_eq!(d.terms, vec!["Kubernetes", "FlowState"]);
    }

    #[test]
    fn user_casing_is_preserved_and_duplicates_dropped() {
        let d = Dictionary::parse("Priya\npriya\nPRIYA\n");
        assert_eq!(d.terms, vec!["Priya"], "first spelling wins, later dupes ignored");
    }

    #[test]
    fn an_empty_dictionary_contributes_nothing_to_the_prompt() {
        assert!(Dictionary::parse("# only comments\n").prompt_line().is_empty());
        assert!(Dictionary::default().prompt_line().is_empty());
    }

    #[test]
    fn prompt_line_lists_every_term() {
        let line = Dictionary::parse("Kubernetes\nPriya\n").prompt_line();
        assert!(line.contains("Kubernetes"));
        assert!(line.contains("Priya"));
        // Must warn against inserting unrelated terms, or the model starts
        // sprinkling dictionary words into unrelated sentences.
        assert!(line.contains("Do not"));
    }
}
