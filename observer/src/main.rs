//! LinkNotifier Observer: a local meeting note taker that works alongside
//! Microsoft Teams.
//!
//! It listens to what Teams plays (WASAPI loopback) and to the microphone,
//! watches the Teams window, and writes notes into a session folder:
//! `notes.md`, `notes.json`, `card.json` (Adaptive Card), `frames/*.png` and
//! `status.json`, and when it stops a self-check quiz (`quiz.html`). The
//! models run on this computer. The network is used for Wikipedia lookups of
//! topic key phrases (unless `--no-web`) and the optional post of the
//! finished card to a Teams Workflows webhook.
//!
//! Commands:
//!   observer run    --session DIR --models DIR [options]
//!   observer quiz   --session DIR --models DIR
//!   observer check  --models DIR
//!   observer post   --session DIR --webhook URL
//!   observer search --root DIR --models DIR [--top N] [--json] QUERY

mod audio;
mod models;
mod notes;
mod quiz;
mod screen;
mod teams;
mod web;

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

use audio::{Chunk, Source};
use models::Paths;
use notes::{Alert, Engine, Keyframe, Notes, Segment};

const USAGE: &str = "\
observer - local meeting notes for Microsoft Teams

USAGE:
  observer run --session DIR --models DIR [options]
  observer quiz --session DIR --models DIR      rebuild the quiz of a finished session
  observer check --models DIR
  observer post --session DIR --webhook URL
  observer search --root DIR --models DIR [--top N] [--json] QUERY

RUN OPTIONS:
  --screen teams|monitor|off   what to watch (default teams: only the Teams window)
  --audio both|others|me|off   what to listen to (default both)
  --language CODE              en, de, fr, es, it, nl or pl (default: detect)
  --beams N                    Whistle beam width (default 5; falls back to 1 when behind)
  --interval SECS              screen check interval (default 5)
  --duration SECS              stop after this long
  --auto-stop                  stop when the Teams meeting window has been gone for 60 s
  --wav FILE                   take the audio from a WAV file instead of the devices
  --webhook URL --post         post the notes to Teams when done
  --names NAME,NAME            your names: alert when a participant says one
  --no-web                     no Wikipedia lookups for the quiz
  --quiz-lang en|el            language of the quiz (default en)

Stop a running session by creating the file DIR/stop, or typing 'stop' on stdin.
";

// ------------------------------------------------------------------ logging

static LOG: OnceLock<Mutex<Option<File>>> = OnceLock::new();

fn log(msg: &str) {
    let line = format!("{} {}", chrono::Local::now().format("%H:%M:%S"), msg);
    eprintln!("{line}");
    if let Some(m) = LOG.get() {
        if let Ok(mut f) = m.lock() {
            if let Some(f) = f.as_mut() {
                let _ = writeln!(f, "{line}");
            }
        }
    }
}

// ------------------------------------------------------------------ arguments

