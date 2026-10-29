# Observer

The meeting note taker behind the **Send Observer** button (tab "Observer" in
Link Notifier). It is written in Rust and its models run on this computer.
The network is used only for Wikipedia lookups of topic key phrases (for the
quiz, can be turned off) and the optional post of the notes to Teams.

## How it works with Microsoft Teams

Observer does not join the meeting as a bot. A Teams bot that receives meeting
media needs an Azure app registration, admin consent and a C# media service,
and it shows up as a participant. Observer works next to whatever Teams client
is open: new Teams (`ms-teams.exe`), classic Teams or Teams in a browser.

| What | How |
|---|---|
| Hears the others | WASAPI loopback of the default speakers/headset: everything Teams plays |
| Hears you | The default microphone, kept apart, so notes say who spoke ("Εγώ" / "Συμμετέχοντες") |
| Sees | Only the Teams window (`PrintWindow`, also when covered). Other windows are never captured. `--screen monitor` takes the whole primary screen instead |
| Knows the meeting | Finds the Teams window and its title. With `--auto-stop` it stops 60 s after the meeting window closes |
| Sends to Teams | At the end (or with the "Αποστολή στο Teams" button) it posts an Adaptive Card to a channel through a **Workflows** webhook |

To get a webhook: in Teams, open the channel, `...` > **Workflows** >
"Post to a channel when a webhook request is received", finish the wizard and
copy the URL into the Observer settings in Link Notifier.

## The models

| Model | Repository | Role |
|---|---|---|
| Cactus Whistle | `vendor/Cactus_Whistle-Pure_Rust` | speech to text for every utterance |
| paraphrase-multilingual-MiniLM-L12-v2 | `vendor/paraphrase-multilingual-MiniLM-L12-v2_PureRust` | sentence embeddings: drops the mic's echo of the speakers, splits the meeting into topics, search across all meetings |
| Needle 3 (or 2) | `vendor/needle-rs` | reads utterances that sound like a commitment, decision, question or plan (cue words, or MiniLM close to an example) as a tool call: `add_action_item`, `record_decision`, `record_open_question`, `schedule_follow_up`. Runs on its own thread (about 10 s per call) |
| SmolLM2-135M-Instruct | `vendor/SmolLM2_135MInstruct-PureRust` (wrapped as a library in `crates/smollm2`) | a short title per topic |
| MobileCLIP-S0 | `vendor/MobileCLIP-S0_PureRust` | decides when the Teams window shows something new (a new slide, a shared screen) and keeps it as a keyframe with labels from the text bank |

Pipeline:

```
loopback ─┐                         ┌─ MiniLM: echo filter, topic split, key points ─┐
          ├─ utterances ─ Whistle ──┤                                                ├─ notes.md / notes.json / card.json
mic ──────┘                         └─ Needle: action items, decisions, questions ───┘        │
Teams window ─ change? ─ MobileCLIP ─ keyframes (frames/*.png) ───────────────────────────────┤
                                     topic closed ─ SmolLM2 title ────────────────────────────┘
```

Small models invent things, so nothing they write goes into the notes
unchecked:

- Topic key points are sentences that were said (the ones MiniLM finds
  closest to the topic), not generated text.
- A Needle call is kept only when most of its words are in the sentence, its
  kind matches the wording (a "decision" needs "decided"/"agreed", a question
  needs a question form, and so on), and its owner and deadline were said.
- A SmolLM2 title is kept only when half its words are in the topic and
  MiniLM finds it close to the topic.
- The overview lists the titles and counts; it is not generated.

## Notifications while it listens

A Windows notification goes off, from the Link Notifier app, when:

- a participant says one of your names (Observer settings, "Τα ονόματά μου");
- someone says something that sounds important: "exam", "deadline",
  "don't forget", "assignment" and the like in Whistle's languages, or a
  sentence MiniLM finds close to "This will be on the exam";
- Needle finds a new action item, decision or follow-up meeting.

Cue alerts are at least 20 s apart, so a burst makes one notification. All
alerts are also listed under "Σημαντικά" in the notes.

## The quiz

When the observer stops it writes `quiz.html`, a self-contained page that
opens in the browser, with feedback on every answer, the score, and "again,
only the mistakes".

- Each closed topic gets key phrases (MiniLM, KeyBERT style). The acronyms
  said in it (CNN, RNN, ...) and its top phrases are searched on Wikipedia in
  the meeting's language while the meeting is still running. Articles MiniLM
  finds close to the topic are kept, with their definition (the first
  sentence) and the sentences closest to the topic; articles of the same
  field are kept for their definitions only. Only key phrases are sent,
  never the transcript and never a name. Turn it off with "Αναζήτηση
  σχετικών πηγών" (`--no-web`).
- Questions, in this order:
  - **concepts**: "What is a CNN?", the options being the Wikipedia
    definitions of the concept and of related ones;
  - **acronyms**: "What does GAN stand for?";
  - **equations**: "Type the equation of the sigmoid function". Sigmoid, its
    derivative, tanh, ReLU, leaky ReLU and softplus are asked when the
    session mentions them. The page evaluates the typed formula at several
    points, so `1/(1+e^-x)`, `e^x/(e^x+1)` and `(1+exp(-x))^-1` all count;
    typing the function's own name (`tanh(x)`) does not;
  - fill-in-the-blank on sentences said in the session and of the sources.
- Every question links back to the moment [mm:ss] or the article it came from.
- **No names.** The quiz is meant to be shared with the people who were
  there: there are no "who" questions, any question that contains a name
  (your names, people addressed, owners of action items, capitalized words
  never written in lower case) is dropped, and names in explanations become
  "…". Names Whistle wrote in lower case cannot be recognized, so look the
  quiz over before sending it.
- Language: English by default, Greek with "Γλώσσα quiz" (`--quiz-lang el`).
  Definitions and quotes stay in the language they were said or written in.
- `observer quiz --session DIR` rebuilds the quiz of any earlier session,
  looking up sources first if it has none.

Only Whistle is required. Without one of the other models, that part of the
notes is left out and a warning goes to `observer.log`.

## Limits

- Whistle recognizes English, German, French, Spanish, Italian, Dutch and
  Polish. **Greek speech is not recognized.**
- SmolLM2-135M is very small: free summaries came out invented, so it only
  proposes titles, and many are rejected. Topics without a title are normal.
- Everything runs on the CPU. When recognition falls more than a minute behind,
  Whistle switches from 5 beams to 1 until it catches up.
- Recording a meeting can require the participants' consent. The app asks you
  to confirm this once.

## Build and models

```bash
cargo build --release
powershell -ExecutionPolicy Bypass -File download_models.ps1
target/release/observer check
```

## Command line

```bash
observer run --session DIR [--screen teams|monitor|off] [--audio both|others|me] [--language en] [--auto-stop] [--webhook URL --post]
observer run --session DIR --wav meeting.wav     # notes from a recording
observer post --session DIR --webhook URL
observer search --root %LOCALAPPDATA%\LinkNotifier\observer "budget for Q3"
```

A running session stops when the file `DIR/stop` appears (this is what the
Stop Observer button does) or when you type `stop` on its console.

Session folder (`%LOCALAPPDATA%\LinkNotifier\observer\<date_time>\`):
`notes.md`, `notes.json` (with embeddings, for search), `card.json`,
`quiz.html`, `quiz.json`, `frames/`, `status.json`, `observer.log`.
