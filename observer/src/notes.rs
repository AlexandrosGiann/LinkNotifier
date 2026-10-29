//! The note taker: turns transcript segments and screen keyframes into notes.
//!
//! - MiniLM embeds every segment. It drops the microphone's copy of what the
//!   speakers played (echo), splits the meeting into topics when a segment
//!   drifts away from the running topic centroid, and picks the sentences
//!   closest to each topic's centroid as its key points.
//! - Needle reads the segments that sound like commitments, decisions,
//!   questions or scheduling, on its own thread, and turns them into function
//!   calls. A call is kept only when its words are in what was said.
//! - SmolLM2 gives each topic a short title. Small models invent things, so a
//!   title is kept only when MiniLM finds it close to the topic itself.

use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use serde::{Deserialize, Serialize};

use crate::audio::Source;
use crate::models::{cosine, Needle};
use crate::quiz;
use crate::web;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub source: Source,
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub language: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Topic {
    pub start: f64,
    pub end: f64,
    pub first: usize,
    pub last: usize,
    #[serde(default)]
    pub title: String,
    /// MiniLM key phrases, best first: web search queries and quiz terms.
    #[serde(default)]
    pub keyphrases: Vec<String>,
    /// Key points, one per line, each starting with "- ".
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Action,
    Decision,
    Question,
    FollowUp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub kind: ItemKind,
    pub text: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub due: String,
    pub at: f64,
    pub source: Source,
    /// What was said, word for word.
    #[serde(default)]
    pub quote: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keyframe {
    pub at: f64,
    /// Relative to the session folder.
    pub file: String,
    pub labels: Vec<(String, f32)>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Notes {
    pub started: String,
    pub duration: f64,
    pub teams_title: String,
    pub overview: String,
    pub segments: Vec<Segment>,
    pub topics: Vec<Topic>,
    pub items: Vec<Item>,
    pub keyframes: Vec<Keyframe>,
    #[serde(default)]
    pub alerts: Vec<Alert>,
    #[serde(default)]
    pub sources: Vec<web::Source>,
    /// The user's own names, kept out of the quiz.
    #[serde(default)]
    pub own_names: Vec<String>,
}

/// Something worth a notification while the meeting runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    pub id: usize,
    pub at: f64,
    pub why: String,
    pub text: String,
}

/// What the engine does beyond the transcript.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The user's names, lower case: a participant saying one raises an alert.
    pub names: Vec<String>,
    /// Look up related Wikipedia articles for each topic.
    pub web: bool,
}

/// Tools offered to Needle. It answers with calls, or `[]` when nothing applies.
const TOOLS: &str = r#"[
{"name":"add_action_item","description":"Record a task that someone in the meeting has to do","parameters":{"type":"object","properties":{"task":{"type":"string","description":"what has to be done"},"owner":{"type":"string","description":"who will do it"},"due":{"type":"string","description":"deadline, if one was said"}},"required":["task"]}},
{"name":"record_decision","description":"Record a decision the meeting agreed on","parameters":{"type":"object","properties":{"decision":{"type":"string"}},"required":["decision"]}},
{"name":"record_open_question","description":"Record a question that was asked and still needs an answer","parameters":{"type":"object","properties":{"question":{"type":"string"}},"required":["question"]}},
{"name":"schedule_follow_up","description":"Record a meeting or call that should be scheduled","parameters":{"type":"object","properties":{"topic":{"type":"string"},"when":{"type":"string"}},"required":["topic"]}}
]"#;

/// Words that mark a commitment, decision, question or plan, in Whistle's
/// languages. Needle takes about 10 s per call on a laptop CPU, so only
/// segments with a cue (or close to one of `INTENTS`) are sent to it.
const CUES: &[&str] = &[
    // en
    "will ", "'ll ", "going to", "can you", "could you", "please", "need to", "needs to", "have to", "has to",
    "must ", "should ", "decided", "decide", "agreed", "agree", "deadline", "by monday", "by tuesday",
    "by wednesday", "by thursday", "by friday", "tomorrow", "next week", "schedule", "follow-up", "follow up",
    "who ", "action item", "assign", "let's ", "let us",
    // de
    "wird ", "werden ", "kannst du", "können sie", "bitte", "müssen", "beschlossen", "vereinbart", "bis ",
    // fr
    "va ", "vais ", "peux-tu", "pouvez-vous", "s'il", "doit ", "décidé", "convenu", "d'ici",
    // es
    "va a ", "voy a ", "puedes", "puede ", "por favor", "tiene que", "decidimos", "acordamos", "para el ",
    // it
    "farà", "puoi ", "per favore", "deve ", "abbiamo deciso", "concordato", "entro ",
    // nl
    "zal ", "kun je", "alsjeblieft", "moet ", "besloten", "afgesproken",
    // pl
    "będzie", "możesz", "proszę", "musi ", "zdecydowaliśmy", "ustaliliśmy", "do piątku",
];

const INTENTS: &[&str] = &[
    "Someone will do this task by Friday.",
    "Can you please take care of this before the deadline?",
    "We decided to go with this option.",
    "Who is going to approve this?",
    "Let's schedule a follow-up meeting next week.",
];
const INTENT_MIN: f32 = 0.45;

