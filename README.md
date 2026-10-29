# LinkNotifier

A small Windows desktop app (Python, Tkinter) for a student's day, with
**Observer**: a local agent written in Rust that takes notes in lectures and
Microsoft Teams meetings and turns them into a self-check quiz.

## What it does

- **Today / Obligations**: a daily checklist built from an editable list of
  obligations (work, classes, gym, driving test practice, personal
  projects, or anything else), each on chosen days, checked by hand, at a
  time, when the day's classes end, or when a driving test is finished.
- **Links**: links that open only during their time window, with a
  notification when the window starts.
- **YouTube / Websites**: notifications for new videos and page changes.
- **Observer** (button "Send Observer"):
  - listens to what Teams plays and to the microphone, and watches only the
    Teams window, all on this computer;
  - writes notes: transcript, topics, action items, decisions, open questions,
    screenshots of new slides;
  - notifies you when your name is said or something sounds important
    ("this will be on the exam", a deadline, a new task);
  - when it stops, builds `quiz.html` for any subject: "What is a CNN?",
    "What is photosynthesis?", "What does ATP stand for?", "Type the
    formula for kinetic energy", and fill-in-the-blank on what was said,
    with related Wikipedia articles. The quiz leaves out people's names, so
    it can be shared.

Details: [observer/README.md](observer/README.md) and [README.txt](README.txt).

## Models

Observer runs five small models in pure Rust, included as git submodules:

| Model | Repository |
|---|---|
| Cactus Whistle (speech to text) | [Cactus_Whistle-Pure_Rust](https://github.com/AlexandrosGiann/Cactus_Whistle-Pure_Rust) |
| SmolLM2-135M-Instruct | [SmolLM2_135MInstruct-PureRust](https://github.com/AlexandrosGiann/SmolLM2_135MInstruct-PureRust) |
| MobileCLIP-S0 | [MobileCLIP-S0_PureRust](https://github.com/AlexandrosGiann/MobileCLIP-S0_PureRust) |
| paraphrase-multilingual-MiniLM-L12-v2 | [paraphrase-multilingual-MiniLM-L12-v2_PureRust](https://github.com/AlexandrosGiann/paraphrase-multilingual-MiniLM-L12-v2_PureRust) |
| Needle | [needle-rs](https://github.com/Geekgineer/needle-rs) |

The model weights are not in this repository; `download_models.ps1` fetches
them (about 240 MB) from Hugging Face and GitHub.

## Install

Needs Windows, Python 3 with Tkinter, Rust (stable) and git.

```bash
git clone --recursive https://github.com/AlexandrosGiann/LinkNotifier
cd LinkNotifier
install_dependencies.bat
cd observer
cargo build --release
powershell -ExecutionPolicy Bypass -File download_models.ps1
cd ..
run_app.bat
```

`run_app.bat` and `install_dependencies.bat` expect Python at
`C:\Python314\python.exe`; edit them if yours is elsewhere.

## Privacy

Audio, screenshots, transcripts and notes stay on the computer
(`%LOCALAPPDATA%\LinkNotifier\observer`). The network is used for Wikipedia
lookups of topic key phrases (can be turned off) and, only if you set a
webhook, to post the notes to a Teams channel. Tell the people in a meeting
that you are taking notes with a recording tool.
