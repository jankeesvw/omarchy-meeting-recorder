//! Correct the words whisper reliably mishears.
//!
//! The usual way to teach whisper a word is an initial prompt, but a prompt
//! only reaches the decoder through its text context, and text context is
//! what makes whisper loop. Measured on a 91-minute meetup recording: with no
//! context the prompt is ignored outright, and every context budget that let
//! it through fixed some mishearings and missed others while reopening the
//! loop risk.
//!
//! So vocabulary is fixed after decoding instead. Each term lists the ways it
//! actually comes out, and those are rewritten to the term that was meant. A
//! term also normalises its own spelling, so "github" comes out as "GitHub".
//! Matching is whole words only, case-insensitive, and never inside a domain,
//! a path or a hyphenated word; the longest match wins so a phrase beats a
//! word inside it.
//!
//! A variant belongs here only if it is not an ordinary word in its own
//! right. "pseudo" is how whisper hears sudo, but people say pseudo; "cloud"
//! is how it hears Claude, but people say cloud; "quick shell", "fast fetch"
//! and "flat pack" are ordinary phrases too. Those are left alone on purpose.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;

const OMARCHY: &[(&str, &[&str])] = &[
    (
        "Omarchy",
        &[
            "Omaki", "Omarchi", "Omarchie", "Omarchic", "Omachi", "Omachy", "Omarze", "Omarky",
            "Omarkey", "O'Marchy", "O Marchy", "Amarty", "Amachi",
        ],
    ),
    ("Hyprland", &["Hyperland", "Hyper land", "Hyperlands"]),
    ("Waybar", &["Way bar"]),
    ("Quickshell", &[]),
];

const LINUX: &[(&str, &[&str])] = &[
    ("Linux", &["Linox", "Linucks"]),
    ("Arch Linux", &["Arch Linox"]),
    ("Ubuntu", &["Ubunto", "Oobuntu", "Uboontu"]),
    ("Debian", &[]),
    ("NixOS", &["Nix OS", "Nicks OS"]),
    ("Wayland", &["Way land"]),
    ("PipeWire", &["Pipe wire", "Pipewire"]),
    ("PulseAudio", &["Pulse audio"]),
    ("systemd", &["System D", "SystemD"]),
    ("sudo", &[]),
    ("pacman", &["Pac-Man", "Pacman"]),
    ("AUR", &["A U R", "A.U.R."]),
    ("tmux", &["T mux", "Tee mux", "Teamux"]),
    ("btop", &["B top", "Bee top"]),
    ("fastfetch", &[]),
    ("neofetch", &["Neo fetch"]),
    ("Btrfs", &["Butter FS", "Butter F S", "ButterFS"]),
    ("Alacritty", &["Alacrity", "Alacritie"]),
    ("Ghostty", &["Ghosty"]),
    ("KDE", &[]),
    ("Flatpak", &[]),
];

const TECH: &[(&str, &[&str])] = &[
    ("Claude Code", &["Clock code", "Clawed code", "Claude code"]),
    ("OSINT", &["OSIT", "O S I N T"]),
    ("Wispr Flow", &["Whisper flow", "Whisperflow", "Wisper flow"]),
    ("GitHub", &["Git hub", "Get hub", "Github"]),
    ("GitLab", &["Git lab", "Gitlab"]),
    (
        "Kubernetes",
        &["Cooper Netties", "Kuber Nettes", "Kubernetties", "Kubernettes"],
    ),
    ("YAML", &["Yamel"]),
    ("Nginx", &["Engine X", "Engine-X", "EngineX"]),
    ("Postgres", &["Post Gres", "Postgress"]),
    ("SQLite", &["Sequel light", "SQL light", "Sequel lite"]),
    ("Ollama", &["Olama", "Olamma"]),
    ("Tailscale", &["Tail scale", "Tailscail"]),
    ("Cloudflare", &["Cloud flare"]),
    ("Vercel", &["Versel"]),
    ("Neovim", &["Neo vim", "Neovim", "NeoVim"]),
    ("VS Code", &["V S Code", "VSCode", "VS code"]),
    ("TypeScript", &["Type script", "Typescript"]),
    ("JavaScript", &["Java script", "Javascript"]),
    ("npm", &["N P M"]),
    ("API", &[]),
    ("CPU", &[]),
    ("GPU", &[]),
    ("SSD", &[]),
    ("SSH", &[]),
    ("NVMe", &["N V M E", "NVME"]),
    ("NVIDIA", &[]),
    ("LLM", &["L L M"]),
    ("macOS", &["Mac OS", "MacOS"]),
];