/// Echo: the microphone repeating what the loudspeakers played.
const ECHO_SIMILARITY: f32 = 0.85;
const ECHO_WINDOW_S: f64 = 15.0;
/// A segment this far from the topic centroid starts a new topic.
const TOPIC_SPLIT: f32 = 0.30;
const TOPIC_MIN_SEGMENTS: usize = 4;
const TOPIC_MAX_S: f64 = 300.0;
const KEY_POINTS: usize = 3;
/// A SmolLM2 title at least this close to its topic is kept.
const TITLE_MIN: f32 = 0.35;
/// SmolLM2-135M follows examples far better than instructions.
const TITLE_SHOTS: &[(&str, &str)] = &[
    ("system", "Reply with a short topic title only."),
    ("user", "We need more servers before launch.\nThe database is too slow under load.\nAnna will test the cache tomorrow."),
    ("assistant", "Server capacity and performance"),
    ("user", "The new logo looks great.\nCan we try a darker blue?\nWe agreed to keep the old font."),
    ("assistant", "Logo design"),
];
const NEEDLE_MIN_WORDS: usize = 4;

/// Words a lecturer or colleague uses for what matters, in Whistle's languages.
const IMPORTANT_CUES: &[&str] = &[
    "important", "remember", "don't forget", "do not forget", "exam", "on the test", "in the test", "quiz", "deadline",
    "homework", "assignment", "submit", "key point", "pay attention", "make sure", "mandatory", "grade", "grading",
    "wichtig", "prüfung", "nicht vergessen", "abgabe", "hausaufgabe", "frist",
    "examen", "n'oubliez pas", "devoir", "date limite",
    "importante", "no olviden", "tarea", "fecha límite",
    "esame", "non dimenticate", "compito", "scadenza",
    "belangrijk", "tentamen", "vergeet niet", "opdracht",
    "ważne", "egzamin", "nie zapomnijcie", "zadanie", "termin",
];
const IMPORTANT: &[&str] = &[
    "This will be on the exam.",
    "This is very important, please remember it.",
    "The deadline for the assignment is next week.",
];
const IMPORTANT_MIN: f32 = 0.55;
/// Quiet time after an alert, so a burst of cues makes one notification.
const ALERT_GAP_S: f64 = 20.0;
const TOPIC_PHRASES: usize = 6;
/// A Wikipedia article this close to the topic is used.
const SOURCE_MIN: f32 = 0.45;
const SOURCES_PER_TOPIC: usize = 2;
/// Articles of the same field, kept only for their definitions.
const NEIGHBOR_MIN: f32 = 0.25;
const MAX_ARTICLES: usize = 5;
const SOURCE_SENTENCES: usize = 4;

struct WebJob {
    topic: usize,
    lang: String,
    phrases: Vec<String>,
    centroid: Vec<f32>,
}

struct NeedleJob {
    at: f64,
    source: Source,
    text: String,
}

pub struct Engine {
    pub notes: Notes,
    minilm: Option<Arc<minilm::MiniLM>>,
    smol: Option<smollm2::SmolLM>,
    opts: Options,
    intents: Vec<Vec<f32>>,
    important: Vec<Vec<f32>>,
    last_alert: f64,
    web_tx: Option<Sender<WebJob>>,
    web_out: Arc<Mutex<Vec<web::Source>>>,
    web_thread: Option<JoinHandle<()>>,
    needle_tx: Option<Sender<NeedleJob>>,
    needle_out: Arc<Mutex<Vec<Item>>>,
    needle_thread: Option<JoinHandle<()>>,
    topic_first: usize,
    centroid: Vec<f32>,
    centroid_n: usize,
    log: fn(&str),
}

