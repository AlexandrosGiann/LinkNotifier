//! A self-check quiz built when the observer stops.
//!
//! Every question comes from text that exists: a sentence said in the
//! meeting, an action item Needle found, or a sentence of a Wikipedia article
//! related to a topic. The models choose *what* to ask (key phrases close to
//! the topic, plausible wrong answers close to the right one); they never
//! write the facts. Wrong options are other key phrases of the same meeting
//! or its sources, other numbers, other names, other weekdays.

use std::collections::HashMap;

use serde::Serialize;

use crate::models::cosine;
use crate::notes::{clock, ItemKind, Notes, Segment};
use crate::web;

#[derive(Debug, Clone, Serialize, Default)]
pub struct Question {
    /// "meeting" or "source".
    pub origin: String,
    /// "choice", or "input" for a typed equation.
    pub kind: String,
    pub prompt: String,
    pub options: Vec<String>,
    pub answer: usize,
    pub explain: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// "input": the right answer as a function of x, in the page's expression syntax.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub expr: String,
    /// "input": the right answer as shown after answering.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub shown: String,
    /// "input": the variables of the formula.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub vars: Vec<String>,
    /// "input": names that would answer the question by themselves ("tanh").
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub forbid: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceRef {
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Quiz {
    pub id: String,
    /// "en" or "el": the language of the page and of the questions' wording.
    pub lang: String,
    pub title: String,
    pub questions: Vec<Question>,
    pub sources: Vec<SourceRef>,
}

const MAX_CONCEPTS: usize = 6;
const MAX_ACRONYMS: usize = 3;
const MAX_EQUATIONS: usize = 3;
const MAX_MEETING: usize = 4;
const MAX_SOURCE: usize = 2;
/// Long definitions are cut here so the options stay readable.
const MAX_OPTION_CHARS: usize = 220;

/// The wording of the questions, per quiz language.
struct Words {
    what_is: fn(&str, bool) -> String,
    stand_for: fn(&str) -> String,
    equation: fn(&str, &str) -> String,
    formula: fn(&str, &str, &str) -> String,
    fill: &'static str,
    from_source: fn(&str) -> String,
    said_at: fn(&str) -> String,
    by_when: fn(&str) -> String,
}

fn words(lang: &str) -> Words {
    if lang == "el" {
        Words {
            what_is: |x, _| format!("Τι είναι το {x};"),
            stand_for: |x| format!("Τι σημαίνει το {x};"),
            equation: |name, f| format!("Γράψε την εξίσωση της συνάρτησης {name}, {f} = ;"),
            formula: |name, f, vars| format!("Γράψε τον τύπο για {name}: {f} = ; (μεταβλητές: {vars})"),
            fill: "Συμπλήρωσε όπως ειπώθηκε στη συνάντηση:\n",
            from_source: |t| format!("Από τη Wikipedia ({t}):\n"),
            said_at: |t| format!("Ειπώθηκε στο [{t}]:"),
            by_when: |x| format!("Μέχρι πότε πρέπει να γίνει: {x};"),
        }
    } else {
        Words {
            what_is: |x, countable| {
                if countable {
                    format!("What is {} {x}?", a_or_an(x))
                } else {
                    format!("What is {x}?")
                }
            },
            stand_for: |x| format!("What does {x} stand for?"),
            equation: |name, f| format!("Type the equation of the {name} function, {f} = ?"),
            formula: |name, f, vars| format!("Type the formula for {name}: {f} = ? (variables: {vars})"),
            fill: "Fill in the blank, as it was said in the session:\n",
            from_source: |t| format!("From Wikipedia ({t}):\n"),
            said_at: |t| format!("Said at [{t}]:"),
            by_when: |x| format!("By when must this be done: {x}?"),
        }
    }
}

/// "an RNN", "a CNN", "an encoder".
fn a_or_an(x: &str) -> &'static str {
    let acronym = x.len() <= 6 && x.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    let first = x.chars().next().unwrap_or('x');
    let vowel_sound = if acronym { "AEFHILMNORSX".contains(first) } else { "aeiouAEIOU".contains(first) };
    if vowel_sound {
        "an"
    } else {
        "a"
    }
}

/// Formulas a lecture keeps coming back to, from machine learning to
/// physics, chemistry, finance, statistics and biology. They are fixed here
/// rather than read from a model or a page, and a typed answer is checked by
/// evaluating it, so any equivalent form counts.
struct Formula {
    /// Any of these in the transcript or the sources brings the question.
    keys: &'static [&'static str],
    /// ...but only together with one of these, when not empty: "momentum"
    /// in a meeting about sales is not physics.
    with: &'static [&'static str],
    name: &'static str,
    name_el: &'static str,
    lhs: &'static str,
    shown: &'static str,
    /// The reference, in the page's syntax and lower-case variables.
    expr: &'static str,
    /// The variables the answer may use, as shown to the user.
    vars: &'static [&'static str],
    /// "the equation of the sigmoid function" rather than "the formula for".
    function: bool,
    /// Typing these would be the answer by name, not the equation.
    forbid: &'static [&'static str],
    url: &'static str,
}