/// The default terms, in the order they are checked for a user override:
/// tech first, then Linux, then Omarchy itself.
fn default_vocabulary() -> impl Iterator<Item = (&'static str, &'static [&'static str])> {
    TECH.iter().chain(LINUX).chain(OMARCHY).copied()
}

/// Keeps only well-formed entries: a string term mapped to an array of
/// strings. Anything else in the user's `vocabulary` setting is ignored
/// rather than rejecting the whole thing.
fn clean(vocabulary: &serde_json::Value) -> HashMap<String, Vec<String>> {
    let mut clean = HashMap::new();
    let Some(object) = vocabulary.as_object() else {
        return clean;
    };
    for (term, heard) in object {
        let term = term.trim();
        let Some(heard) = heard.as_array() else {
            continue;
        };
        if term.is_empty() {
            continue;
        }
        let heard: Vec<String> = heard
            .iter()
            .filter_map(|h| h.as_str())
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(str::to_owned)
            .collect();
        clean.insert(term.to_owned(), heard);
    }
    clean
}

/// A char inside a word, a domain, a path or a hyphenation: nothing on either
/// side of a match may touch one of these, or it is part of something bigger.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

pub struct Corrector {
    /// Lowercased, whitespace-collapsed variant -> the term it corrects to.
    replacement: HashMap<String, String>,
    pattern: Regex,
}

impl Corrector {
    pub fn new(user_vocabulary: &serde_json::Value) -> Self {
        let mut merged: HashMap<String, Vec<String>> = HashMap::new();
        for (term, heard) in default_vocabulary() {
            merged.insert(term.to_owned(), heard.iter().map(|h| h.to_string()).collect());
        }
        for (term, heard) in clean(user_vocabulary) {
            merged.entry(term).or_default().extend(heard);
        }

        let mut replacement: HashMap<String, String> = HashMap::new();
        for (term, heard) in &merged {
            // The term itself, so its spelling is normalised too.
            for variant in std::iter::once(term.as_str()).chain(heard.iter().map(String::as_str)) {
                let key = normalize_whitespace(variant.trim()).to_lowercase();
                if !key.is_empty() {
                    replacement.insert(key, term.clone());
                }
            }
        }

        // Longest first, so "omarchi.nickstread.com" is rewritten as a whole
        // before "omarchi" on its own gets the chance.
        let mut alternatives: Vec<&String> = replacement.keys().collect();
        alternatives.sort_by_key(|k| std::cmp::Reverse(k.chars().count()));
        let body = alternatives
            .iter()
            .map(|v| {
                v.split(' ')
                    .map(regex::escape)
                    .collect::<Vec<_>>()
                    .join(r"\s+")
            })
            .collect::<Vec<_>>()
            .join("|");
        let pattern = if body.is_empty() {
            // Nothing to match at all (an empty user vocabulary would never
            // land here in practice, since the defaults are never empty).
            Regex::new(r"\z\A").expect("unmatchable pattern")
        } else {
            Regex::new(&format!("(?i:{body})")).expect("vocabulary pattern")
        };

        Corrector { replacement, pattern }
    }

    pub fn apply(&self, text: &str) -> String {
        if text.is_empty() {
            return text.to_owned();
        }
        let mut out = String::with_capacity(text.len());
        let mut last_end = 0;
        for m in self.pattern.find_iter(text) {
            if !self.boundary_ok(text, m.start(), m.end()) {
                continue;
            }
            let key = normalize_whitespace(m.as_str()).to_lowercase();
            let Some(term) = self.replacement.get(&key) else {
                continue;
            };
            out.push_str(&text[last_end..m.start()]);
            out.push_str(term);
            last_end = m.end();
        }
        out.push_str(&text[last_end..]);
        out
    }

    /// Not inside a word, a domain, a path or a hyphenation: nothing touching
    /// the match on either side that would make it part of something bigger.
    fn boundary_ok(&self, text: &str, start: usize, end: usize) -> bool {
        let before_ok = match text[..start].chars().next_back() {
            None => true,
            Some(c) => !(is_word_char(c) || matches!(c, '\'' | '.' | '/' | '@' | '-')),
        };
        if !before_ok {
            return false;
        }
        let mut after = text[end..].chars();
        match after.next() {
            None => true,
            Some(c) if is_word_char(c) || matches!(c, '\'' | '/' | '@' | '-') => false,
            Some('.') => !after.next().is_some_and(is_word_char),
            Some(_) => true,
        }
    }
}

fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

static CORRECTOR: OnceLock<Corrector> = OnceLock::new();

/// The corrector built from the defaults merged with the user's own
/// `vocabulary` setting, built once and reused for the rest of the run.
pub fn corrector() -> &'static Corrector {
    CORRECTOR.get_or_init(|| Corrector::new(&crate::settings::load_vocabulary()))
}