#[derive(Default)]
struct Args {
    cmd: String,
    session: Option<PathBuf>,
    models: Option<PathBuf>,
    root: Option<PathBuf>,
    screen: String,
    audio: String,
    language: Option<String>,
    beams: usize,
    interval: f64,
    duration: Option<f64>,
    auto_stop: bool,
    wav: Option<PathBuf>,
    webhook: Option<String>,
    post: bool,
    top: usize,
    json: bool,
    names: Vec<String>,
    no_web: bool,
    quiz_lang: String,
    rest: Vec<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { screen: "teams".into(), audio: "both".into(), beams: 5, interval: 5.0, top: 10, ..Default::default() };
    let mut it = std::env::args().skip(1);
    let need = |it: &mut dyn Iterator<Item = String>, flag: &str| it.next().ok_or_else(|| format!("{flag} needs a value"));
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--session" => a.session = Some(need(&mut it, &arg)?.into()),
            "--models" => a.models = Some(need(&mut it, &arg)?.into()),
            "--root" => a.root = Some(need(&mut it, &arg)?.into()),
            "--screen" => a.screen = need(&mut it, &arg)?,
            "--audio" => a.audio = need(&mut it, &arg)?,
            "--language" => a.language = Some(need(&mut it, &arg)?).filter(|l| !l.is_empty() && l != "auto"),
            "--beams" => a.beams = need(&mut it, &arg)?.parse().map_err(|_| "--beams needs a number")?,
            "--interval" => a.interval = need(&mut it, &arg)?.parse().map_err(|_| "--interval needs seconds")?,
            "--duration" => a.duration = Some(need(&mut it, &arg)?.parse().map_err(|_| "--duration needs seconds")?),
            "--auto-stop" => a.auto_stop = true,
            "--wav" => a.wav = Some(need(&mut it, &arg)?.into()),
            "--webhook" => a.webhook = Some(need(&mut it, &arg)?).filter(|w| !w.is_empty()),
            "--post" => a.post = true,
            "--top" => a.top = need(&mut it, &arg)?.parse().map_err(|_| "--top needs a number")?,
            "--json" => a.json = true,
            "--names" => {
                a.names = need(&mut it, &arg)?.split(',').map(|n| n.trim().to_lowercase()).filter(|n| !n.is_empty()).collect()
            }
            "--no-web" => a.no_web = true,
            "--quiz-lang" => a.quiz_lang = need(&mut it, &arg)?,
            "-h" | "--help" => return Err(String::new()),
            _ if a.cmd.is_empty() => a.cmd = arg,
            _ => a.rest.push(arg),
        }
    }
    if a.cmd.is_empty() {
        return Err(String::new());
    }
    Ok(a)
}

fn models_dir(a: &Args) -> PathBuf {
    if let Some(m) = &a.models {
        return m.clone();
    }
    // Default: models/ next to the crate, found from the executable (target/release/observer.exe).
    let exe = std::env::current_exe().unwrap_or_default();
    exe.ancestors().map(|p| p.join("models")).find(|p| p.is_dir()).unwrap_or_else(|| PathBuf::from("models"))
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("error: {e}\n");
            }
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    };
    let r = match args.cmd.as_str() {
        "run" => run(&args),
        "check" => check(&args),
        "post" => post_cmd(&args),
        "search" => search(&args),
        "quiz" => quiz_cmd(&args),
        other => Err(format!("unknown command '{other}'")),
    };
    if let Err(e) = r {
        log(&format!("error: {e}"));
        std::process::exit(1);
    }
}

// ------------------------------------------------------------------ check / post

fn check(a: &Args) -> Result<(), String> {
    let p = Paths { root: models_dir(a) };
    let mut ok = true;
    for (name, present, path) in models::report(&p) {
        println!("{:<11} {:<8} {}", name, if present { "ok" } else { "MISSING" }, path.display());
        ok &= present || name != "whistle";
    }
    if let Some(t) = screen::find_teams() {
        println!("teams       found    {} ({}){}", t.title, t.exe, if t.in_meeting { ", in a meeting" } else { "" });
    } else {
        println!("teams       -        no Teams window open");
    }
    if ok {
        Ok(())
    } else {
        Err("whistle.cact is required".into())
    }
}

fn load_notes(session: &Path) -> Result<Notes, String> {
    let path = session.join("notes.json");
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {}", path.display(), e))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {}", path.display(), e))
}

fn write_quiz(session: &Path, q: &quiz::Quiz) -> Result<PathBuf, String> {
    write_atomic(&session.join("quiz.json"), serde_json::to_string_pretty(q).unwrap_or_default().as_bytes());
    let path = session.join("quiz.html");
    fs::write(&path, quiz::html(q)).map_err(|e| format!("{}: {}", path.display(), e))?;
    Ok(path)
}

fn quiz_lang(a: &Args) -> &str {
    if a.quiz_lang == "el" {
        "el"
    } else {
        "en"
    }
}

fn quiz_seed() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64)
}