impl Engine {
    pub fn new(notes: Notes, minilm: Option<minilm::MiniLM>, smol: Option<smollm2::SmolLM>, needle: Option<Needle>, opts: Options, log: fn(&str)) -> Engine {
        let minilm = minilm.map(Arc::new);
        let needle_out = Arc::new(Mutex::new(Vec::new()));
        let (needle_tx, needle_thread) = match needle {
            Some(n) => {
                let (tx, rx) = channel::<NeedleJob>();
                let out = needle_out.clone();
                let h = std::thread::spawn(move || {
                    for job in rx {
                        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| extract(&n, &job)));
                        match r {
                            Ok(items) => out.lock().unwrap().extend(items),
                            Err(_) => log("needle panicked on a segment; skipped"),
                        }
                    }
                });
                (Some(tx), Some(h))
            }
            None => (None, None),
        };
        let (intents, important) = match &minilm {
            Some(m) => (m.embed(INTENTS, false).unwrap_or_default(), m.embed(IMPORTANT, false).unwrap_or_default()),
            None => (Vec::new(), Vec::new()),
        };
        let web_out = Arc::new(Mutex::new(Vec::new()));
        let (web_tx, web_thread) = match (&minilm, opts.web) {
            (Some(m), true) => {
                let (tx, rx) = channel::<WebJob>();
                let out = web_out.clone();
                let m = m.clone();
                let h = std::thread::spawn(move || {
                    for job in rx {
                        let found = find_sources(&m, &job, log);
                        out.lock().unwrap().extend(found);
                    }
                });
                (Some(tx), Some(h))
            }
            _ => (None, None),
        };
        Engine {
            notes,
            minilm,
            smol,
            opts,
            intents,
            important,
            last_alert: f64::NEG_INFINITY,
            web_tx,
            web_out,
            web_thread,
            needle_tx,
            needle_out,
            needle_thread,
            topic_first: 0,
            centroid: Vec::new(),
            centroid_n: 0,
            log,
        }
    }

    fn alert(&mut self, at: f64, why: &str, text: &str, urgent: bool) {
        if !urgent && at - self.last_alert < ALERT_GAP_S {
            return;
        }
        if self.notes.alerts.iter().any(|a| (a.at - at).abs() < 0.01) {
            return;
        }
        self.last_alert = at;
        let id = self.notes.alerts.len() + 1;
        (self.log)(&format!("alert: {why}: {text}"));
        self.notes.alerts.push(Alert { id, at, why: why.into(), text: text.into() });
    }

    /// Name mentions, importance cues, or MiniLM close to an "important" example.
    fn check_alert(&mut self, seg: &Segment) {
        let lower = seg.text.to_lowercase();
        if seg.source == Source::Others {
            let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).collect();
            if self.opts.names.iter().any(|n| words.contains(&n.as_str())) {
                self.alert(seg.start, "Αναφέρθηκε το όνομά σου", &seg.text, true);
                return;
            }
        }
        let cue = IMPORTANT_CUES.iter().any(|c| lower.contains(c));
        let close = !seg.embedding.is_empty() && self.important.iter().any(|i| cosine(i, &seg.embedding) >= IMPORTANT_MIN);
        if cue || close {
            self.alert(seg.start, "Ακούστηκε κάτι σημαντικό", &seg.text, false);
        }
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let Some(m) = &self.minilm else { return Vec::new() };
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| m.embed(&[text], false)));
        match r {
            Ok(Ok(mut v)) if !v.is_empty() => v.swap_remove(0),
            _ => Vec::new(),
        }
    }

    /// Adds a recognized segment. Returns false when it was dropped as echo.
    pub fn add_segment(&mut self, mut seg: Segment) -> bool {
        seg.text = seg.text.trim().to_string();
        if seg.text.chars().filter(|c| c.is_alphanumeric()).count() < 2 {
            return false;
        }
        seg.embedding = self.embed(&seg.text);
        if seg.source == Source::Me && !seg.embedding.is_empty() {
            let echo = self.notes.segments.iter().rev().take(12).any(|o| {
                o.source == Source::Others
                    && (o.start - seg.start).abs() < ECHO_WINDOW_S
                    && cosine(&o.embedding, &seg.embedding) > ECHO_SIMILARITY
            });
            if echo {
                (self.log)(&format!("echo dropped: {}", seg.text));
                return false;
            }
        }

        // Topic boundary, decided before the segment joins the topic.
        let in_topic = self.notes.segments.len() - self.topic_first;
        if in_topic > 0 {
            let topic_start = self.notes.segments[self.topic_first].start;
            let drift = !seg.embedding.is_empty()
                && !self.centroid.is_empty()
                && in_topic >= TOPIC_MIN_SEGMENTS
                && cosine(&self.centroid, &seg.embedding) < TOPIC_SPLIT;
            if drift || seg.start - topic_start > TOPIC_MAX_S {
                self.close_topic();
            }
        }

        self.check_alert(&seg);
        if let Some(tx) = &self.needle_tx {
            if self.worth_a_look(&seg) {
                let _ = tx.send(NeedleJob { at: seg.start, source: seg.source, text: seg.text.clone() });
            }
        }
        if !seg.embedding.is_empty() {
            if self.centroid.is_empty() {
                self.centroid = vec![0.0; seg.embedding.len()];
            }
            let n = self.centroid_n as f32;
            for (c, e) in self.centroid.iter_mut().zip(&seg.embedding) {
                *c = (*c * n + e) / (n + 1.0);
            }
            self.centroid_n += 1;
        }
        self.notes.segments.push(seg);
        true
    }

    fn worth_a_look(&self, seg: &Segment) -> bool {
        if seg.text.split_whitespace().count() < NEEDLE_MIN_WORDS {
            return false;
        }
        let lower = format!("{} ", seg.text.to_lowercase());
        if lower.contains('?') || CUES.iter().any(|c| lower.contains(c)) {
            return true;
        }
        !seg.embedding.is_empty() && self.intents.iter().any(|i| cosine(i, &seg.embedding) >= INTENT_MIN)
    }

    /// Moves the items Needle has found so far into the notes. True when any were new.
    pub fn collect_items(&mut self) -> bool {
        let found: Vec<Item> = std::mem::take(&mut *self.needle_out.lock().unwrap());
        let mut added = false;
        for item in found {
            let dup = self.notes.items.iter().any(|i| i.kind == item.kind && i.text.eq_ignore_ascii_case(&item.text));
            if !dup {
                (self.log)(&format!("{:?}: {}", item.kind, item_line(&item)));
                let why = match item.kind {
                    ItemKind::Action => Some("Νέα ενέργεια"),
                    ItemKind::Decision => Some("Νέα απόφαση"),
                    ItemKind::FollowUp => Some("Νέα συνάντηση"),
                    ItemKind::Question => None,
                };
                if let Some(why) = why {
                    self.alert(item.at, why, &item_line(&item), true);
                }
                self.notes.items.push(item);
                added = true;
            }
        }
        if added {
            self.notes.items.sort_by(|a, b| a.at.total_cmp(&b.at));
        }
        added
    }

    fn ask_smol(&mut self, messages: Vec<(String, String)>) -> String {
        let Some(smol) = &mut self.smol else { return String::new() };
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| smol.reply(&messages)));
        match r {
            Ok(Ok(text)) => text,
            Ok(Err(e)) => {
                (self.log)(&format!("smollm2: {e}"));
                String::new()
            }
            Err(_) => String::new(),
        }
    }

    /// Closes the running topic: key points from MiniLM, title from SmolLM2.
    pub fn close_topic(&mut self) {
        let first = self.topic_first;
        let last = self.notes.segments.len();
        if last <= first {
            return;
        }
        let segs = &self.notes.segments[first..last];
        let summary = key_points(segs, &self.centroid);
        let texts: Vec<&str> = segs.iter().map(|s| s.text.as_str()).collect();
        let keyphrases = match &self.minilm {
            Some(m) => {
                let embed = |t: &str| m.embed(&[t], false).ok().and_then(|mut v| v.pop()).unwrap_or_default();
                quiz::keyphrases(&texts, &self.centroid, &embed, TOPIC_PHRASES)
            }
            None => Vec::new(),
        };
        // Key phrases go to Wikipedia and into the quiz: never with a person's name.
        let private = quiz::private_names(&self.notes);
        let keyphrases = without_names(keyphrases, &private);
        let lang = majority_language(segs);
        let excerpt = transcript_tail(segs, 1000);
        let said = excerpt.to_lowercase();
        let mut messages: Vec<(String, String)> = TITLE_SHOTS.iter().map(|(r, t)| (r.to_string(), t.to_string())).collect();
        messages.push(("user".into(), excerpt));
        let raw = self.ask_smol(messages);
        let title = clean_title(&raw);
        let fit = if title.is_empty() || self.centroid.is_empty() || !grounded_half(&title, &said) {
            0.0
        } else {
            cosine(&self.embed(&title), &self.centroid)
        };
        let title = if fit >= TITLE_MIN {
            title
        } else {
            if !title.is_empty() {
                (self.log)(&format!("title dropped ({fit:.2}): {title}"));
            }
            String::new()
        };
        self.notes.topics.push(Topic {
            start: self.notes.segments[first].start,
            end: self.notes.segments[last - 1].end,
            first,
            last: last - 1,
            title,
            keyphrases: keyphrases.clone(),
            summary,
        });
        if let Some(tx) = &self.web_tx {
            if !keyphrases.is_empty() && !self.centroid.is_empty() {
                let topic = self.notes.topics.len() - 1;
                let segs = &self.notes.segments[first..last];
                let mut phrases = acronyms(segs, &keyphrases[0]);
                phrases.extend(keyphrases.into_iter().take(2));
                let _ = tx.send(WebJob { topic, lang, phrases, centroid: self.centroid.clone() });
            }
        }
        self.topic_first = last;
        self.centroid.clear();
        self.centroid_n = 0;
    }

    pub fn add_keyframe(&mut self, kf: Keyframe) {
        self.notes.keyframes.push(kf);
    }

    /// Moves the sources found so far into the notes. True when any were new.
    pub fn collect_sources(&mut self) -> bool {
        let found: Vec<web::Source> = std::mem::take(&mut *self.web_out.lock().unwrap());
        let mut added = false;
        for s in found {
            if !self.notes.sources.iter().any(|x| x.url == s.url) {
                if !s.neighbor {
                    (self.log)(&format!("source for topic {}: {} ({:.2})", s.topic + 1, s.title, s.score));
                }
                self.notes.sources.push(s);
                added = true;
            }
        }
        added
    }

    /// Closes the last topic, waits for Needle and the web lookups, and writes the overview.
    pub fn finish(&mut self, duration: f64) {
        self.close_topic();
        self.needle_tx = None;
        if let Some(h) = self.needle_thread.take() {
            let _ = h.join();
        }
        self.web_tx = None;
        if let Some(h) = self.web_thread.take() {
            let _ = h.join();
        }
        self.collect_items();
        self.collect_sources();
        self.notes.duration = duration;
        self.notes.overview = overview(&self.notes);
    }

    /// Recomputes every topic's key phrases, e.g. for a session saved by an older version.
    pub fn refresh_keyphrases(&mut self) {
        let Some(m) = self.minilm.clone() else { return };
        let embed = |t: &str| m.embed(&[t], false).ok().and_then(|mut v| v.pop()).unwrap_or_default();
        for t in &mut self.notes.topics {
            let segs = &self.notes.segments[t.first..=t.last.min(self.notes.segments.len().saturating_sub(1))];
            let embs: Vec<&Vec<f32>> = segs.iter().map(|s| &s.embedding).filter(|e| !e.is_empty()).collect();
            let Some(first) = embs.first() else { continue };
            let mut centroid = vec![0.0f32; first.len()];
            for e in &embs {
                for (c, x) in centroid.iter_mut().zip(e.iter()) {
                    *c += x / embs.len() as f32;
                }
            }
            let texts: Vec<&str> = segs.iter().map(|s| s.text.as_str()).collect();
            t.keyphrases = quiz::keyphrases(&texts, &centroid, &embed, TOPIC_PHRASES);
        }
        let private = quiz::private_names(&self.notes);
        for t in &mut self.notes.topics {
            t.keyphrases = without_names(std::mem::take(&mut t.keyphrases), &private);
        }
    }

    pub fn quiz(&self, seed: u64, lang: &str) -> quiz::Quiz {
        let embed = |t: &str| self.embed(t);
        quiz::build(&self.notes, &embed, seed, lang)
    }

    fn topic_centroid(&self, t: &Topic) -> Vec<f32> {
        let last = t.last.min(self.notes.segments.len().saturating_sub(1));
        let embs: Vec<&Vec<f32>> = self.notes.segments[t.first..=last].iter().map(|s| &s.embedding).filter(|e| !e.is_empty()).collect();
        let Some(first) = embs.first() else { return Vec::new() };
        let mut c = vec![0.0f32; first.len()];
        for e in &embs {
            for (x, y) in c.iter_mut().zip(e.iter()) {
                *x += y / embs.len() as f32;
            }
        }
        c
    }

    /// Looks up sources now, for a session recorded without them. Only the
    /// `max_topics` longest topics, to keep the number of requests small.
    pub fn fetch_sources_now(&mut self, max_topics: usize) {
        let Some(m) = self.minilm.clone() else { return };
        let mut order: Vec<usize> = (0..self.notes.topics.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(self.notes.topics[i].last - self.notes.topics[i].first));
        order.truncate(max_topics);
        for i in order {
            let t = &self.notes.topics[i];
            if t.keyphrases.is_empty() {
                continue;
            }
            let centroid = self.topic_centroid(t);
            let last = t.last.min(self.notes.segments.len().saturating_sub(1));
            let mut phrases = acronyms(&self.notes.segments[t.first..=last], &t.keyphrases[0]);
            phrases.extend(t.keyphrases.iter().take(2).cloned());
            let lang = majority_language(&self.notes.segments[t.first..=last]);
            let found = find_sources(&m, &WebJob { topic: i, lang, phrases, centroid }, self.log);
            self.web_out.lock().unwrap().extend(found);
        }
        self.collect_sources();
    }
}