/// Corrects the mishearings in `text` with the shared corrector.
pub fn correct(text: &str) -> String {
    corrector().apply(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corrector_with(user: serde_json::Value) -> Corrector {
        Corrector::new(&user)
    }

    fn default_corrector() -> Corrector {
        corrector_with(serde_json::json!({}))
    }

    #[test]
    fn basic_mishearing_fix() {
        let c = default_corrector();
        assert_eq!(c.apply("I use Omarchi on my laptop"), "I use Omarchy on my laptop");
    }

    #[test]
    fn case_normalisation() {
        let c = default_corrector();
        assert_eq!(c.apply("I pushed it to github"), "I pushed it to GitHub");
    }

    #[test]
    fn multi_word_variant_with_varying_whitespace() {
        let c = default_corrector();
        assert_eq!(c.apply("open  way   bar"), "open  Waybar");
        assert_eq!(c.apply("open way bar"), "open Waybar");
    }

    #[test]
    fn no_match_inside_a_domain() {
        let c = default_corrector();
        assert_eq!(c.apply("check omarchi.example.com"), "check omarchi.example.com");
    }

    #[test]
    fn no_match_inside_a_path() {
        let c = default_corrector();
        assert_eq!(c.apply("it lives in /usr/omarchi"), "it lives in /usr/omarchi");
    }

    #[test]
    fn no_match_inside_a_hyphenation() {
        let c = default_corrector();
        assert_eq!(c.apply("the pre-omarchi days"), "the pre-omarchi days");
    }

    #[test]
    fn no_match_inside_a_word() {
        let c = default_corrector();
        // "Omarchy" is a real prefix here, but "Omarchyland" is not the term.
        assert_eq!(c.apply("Omarchyland is fictional"), "Omarchyland is fictional");
    }

    #[test]
    fn longest_match_wins() {
        let c = default_corrector();
        // "Arch Linox" beats "Linux" (from "Linox") matching only the tail.
        assert_eq!(c.apply("I run Arch Linox"), "I run Arch Linux");
    }

    #[test]
    fn user_vocabulary_merge() {
        let c = corrector_with(serde_json::json!({
            "Jankeesvw": ["Yankee's view", "Jankie's view"],
        }));
        assert_eq!(c.apply("built by Yankee's view"), "built by Jankeesvw");
        // Defaults still work alongside the user's own terms.
        assert_eq!(c.apply("running Omarchi"), "running Omarchy");
    }

    #[test]
    fn user_vocabulary_can_extend_a_default_term() {
        let c = corrector_with(serde_json::json!({
            "Omarchy": ["Omarshee"],
        }));
        assert_eq!(c.apply("I love Omarshee"), "I love Omarchy");
        // The built-in variants for that term are kept, not replaced.
        assert_eq!(c.apply("I love Omaki"), "I love Omarchy");
    }

    #[test]
    fn malformed_user_entries_are_ignored() {
        let c = corrector_with(serde_json::json!({
            "": ["Empty term"],
            "Fine": "not a list",
            "Good": ["Good variant", 42, null],
        }));
        // The one well-formed entry still works, and the non-string entries
        // inside its list are dropped rather than poisoning the whole term.
        assert_eq!(c.apply("say Good variant"), "say Good");
        // The malformed ones never made it into the pattern.
        assert_eq!(c.apply("say Empty term"), "say Empty term");
        assert_eq!(c.apply("say not a list"), "say not a list");
    }

    #[test]
    fn not_a_vocabulary_object_is_ignored() {
        let c = corrector_with(serde_json::json!(["not", "an", "object"]));
        assert_eq!(c.apply("running Omarchi"), "running Omarchy");
    }
}