fn quiz_cmd(a: &Args) -> Result<(), String> {
    let session = a.session.as_ref().ok_or("--session is required")?;
    let mut notes = load_notes(session)?;
    for n in &a.names {
        if !notes.own_names.contains(n) {
            notes.own_names.push(n.clone());
        }
    }
    let minilm = models::load_minilm(&Paths { root: models_dir(a) }, 0)?;
    let mut engine = Engine::new(notes, Some(minilm), None, None, notes::Options::default(), |m| eprintln!("{m}"));
    engine.refresh_keyphrases();
    if !a.no_web && !engine.notes.sources.iter().any(|s| !s.neighbor && !s.definition.is_empty()) {
        eprintln!("looking up sources");
        engine.notes.sources.clear();
        engine.fetch_sources_now(8);
        save(&engine.notes, session, false);
    }
    let q = engine.quiz(quiz_seed(), quiz_lang(a));
    let path = write_quiz(session, &q)?;
    println!("{} questions: {}", q.questions.len(), path.display());
    Ok(())
}

fn post_cmd(a: &Args) -> Result<(), String> {
    let session = a.session.as_ref().ok_or("--session is required")?;
    let webhook = a.webhook.as_ref().ok_or("--webhook is required")?;
    let n = load_notes(session)?;
    teams::post(webhook, &teams::adaptive_card(&n))?;
    println!("posted to Teams");
    Ok(())
}

// ------------------------------------------------------------------ search

#[derive(Serialize)]
struct Hit {
    score: f32,
    session: String,
    at: f64,
    kind: String,
    text: String,
}