fn without_names(phrases: Vec<String>, names: &[String]) -> Vec<String> {
    phrases
        .into_iter()
        .filter(|p| {
            let words: Vec<String> = p.split_whitespace().map(str::to_lowercase).collect();
            !names.iter().any(|n| words.contains(&n.to_lowercase()))
        })
        .collect()
}

fn majority_language(segs: &[Segment]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for s in segs {
        match counts.iter_mut().find(|c| c.0 == s.language) {
            Some(c) => c.1 += 1,
            None => counts.push((&s.language, 1)),
        }
    }
    counts.into_iter().max_by_key(|c| c.1).map(|c| c.0).filter(|l| !l.is_empty()).unwrap_or("en").to_string()
}

/// Wikipedia articles for a topic's key phrases, kept when MiniLM finds them
/// close to the topic, with their sentences closest to it.
fn find_sources(m: &minilm::MiniLM, job: &WebJob, log: fn(&str)) -> Vec<web::Source> {
    let embed = |t: &str| m.embed(&[t], false).ok().and_then(|mut v| v.pop()).unwrap_or_default();
    let mut titles: Vec<String> = Vec::new();
    for p in &job.phrases {
        match web::search(&job.lang, p, 3) {
            Ok(found) => {
                for t in found {
                    if !titles.contains(&t) {
                        titles.push(t);
                    }
                }
            }
            Err(e) => {
                log(&format!("wikipedia search: {e}"));
                return Vec::new();
            }
        }
    }
    let mut kept: Vec<web::Source> = Vec::new();
    let mut neighbors: Vec<web::Source> = Vec::new();
    for title in titles.into_iter().take(MAX_ARTICLES) {
        let Ok((text, url)) = web::article(&job.lang, &title) else { continue };
        let intro: String = text.chars().take(600).collect();
        let score = cosine(&embed(&format!("{title}. {intro}")), &job.centroid);
        let (definition, acronym, countable) = web::definition(&web::first_sentence(&text)).unwrap_or_default();
        if score < SOURCE_MIN {
            // Same field, not the topic: its definition makes a good wrong option.
            if score >= NEIGHBOR_MIN && !definition.is_empty() {
                neighbors.push(web::Source { title, url, topic: job.topic, score, sentences: Vec::new(), definition, acronym, countable, neighbor: true });
            }
            continue;
        }
        let mut sents: Vec<(f32, String)> = web::sentences(&text).into_iter().take(80).map(|s| (cosine(&embed(&s), &job.centroid), s)).collect();
        sents.sort_by(|a, b| b.0.total_cmp(&a.0));
        let sentences = sents.into_iter().take(SOURCE_SENTENCES).map(|s| s.1).collect();
        kept.push(web::Source { title, url, topic: job.topic, score, sentences, definition, acronym, countable, neighbor: false });
    }
    kept.sort_by(|a, b| b.score.total_cmp(&a.score));
    // Past the best few, a relevant article still serves as a neighbor.
    for mut extra in kept.split_off(kept.len().min(SOURCES_PER_TOPIC)) {
        extra.neighbor = true;
        neighbors.push(extra);
    }
    kept.extend(neighbors);
    kept
}

