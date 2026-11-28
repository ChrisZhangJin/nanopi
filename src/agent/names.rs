//! Agent names: who the user sees working, in place of `a1`, `a2`.
//!
//! Models are not random: asked for "a historical figure whose work suits
//! the task", they pick the same famous name for similar work every
//! time. So each run of nanopi draws a fresh [`shortlist`] from
//! [`FIGURES`] at random, the dispatching model is shown it and picks the
//! name that suits each task from it, and an agent dispatched without a
//! usable name gets a random unused one. The id stays the agent's
//! identity everywhere (tools, reports, `/agents show a2`); the name is
//! for people, and `/agents show <name>` finds it too.

use std::sync::OnceLock;

/// The pool names are drawn from. Historical figures only (no living
/// people), from many fields, eras and places, short enough for the
/// agents strip.
pub const FIGURES: &[&str] = &[
    // Computing and information
    "Turing", "Lovelace", "Babbage", "Hopper", "Shannon", "Dijkstra", "Zuse", "Hamming",
    "von Neumann", "Church", "Backus", "McCarthy", "Ritchie", "Engelbart", "Licklider", "Wiener",
    // Mathematics
    "Euler", "Gauss", "Noether", "Euclid", "Pascal", "Leibniz", "Ramanujan", "Fourier",
    "Laplace", "Riemann", "Hilbert", "Cantor", "Galois", "Fibonacci", "Descartes", "Fermat",
    "Bernoulli", "Lagrange", "Cauchy", "Poincare", "Kolmogorov", "Markov", "Bayes", "Pythagoras",
    // Physics and astronomy
    "Newton", "Einstein", "Galileo", "Kepler", "Copernicus", "Faraday", "Maxwell", "Planck",
    "Bohr", "Feynman", "Fermi", "Curie", "Tesla", "Volta", "Boltzmann", "Heisenberg",
    "Dirac", "Rutherford", "Meitner", "Hubble", "Herschel", "Brahe", "Huygens", "Hooke",
    // Life sciences and chemistry
    "Darwin", "Mendel", "Pasteur", "Franklin", "Lavoisier", "Linnaeus", "Wallace", "Cajal",
    "Mendeleev", "Dalton", "Boyle", "Leeuwenhoek", "Nightingale", "Vesalius",
    // China
    "Zhang Heng", "Shen Kuo", "Zu Chongzhi", "Mozi", "Lu Ban", "Cai Lun", "Bi Sheng",
    "Guo Shoujing", "Li Shizhen", "Xu Guangqi", "Liu Hui", "Huang Daopo",
    // The Islamic world and India
    "Avicenna", "Al-Khwarizmi", "Al-Biruni", "Khayyam", "Aryabhata", "Brahmagupta",
    // Antiquity and the Renaissance
    "Archimedes", "Hypatia", "Aristotle", "Socrates", "da Vinci", "Humboldt",
];

/// Names offered to the model per run.
pub const SHORTLIST_LEN: usize = 12;

/// Longest name kept, in characters.
const MAX_CHARS: usize = 20;

/// A small xorshift generator; enough to shuffle a list of names.
struct Rng(u64);

impl Rng {
    /// Seeded from the process's random hasher keys and the clock, so
    /// every run differs without a `rand` dependency.
    fn new() -> Self {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        h.write_u32(std::process::id());
        Self(h.finish() | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// The pool in random order (Fisher-Yates).
    fn shuffled(&mut self) -> Vec<&'static str> {
        let mut v = FIGURES.to_vec();
        for i in (1..v.len()).rev() {
            let j = (self.next() % (i as u64 + 1)) as usize;
            v.swap(i, j);
        }
        v
    }
}

/// The whole pool in this run's random order: the first
/// [`SHORTLIST_LEN`] are offered to the model, the rest are fallbacks.
fn run_order() -> &'static [&'static str] {
    static ORDER: OnceLock<Vec<&'static str>> = OnceLock::new();
    ORDER.get_or_init(|| Rng::new().shuffled())
}

/// The names offered to the model in this run, drawn at random once.
pub fn shortlist() -> &'static [&'static str] {
    &run_order()[..SHORTLIST_LEN]
}