fn search(a: &Args) -> Result<(), String> {
    let root = a.root.as_ref().ok_or("--root is required")?;
    let query = a.rest.join(" ");
    if query.trim().is_empty() {
        return Err("no query".into());
    }
    let m = models::load_minilm(&Paths { root: models_dir(a) }, 0)?;
    let q = m.embed(&[query.as_str()], false)?.swap_remove(0);
    let mut hits: Vec<Hit> = Vec::new();
    let mut dirs: Vec<PathBuf> = fs::read_dir(root).map_err(|e| e.to_string())?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    for dir in dirs {
        let Ok(n) = load_notes(&dir) else { continue };
        let name = dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        for s in &n.segments {
            if !s.embedding.is_empty() {
                hits.push(Hit { score: models::cosine(&q, &s.embedding), session: name.clone(), at: s.start, kind: s.source.label().into(), text: s.text.clone() });
            }
        }
        let extra: Vec<(f64, String, String)> = n
            .topics
            .iter()
            .map(|t| (t.start, "θέμα".to_string(), t.summary.clone()))
            .chain(n.items.iter().map(|i| (i.at, i.kind.title().to_string(), notes::item_line(i))))
            .collect();
        if !extra.is_empty() {
            let texts: Vec<&str> = extra.iter().map(|e| e.2.as_str()).collect();
            if let Ok(embs) = m.embed(&texts, false) {
                for ((at, kind, text), e) in extra.into_iter().zip(embs) {
                    hits.push(Hit { score: models::cosine(&q, &e), session: name.clone(), at, kind, text });
                }
            }
        }
    }
    hits.sort_by(|x, y| y.score.total_cmp(&x.score));
    hits.truncate(a.top);
    if a.json {
        println!("{}", serde_json::to_string(&hits).unwrap_or_default());
    } else {
        for h in hits {
            println!("{:.3}  {} [{}] {}: {}", h.score, h.session, notes::clock(h.at), h.kind, h.text);
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ run

#[derive(Serialize, Default, Clone)]
struct Status {
    state: String,
    started: String,
    elapsed: f64,
    segments: usize,
    keyframes: usize,
    items: usize,
    topics: usize,
    /// Seconds of speech waiting for the recognizer.
    backlog: f64,
    teams: String,
    last: String,
    models: Vec<(String, bool)>,
    warnings: Vec<String>,
    error: String,
    pid: u32,
    /// Notifications for the app, newest last.
    alerts: Vec<Alert>,
    /// quiz.html once it is written.
    quiz: String,
    sources: usize,
}

enum Event {
    Segment(Segment),
    Keyframe(Keyframe),
}

fn write_atomic(path: &Path, data: &[u8]) {
    let tmp = path.with_extension("tmp");
    if fs::write(&tmp, data).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

fn write_status(session: &Path, st: &Status) {
    write_atomic(&session.join("status.json"), serde_json::to_string_pretty(st).unwrap_or_default().as_bytes());
}

fn run(a: &Args) -> Result<(), String> {
    let session = a.session.clone().ok_or("--session is required")?;
    fs::create_dir_all(session.join("frames")).map_err(|e| format!("{}: {}", session.display(), e))?;
    let _ = fs::remove_file(session.join("stop"));
    let log_file = OpenOptions::new().create(true).append(true).open(session.join("observer.log")).ok();
    let _ = LOG.set(Mutex::new(log_file));
    screen::init_dpi();

    let paths = Arc::new(Paths { root: models_dir(a) });
    let started = chrono::Local::now().format("%d/%m/%Y %H:%M").to_string();
    let status = Arc::new(Mutex::new(Status {
        state: "loading".into(),
        started: started.clone(),
        models: models::report(&paths).into_iter().map(|(n, ok, _)| (n.to_string(), ok)).collect(),
        pid: std::process::id(),
        ..Default::default()
    }));
    if !paths.whistle().is_file() {
        let e = format!("missing {}", paths.whistle().display());
        let mut st = status.lock().unwrap();
        st.state = "error".into();
        st.error = e.clone();
        write_status(&session, &st);
        return Err(e);
    }
    log(&format!("session {} models {}", session.display(), paths.root.display()));
    write_status(&session, &status.lock().unwrap());

    let t0 = Instant::now();
    let stop = Arc::new(AtomicBool::new(false));
    let cores = thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let warn = {
        let status = status.clone();
        move |w: String| {
            log(&w);
            status.lock().unwrap().warnings.push(w);
        }
    };

    // Audio capture -> chunks.
    let (chunk_tx, chunk_rx) = channel::<Chunk>();
    let backlog = Arc::new(AtomicU64::new(0)); // milliseconds of queued speech
    let (counted_tx, counted_rx) = channel::<Chunk>();
    {
        let backlog = backlog.clone();
        thread::spawn(move || {
            for c in chunk_rx {
                backlog.fetch_add(((c.end - c.start) * 1000.0) as u64, Ordering::Relaxed);
                if counted_tx.send(c).is_err() {
                    break;
                }
            }
        });
    }
    let mut capture_threads = Vec::new();
    if let Some(wav) = a.wav.clone() {
        let tx = chunk_tx.clone();
        let stop = stop.clone();
        capture_threads.push(thread::spawn(move || {
            match whistle::audio::read_wav_file(&wav) {
                Ok((samples, rate)) => {
                    let mut seg = audio::Segmenter::new(Source::Others, rate as usize, 0.0);
                    for block in samples.chunks(rate as usize / 10) {
                        seg.push(block, &tx);
                    }
                    seg.flush(&tx);
                }
                Err(e) => log(&format!("{}: {}", wav.display(), e)),
            }
            // A file session ends when the file does.
            stop.store(true, Ordering::Relaxed);
        }));
    } else {
        let sources: &[Source] = match a.audio.as_str() {
            "both" => &[Source::Others, Source::Me],
            "others" => &[Source::Others],
            "me" => &[Source::Me],
            _ => &[],
        };
        for &src in sources {
            let tx = chunk_tx.clone();
            let stop = stop.clone();
            let warn = warn.clone();
            capture_threads.push(thread::spawn(move || {
                if let Err(e) = audio::capture(src, t0, stop, tx) {
                    warn(format!("audio: {e}"));
                }
            }));
        }
    }
    drop(chunk_tx);

    // Events for the note taker.
    let (ev_tx, ev_rx) = channel::<Event>();

    // Speech recognition.
    let asr = {
        let paths = paths.clone();
        let ev_tx = ev_tx.clone();
        let backlog = backlog.clone();
        let language = a.language.clone();
        let beams = a.beams.max(1);
        let warn = warn.clone();
        thread::spawn(move || {
            let model = match models::load_whistle(&paths) {
                Ok(m) => m,
                Err(e) => {
                    warn(format!("whistle: {e}"));
                    for _ in counted_rx {}
                    return;
                }
            };
            log("whistle loaded");
            for c in counted_rx {
                let secs = c.end - c.start;
                // More than a minute behind: one beam instead of five.
                let behind = backlog.load(Ordering::Relaxed) > 60_000;
                let opts = whistle::Options { language: language.clone(), beams: if behind { 1 } else { beams } };
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| model.transcribe(&c.pcm, &opts)));
                backlog.fetch_sub(((secs) * 1000.0) as u64, Ordering::Relaxed);
                match r {
                    Ok(Ok(t)) if !t.silent && !t.text.trim().is_empty() => {
                        let _ = ev_tx.send(Event::Segment(Segment {
                            source: c.source,
                            start: c.start,
                            end: c.end,
                            text: t.text,
                            language: t.language,
                            embedding: Vec::new(),
                        }));
                    }
                    Ok(Err(e)) => log(&format!("whistle: {e}")),
                    Err(_) => log("whistle panicked on a chunk; skipped"),
                    _ => {}
                }
            }
        })
    };

    // Screen.
    let vision = if a.screen != "off" && a.wav.is_none() {
        let paths = paths.clone();
        let ev_tx = ev_tx.clone();
        let stop = stop.clone();
        let mode = a.screen.clone();
        let interval = Duration::from_secs_f64(a.interval.max(1.0));
        let session = session.clone();
        let threads = (cores / 4).max(1);
        let warn = warn.clone();
        Some(thread::spawn(move || {
            let clip = match models::load_mobileclip(&paths, threads) {
                Ok(c) => {
                    log("mobileclip loaded");
                    Some(c)
                }
                Err(e) => {
                    warn(format!("mobileclip: {e} (frames are kept without labels)"));
                    None
                }
            };
            watch_screen(&mode, interval, &session, clip, &stop, t0, &ev_tx);
        }))
    } else {
        None
    };

    // Note taker.
    let notes_thread = {
        let paths = paths.clone();
        let session = session.clone();
        let status = status.clone();
        let warn = warn.clone();
        let started = started.clone();
        let threads = (cores / 2).max(1);
        let webhook = if a.post { a.webhook.clone() } else { None };
        let from_wav = a.wav.is_some();
        let quiz_lang = quiz_lang(a).to_string();
        let opts = notes::Options { names: a.names.clone(), web: !a.no_web };
        thread::spawn(move || {
            let minilm = models::load_minilm(&paths, 2).map_err(|e| warn(format!("minilm: {e}"))).ok();
            let smol = models::load_smollm(&paths, threads).map_err(|e| warn(format!("smollm2: {e}"))).ok();
            let needle = models::load_needle(&paths).map_err(|e| warn(format!("needle: {e}"))).ok();
            log("note models loaded");
            status.lock().unwrap().state = "running".into();
            let notes = Notes { started, own_names: opts.names.clone(), ..Default::default() };
            let mut engine = Engine::new(notes, minilm, smol, needle, opts, log);
            take_notes(&mut engine, ev_rx, &session, &status, t0);
            status.lock().unwrap().state = "finishing".into();
            write_status(&session, &status.lock().unwrap());
            log("writing the final notes");
            // A WAV session is read faster than real time: its length is the audio's.
            let duration = if from_wav { engine.notes.segments.last().map_or(0.0, |s| s.end) } else { t0.elapsed().as_secs_f64() };
            engine.finish(duration);
            save(&engine.notes, &session, false);
            log("building the quiz");
            match write_quiz(&session, &engine.quiz(quiz_seed(), &quiz_lang)) {
                Ok(path) => {
                    let mut st = status.lock().unwrap();
                    st.quiz = path.display().to_string();
                    st.sources = engine.notes.sources.len();
                    st.alerts = engine.notes.alerts.clone();
                }
                Err(e) => warn(format!("quiz: {e}")),
            }
            let card = teams::adaptive_card(&engine.notes);
            write_atomic(&session.join("card.json"), serde_json::to_string_pretty(&card).unwrap_or_default().as_bytes());
            if let Some(url) = webhook {
                match teams::post(&url, &card) {
                    Ok(()) => log("posted to Teams"),
                    Err(e) => warn(format!("Teams post: {e}")),
                }
            }
        })
    };
    drop(ev_tx);

    // Typing 'stop' on stdin also stops. EOF (no console) is ignored.
    {
        let stop = stop.clone();
        thread::spawn(move || {
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                if matches!(line.trim(), "stop" | "q" | "quit") {
                    stop.store(true, Ordering::Relaxed);
                }
            }
        });
    }

    // Supervision: stop conditions, Teams presence, status file.
    let mut seen_meeting = false;
    let mut meeting_gone: Option<Instant> = None;
    let mut last_teams = String::new();
    let mut tick = 0u64;
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(500));
        tick += 1;
        if session.join("stop").exists() {
            log("stop requested");
            stop.store(true, Ordering::Relaxed);
        }
        if a.duration.is_some_and(|d| t0.elapsed().as_secs_f64() >= d) {
            stop.store(true, Ordering::Relaxed);
        }
        if tick % 10 == 0 && a.wav.is_none() {
            let teams = screen::find_teams();
            let title = teams.as_ref().map(|t| t.title.clone()).unwrap_or_default();
            if title != last_teams {
                last_teams = title.clone();
                status.lock().unwrap().teams = title;
            }
            let in_meeting = teams.as_ref().is_some_and(|t| t.in_meeting);
            seen_meeting |= in_meeting;
            if in_meeting {
                meeting_gone = None;
            } else if seen_meeting && meeting_gone.is_none() {
                meeting_gone = Some(Instant::now());
            }
            if a.auto_stop && meeting_gone.is_some_and(|g| g.elapsed() > Duration::from_secs(60)) {
                log("Teams meeting ended; stopping");
                stop.store(true, Ordering::Relaxed);
            }
        }
        if tick % 2 == 0 {
            let mut st = status.lock().unwrap();
            st.elapsed = t0.elapsed().as_secs_f64();
            st.backlog = backlog.load(Ordering::Relaxed) as f64 / 1000.0;
            write_status(&session, &st);
        }
    }

    for t in capture_threads {
        let _ = t.join();
    }
    if let Some(v) = vision {
        let _ = v.join();
    }
    status.lock().unwrap().state = "finishing".into();
    write_status(&session, &status.lock().unwrap());
    let _ = asr.join();
    let _ = notes_thread.join();
    let _ = fs::remove_file(session.join("stop"));
    let mut st = status.lock().unwrap();
    st.state = "done".into();
    st.elapsed = t0.elapsed().as_secs_f64();
    st.backlog = 0.0;
    write_status(&session, &st);
    log("done");
    Ok(())
}