/// "CNN", "RNNs", "LSTM": the acronyms said in a topic, searched with the
/// topic's best phrase so "CNN" finds the network, not the channel.
fn acronyms(segs: &[Segment], context: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in segs {
        for w in s.text.split(|c: char| !c.is_alphanumeric()) {
            let w = w.strip_suffix('s').unwrap_or(w);
            let ok = (2..=5).contains(&w.len()) && w.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) && w.chars().any(|c| c.is_ascii_uppercase());
            if ok && !["OK", "AM", "PM", "TV", "US", "UK", "EU", "II", "III"].contains(&w) {
                let q = format!("{w} {context}");
                if !out.contains(&q) {
                    out.push(q);
                }
            }
        }
    }
    out.truncate(3);
    out
}

/// Needle on one segment, keeping only calls grounded in what was said.
fn extract(needle: &Needle, job: &NeedleJob) -> Vec<Item> {
    let (json, _) = needle.call(&job.text, TOOLS);
    let Some(json) = json else { return Vec::new() };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) else { return Vec::new() };
    let calls = match value {
        serde_json::Value::Array(a) => a,
        v @ serde_json::Value::Object(_) => vec![v],
        _ => return Vec::new(),
    };
    let said = job.text.to_lowercase();
    let mut out = Vec::new();
    for call in calls {
        let name = call.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let args = call.get("arguments").cloned().unwrap_or_default();
        let arg = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let (kind, text, owner, due) = match name {
            "add_action_item" => (ItemKind::Action, arg("task"), arg("owner"), arg("due")),
            "record_decision" => (ItemKind::Decision, arg("decision"), String::new(), String::new()),
            "record_open_question" => (ItemKind::Question, arg("question"), String::new(), String::new()),
            "schedule_follow_up" => (ItemKind::FollowUp, arg("topic"), String::new(), arg("when")),
            _ => continue,
        };
        if !grounded(&text, &said) {
            continue;
        }
        let Some(kind) = refine(kind, &said) else { continue };
        // A question is clearest in the speaker's own words.
        let text = if kind == ItemKind::Question { job.text.trim().trim_matches('"').to_string() } else { text };
        // Owner and deadline must be in the sentence too, or they are left blank.
        let owner = if owner.is_empty() { addressee(&job.text) } else { owner };
        let owner = if !owner.is_empty() && said.contains(&owner.to_lowercase()) && looks_like_name(&owner) {
            owner
        } else if job.source == Source::Me && ["i will", "i'll", "i am going to", "i'm going to"].iter().any(|c| said.contains(c)) {
            "Εγώ".to_string()
        } else {
            String::new()
        };
        let due = if !due.is_empty() && grounded(&due, &said) && looks_like_time(&due) { due } else { String::new() };
        // "X will be on the exam" is not a task: a bare "will" needs someone to do it.
        if kind == ItemKind::Action && owner.is_empty() && !STRONG_REQUEST.iter().any(|c| said.contains(c)) {
            continue;
        }
        if out.iter().any(|i: &Item| i.kind == kind) {
            continue;
        }
        out.push(Item { kind, text, owner, due, at: job.at, source: job.source, quote: job.text.clone() });
    }
    out
}

