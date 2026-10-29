LINK NOTIFIER - STARTUP VERSION
=================================

No Windows Service is used.

WHAT IT DOES
------------
1. Scheduled links:
   - Choose one or more weekdays.
   - Choose start/end time, e.g. Monday 18:00-21:00.
   - The Open button is enabled only during the allowed time.
   - A notification is sent when the allowed window begins.

2. YouTube:
   - Add YouTube channel URLs.
   - Checks every 3 hours.
   - Notifies only when a new video appears since the last stored video.

3. Websites:
   - Ollama newest models
   - Hugging Face models
   - Futurepedia
   - Checks every 3 hours.
   - First check creates a baseline.
   - Later changes generate a notification.

4. Persistent data:
   %LOCALAPPDATA%\LinkNotifier\data.json

INSTALL
-------
Recommended permanent folder:
C:\LinkNotifier

1. Copy/extract all files to C:\LinkNotifier

2. Install notification dependency:
   install_dependencies.bat

3. Test the program:
   run_app.bat

4. Add it to Windows Startup:
   install_startup.bat

At the next Windows login it will launch with pythonw.exe
and minimize itself automatically.

REMOVE FROM STARTUP
-------------------
Run:
remove_startup.bat

LOG
---
%LOCALAPPDATA%\LinkNotifier\app.log


BULK YOUTUBE IMPORT
-------------------
Open the YouTube tab and click:
"Μαζική επικόλληση"

Paste one channel per line in this format:

Two Minute Papers https://www.youtube.com/channel/UCbfYPyITQ-7l4upoX8nvctg
AI Search https://www.youtube.com/channel/UCIgnGlGkVRhd4qNFcEwLL4A
Bycloud https://www.youtube.com/channel/UCgfe2ooZD3VJPB6aJAnuQng

Channel names may contain spaces.
The final space-separated item on each line must be the YouTube URL.
Duplicate URLs are skipped automatically.


BULK IMPORT FIX
---------------
The paste window accepts both:
- real line breaks (Enter)
- literal \n copied from formatted text

A direct /channel/ URL is saved even if YouTube temporarily returns
HTTP 404/other feed errors. It will be retried during the next
automatic 3-hour check.


DAILY CHECKLIST (tabs "Σήμερα" and "Υποχρεώσεις")
--------------------------------------------------
The obligations are an editable list (tab "Υποχρεώσεις"): add, edit
(double click), delete, move up/down, or make one inactive to hide it
without losing its history. Each has:
- a name;
- days (none = every day): it appears in "Σήμερα" only on those days;
- how it gets checked:
  - Χειροκίνητα: by you;
  - Σε συγκεκριμένη ώρα: automatically at a time (e.g. work at 18:30);
  - Στο τέλος του μαθήματος: when the day's classes end (class end times
    under "Ώρες μαθημάτων / τεστ Κ.Ο.Κ.", seeded from the scheduled links);
  - Όταν ολοκληρωθεί τεστ Κ.Ο.Κ.: the K.O.K. test
    (alex151807\quizzes_kok.html) opens once a day (default 10:00) via a
    local server at http://127.0.0.1:8765; a finished test is recorded in
    %LOCALAPPDATA%\LinkNotifier\driving_tests.json and checks the task.
    Tests taken on the GitHub Pages site are NOT detected - use the
    "Άνοιγμα τεστ" button or the daily auto-open.
The first start seeds the five original obligations (Δουλειά Mon-Fri at
18:30, Μεταπτυχιακό, Γυμναστική, Οδήγηση, Προσωπικά projects) with the
settings they had.

"Σήμερα" resets every day; past days are kept in the history table
(60 days). A task you check or uncheck yourself is left alone that day.


OBSERVER (tab "Observer")
-------------------------
"Send Observer" starts a local agent that takes meeting notes from what
it hears (Teams audio + microphone) and sees (only the Teams window).
"Stop Observer" ends it; the final summary is written after a few seconds.

Setup (once):
  cd C:\LinkNotifier\observer
  cargo build --release
  powershell -ExecutionPolicy Bypass -File download_models.ps1

Notes per meeting: %LOCALAPPDATA%\LinkNotifier\observer\<date_time>\notes.md
To post the notes to a Teams channel, paste a Teams Workflows webhook URL
in Observer > Ρυθμίσεις. Details: observer\README.md
Speech recognition does not support Greek (en, de, fr, es, it, nl, pl).

While it runs, a notification appears when your name is said (set your
names in Observer > Ρυθμίσεις), when something sounds important (exam,
deadline, assignment...) or when a new action item/decision is heard.
When it stops it also writes quiz.html: a self-check quiz from what was
said plus related Wikipedia articles (button "Quiz").