fn save(notes: &Notes, session: &Path, live: bool) {
    write_atomic(&session.join("notes.md"), notes.markdown(live).as_bytes());
    write_atomic(&session.join("notes.json"), serde_json::to_string(notes).unwrap_or_default().as_bytes());
}

fn take_notes(engine: &mut Engine, rx: Receiver<Event>, session: &Path, status: &Arc<Mutex<Status>>, t0: Instant) {
    let mut dirty = true;
    let mut last_save = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Event::Segment(s)) => {
                let text = format!("{}: {}", s.source.label(), s.text);
                if engine.add_segment(s) {
                    status.lock().unwrap().last = text;
                    dirty = true;
                }
            }
            Ok(Event::Keyframe(k)) => {
                engine.add_keyframe(k);
                dirty = true;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let alerts = engine.notes.alerts.len();
        dirty |= engine.collect_items();
        dirty |= engine.collect_sources();
        // A new alert is written at once: the app turns it into a notification.
        let urgent = engine.notes.alerts.len() != alerts || status.lock().unwrap().alerts.len() != engine.notes.alerts.len();
        // The supervisor tracks the Teams window; the first title seen names the meeting.
        {
            let st = status.lock().unwrap();
            if !st.teams.is_empty() && engine.notes.teams_title.is_empty() {
                engine.notes.teams_title = st.teams.clone();
                dirty = true;
            }
        }
        if urgent || (dirty && last_save.elapsed() > Duration::from_secs(3)) {
            engine.notes.duration = t0.elapsed().as_secs_f64();
            save(&engine.notes, session, true);
            let mut st = status.lock().unwrap();
            st.segments = engine.notes.segments.len();
            st.keyframes = engine.notes.keyframes.len();
            st.items = engine.notes.items.len();
            st.topics = engine.notes.topics.len();
            st.sources = engine.notes.sources.len();
            st.alerts = engine.notes.alerts.clone();
            write_status(session, &st);
            dirty = false;
            last_save = Instant::now();
        }
    }
}