const STRONG_REQUEST: &[&str] = &[
    "can you", "could you", "would you", "please", "need to", "needs to", "have to", "has to", "must ", "kannst du",
    "können sie", "bitte", "peux-tu", "pouvez-vous", "puedes", "por favor", "puoi ", "per favore", "kun je", "możesz", "proszę",
];
const TIME_WORDS: &[&str] = &[
    "monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday", "today", "tonight", "tomorrow",
    "morrow", "week", "month", "year", "morning", "afternoon", "evening", "end of", "asap", "january", "february",
    "march", "april", "may", "june", "july", "august", "september", "october", "november", "december", "o'clock",
    "montag", "dienstag", "mittwoch", "donnerstag", "freitag", "morgen", "woche", "lundi", "mardi", "mercredi", "jeudi",
    "vendredi", "demain", "semaine", "lunes", "martes", "miércoles", "jueves", "viernes", "mañana", "semana", "lunedì",
    "martedì", "mercoledì", "giovedì", "venerdì", "domani", "settimana", "maandag", "dinsdag", "woensdag", "donderdag",
    "vrijdag", "morgen", "poniedziałek", "wtorek", "środa", "czwartek", "piątek", "jutro", "tydzień",
];

/// A deadline names a day, a period or a number ("Friday", "next week", "the 15th").
fn looks_like_time(due: &str) -> bool {
    let d = due.to_lowercase();
    d.chars().any(|c| c.is_ascii_digit()) || TIME_WORDS.iter().any(|w| d.contains(w))
}

/// "Maria" or "Alex Andrus"; not "install", "Okay", "Hello", "No".
fn looks_like_name(owner: &str) -> bool {
    let words: Vec<&str> = owner.split_whitespace().collect();
    (1..=3).contains(&words.len())
        && words.iter().all(|w| w.chars().next().is_some_and(char::is_uppercase))
        && !words.iter().any(|w| PRONOUNS.contains(&w.to_lowercase().as_str()) || NOT_NAMES.contains(&w.to_lowercase().as_str()))
}

const NOT_NAMES: &[&str] = &[
    "okay", "ok", "hello", "hi", "no", "yes", "yeah", "well", "so", "thanks", "thank", "sorry", "right", "now", "please",
    "good", "great", "sure", "can", "could", "would", "will", "let's", "the", "this", "that", "what", "who", "how",
];