const FORMULAS: &[Formula] = &[
    Formula {
        keys: &["sigmoid", "logistic function"],
        with: &[],
        name: "sigmoid",
        name_el: "sigmoid",
        lhs: "σ(x)",
        shown: "σ(x) = 1 / (1 + e^(−x))",
        expr: "1/(1+exp(-x))",
        vars: &["x"],
        function: true,
        forbid: &["sigmoid", "sigma", "σ"],
        url: "https://en.wikipedia.org/wiki/Sigmoid_function",
    },
    Formula {
        keys: &["sigmoid"],
        with: &["derivative", "backpropagation", "back propagation", "gradient"],
        name: "sigmoid derivative",
        name_el: "παράγωγος της sigmoid",
        lhs: "σ′(x)",
        shown: "σ′(x) = σ(x) · (1 − σ(x))",
        expr: "sigmoid(x)*(1-sigmoid(x))",
        vars: &["x"],
        function: true,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Logistic_function#Derivative",
    },
    Formula {
        keys: &["tanh", "hyperbolic tangent"],
        with: &[],
        name: "tanh",
        name_el: "tanh",
        lhs: "tanh(x)",
        shown: "tanh(x) = (e^x − e^(−x)) / (e^x + e^(−x))",
        expr: "(exp(x)-exp(-x))/(exp(x)+exp(-x))",
        vars: &["x"],
        function: true,
        forbid: &["tanh", "sinh", "cosh"],
        url: "https://en.wikipedia.org/wiki/Hyperbolic_functions",
    },
    Formula {
        keys: &["relu", "relue", "rectified linear", "rectifier"],
        with: &[],
        name: "ReLU",
        name_el: "ReLU",
        lhs: "ReLU(x)",
        shown: "ReLU(x) = max(0, x)",
        expr: "max(0,x)",
        vars: &["x"],
        function: true,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Rectifier_(neural_networks)",
    },
    Formula {
        keys: &["leaky relu", "leaky re lu", "leaky rectified"],
        with: &[],
        name: "leaky ReLU (slope 0.01)",
        name_el: "leaky ReLU (κλίση 0.01)",
        lhs: "LeakyReLU(x)",
        shown: "LeakyReLU(x) = max(0.01·x, x)",
        expr: "max(0.01*x,x)",
        vars: &["x"],
        function: true,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Rectifier_(neural_networks)#Leaky_ReLU",
    },
    Formula {
        keys: &["softplus"],
        with: &[],
        name: "softplus",
        name_el: "softplus",
        lhs: "softplus(x)",
        shown: "softplus(x) = ln(1 + e^x)",
        expr: "log(1+exp(x))",
        vars: &["x"],
        function: true,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Softplus",
    },
    Formula {
        keys: &["pythagorean", "pythagoras", "hypotenuse"],
        with: &[],
        name: "the hypotenuse (Pythagorean theorem)",
        name_el: "την υποτείνουσα (Πυθαγόρειο θεώρημα)",
        lhs: "c",
        shown: "c = √(a² + b²)",
        expr: "sqrt(a^2+b^2)",
        vars: &["a", "b"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Pythagorean_theorem",
    },
    Formula {
        keys: &["area of a circle", "area of the circle", "circle area"],
        with: &[],
        name: "the area of a circle",
        name_el: "το εμβαδόν κύκλου",
        lhs: "A",
        shown: "A = π · r²",
        expr: "pi*r^2",
        vars: &["r"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Area_of_a_circle",
    },
    Formula {
        keys: &["circumference"],
        with: &[],
        name: "the circumference of a circle",
        name_el: "την περίμετρο κύκλου",
        lhs: "C",
        shown: "C = 2 · π · r",
        expr: "2*pi*r",
        vars: &["r"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Circumference",
    },
    Formula {
        keys: &["discriminant", "quadratic formula", "quadratic equation"],
        with: &[],
        name: "the discriminant of ax² + bx + c",
        name_el: "τη διακρίνουσα του ax² + bx + c",
        lhs: "Δ",
        shown: "Δ = b² − 4ac",
        expr: "b^2-4*a*c",
        vars: &["a", "b", "c"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Discriminant",
    },
    Formula {
        keys: &["second law of motion", "newton's second law", "newtons second law", "force equals mass"],
        with: &[],
        name: "Newton's second law",
        name_el: "τον δεύτερο νόμο του Νεύτωνα",
        lhs: "F",
        shown: "F = m · a",
        expr: "m*a",
        vars: &["m", "a"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Newton%27s_laws_of_motion",
    },
    Formula {
        keys: &["kinetic energy"],
        with: &[],
        name: "kinetic energy",
        name_el: "την κινητική ενέργεια",
        lhs: "E",
        shown: "E = ½ · m · v²",
        expr: "m*v^2/2",
        vars: &["m", "v"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Kinetic_energy",
    },
    Formula {
        keys: &["potential energy"],
        with: &["gravit", "height", "mass"],
        name: "gravitational potential energy",
        name_el: "τη δυναμική ενέργεια βαρύτητας",
        lhs: "E",
        shown: "E = m · g · h",
        expr: "m*g*h",
        vars: &["m", "g", "h"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Potential_energy",
    },
    Formula {
        keys: &["ohm's law", "ohms law", "ohm law"],
        with: &[],
        name: "Ohm's law",
        name_el: "τον νόμο του Ohm",
        lhs: "V",
        shown: "V = I · R",
        expr: "i*r",
        vars: &["I", "R"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Ohm%27s_law",
    },
    Formula {
        keys: &["electric power", "electrical power"],
        with: &[],
        name: "electric power",
        name_el: "την ηλεκτρική ισχύ",
        lhs: "P",
        shown: "P = V · I",
        expr: "v*i",
        vars: &["V", "I"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Electric_power",
    },
    Formula {
        keys: &["momentum"],
        with: &["mass", "velocity"],
        name: "momentum",
        name_el: "την ορμή",
        lhs: "p",
        shown: "p = m · v",
        expr: "m*v",
        vars: &["m", "v"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Momentum",
    },
    Formula {
        keys: &["density"],
        with: &["mass", "volume"],
        name: "density",
        name_el: "την πυκνότητα",
        lhs: "ρ",
        shown: "ρ = m / V",
        expr: "m/v",
        vars: &["m", "V"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Density",
    },
    Formula {
        keys: &["wave speed", "wavelength"],
        with: &["frequency"],
        name: "wave speed",
        name_el: "την ταχύτητα κύματος",
        lhs: "v",
        shown: "v = f · λ",
        expr: "f*λ",
        vars: &["f", "λ"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Wavelength",
    },
    Formula {
        keys: &["e=mc", "e = mc", "mass-energy", "mass energy equivalence"],
        with: &[],
        name: "mass–energy equivalence",
        name_el: "την ισοδυναμία μάζας–ενέργειας",
        lhs: "E",
        shown: "E = m · c²",
        expr: "m*c^2",
        vars: &["m", "c"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Mass%E2%80%93energy_equivalence",
    },
    Formula {
        keys: &["ideal gas"],
        with: &[],
        name: "the ideal gas law",
        name_el: "τον νόμο των ιδανικών αερίων",
        lhs: "P",
        shown: "P = n · R · T / V",
        expr: "n*r*t/v",
        vars: &["n", "R", "T", "V"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Ideal_gas_law",
    },
    Formula {
        keys: &["molarity", "molar concentration"],
        with: &[],
        name: "molarity",
        name_el: "τη μοριακότητα",
        lhs: "c",
        shown: "c = n / V",
        expr: "n/v",
        vars: &["n", "V"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Molar_concentration",
    },
    Formula {
        keys: &["ph value", "ph scale", "the ph of"],
        with: &[],
        name: "pH from the hydrogen ion concentration h",
        name_el: "το pH από τη συγκέντρωση ιόντων υδρογόνου h",
        lhs: "pH",
        shown: "pH = −log₁₀(h)",
        expr: "-log10(h)",
        vars: &["h"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/PH",
    },
    Formula {
        keys: &["simple interest"],
        with: &[],
        name: "simple interest",
        name_el: "τον απλό τόκο",
        lhs: "I",
        shown: "I = P · r · t",
        expr: "p*r*t",
        vars: &["P", "r", "t"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Interest",
    },
    Formula {
        keys: &["compound interest"],
        with: &[],
        name: "compound interest (yearly)",
        name_el: "τον ανατοκισμό (ετήσιο)",
        lhs: "A",
        shown: "A = P · (1 + r)^t",
        expr: "p*(1+r)^t",
        vars: &["P", "r", "t"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Compound_interest",
    },
    Formula {
        keys: &["profit"],
        with: &["revenue", "cost"],
        name: "profit",
        name_el: "το κέρδος",
        lhs: "π",
        shown: "π = R − C (revenue − cost)",
        expr: "r-c",
        vars: &["R", "C"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Profit_(economics)",
    },
    Formula {
        keys: &["return on investment"],
        with: &[],
        name: "return on investment",
        name_el: "την απόδοση επένδυσης",
        lhs: "ROI",
        shown: "ROI = (G − C) / C (gain, cost)",
        expr: "(g-c)/c",
        vars: &["G", "C"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Return_on_investment",
    },
    Formula {
        keys: &["z-score", "z score", "standard score"],
        with: &[],
        name: "the z-score",
        name_el: "το z-score",
        lhs: "z",
        shown: "z = (x − μ) / σ",
        expr: "(x-μ)/σ",
        vars: &["x", "μ", "σ"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Standard_score",
    },
    Formula {
        keys: &["body mass index", "bmi"],
        with: &[],
        name: "body mass index",
        name_el: "τον δείκτη μάζας σώματος",
        lhs: "BMI",
        shown: "BMI = m / h² (kg, m)",
        expr: "m/h^2",
        vars: &["m", "h"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Body_mass_index",
    },
    Formula {
        keys: &["exponential growth", "population growth"],
        with: &[],
        name: "exponential population growth",
        name_el: "την εκθετική αύξηση πληθυσμού",
        lhs: "N(t)",
        shown: "N(t) = N0 · e^(r·t)",
        expr: "n0*exp(r*t)",
        vars: &["N0", "r", "t"],
        function: false,
        forbid: &[],
        url: "https://en.wikipedia.org/wiki/Exponential_growth",
    },
];
const BLANK: &str = "_____";

// ------------------------------------------------------------------ key phrases

const STOPWORDS: &[&str] = &[
    // en
    "a", "about", "above", "after", "again", "all", "also", "am", "an", "and", "any", "are", "as", "at", "be", "because",
    "been", "before", "being", "between", "both", "but", "by", "can", "could", "did", "do", "does", "doing", "done", "down",
    "during", "each", "even", "every", "few", "for", "from", "further", "get", "gets", "go", "going", "good", "got", "had",
    "has", "have", "having", "he", "her", "here", "him", "his", "how", "i", "if", "in", "into", "is", "it", "its", "just",
    "know", "let", "lets", "like", "look", "looked", "made", "make", "many", "may", "me", "might", "more", "most", "much",
    "must", "my", "need", "next", "no", "not", "now", "of", "off", "ok", "okay", "on", "once", "one", "only", "or", "other",
    "our", "out", "over", "own", "really", "right", "same", "say", "see", "she", "should", "so", "some", "start", "such",
    "take", "talk", "than", "thank", "thanks", "that", "the", "their", "them", "then", "there", "these", "they", "thing",
    "things", "think", "this", "those", "through", "to", "today", "too", "under", "until", "up", "us", "use", "used",
    "using", "very", "want", "was", "way", "we", "well", "were", "what", "when", "where", "which", "while", "who", "why",
    "will", "with", "would", "yeah", "yes", "you", "your", "everyone", "everybody", "week", "time", "lot", "kind",
    "often", "also", "first", "second", "third", "two", "three", "new", "old", "great", "sure", "still",
    // de
    "der", "die", "das", "und", "ist", "nicht", "ein", "eine", "wir", "ich", "sie", "es", "zu", "mit", "auf", "für", "den",
    "dem", "von", "auch", "noch", "dass", "wird", "werden", "haben", "sind",
    // fr
    "le", "la", "les", "et", "est", "un", "une", "des", "du", "de", "nous", "vous", "il", "elle", "pour", "que", "qui",
    "dans", "pas", "sur", "avec", "ce", "cette",
    // es / it
    "el", "los", "las", "y", "es", "una", "que", "en", "por", "con", "para", "del", "lo", "il", "e", "di", "che", "per",
    "non", "sono", "gli", "della",
    // nl / pl
    "het", "een", "en", "van", "ik", "je", "wij", "niet", "op", "met", "voor", "dat", "jest", "nie", "się", "na", "to",
    "że", "w", "z", "do",
];

const WEEKDAYS: &[&str] = &["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
const MONTHS: &[&str] = &[
    "January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December",
];

/// Common verbs and adverbs: fine inside a phrase, never at its edge
/// ("neural network learns" is not a concept, "learning rate" is).
const EDGE_WORDS: &[&str] = &[
    "learns", "learn", "uses", "use", "controls", "control", "computes", "compute", "process", "processes", "reduce",
    "reduces", "detect", "detects", "explain", "explains", "share", "shares", "move", "moves", "continue", "continues",
    "instead", "usually", "means", "mean", "called", "show", "shows", "gives", "give", "makes", "works", "work",
    "becomes", "become", "needs", "says", "said", "tell", "told", "find", "found", "keep", "keeps", "put", "run", "runs",
    "try", "tries", "call", "calls", "ask", "asks", "seem", "seems", "help", "helps", "change", "changes", "include",
    "includes", "provide", "provides", "allow", "allows", "consider", "already", "always", "never", "maybe", "perhaps",
    "actually", "basically", "simply", "quite", "rather", "almost", "back", "away", "together", "here's", "there's",
    "that's", "it's", "let's", "we're", "you're", "they're", "i'm", "don't", "doesn't", "can't", "won't", "isn't",
    "aren't", "welcome", "hello", "hi", "bye", "please", "thank", "today", "tomorrow", "yesterday", "morrow",
    "important", "very", "remember", "exam", "minimizing", "minimize", "using",
    "depends", "becomes", "absorbs", "takes", "turns", "breaks", "stores", "appears", "produces", "contains",
    "four", "five", "six", "seven", "eight", "nine", "ten", "twice", "times", "larger", "smaller",
    "monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday", "week", "weeks", "month", "months",
];

fn is_stop(w: &str) -> bool {
    STOPWORDS.contains(&w.to_lowercase().as_str())
}

fn is_edge_stop(w: &str) -> bool {
    let l = w.to_lowercase();
    is_stop(&l) || EDGE_WORDS.contains(&l.as_str()) || l.ends_with("ly") || l.ends_with("ed")
}

/// Word set with plural "s" dropped, to spot two names for one thing.
fn stems(t: &str) -> Vec<String> {
    tokens(t)
        .iter()
        .map(|w| {
            let l = w.to_lowercase();
            if l.len() > 3 && l.ends_with('s') && !l.ends_with("ss") { l[..l.len() - 1].to_string() } else { l }
        })
        .collect()
}

/// One term's words contain the other's: "convolutional networks" and
/// "convolutional neural networks" are not different answers.
fn overlapping(a: &str, b: &str) -> bool {
    let (x, y) = (stems(a), stems(b));
    x.iter().all(|w| y.contains(w)) || y.iter().all(|w| x.contains(w))
}

fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '\''))
        .map(|w| w.trim_matches(|c| c == '-' || c == '\''))
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// KeyBERT-style key phrases: 1-3 word candidates without stop words at the
/// edges, ranked by MiniLM similarity to the topic, kept diverse with MMR.
pub fn keyphrases(texts: &[&str], centroid: &[f32], embed: &dyn Fn(&str) -> Vec<f32>, k: usize) -> Vec<String> {
    let mut freq: HashMap<String, usize> = HashMap::new();
    for t in texts {
        let w = tokens(t);
        for n in 1..=3 {
            for win in w.windows(n) {
                // Inside a phrase only "of" may be a stop word ("rate of change").
                let inner_stop = win[1..n.max(2) - 1].iter().any(|x| (is_stop(x) || EDGE_WORDS.contains(&x.to_lowercase().as_str())) && !x.eq_ignore_ascii_case("of"));
                if inner_stop || is_edge_stop(&win[0]) || is_edge_stop(&win[n - 1]) || win.iter().any(|x| x.chars().all(|c| c.is_numeric())) {
                    continue;
                }
                if n == 1 && win[0].chars().count() < 4 {
                    continue;
                }
                *freq.entry(win.join(" ").to_lowercase()).or_default() += 1;
            }
        }
    }
    let mut cands: Vec<(String, usize)> = freq.into_iter().collect();
    // Longer phrases first among equals: "loss function" before "loss".
    cands.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.split(' ').count().cmp(&a.0.split(' ').count())).then(a.0.cmp(&b.0)));
    cands.truncate(60);
    if centroid.is_empty() {
        return cands.into_iter().take(k).map(|c| c.0).collect();
    }
    let embs: Vec<(String, Vec<f32>, f32)> = cands
        .into_iter()
        .filter_map(|(c, _)| {
            let e = embed(&c);
            (!e.is_empty()).then(|| {
                let s = cosine(&e, centroid);
                (c, e, s)
            })
        })
        .collect();
    let mut chosen: Vec<usize> = Vec::new();
    while chosen.len() < k {
        let best = embs
            .iter()
            .enumerate()
            .filter(|(i, _)| !chosen.contains(i))
            .map(|(i, (_, e, s))| {
                let redundancy = chosen.iter().map(|&j| cosine(e, &embs[j].1)).fold(0.0f32, f32::max);
                (i, 0.7 * s - 0.3 * redundancy)
            })
            .max_by(|a, b| a.1.total_cmp(&b.1));
        match best {
            Some((i, _)) => chosen.push(i),
            None => break,
        }
    }
    chosen.into_iter().map(|i| embs[i].0.clone()).collect()
}

// ------------------------------------------------------------------ helpers

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i + 1);
            v.swap(i, j);
        }
    }
}

/// Byte range of `term` in `text`, case-insensitive, on word boundaries.
fn find_term(text: &str, term: &str) -> Option<(usize, usize)> {
    let lower_term = term.to_lowercase();
    let n = lower_term.chars().count();
    if n == 0 {
        return None;
    }
    for (i, _) in text.char_indices() {
        let rest = &text[i..];
        let end = rest.char_indices().nth(n).map_or(rest.len(), |(e, _)| e);
        if rest[..end].to_lowercase() != lower_term {
            continue;
        }
        let before = text[..i].chars().next_back().is_some_and(char::is_alphanumeric);
        let after = rest[end..].chars().next().is_some_and(char::is_alphanumeric);
        if !before && !after {
            return Some((i, i + end));
        }
    }
    None
}

/// Numbers such as 15, 2.1, 10% (with their unit sign).
fn numbers(text: &str) -> Vec<(usize, usize)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() && (i == 0 || !(b[i - 1].is_ascii_alphanumeric())) {
            let s = i;
            while i < b.len() && (b[i].is_ascii_digit() || ((b[i] == b'.' || b[i] == b',') && i + 1 < b.len() && b[i + 1].is_ascii_digit())) {
                i += 1;
            }
            if i < b.len() && b[i] == b'%' {
                i += 1;
            }
            if i >= b.len() || !b[i].is_ascii_alphabetic() {
                out.push((s, i));
            }
        } else {
            i += 1;
        }
    }
    out
}

fn number_distractors(answer: &str, others: &[String], rng: &mut Rng) -> Vec<String> {
    let pct = answer.ends_with('%');
    let core = answer.trim_end_matches('%').replace(',', ".");
    let decimals = core.split('.').nth(1).map_or(0, str::len);
    let Ok(v) = core.parse::<f64>() else { return Vec::new() };
    let mut out: Vec<String> = others.iter().filter(|o| o.ends_with('%') == pct && *o != answer).cloned().collect();
    rng.shuffle(&mut out);
    let step = if decimals > 0 { 10f64.powi(-(decimals as i32)) } else { 1.0 };
    for c in [v * 2.0, v / 2.0, v + step, v - step, v + 10.0 * step, v * 3.0, v + 5.0 * step] {
        if c > 0.0 {
            let mut s = format!("{:.*}", decimals, c);
            if pct {
                s.push('%');
            }
            out.push(s);
        }
    }
    let mut seen = vec![answer.to_string()];
    out.retain(|x| {
        let keep = !seen.contains(x);
        seen.push(x.clone());
        keep
    });
    out.truncate(3);
    out
}

/// Terms close to the answer (plausible) but not the same thing.
fn term_distractors(answer: &str, sentence: &str, pool: &[(String, Vec<f32>)], embed: &dyn Fn(&str) -> Vec<f32>) -> Vec<String> {
    let a = answer.to_lowercase();
    let ae = embed(answer);
    let s = sentence.to_lowercase();
    let mut ranked: Vec<(f32, &String)> = pool
        .iter()
        .filter(|(t, _)| {
            let t = t.to_lowercase();
            t != a && !overlapping(&t, &a) && !s.contains(&t)
        })
        .map(|(t, e)| (if ae.is_empty() || e.is_empty() { 0.0 } else { cosine(&ae, e) }, t))
        .filter(|(c, _)| *c < 0.85)
        .collect();
    ranked.sort_by(|x, y| y.0.total_cmp(&x.0));
    let mut out: Vec<String> = Vec::new();
    for (_, t) in ranked {
        if out.iter().all(|o| !overlapping(o, t)) {
            out.push(match_case(t, answer));
        }
        if out.len() == 3 {
            break;
        }
    }
    out
}

/// Gives a distractor the capitalization style of the answer.
fn match_case(t: &str, answer: &str) -> String {
    if answer.chars().next().is_some_and(char::is_uppercase) {
        let mut c = t.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    } else {
        t.to_string()
    }
}

fn mcq(rng: &mut Rng, origin: &str, prompt: String, answer: String, wrong: Vec<String>, explain: String, url: &str) -> Question {
    let mut options = vec![answer.clone()];
    options.extend(wrong);
    rng.shuffle(&mut options);
    let answer = options.iter().position(|o| *o == answer).unwrap_or(0);
    Question { origin: origin.into(), kind: "choice".into(), prompt, options, answer, explain, url: url.to_string(), ..Default::default() }
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let t: String = s.chars().take(max).collect();
    let t = t.rsplit_once(' ').map_or(t.as_str(), |(a, _)| a).trim_end_matches([',', ';', ' ']);
    format!("{t}…")
}

/// "Neural network (machine learning)" -> "Neural network".
fn plain_title(t: &str) -> String {
    t.split(" (").next().unwrap_or(t).trim().to_string()
}

/// Wikipedia capitalizes every title; "Gradient descent" reads "gradient descent"
/// mid-sentence, while "Bayes' theorem" keeps its capital.
fn common_noun(t: &str) -> String {
    let rest_has_caps = t.split_whitespace().skip(1).any(|w| w.chars().next().is_some_and(char::is_uppercase));
    let first = t.split_whitespace().next().unwrap_or("");
    let acronym = first.chars().filter(|c| c.is_uppercase()).count() > 1;
    if rest_has_caps || acronym || first.contains('\'') {
        t.to_string()
    } else {
        let mut c = t.chars();
        c.next().map(|f| f.to_lowercase().collect::<String>() + c.as_str()).unwrap_or_default()
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// A definition with the term itself hidden, so the option does not give the answer away.
fn hide_term(def: &str, src: &web::Source) -> String {
    let mut out = def.to_string();
    for term in [src.title.as_str(), src.acronym.as_str()] {
        if term.is_empty() {
            continue;
        }
        while let Some((s, e)) = find_term(&out, term) {
            out.replace_range(s..e, "…");
        }
    }
    capitalize(&cut(&out, MAX_OPTION_CHARS))
}

fn quote(s: &str) -> String {
    format!("«{}»", s.trim().trim_matches('"'))
}

// ------------------------------------------------------------------ building

struct Builder<'a> {
    rng: Rng,
    embed: &'a dyn Fn(&str) -> Vec<f32>,
    pool: Vec<(String, Vec<f32>)>,
    numbers: Vec<String>,
    used: Vec<String>,
}

impl Builder<'_> {
    /// A fill-in-the-blank question on a key phrase or number of `sentence`.
    fn cloze(&mut self, sentence: &str, prefer: &[String], origin: &str, intro: &str, explain: String, url: &str) -> Option<Question> {
        let sentence = sentence.trim().trim_matches('"');
        // The topic's own phrases first, then the rest; within each, the longest
        // match ("convolutional neural networks", not "neural networks").
        let longest_first = |mut v: Vec<String>| {
            v.retain(|t| find_term(sentence, t).is_some());
            v.sort_by_key(|t| std::cmp::Reverse(t.chars().count()));
            v
        };
        let mut terms = longest_first(prefer.to_vec());
        terms.extend(longest_first(self.pool.iter().map(|p| p.0.clone()).collect()));
        for term in terms {
            if self.used.contains(&term.to_lowercase()) {
                continue;
            }
            let Some((s, e)) = find_term(sentence, &term) else { continue };
            let answer = sentence[s..e].to_string();
            let wrong = term_distractors(&answer, sentence, &self.pool, self.embed);
            if wrong.len() < 3 {
                continue;
            }
            self.used.push(term.to_lowercase());
            let prompt = format!("{intro}{}", quote(&format!("{}{BLANK}{}", &sentence[..s], &sentence[e..])));
            return Some(mcq(&mut self.rng, origin, prompt, answer, wrong, explain, url));
        }
        for (s, e) in numbers(sentence) {
            let answer = sentence[s..e].to_string();
            if self.used.contains(&answer) {
                continue;
            }
            let others = self.numbers.clone();
            let wrong = number_distractors(&answer, &others, &mut self.rng);
            if wrong.len() < 3 {
                continue;
            }
            self.used.push(answer.clone());
            let prompt = format!("{intro}{}", quote(&format!("{}{BLANK}{}", &sentence[..s], &sentence[e..])));
            return Some(mcq(&mut self.rng, origin, prompt, answer, wrong, explain, url));
        }
        None
    }
}

/// Capitalized words that are not sentence starts, weekdays or months, and
/// never appear in lower case: likely names.
fn names(segs: &[Segment]) -> Vec<String> {
    let lower: Vec<String> = segs.iter().flat_map(|s| tokens(&s.text)).filter(|w| w.chars().next().is_some_and(char::is_lowercase)).collect();
    let mut out: Vec<String> = Vec::new();
    for s in segs {
        for (i, w) in tokens(&s.text).iter().enumerate() {
            let cap = w.chars().next().is_some_and(char::is_uppercase) && w.chars().skip(1).all(char::is_lowercase);
            let common = lower.contains(&w.to_lowercase()) || is_edge_stop(w);
            if i > 0 && cap && !common && w.chars().count() >= 3 && !WEEKDAYS.contains(&w.as_str()) && !MONTHS.contains(&w.as_str()) && !out.contains(w) {
                out.push(w.clone());
            }
        }
    }
    out
}

/// Names of people, so the quiz can leave them out: the user's own names,
/// owners of action items, people addressed ("Ruben, ..." / "..., Ruben?") and
/// capitalized words that never appear in lower case. Names Whistle wrote in
/// lower case cannot be told apart from words.
pub fn private_names(notes: &Notes) -> Vec<String> {
    let mut out: Vec<String> = names(&notes.segments);
    let candidate = |w: &str| {
        w.chars().count() >= 2 && w.chars().next().is_some_and(char::is_uppercase) && !is_edge_stop(w) && !INTERJECTIONS.contains(&w.to_lowercase().as_str())
    };
    for s in &notes.segments {
        for sent in s.text.split(['.', '!', '?']) {
            let t = sent.trim();
            if let Some((first, _)) = t.split_once(',') {
                if first.split_whitespace().count() == 1 && candidate(first) {
                    out.push(first.to_string());
                }
            }
            if let Some((_, last)) = t.rsplit_once(',') {
                let last = last.trim();
                if last.split_whitespace().count() == 1 && candidate(last) {
                    out.push(last.to_string());
                }
            }
        }
    }
    for i in &notes.items {
        out.extend(i.owner.split_whitespace().filter(|w| candidate(w)).map(str::to_string));
    }
    out.extend(notes.own_names.iter().cloned());
    let mut seen: Vec<String> = Vec::new();
    out.retain(|n| {
        let k = n.to_lowercase();
        let keep = !k.is_empty() && !seen.contains(&k);
        seen.push(k);
        keep
    });
    out
}

const INTERJECTIONS: &[&str] = &["okay", "ok", "hello", "hi", "no", "yes", "yeah", "well", "so", "thanks", "sorry", "right", "now", "and", "but", "also"];

fn redact(text: &str, names: &[String]) -> String {
    let mut t = text.to_string();
    for n in names {
        while let Some((s, e)) = find_term(&t, n) {
            t.replace_range(s..e, "…");
        }
    }
    t
}

/// Transcript text in sentences of 5 to 40 words.
fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        cur.push(c);
        if matches!(c, '.' | '!' | '?') {
            out.push(std::mem::take(&mut cur));
        }
    }
    out.push(cur);
    out.into_iter()
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| (5..=40).contains(&s.split_whitespace().count()))
        .collect()
}

/// The sentence of `text` that contains most of `about`'s words.
fn sentence_with(text: &str, about: &str) -> String {
    let words = stems(about);
    let all = split_sentences(text);
    all.into_iter()
        .max_by_key(|s| {
            let st = stems(s);
            words.iter().filter(|w| st.contains(w)).count()
        })
        .unwrap_or_else(|| text.chars().take(200).collect())
}

pub fn build(notes: &Notes, embed: &dyn Fn(&str) -> Vec<f32>, seed: u64, lang: &str) -> Quiz {
    let w = words(lang);
    let mut pool_terms: Vec<String> = notes.topics.iter().flat_map(|t| t.keyphrases.iter().cloned()).collect();
    pool_terms.extend(notes.sources.iter().filter(|s| !s.neighbor).map(|s| s.title.clone()));
    let mut seen = Vec::new();
    pool_terms.retain(|t| {
        let k = t.to_lowercase();
        let keep = !seen.contains(&k);
        seen.push(k);
        keep
    });
    // Terms the sources also use are real concepts: ask about those first.
    let source_text: String = notes.sources.iter().filter(|s| !s.neighbor).flat_map(|s| std::iter::once(&s.title).chain(&s.sentences)).map(|t| t.to_lowercase() + " ").collect();
    let corroborated = |t: &String| source_text.contains(&t.to_lowercase());
    pool_terms.sort_by_key(|t| !corroborated(t));
    let mut pool: Vec<(String, Vec<f32>)> = pool_terms.into_iter().map(|t| {
        let e = embed(&t);
        (t, e)
    }).collect();
    let mut all_numbers: Vec<String> = Vec::new();
    for t in notes.segments.iter().map(|s| s.text.as_str()).chain(notes.sources.iter().flat_map(|s| s.sentences.iter().map(String::as_str))) {
        for (s, e) in numbers(t) {
            all_numbers.push(t[s..e].to_string());
        }
    }
    let private = private_names(notes);
    pool.retain(|(t, _)| !private.iter().any(|n| find_term(t, n).is_some()));
    let mut b = Builder { rng: Rng(seed | 1), embed, pool, numbers: all_numbers, used: Vec::new() };
    let transcript: String = notes.segments.iter().map(|s| s.text.clone() + " ").collect();
    let said_lower = transcript.to_lowercase();
    let said = |x: &str| !x.is_empty() && (find_term(&transcript, x).is_some() || said_lower.contains(&x.to_lowercase()));

    // Concepts: articles about a topic, or related articles whose term was said.
    let mut concepts: Vec<&web::Source> = Vec::new();
    let mut by_score: Vec<&web::Source> = notes.sources.iter().filter(|s| !s.definition.is_empty()).collect();
    by_score.sort_by(|a, b| b.score.total_cmp(&a.score));
    for s in &by_score {
        let relevant = !s.neighbor || said(&s.title) || (s.acronym.len() >= 2 && find_term(&transcript, &s.acronym).is_some());
        if relevant && !concepts.iter().any(|c| c.title == s.title) {
            concepts.push(s);
        }
    }
    let mut all_defs: Vec<(&web::Source, Vec<f32>)> = Vec::new();
    for s in &by_score {
        if !all_defs.iter().any(|(d, _)| d.title == s.title) {
            all_defs.push((s, embed(&s.definition)));
        }
    }

    // "What is a CNN?"
    let mut concept_qs: Vec<Question> = Vec::new();
    for c in &concepts {
        if concept_qs.len() >= MAX_CONCEPTS {
            break;
        }
        // "Monday" is in Wikipedia too, but not a concept of the lecture.
        if WEEKDAYS.iter().chain(MONTHS).any(|d| d.eq_ignore_ascii_case(&plain_title(&c.title))) {
            continue;
        }
        let ce = embed(&c.definition);
        let mut others: Vec<(f32, String)> = all_defs
            .iter()
            .filter(|(d, _)| d.title != c.title && !overlapping(&d.title, &c.title))
            .map(|(d, e)| (cosine(&ce, e), hide_term(&d.definition, d)))
            // Below 0.75: a definition that close says the same thing in other words.
            .filter(|(sim, text)| *sim < 0.75 && find_term(text, &c.title).is_none() && (c.acronym.is_empty() || find_term(text, &c.acronym).is_none()))
            .collect();
        others.sort_by(|a, b| b.0.total_cmp(&a.0));
        let wrong: Vec<String> = others.into_iter().map(|o| o.1).take(3).collect();
        if wrong.len() < 3 {
            continue;
        }
        let name = if c.acronym.is_empty() { common_noun(&plain_title(&c.title)) } else { c.acronym.clone() };
        let prompt = (w.what_is)(&name, c.countable);
        let explain = format!("{}: «{}»", c.title, cut(&c.definition, 400));
        let answer = hide_term(&c.definition, c);
        concept_qs.push(mcq(&mut b.rng, "source", prompt, answer, wrong, explain, &c.url));
    }

    // "What does CNN stand for?"
    let mut acronym_qs: Vec<Question> = Vec::new();
    for c in concepts.iter().filter(|c| !c.acronym.is_empty()) {
        if acronym_qs.len() >= MAX_ACRONYMS {
            break;
        }
        let ce = embed(&c.title);
        let mut others: Vec<(f32, String)> = all_defs
            .iter()
            .filter(|(d, _)| d.title != c.title && !overlapping(&d.title, &c.title))
            .map(|(d, _)| (cosine(&ce, &embed(&d.title)), plain_title(&d.title)))
            .collect();
        others.sort_by(|a, b| b.0.total_cmp(&a.0));
        let wrong: Vec<String> = others.into_iter().map(|o| o.1).take(3).collect();
        if wrong.len() < 3 {
            continue;
        }
        let explain = format!("{}: «{}»", c.title, cut(&c.definition, 400));
        acronym_qs.push(mcq(&mut b.rng, "source", (w.stand_for)(&c.acronym), plain_title(&c.title), wrong, explain, &c.url));
    }

    // "Type the equation of the sigmoid function"
    let mut equation_qs: Vec<Question> = Vec::new();
    let everything = format!("{} {}", said_lower, source_text);
    for f in FORMULAS {
        if equation_qs.len() >= MAX_EQUATIONS {
            break;
        }
        // Whole words: "bmi" must not match "submit".
        let present = f.keys.iter().any(|k| find_term(&everything, k).is_some());
        let context = f.with.is_empty() || f.with.iter().any(|k| everything.contains(k));
        if present && context {
            let name = if lang == "el" { f.name_el } else { f.name };
            let prompt = if f.function { (w.equation)(name, f.lhs) } else { (w.formula)(name, f.lhs, &f.vars.join(", ")) };
            equation_qs.push(Question {
                origin: "source".into(),
                kind: "input".into(),
                prompt,
                vars: f.vars.iter().map(|v| v.to_string()).collect(),
                explain: f.shown.into(),
                url: f.url.into(),
                expr: f.expr.into(),
                shown: f.shown.into(),
                forbid: f.forbid.iter().map(|x| x.to_string()).collect(),
                ..Default::default()
            });
        }
    }

    let mut meeting: Vec<Question> = Vec::new();
    // Until when, for action items with a weekday. "Who" questions are not
    // asked: the quiz may be shared with the people who were in the meeting.
    for i in notes.items.iter().filter(|i| i.kind == ItemKind::Action) {
        if let Some(day) = WEEKDAYS.iter().find(|d| i.due.contains(*d)) {
            let mut wrong: Vec<String> = WEEKDAYS.iter().filter(|d| *d != day && !d.starts_with('S')).map(|d| d.to_string()).collect();
            b.rng.shuffle(&mut wrong);
            wrong.truncate(3);
            let explain = format!("{} {}", (w.said_at)(&clock(i.at)), quote(&sentence_with(&i.quote, &i.text)));
            meeting.push(mcq(&mut b.rng, "meeting", (w.by_when)(&quote(&i.text)), day.to_string(), wrong, explain, ""));
        }
    }

    // Decisions and topic key points as fill-in-the-blank, one sentence at a time.
    let mut sentences: Vec<(String, f64, Vec<String>)> = Vec::new();
    let add = |text: &str, at: f64, prefer: Vec<String>, out: &mut Vec<(String, f64, Vec<String>)>| {
        for sent in split_sentences(text) {
            if !out.iter().any(|s| s.0 == sent) && !private.iter().any(|n| find_term(&sent, n).is_some()) {
                out.push((sent, at, prefer.clone()));
            }
        }
    };
    for i in notes.items.iter().filter(|i| matches!(i.kind, ItemKind::Decision | ItemKind::FollowUp)) {
        add(&i.quote, i.at, Vec::new(), &mut sentences);
    }
    for t in &notes.topics {
        let mut prefer = t.keyphrases.clone();
        prefer.sort_by_key(|k| !corroborated(k));
        for line in t.summary.lines() {
            let text = line.trim_start_matches("- ");
            let at = notes.segments.iter().find(|s| s.text == text).map_or(t.start, |s| s.start);
            add(text, at, prefer.clone(), &mut sentences);
        }
    }
    for (text, at, prefer) in sentences {
        if meeting.len() >= MAX_MEETING {
            break;
        }
        let explain = format!("{} {}", (w.said_at)(&clock(at)), quote(&text));
        if let Some(q) = b.cloze(&text, &prefer, "meeting", w.fill, explain, "") {
            meeting.push(q);
        }
    }

    // Sentences of the sources.
    let mut from_sources: Vec<Question> = Vec::new();
    'src: for src in notes.sources.iter().filter(|s| !s.neighbor) {
        let prefer = vec![src.title.clone()];
        for sent in &src.sentences {
            if from_sources.len() >= MAX_SOURCE {
                break 'src;
            }
            let intro = (w.from_source)(&src.title);
            let explain = format!("{}: {}", src.title, quote(sent));
            if let Some(q) = b.cloze(sent, &prefer, "source", &intro, explain, &src.url) {
                from_sources.push(q);
                // One question per article keeps the quiz about the meeting.
                continue 'src;
            }
        }
    }

    let mut questions = concept_qs;
    questions.extend(acronym_qs);
    questions.extend(equation_qs);
    questions.extend(meeting);
    questions.extend(from_sources);
    // Last line of defence: no question names anyone, and explanations are redacted.
    questions.retain(|q| !std::iter::once(&q.prompt).chain(&q.options).any(|t| private.iter().any(|n| find_term(t, n).is_some())));
    for q in &mut questions {
        q.explain = redact(&q.explain, &private);
    }
    let sources = notes.sources.iter().filter(|s| !s.neighbor).map(|s| SourceRef { title: s.title.clone(), url: s.url.clone() }).collect();
    Quiz { id: format!("{seed:x}"), lang: lang.to_string(), title: format!("Quiz — {}", notes.started), questions, sources }
}

const TEMPLATE: &str = include_str!("quiz.html");

/// A single self-contained page; results stay in the browser.
pub fn html(q: &Quiz) -> String {
    let data = serde_json::to_string(q).unwrap_or_default().replace("</", "<\\/");
    TEMPLATE.replace("/*QUIZ_DATA*/null", &data).replace("{{TITLE}}", &q.title)
}
