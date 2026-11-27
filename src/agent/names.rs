//! Agent names: who the user sees working, in place of `a1`, `a2`.
//!
//! The dispatching model names each agent after a famous historical
//! figure whose work suits the task (`Curie` for an investigation,
//! `Turing` for code). When it gives none, or an unusable one, the
//! agent is named from [`FIGURES`] by its id. The id stays the agent's
//! identity everywhere (tools, reports, `/agents show a2`); the name is
//! for people, and `/agents show Curie` finds it too.

/// Fallback names, in the order agents are numbered. Historical figures
/// only (no living people), short enough for the agents strip.
pub const FIGURES: [&str; 48] = [
    "Turing", "Curie", "Darwin", "Newton", "Lovelace", "Einstein", "Galileo", "Hopper",
    "Tesla", "Faraday", "Euler", "Gauss", "Noether", "Kepler", "Archimedes", "Euclid",
    "Pascal", "Leibniz", "Bohr", "Feynman", "Shannon", "Dijkstra", "Babbage", "Hypatia",
    "Copernicus", "Mendel", "Pasteur", "Franklin", "Maxwell", "Planck", "Fermi", "Ramanujan",
    "Zuse", "Hamming", "Nightingale", "Zhang Heng", "Shen Kuo", "Zu Chongzhi", "Mozi", "Lu Ban",
    "Socrates", "Aristotle", "Avicenna", "Al-Khwarizmi", "da Vinci", "Humboldt", "Lavoisier", "Volta",
];

/// Longest name kept, in characters.
const MAX_CHARS: usize = 20;

/// The fallback name for agent `id` (`a1` → `Turing`). Past the end of
/// the list the names repeat with a number (`Turing 2`).
pub fn default_for(id: &str) -> String {
    let n: usize = id.trim_start_matches('a').parse().unwrap_or(1).max(1);
    let figure = FIGURES[(n - 1) % FIGURES.len()];
    match (n - 1) / FIGURES.len() {
        0 => figure.to_string(),
        round => format!("{figure} {}", round + 1),
    }
}

/// A model-supplied name made safe to show: one line, no control
/// characters, collapsed spaces, at most [`MAX_CHARS`]. `None` when
/// nothing usable is left.
pub fn clean(name: &str) -> Option<String> {
    let words: Vec<&str> = name.split_whitespace().collect();
    let joined: String = words.join(" ").chars().filter(|c| !c.is_control()).collect();
    let cut: String = joined.chars().take(MAX_CHARS).collect();
    let cut = cut.trim().to_string();
    (!cut.is_empty()).then_some(cut)
}

/// The name an agent gets: the model's, when usable, else a fallback
/// that no other agent of the run (`taken`) has, starting from the one
/// its id points at.
pub fn choose(given: Option<&str>, id: &str, taken: &[String]) -> String {
    if let Some(name) = given.and_then(clean) {
        return name;
    }
    let free = |n: &String| !taken.iter().any(|t| t.eq_ignore_ascii_case(n));
    let start: usize = id.trim_start_matches('a').parse().unwrap_or(1).max(1);
    (start..start + 4 * FIGURES.len())
        .map(|n| default_for(&format!("a{n}")))
        .find(free)
        .unwrap_or_else(|| default_for(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_models_name_wins_and_a_fallback_always_exists() {
        assert_eq!(choose(Some("Curie"), "a3", &[]), "Curie");
        assert_eq!(choose(Some("  Marie \n Curie "), "a3", &[]), "Marie Curie");
        assert_eq!(choose(Some(" \u{1b}\n "), "a3", &[]), "Darwin", "control-only input falls back");
        assert_eq!(choose(None, "a1", &[]), "Turing");
        assert_eq!(choose(None, "a49", &[]), "Turing 2", "the list repeats, numbered");
        assert_eq!(choose(None, "x", &[]), "Turing");
        assert_eq!(clean(&"x".repeat(50)).unwrap().chars().count(), 20);
    }

    /// The model named an agent Curie; the next one, unnamed, would be
    /// Curie by its id. It gets the next free name instead.
    #[test]
    fn a_fallback_never_repeats_a_name_in_use() {
        let taken = vec!["Curie".to_string()];
        assert_eq!(choose(None, "a2", &taken), "Darwin");
        assert_eq!(choose(Some("Curie"), "a3", &taken), "Curie", "the model's own choice stands");
    }

    #[test]
    fn fallback_names_are_distinct_and_short() {
        let mut seen = std::collections::HashSet::new();
        for f in FIGURES {
            assert!(seen.insert(f), "duplicate {f}");
            assert!(f.chars().count() <= MAX_CHARS);
        }
    }
}