const PRONOUNS: &[&str] = &["i", "we", "you", "they", "he", "she", "us", "me", "someone", "everyone", "team", "the team"];
const DECISION_CUES: &[&str] = &[
    "decided", "decision", "agreed", "we will go with", "beschlossen", "vereinbart", "entschieden", "décidé", "convenu",
    "decidimos", "acordamos", "deciso", "concordato", "besloten", "afgesproken", "zdecydowali", "ustalili",
];
const SCHEDULE_CUES: &[&str] = &[
    "schedule", "follow-up", "follow up", "set up a", "book a", "arrange", "next meeting", "another meeting", "termin",
    "réunion", "reunión", "riunione", "vergadering", "spotkanie",
];
const REQUEST_CUES: &[&str] = &[
    "can you", "could you", "would you", "please", "will ", "'ll ", "going to", "need to", "needs to", "have to",
    "has to", "must ", "should ", "kannst du", "können sie", "bitte", "peux-tu", "pouvez-vous", "puedes", "por favor",
    "puoi ", "per favore", "kun je", "możesz", "proszę",
];
const QUESTION_WORDS: &[&str] = &[
    "who", "what", "when", "where", "why", "how", "which", "can", "could", "should", "do", "does", "is", "are",
    "wer", "was", "wann", "wie", "qui", "quoi", "quand", "comment", "quién", "qué", "cuándo", "chi", "cosa",
    "quando", "wie", "wat", "wanneer", "kto", "co", "kiedy",
];

/// Needle often picks the wrong tool for a sentence it read correctly. The
/// kind is checked against the wording, and calls that fit no kind are dropped.
fn refine(kind: ItemKind, said: &str) -> Option<ItemKind> {
    let padded = format!("{said} ");
    let has = |cues: &[&str]| cues.iter().any(|c| padded.contains(c));
    let first = said.trim_start_matches(|c: char| !c.is_alphanumeric()).split(|c: char| !c.is_alphanumeric()).next().unwrap_or("");
    let question = said.contains('?') || QUESTION_WORDS.contains(&first);
    let request = has(REQUEST_CUES);
    if has(DECISION_CUES) && !(kind == ItemKind::FollowUp && has(SCHEDULE_CUES)) {
        return Some(ItemKind::Decision);
    }
    if has(SCHEDULE_CUES) && (kind == ItemKind::FollowUp || request || said.contains("let's") || said.contains("let us")) {
        return Some(ItemKind::FollowUp);
    }
    if question {
        let ask = ["can you", "could you", "would you", "kannst du", "können sie", "peux-tu", "pouvez-vous", "puedes", "puoi ", "kun je", "możesz"];
        return Some(if ask.iter().any(|c| padded.contains(c)) { ItemKind::Action } else { ItemKind::Question });
    }
    match kind {
        ItemKind::Action | ItemKind::Decision if request => Some(ItemKind::Action),
        _ => None,
    }
}

/// "John, can you ..." -> "John".
fn addressee(text: &str) -> String {
    let t = text.trim().trim_start_matches(|c: char| !c.is_alphanumeric());
    match t.split_once(',') {
        Some((name, _)) if name.split_whitespace().count() == 1 && name.chars().next().is_some_and(char::is_uppercase) => name.to_string(),
        _ => String::new(),
    }
}

fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() > 2)
        .map(str::to_string)
        .collect()
}

/// At least two thirds of the longer words of `text` occur in `said`.
fn grounded(text: &str, said: &str) -> bool {
    let w = words(text);
    let found = w.iter().filter(|x| said.contains(x.as_str())).count();
    !w.is_empty() && found * 3 >= w.len() * 2
}

/// At least half: titles may add a word of their own ("discussion", "plan").
fn grounded_half(text: &str, said: &str) -> bool {
    let w = words(text);
    let found = w.iter().filter(|x| said.contains(x.as_str())).count();
    !w.is_empty() && found * 2 >= w.len()
}

/// The segments closest to the topic centroid, in the order they were said.
fn key_points(segs: &[Segment], centroid: &[f32]) -> String {
    let mut ranked: Vec<(f32, &Segment)> = segs
        .iter()
        .filter(|s| s.text.split_whitespace().count() >= 4)
        .map(|s| (if s.embedding.is_empty() || centroid.is_empty() { s.text.len() as f32 } else { cosine(&s.embedding, centroid) }, s))
        .collect();
    if ranked.is_empty() {
        ranked = segs.iter().map(|s| (0.0, s)).collect();
    }
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
    ranked.truncate(KEY_POINTS);
    ranked.sort_by(|a, b| a.1.start.total_cmp(&b.1.start));
    ranked.iter().map(|(_, s)| format!("- {}", s.text)).collect::<Vec<_>>().join("\n")
}

/// First line, up to the first ':' or sentence end.
fn clean_title(raw: &str) -> String {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let line = line.trim_start_matches(|c: char| c == '#' || c == '-' || c == '*' || c.is_whitespace());
    let line = line.strip_prefix("Title:").or_else(|| line.strip_prefix("title:")).unwrap_or(line);
    let line = line.split([':', '.', '!', '?']).next().unwrap_or("");
    let line = line.trim().trim_matches(|c| c == '"' || c == '\'' || c == '*').trim();
    if line.split_whitespace().count() > 8 {
        return String::new();
    }
    line.to_string()
}