/// The line the system prompt carries when the `agent` tool is present.
pub fn prompt_line() -> String {
    format!(
        "Agent names for this session (give each agent you dispatch the `name` that best suits \
         its task, from this list, and never reuse one): {}.",
        shortlist().join(", ")
    )
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

/// The name an agent gets. The model's choice stands only when it is
/// one of this run's offered names and no other agent (`taken`) has it;
/// otherwise the agent gets this run's next free name. So names never
/// repeat, and are always historical figures from the pool.
pub fn choose(given: Option<&str>, taken: &[String]) -> String {
    choose_from(given, taken, run_order(), shortlist())
}

/// [`choose`] over an explicit order and offer (tests). When every name
/// in the pool is taken, they come round again numbered (`Turing 2`).
fn choose_from(given: Option<&str>, taken: &[String], order: &[&str], offered: &[&str]) -> String {
    let free = |n: &str| !taken.iter().any(|t| t.eq_ignore_ascii_case(n));
    if let Some(name) = given.and_then(clean) {
        if let Some(offer) = offered.iter().find(|o| o.eq_ignore_ascii_case(&name)) {
            if free(offer) {
                return offer.to_string();
            }
        }
    }
    (1..)
        .flat_map(|round| {
            order.iter().map(move |f| match round {
                1 => f.to_string(),
                r => format!("{f} {r}"),
            })
        })
        .find(|n| free(n))
        .expect("an unbounded sequence of distinct names")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_offered_name_wins_and_a_fallback_always_exists() {
        let order = ["Turing", "Curie", "Darwin"];
        let offered = ["Turing", "Curie"];
        assert_eq!(choose_from(Some("curie"), &[], &order, &offered), "Curie", "offered: kept, canonical case");
        assert_eq!(choose_from(Some("Darwin"), &[], &order, &offered), "Turing", "in the pool but not offered");
        assert_eq!(choose_from(Some("Marie Kondo"), &[], &order, &offered), "Turing", "not a pool figure");
        assert_eq!(choose_from(Some(" \u{1b}\n "), &[], &order, &offered), "Turing", "control-only input");
        assert_eq!(choose_from(None, &[], &order, &offered), "Turing");
        assert_eq!(clean(&"x".repeat(50)).unwrap().chars().count(), 20);
    }

    /// The bug seen in use: the model named two agents "Marie Kondo".
    /// A name another agent has is never given again, whoever asks.
    #[test]
    fn names_never_repeat_within_a_run() {
        let order = ["Turing", "Curie"];
        let offered = ["Turing", "Curie"];
        let taken = vec!["turing".to_string()];
        assert_eq!(choose_from(Some("Turing"), &taken, &order, &offered), "Curie", "taken: next free");
        assert_eq!(choose_from(None, &taken, &order, &offered), "Curie");
        let all = vec!["Turing".to_string(), "Curie".to_string()];
        assert_eq!(choose_from(Some("Curie"), &all, &order, &offered), "Turing 2");
    }

    /// Each run draws its own shortlist: different seeds give different
    /// orders, and a shortlist never repeats a name.
    #[test]
    fn runs_draw_different_shortlists() {
        let a = Rng(0x9E37_79B9_7F4A_7C15).shuffled();
        let b = Rng(0xD1B5_4A32_D192_ED03).shuffled();
        assert_ne!(a[..SHORTLIST_LEN], b[..SHORTLIST_LEN]);
        let mut seen = std::collections::HashSet::new();
        assert!(shortlist().iter().all(|n| seen.insert(*n)));
        assert_eq!(shortlist().len(), SHORTLIST_LEN);
        assert_eq!(shortlist(), shortlist(), "fixed for the run");
        assert!(prompt_line().contains(shortlist()[0]));
    }

    /// Over many draws the first name offered is spread across the pool,
    /// not stuck on one figure.
    #[test]
    fn the_first_name_is_spread_out() {
        let mut first = std::collections::HashSet::new();
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        for _ in 0..200 {
            first.insert(rng.shuffled()[0]);
        }
        assert!(first.len() > 50, "only {} distinct first names", first.len());
    }

    #[test]
    fn pool_names_are_distinct_and_short() {
        let mut seen = std::collections::HashSet::new();
        for f in FIGURES {
            assert!(seen.insert(f.to_ascii_lowercase()), "duplicate {f}");
            assert!(f.chars().count() <= MAX_CHARS);
        }
    }
}