// ------------------------------------------------------------------ screen

/// 32x18 grey thumbnail, for a cheap "did anything change" test.
fn thumb(img: &image::RgbImage) -> Vec<u8> {
    let small = image::imageops::resize(img, 32, 18, image::imageops::FilterType::Triangle);
    small.pixels().map(|p| ((p[0] as u16 + p[1] as u16 + p[2] as u16) / 3) as u8).collect()
}

fn thumb_diff(a: &[u8], b: &[u8]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 255.0;
    }
    a.iter().zip(b).map(|(x, y)| (*x as i16 - *y as i16).unsigned_abs() as f32).sum::<f32>() / a.len() as f32
}

const MIN_KEYFRAME_GAP: Duration = Duration::from_secs(10);
/// MobileCLIP embeddings this similar show the same thing.
const SAME_SCENE: f32 = 0.93;

fn watch_screen(
    mode: &str,
    interval: Duration,
    session: &Path,
    clip: Option<(mobileclip_s0::MobileClip, Option<mobileclip_s0::TextBank>)>,
    stop: &AtomicBool,
    t0: Instant,
    tx: &Sender<Event>,
) {
    let mut last_thumb: Vec<u8> = Vec::new();
    let mut last_emb: Vec<f32> = Vec::new();
    let mut last_kf = Instant::now() - MIN_KEYFRAME_GAP;
    let mut n = 0usize;
    while !stop.load(Ordering::Relaxed) {
        let tick = Instant::now();
        let img = if mode == "monitor" { screen::capture_screen() } else { screen::find_teams().and_then(|w| screen::capture_window(w.hwnd)) };
        if let Some(img) = img {
            let th = thumb(&img);
            let moved = thumb_diff(&th, &last_thumb);
            if moved > 6.0 && last_kf.elapsed() >= MIN_KEYFRAME_GAP {
                let mut keep = clip.is_none() && moved > 12.0;
                let mut labels = Vec::new();
                if let Some((model, bank)) = &clip {
                    let small = image::imageops::resize(&img, 256, 256, image::imageops::FilterType::Triangle);
                    let pixels = mobileclip_s0::preprocess::to_nchw(&small);
                    if let Ok(emb) = model.embed(&pixels) {
                        keep = last_emb.is_empty() || models::cosine(&emb, &last_emb) < SAME_SCENE;
                        if keep {
                            if let Some(b) = bank {
                                labels = b.top_k(&emb, 3).map(|v| v.into_iter().map(|m| (m.label, m.score)).collect()).unwrap_or_default();
                            }
                            last_emb = emb;
                        }
                    }
                }
                if keep {
                    n += 1;
                    let at = t0.elapsed().as_secs_f64();
                    let file = format!("frames/{:04}_{}.png", n, notes::clock(at).replace(':', "-"));
                    let out = if img.width() > 1280 {
                        let h = img.height() * 1280 / img.width();
                        image::imageops::resize(&img, 1280, h, image::imageops::FilterType::Triangle)
                    } else {
                        img.clone()
                    };
                    match out.save(session.join(&file)) {
                        Ok(()) => {
                            log(&format!("keyframe {file} {:?}", labels.iter().map(|l| &l.0).collect::<Vec<_>>()));
                            let _ = tx.send(Event::Keyframe(Keyframe { at, file, labels }));
                        }
                        Err(e) => log(&format!("{file}: {e}")),
                    }
                    last_kf = Instant::now();
                }
                last_thumb = th;
            }
        }
        while tick.elapsed() < interval && !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(200));
        }
    }
}