fn transcript_tail(segs: &[Segment], max_chars: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut total = 0;
    for s in segs.iter().rev() {
        total += s.text.len() + 1;
        if total > max_chars && !lines.is_empty() {
            break;
        }
        lines.push(s.text.clone());
    }
    lines.reverse();
    lines.join("\n")
}

/// Built from the topics and items, so it says nothing that was not said.
fn overview(n: &Notes) -> String {
    let mut parts = Vec::new();
    let titles: Vec<&str> = n.topics.iter().map(|t| t.title.as_str()).filter(|t| !t.is_empty()).collect();
    if !titles.is_empty() {
        parts.push(format!("Θέματα: {}.", titles.join("; ")));
    }
    let count = |k: ItemKind| n.items.iter().filter(|i| i.kind == k).count();
    let mut counts = Vec::new();
    for (k, one, many) in [
        (ItemKind::Action, "ενέργεια", "ενέργειες"),
        (ItemKind::Decision, "απόφαση", "αποφάσεις"),
        (ItemKind::Question, "ανοιχτό ερώτημα", "ανοιχτά ερωτήματα"),
        (ItemKind::FollowUp, "επόμενη συνάντηση", "επόμενες συναντήσεις"),
    ] {
        match count(k) {
            0 => {}
            1 => counts.push(format!("1 {one}")),
            c => counts.push(format!("{c} {many}")),
        }
    }
    if !counts.is_empty() {
        parts.push(format!("{}.", counts.join(", ")));
    }
    parts.join(" ")
}

pub fn clock(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

impl ItemKind {
    pub fn title(self) -> &'static str {
        match self {
            ItemKind::Action => "Ενέργειες",
            ItemKind::Decision => "Αποφάσεις",
            ItemKind::Question => "Ανοιχτά ερωτήματα",
            ItemKind::FollowUp => "Επόμενες συναντήσεις",
        }
    }
}

pub const ITEM_ORDER: [ItemKind; 4] = [ItemKind::Action, ItemKind::Decision, ItemKind::Question, ItemKind::FollowUp];

pub fn item_line(i: &Item) -> String {
    let mut line = i.text.clone();
    if !i.owner.is_empty() {
        line.push_str(&format!(" — {}", i.owner));
    }
    if !i.due.is_empty() {
        line.push_str(&format!(" (έως: {})", i.due));
    }
    line
}

pub fn topic_heading(n: usize, t: &Topic) -> String {
    let title = if t.title.is_empty() { String::new() } else { format!(" · {}", t.title) };
    format!("{}. {} – {}{}", n, clock(t.start), clock(t.end), title)
}

impl Notes {
    pub fn markdown(&self, live: bool) -> String {
        let mut md = String::new();
        md.push_str(&format!("# Σημειώσεις συνάντησης — {}\n\n", self.started));
        if !self.teams_title.is_empty() {
            md.push_str(&format!("**Teams:** {}  \n", self.teams_title));
        }
        md.push_str(&format!("**Διάρκεια:** {}{}\n\n", clock(self.duration), if live { " (σε εξέλιξη)" } else { "" }));
        if !self.overview.is_empty() {
            md.push_str(&format!("## Σύνοψη\n\n{}\n\n", self.overview));
        }
        if !self.alerts.is_empty() {
            md.push_str("## Σημαντικά\n\n");
            for a in &self.alerts {
                md.push_str(&format!("- [{}] {}: {}\n", clock(a.at), a.why, a.text));
            }
            md.push('\n');
        }
        for kind in ITEM_ORDER {
            let items: Vec<&Item> = self.items.iter().filter(|i| i.kind == kind).collect();
            if items.is_empty() {
                continue;
            }
            md.push_str(&format!("## {}\n\n", kind.title()));
            for i in items {
                md.push_str(&format!("- [{}] {}\n", clock(i.at), item_line(i)));
            }
            md.push('\n');
        }
        if !self.topics.is_empty() {
            md.push_str("## Θέματα\n\n");
            for (n, t) in self.topics.iter().enumerate() {
                md.push_str(&format!("### {}\n\n{}\n\n", topic_heading(n + 1, t), t.summary));
            }
        }
        if !self.keyframes.is_empty() {
            md.push_str("## Οθόνη\n\n");
            for k in &self.keyframes {
                let labels: Vec<String> = k.labels.iter().map(|(l, _)| l.clone()).collect();
                let labels = if labels.is_empty() { String::new() } else { format!(" — {}", labels.join(", ")) };
                md.push_str(&format!("- [{}] ![{}]({}){}\n", clock(k.at), clock(k.at), k.file.replace('\\', "/"), labels));
            }
            md.push('\n');
        }
        if self.sources.iter().any(|s| !s.neighbor) {
            md.push_str("## Πηγές για διάβασμα\n\n");
            for s in self.sources.iter().filter(|s| !s.neighbor) {
                md.push_str(&format!("- [{}]({}) (θέμα {})\n", s.title, s.url, s.topic + 1));
            }
            md.push('\n');
        }
        md.push_str("## Απομαγνητοφώνηση\n\n");
        for s in &self.segments {
            md.push_str(&format!("- [{}] **{}:** {}\n", clock(s.start), s.source.label(), s.text));
        }
        md
    }
}
