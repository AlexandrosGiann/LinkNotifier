import json
import os
import re
import subprocess
import hashlib
import threading
import time
import webbrowser
from pathlib import Path
from datetime import datetime
from http.server import ThreadingHTTPServer, SimpleHTTPRequestHandler
from functools import partial
from urllib.request import Request, urlopen
import xml.etree.ElementTree as ET

import tkinter as tk
from tkinter import ttk, messagebox, filedialog

try:
    from winotify import Notification
except Exception:
    Notification = None


# ============================================================
# Link Notifier - Startup version (no Windows service)
# ============================================================

APP_DIR = Path(os.environ.get("LOCALAPPDATA", str(Path.home()))) / "LinkNotifier"
APP_DIR.mkdir(parents=True, exist_ok=True)

DATA_FILE = APP_DIR / "data.json"
LOG_FILE = APP_DIR / "app.log"

CHECK_INTERVAL_SECONDS = 30          # scheduled links
ONLINE_CHECK_INTERVAL = 3 * 60 * 60 # 3 hours

DAY_NAMES = [
    "Monday", "Tuesday", "Wednesday", "Thursday",
    "Friday", "Saturday", "Sunday"
]

DAY_LABELS = {
    "Monday": "Δευτέρα",
    "Tuesday": "Τρίτη",
    "Wednesday": "Τετάρτη",
    "Thursday": "Πέμπτη",
    "Friday": "Παρασκευή",
    "Saturday": "Σάββατο",
    "Sunday": "Κυριακή",
}

DEFAULT_DATA = {
    "scheduled_links": [],
    "youtube_channels": [],
    "watched_pages": [
        {
            "name": "Ollama - Newest models",
            "url": "https://ollama.com/search?o=newest",
            "last_hash": ""
        },
        {
            "name": "Hugging Face - Models",
            "url": "https://huggingface.co/models",
            "last_hash": ""
        },
        {
            "name": "Futurepedia",
            "url": "https://www.futurepedia.io/",
            "last_hash": ""
        }
    ],
    "last_online_check": "",
    "checklist": {},
    "checklist_settings": {},
    "observer_settings": {}
}

# ------------------------------------------------------------
# Daily checklist
# ------------------------------------------------------------

DRIVING_FILE = APP_DIR / "driving_tests.json"
DATA_LOCK = threading.RLock()
DRIVING_LOCK = threading.Lock()

CHECKLIST_TASKS = [
    ("work", "Δουλειά"),
    ("masters", "Μεταπτυχιακό"),
    ("gym", "Γυμναστική"),
    ("driving", "Οδήγηση (τεστ Κ.Ο.Κ.)"),
    ("projects", "Προσωπικά projects"),
]

DEFAULT_CHECKLIST_SETTINGS = {
    "work_auto_time": "18:30",
    "work_days": DAY_NAMES[:5],
    # Day -> time the last class ends. Seeded from the scheduled links.
    "masters_schedule": {},
    "driving_open_time": "10:00",
    "alex_dir": str(Path.home() / "Desktop" / "alex151807"),
    "server_port": 8765,
}

HISTORY_DAYS = 60

# ------------------------------------------------------------
# Observer (meeting notes agent, Rust: observer/)
# ------------------------------------------------------------

OBSERVER_DIR = Path(__file__).resolve().parent / "observer"
OBSERVER_EXE = OBSERVER_DIR / "target" / "release" / "observer.exe"
OBSERVER_MODELS = OBSERVER_DIR / "models"
OBSERVER_SESSIONS = APP_DIR / "observer"
OBSERVER_LANGUAGES = ["auto", "en", "de", "fr", "es", "it", "nl", "pl"]

DEFAULT_OBSERVER_SETTINGS = {
    # Teams > Workflows > "Post to a channel when a webhook request is received"
    "webhook": "",
    "auto_post": False,
    "language": "auto",
    "screen": "teams",     # teams | monitor | off
    "audio": "both",       # both | others | me
    "auto_stop": True,     # stop when the Teams meeting window closes
    "consent_ack": False,
    # Comma separated; a participant saying one raises a notification.
    "my_names": "",
    # Wikipedia lookups of topic key phrases, for the quiz.
    "web_sources": True,
    # Language of the quiz page and questions: "en" or "el".
    "quiz_language": "en",
}

OBSERVER_STATES = {
    "loading": "Φόρτωση μοντέλων...",
    "running": "Καταγράφει",
    "finishing": "Γράφει τις τελικές σημειώσεις...",
    "done": "Ολοκληρώθηκε",
    "error": "Σφάλμα",
}
DRIVING_POST_PATH = "/linknotifier/driving-done"

# Injected into quizzes_kok.html when it is served locally. It forwards
# finished K.O.K. attempts (stored by the page in localStorage) to the app.
DRIVING_HOOK_SCRIPT = """
<script>
(function () {
  var KEY = 'quizProgressHistory', SENT = 'linkNotifierSentTs';
  function sync() {
    try {
      var all = JSON.parse(localStorage.getItem(KEY) || '[]');
      var last = +(localStorage.getItem(SENT) || 0);
      var fresh = all.filter(function (e) {
        return e && e.topic === 'kok' && +e.ts > last;
      });
      if (!fresh.length) return;
      fetch('""" + DRIVING_POST_PATH + """', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify(fresh)
      }).then(function (r) {
        if (r.ok) {
          var max = Math.max.apply(null, fresh.map(function (e) { return +e.ts; }));
          localStorage.setItem(SENT, String(max));
        }
      }).catch(function () {});
    } catch (e) {}
  }
  setInterval(sync, 3000);
  window.addEventListener('pagehide', sync);
  sync();
})();
</script>
"""


def log(message):
    try:
        with LOG_FILE.open("a", encoding="utf-8") as f:
            f.write(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] {message}\n")
    except Exception:
        pass


def load_data():
    try:
        if DATA_FILE.exists():
            data = json.loads(DATA_FILE.read_text(encoding="utf-8"))
        else:
            data = json.loads(json.dumps(DEFAULT_DATA))
    except Exception as exc:
        log(f"load_data error: {exc!r}")
        data = json.loads(json.dumps(DEFAULT_DATA))

    for key, value in DEFAULT_DATA.items():
        data.setdefault(key, json.loads(json.dumps(value)))

    # Restore the three default pages if an older data file is used.
    existing_urls = {x.get("url") for x in data["watched_pages"]}
    for item in DEFAULT_DATA["watched_pages"]:
        if item["url"] not in existing_urls:
            data["watched_pages"].append(dict(item))

    settings = data["checklist_settings"]
    if "masters_schedule" not in settings:
        settings["masters_schedule"] = masters_schedule_from_links(
            data["scheduled_links"]
        )
    for key, value in DEFAULT_CHECKLIST_SETTINGS.items():
        settings.setdefault(key, json.loads(json.dumps(value)))

    for key, value in DEFAULT_OBSERVER_SETTINGS.items():
        data["observer_settings"].setdefault(key, value)

    return data


def save_data(data):
    tmp = DATA_FILE.with_suffix(".tmp")
    tmp.write_text(
        json.dumps(data, ensure_ascii=False, indent=2),
        encoding="utf-8"
    )
    tmp.replace(DATA_FILE)


def notify(title, message, url=""):
    try:
        if Notification:
            toast = Notification(
                app_id="Link Notifier",
                title=title,
                msg=message,
                duration="long"
            )
            if url:
                toast.add_actions(label="Άνοιγμα", launch=url)
            toast.show()
        else:
            # Fallback is logged; GUI remains usable without winotify.
            log(f"NOTIFICATION: {title} | {message} | {url}")
    except Exception as exc:
        log(f"notification error: {exc!r}")


def fetch_text(url):
    req = Request(
        url,
        headers={
            "User-Agent": (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) "
                "AppleWebKit/537.36 Chrome/130 Safari/537.36 "
                "LinkNotifier/3.0"
            )
        }
    )
    with urlopen(req, timeout=30) as response:
        return response.read().decode("utf-8", errors="ignore")


def page_fingerprint(url):
    html = fetch_text(url)

    # Remove volatile blocks.
    html = re.sub(
        r"(?is)<(script|style|noscript|svg)\b.*?>.*?</\1>",
        " ",
        html
    )

    # Prefer catalogue/card-like content.
    parts = re.findall(
        r'(?is)<(?:a|h1|h2|h3|article)\b[^>]*>.*?</(?:a|h1|h2|h3|article)>',
        html
    )

    sample = "\n".join(parts[:600]) if parts else html[:150000]
    sample = re.sub(r"(?is)<[^>]+>", " ", sample)
    sample = re.sub(r"\s+", " ", sample).strip()

    return hashlib.sha256(
        sample.encode("utf-8", errors="ignore")
    ).hexdigest()


def extract_youtube_channel_id(channel_url):
    direct = re.search(r"/channel/(UC[A-Za-z0-9_-]+)", channel_url)
    if direct:
        return direct.group(1)

    html = fetch_text(channel_url)

    for pattern in (
        r'"channelId":"(UC[A-Za-z0-9_-]+)"',
        r'"externalId":"(UC[A-Za-z0-9_-]+)"',
        r'itemprop="channelId"\s+content="(UC[A-Za-z0-9_-]+)"'
    ):
        m = re.search(pattern, html)
        if m:
            return m.group(1)

    raise ValueError("Δεν βρέθηκε YouTube channel ID.")


def get_latest_youtube_video(channel_id):
    xml_text = fetch_text(
        f"https://www.youtube.com/feeds/videos.xml?channel_id={channel_id}"
    )

    root = ET.fromstring(xml_text)

    ns = {
        "atom": "http://www.w3.org/2005/Atom",
        "yt": "http://www.youtube.com/xml/schemas/2015"
    }

    entry = root.find("atom:entry", ns)
    if entry is None:
        raise ValueError("Δεν βρέθηκαν videos.")

    video = entry.find("yt:videoId", ns)
    title = entry.find("atom:title", ns)
    link = entry.find("atom:link", ns)

    return (
        video.text if video is not None else "",
        title.text if title is not None else "Νέο YouTube video",
        link.attrib.get("href", "") if link is not None else ""
    )


def is_link_available(item, now=None):
    now = now or datetime.now()

    today = now.strftime("%A")
    current = now.strftime("%H:%M")
    days = item.get("days", [])

    start = item.get("start_time", "00:00")
    end = item.get("end_time", "23:59")

    # Normal window, e.g. 18:00-21:00
    if start <= end:
        return today in days and start <= current <= end

    # Window crossing midnight, e.g. Monday 22:00-02:00
    if today in days and current >= start:
        return True

    yesterday = DAY_NAMES[(DAY_NAMES.index(today) - 1) % 7]
    return yesterday in days and current <= end


def masters_schedule_from_links(links):
    # Latest end time per weekday among the scheduled class links.
    schedule = {}
    for item in links:
        end = item.get("end_time", "")
        if not end:
            continue
        for day in item.get("days", []):
            schedule[day] = max(schedule.get(day, ""), end)
    return schedule


def ensure_checklist_today(data, now=None):
    """Resets the checklist when the date has changed. Returns True if reset."""
    now = now or datetime.now()
    checklist = data["checklist"]
    today = now.strftime("%Y-%m-%d")

    if checklist.get("date") == today:
        return False

    if checklist.get("date"):
        history = checklist.setdefault("history", {})
        history[checklist["date"]] = [
            task_id for task_id, state in checklist.get("items", {}).items()
            if state.get("done")
        ]
        for old in sorted(history)[:-HISTORY_DAYS]:
            del history[old]

    checklist["date"] = today
    checklist["items"] = {}
    checklist["driving_opened"] = False
    return True


def set_checklist_item(data, task_id, done, by, note="", now=None):
    now = now or datetime.now()
    data["checklist"]["items"][task_id] = {
        "done": done,
        "by": by,
        "at": now.strftime("%H:%M"),
        "note": note
    }


def load_driving_tests():
    try:
        if DRIVING_FILE.exists():
            return json.loads(DRIVING_FILE.read_text(encoding="utf-8"))
    except Exception as exc:
        log(f"load_driving_tests error: {exc!r}")
    return []


def record_driving_tests(entries):
    """Stores finished K.O.K. attempts. Returns how many were new."""
    if not isinstance(entries, list):
        return 0

    with DRIVING_LOCK:
        tests = load_driving_tests()
        known = {t.get("ts") for t in tests}
        added = 0

        for entry in entries[:50]:
            if not isinstance(entry, dict):
                continue
            try:
                ts = int(entry.get("ts"))
                when = datetime.fromtimestamp(ts / 1000)
                record = {
                    "ts": ts,
                    "date": when.strftime("%Y-%m-%d"),
                    "time": when.strftime("%H:%M"),
                    "quiz": str(entry.get("quizTitleEl", ""))[:200],
                    "score": int(entry.get("scorePercent", 0)),
                    "correct": int(entry.get("correctCount", 0)),
                    "total": int(entry.get("totalQuestions", 0)),
                }
            except (TypeError, ValueError, OverflowError, OSError):
                continue

            if ts in known:
                continue
            known.add(ts)
            tests.append(record)
            added += 1

        if added:
            tmp = DRIVING_FILE.with_suffix(".tmp")
            tmp.write_text(
                json.dumps(tests, ensure_ascii=False, indent=2),
                encoding="utf-8"
            )
            tmp.replace(DRIVING_FILE)

    return added


def driving_test_on(date_key):
    tests = [t for t in load_driving_tests() if t.get("date") == date_key]
    return tests[-1] if tests else None


def describe_driving_test(test):
    return (
        f"Τεστ ολοκληρώθηκε {test['time']} — {test['score']}% "
        f"({test['correct']}/{test['total']})"
    )


def apply_checklist_rules(data, now=None):
    """Auto-checks tasks whose rule is met. Returns the newly checked ids."""
    now = now or datetime.now()
    settings = data["checklist_settings"]
    items = data["checklist"]["items"]
    current = now.strftime("%H:%M")
    weekday = now.strftime("%A")
    newly = []

    # A task the user has already touched today is left alone.
    if "work" not in items:
        if weekday in settings["work_days"] and current >= settings["work_auto_time"]:
            set_checklist_item(data, "work", True, "auto", now=now)
            newly.append("work")

    if "masters" not in items:
        end = settings["masters_schedule"].get(weekday, "")
        if end and current >= end:
            set_checklist_item(data, "masters", True, "auto", now=now)
            newly.append("masters")

    # A finished test is hard evidence, so it overrides a manual uncheck.
    if not items.get("driving", {}).get("done"):
        test = driving_test_on(now.strftime("%Y-%m-%d"))
        if test:
            set_checklist_item(
                data, "driving", True, "test",
                note=describe_driving_test(test), now=now
            )
            newly.append("driving")

    return newly


class QuizRequestHandler(SimpleHTTPRequestHandler):
    """Serves the alex151807 site locally and receives finished K.O.K. tests."""

    def log_message(self, format, *args):
        pass

    def do_GET(self):
        path = self.path.split("?", 1)[0]

        if path.startswith("/.git"):
            self.send_error(404)
            return

        if path in ("/", "/quizzes_kok.html"):
            try:
                html = (Path(self.directory) / "quizzes_kok.html").read_text(
                    encoding="utf-8"
                )
            except OSError:
                self.send_error(404, "quizzes_kok.html not found")
                return

            pos = html.lower().rfind("</body>")
            if pos == -1:
                pos = len(html)
            body = (html[:pos] + DRIVING_HOOK_SCRIPT + html[pos:]).encode("utf-8")

            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(body)
            return

        super().do_GET()

    def do_POST(self):
        if self.path != DRIVING_POST_PATH:
            self.send_error(404)
            return

        # Only accept the page served by this server.
        origin = self.headers.get("Origin")
        allowed = {
            f"http://127.0.0.1:{self.server.server_port}",
            f"http://localhost:{self.server.server_port}",
        }
        content_type = self.headers.get("Content-Type", "")
        if (origin and origin not in allowed) or not content_type.startswith(
            "application/json"
        ):
            self.send_error(403)
            return

        try:
            length = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            length = 0
        if length <= 0 or length > 200_000:
            self.send_error(400)
            return

        try:
            entries = json.loads(self.rfile.read(length).decode("utf-8"))
        except Exception:
            self.send_error(400)
            return

        added = record_driving_tests(entries)
        if added:
            log(f"Driving test recorded ({added} new)")
            callback = getattr(self.server, "on_driving_done", None)
            if callback:
                callback()

        self.send_response(204)
        self.end_headers()


class LinkNotifierApp:
    def __init__(self, root):
        self.root = root
        self.root.title("Link Notifier")
        self.root.geometry("1120x720")
        self.root.minsize(900, 600)

        self.stop_event = threading.Event()
        self.quiz_server = None
        self.observer_proc = None
        self.observer_session = None

        self.build_ui()
        self.start_quiz_server()
        self.refresh_all()

        # Starts minimized when launched from Windows Startup.
        if "--startup" in os.sys.argv:
            self.root.after(500, self.root.iconify)

        self.worker = threading.Thread(
            target=self.background_loop,
            daemon=True
        )
        self.worker.start()

        self.root.protocol("WM_DELETE_WINDOW", self.on_close)

    # --------------------------------------------------------
    # UI
    # --------------------------------------------------------

    def build_ui(self):
        notebook = ttk.Notebook(self.root)
        notebook.pack(fill="both", expand=True, padx=10, pady=10)

        self.checklist_tab = ttk.Frame(notebook)
        self.links_tab = ttk.Frame(notebook)
        self.youtube_tab = ttk.Frame(notebook)
        self.pages_tab = ttk.Frame(notebook)
        self.observer_tab = ttk.Frame(notebook)

        notebook.add(self.checklist_tab, text="Σήμερα")
        notebook.add(self.links_tab, text="Σύνδεσμοι")
        notebook.add(self.youtube_tab, text="YouTube")
        notebook.add(self.pages_tab, text="Ιστοσελίδες")
        notebook.add(self.observer_tab, text="Observer")

        self.build_checklist_tab()
        self.build_links_tab()
        self.build_youtube_tab()
        self.build_pages_tab()
        self.build_observer_tab()

        self.status_var = tk.StringVar(value="Έτοιμο")
        ttk.Label(
            self.root,
            textvariable=self.status_var,
            anchor="w",
            relief="sunken"
        ).pack(fill="x", side="bottom")

    def build_checklist_tab(self):
        header = ttk.Frame(self.checklist_tab)
        header.pack(fill="x", padx=10, pady=(10, 0))

        self.checklist_date_var = tk.StringVar()
        ttk.Label(
            header,
            textvariable=self.checklist_date_var,
            font=("Segoe UI", 13, "bold")
        ).pack(side="left")

        self.checklist_progress_var = tk.StringVar()
        ttk.Label(
            header,
            textvariable=self.checklist_progress_var,
            font=("Segoe UI", 11)
        ).pack(side="right")

        box = ttk.LabelFrame(
            self.checklist_tab,
            text="Καθημερινές υποχρεώσεις (μηδενίζονται κάθε μέρα)"
        )
        box.pack(fill="x", padx=10, pady=10)
        box.columnconfigure(1, weight=1)

        self.task_vars = {}
        self.task_info_vars = {}

        for row, (task_id, label) in enumerate(CHECKLIST_TASKS):
            var = tk.BooleanVar()
            self.task_vars[task_id] = var
            ttk.Checkbutton(
                box,
                text=label,
                variable=var,
                width=26,
                command=lambda t=task_id: self.toggle_task(t)
            ).grid(row=row, column=0, padx=8, pady=6, sticky="w")

            info = tk.StringVar()
            self.task_info_vars[task_id] = info
            ttk.Label(box, textvariable=info, foreground="#555").grid(
                row=row, column=1, padx=8, pady=6, sticky="w"
            )

            if task_id == "driving":
                ttk.Button(
                    box,
                    text="Άνοιγμα τεστ",
                    command=self.open_driving_test
                ).grid(row=row, column=2, padx=8, pady=6)

        buttons = ttk.Frame(self.checklist_tab)
        buttons.pack(fill="x", padx=10)

        ttk.Button(
            buttons,
            text="Ρυθμίσεις",
            command=self.open_checklist_settings
        ).pack(side="left", padx=5)

        ttk.Label(
            self.checklist_tab,
            text="Τελευταίες 14 ημέρες"
        ).pack(anchor="w", padx=12, pady=(14, 0))

        columns = ("date",) + tuple(t for t, _ in CHECKLIST_TASKS)
        self.history_tree = ttk.Treeview(
            self.checklist_tab,
            columns=columns,
            show="headings",
            height=8
        )
        self.history_tree.heading("date", text="Ημερομηνία")
        self.history_tree.column("date", width=160)
        for task_id, label in CHECKLIST_TASKS:
            self.history_tree.heading(task_id, text=label)
            self.history_tree.column(task_id, width=150, anchor="center")
        self.history_tree.pack(fill="both", expand=True, padx=10, pady=5)

    def build_links_tab(self):
        box = ttk.LabelFrame(
            self.links_tab,
            text="Νέος προγραμματισμένος σύνδεσμος"
        )
        box.pack(fill="x", padx=10, pady=10)

        ttk.Label(box, text="Όνομα:").grid(
            row=0, column=0, padx=5, pady=5
        )
        self.link_name = ttk.Entry(box, width=28)
        self.link_name.grid(row=0, column=1, padx=5, pady=5)

        ttk.Label(box, text="Από:").grid(
            row=0, column=2, padx=5
        )
        self.start_time = ttk.Entry(box, width=8)
        self.start_time.insert(0, "18:00")
        self.start_time.grid(row=0, column=3, padx=5)

        ttk.Label(box, text="Έως:").grid(
            row=0, column=4, padx=5
        )
        self.end_time = ttk.Entry(box, width=8)
        self.end_time.insert(0, "21:00")
        self.end_time.grid(row=0, column=5, padx=5)

        ttk.Label(box, text="URL:").grid(
            row=1, column=0, padx=5, pady=5
        )
        self.link_url = ttk.Entry(box, width=72)
        self.link_url.grid(
            row=1, column=1, columnspan=5,
            padx=5, pady=5, sticky="ew"
        )

        day_frame = ttk.Frame(box)
        day_frame.grid(
            row=2, column=0, columnspan=6,
            padx=5, pady=8, sticky="w"
        )

        self.day_vars = {}
        for i, day in enumerate(DAY_NAMES):
            var = tk.BooleanVar()
            self.day_vars[day] = var
            ttk.Checkbutton(
                day_frame,
                text=DAY_LABELS[day],
                variable=var
            ).grid(row=0, column=i, padx=4)

        ttk.Button(
            box,
            text="Προσθήκη",
            command=self.add_link
        ).grid(row=3, column=0, padx=5, pady=8)

        self.links_tree = ttk.Treeview(
            self.links_tab,
            columns=("name", "days", "window", "status", "url"),
            show="headings"
        )

        columns = [
            ("name", "Όνομα", 150),
            ("days", "Μέρες", 270),
            ("window", "Ώρες", 110),
            ("status", "Κατάσταση", 100),
            ("url", "URL", 390),
        ]

        for col, text, width in columns:
            self.links_tree.heading(col, text=text)
            self.links_tree.column(col, width=width)

        self.links_tree.pack(
            fill="both", expand=True, padx=10, pady=5
        )

        buttons = ttk.Frame(self.links_tab)
        buttons.pack(fill="x", padx=10, pady=10)

        self.open_link_button = ttk.Button(
            buttons,
            text="Άνοιγμα",
            command=self.open_selected_link
        )
        self.open_link_button.pack(side="left", padx=5)

        ttk.Button(
            buttons,
            text="Διαγραφή",
            command=self.delete_selected_link
        ).pack(side="left", padx=5)

        self.links_tree.bind(
            "<<TreeviewSelect>>",
            lambda event: self.update_open_button()
        )

    def build_youtube_tab(self):
        box = ttk.LabelFrame(
            self.youtube_tab,
            text="Νέο YouTube κανάλι"
        )
        box.pack(fill="x", padx=10, pady=10)

        ttk.Label(box, text="Όνομα:").grid(
            row=0, column=0, padx=5, pady=5
        )
        self.yt_name = ttk.Entry(box, width=28)
        self.yt_name.grid(row=0, column=1, padx=5, pady=5)

        ttk.Label(box, text="URL:").grid(
            row=1, column=0, padx=5, pady=5
        )
        self.yt_url = ttk.Entry(box, width=72)
        self.yt_url.grid(row=1, column=1, padx=5, pady=5)

        ttk.Button(
            box,
            text="Προσθήκη",
            command=self.add_youtube
        ).grid(row=2, column=0, padx=5, pady=8)

        ttk.Button(
            box,
            text="Έλεγχος τώρα",
            command=self.check_youtube_now
        ).grid(row=2, column=1, padx=5, pady=8, sticky="w")

        ttk.Button(
            box,
            text="Μαζική επικόλληση",
            command=self.open_bulk_youtube_import
        ).grid(row=2, column=2, padx=5, pady=8, sticky="w")

        ttk.Label(
            self.youtube_tab,
            text="Αυτόματος έλεγχος: κάθε 3 ώρες. Για πολλές εγγραφές χρησιμοποίησε «Μαζική επικόλληση».",
            wraplength=900
        ).pack(anchor="w", padx=12)

        self.youtube_tree = ttk.Treeview(
            self.youtube_tab,
            columns=("name", "url"),
            show="headings"
        )
        self.youtube_tree.heading("name", text="Κανάλι")
        self.youtube_tree.heading("url", text="URL")
        self.youtube_tree.column("name", width=250)
        self.youtube_tree.column("url", width=740)
        self.youtube_tree.pack(
            fill="both", expand=True, padx=10, pady=5
        )

        buttons = ttk.Frame(self.youtube_tab)
        buttons.pack(fill="x", padx=10, pady=10)

        ttk.Button(
            buttons,
            text="Άνοιγμα καναλιού",
            command=self.open_selected_youtube
        ).pack(side="left", padx=5)

        ttk.Button(
            buttons,
            text="Διαγραφή",
            command=self.delete_selected_youtube
        ).pack(side="left", padx=5)

    def build_pages_tab(self):
        ttk.Label(
            self.pages_tab,
            text=(
                "Οι παρακάτω ιστοσελίδες ελέγχονται αυτόματα "
                "κάθε 3 ώρες για αλλαγές."
            )
        ).pack(anchor="w", padx=12, pady=10)

        self.pages_tree = ttk.Treeview(
            self.pages_tab,
            columns=("name", "url", "status"),
            show="headings"
        )

        self.pages_tree.heading("name", text="Ιστοσελίδα")
        self.pages_tree.heading("url", text="URL")
        self.pages_tree.heading("status", text="Κατάσταση")

        self.pages_tree.column("name", width=260)
        self.pages_tree.column("url", width=650)
        self.pages_tree.column("status", width=180)

        self.pages_tree.pack(
            fill="both", expand=True, padx=10, pady=5
        )

        buttons = ttk.Frame(self.pages_tab)
        buttons.pack(fill="x", padx=10, pady=10)

        ttk.Button(
            buttons,
            text="Άνοιγμα",
            command=self.open_selected_page
        ).pack(side="left", padx=5)

        ttk.Button(
            buttons,
            text="Έλεγχος τώρα",
            command=self.check_pages_now
        ).pack(side="left", padx=5)

        self.last_check_var = tk.StringVar(value="Τελευταίος έλεγχος: -")
        ttk.Label(
            self.pages_tab,
            textvariable=self.last_check_var
        ).pack(anchor="w", padx=12, pady=(0, 10))

    # --------------------------------------------------------
    # Daily checklist
    # --------------------------------------------------------

    def build_observer_tab(self):
        header = ttk.Frame(self.observer_tab)
        header.pack(fill="x", padx=10, pady=(10, 0))

        self.observer_button = ttk.Button(
            header,
            text="Send Observer",
            command=self.toggle_observer,
            width=22
        )
        self.observer_button.pack(side="left", ipady=6)

        self.observer_state_var = tk.StringVar(value="Ανενεργός")
        ttk.Label(
            header,
            textvariable=self.observer_state_var,
            font=("Segoe UI", 11, "bold")
        ).pack(side="left", padx=12)

        ttk.Button(
            header,
            text="Ρυθμίσεις",
            command=self.open_observer_settings
        ).pack(side="right")

        self.observer_info_var = tk.StringVar(value="")
        ttk.Label(
            self.observer_tab,
            textvariable=self.observer_info_var,
            foreground="#555"
        ).pack(anchor="w", padx=10, pady=(6, 0))

        ttk.Label(
            self.observer_tab,
            text=(
                "Ακούει ό,τι παίζει το Teams και το μικρόφωνο, βλέπει μόνο το "
                "παράθυρο του Teams και κρατά σημειώσεις τοπικά, χωρίς cloud."
            ),
            foreground="#555"
        ).pack(anchor="w", padx=10)

        text_frame = ttk.Frame(self.observer_tab)
        text_frame.pack(fill="both", expand=True, padx=10, pady=8)
        self.observer_text = tk.Text(
            text_frame,
            wrap="word",
            height=18,
            state="disabled",
            font=("Segoe UI", 10)
        )
        scroll = ttk.Scrollbar(
            text_frame,
            orient="vertical",
            command=self.observer_text.yview
        )
        self.observer_text.configure(yscrollcommand=scroll.set)
        self.observer_text.pack(side="left", fill="both", expand=True)
        scroll.pack(side="right", fill="y")

        buttons = ttk.Frame(self.observer_tab)
        buttons.pack(fill="x", padx=10)
        ttk.Button(
            buttons,
            text="Άνοιγμα σημειώσεων",
            command=self.open_observer_notes
        ).pack(side="left")
        ttk.Button(
            buttons,
            text="Quiz",
            command=self.open_observer_quiz
        ).pack(side="left", padx=(6, 0))
        ttk.Button(
            buttons,
            text="Φάκελος συνεδριών",
            command=self.open_observer_folder
        ).pack(side="left", padx=6)
        ttk.Button(
            buttons,
            text="Αποστολή στο Teams",
            command=self.post_observer_to_teams
        ).pack(side="left")

        search = ttk.Frame(self.observer_tab)
        search.pack(fill="x", padx=10, pady=(8, 10))
        ttk.Label(search, text="Αναζήτηση σε όλες τις συναντήσεις:").pack(
            side="left"
        )
        self.observer_query = ttk.Entry(search, width=48)
        self.observer_query.pack(side="left", padx=6)
        self.observer_query.bind(
            "<Return>",
            lambda _event: self.search_observer()
        )
        ttk.Button(
            search,
            text="Αναζήτηση",
            command=self.search_observer
        ).pack(side="left")

        self.observer_shown = None
        self.observer_session = self.latest_observer_session()
        self.refresh_observer()

    # --------------------------------------------------------
    # Observer
    # --------------------------------------------------------

    def observer_running(self):
        return (
            self.observer_proc is not None
            and self.observer_proc.poll() is None
        )

    def latest_observer_session(self):
        if not OBSERVER_SESSIONS.exists():
            return None
        dirs = sorted(
            p for p in OBSERVER_SESSIONS.iterdir() if p.is_dir()
        )
        return dirs[-1] if dirs else None

    def observer_models_missing(self):
        needed = {
            "Whistle (ομιλία σε κείμενο)": OBSERVER_MODELS / "whistle.cact",
        }
        return [name for name, path in needed.items() if not path.exists()]

    def toggle_observer(self):
        if self.observer_running():
            self.stop_observer()
        else:
            self.start_observer()

    def start_observer(self):
        if not OBSERVER_EXE.exists():
            messagebox.showerror(
                "Observer",
                "Δεν βρέθηκε το observer.exe.\n\n"
                f"Κάντε build:\ncd {OBSERVER_DIR}\ncargo build --release"
            )
            return
        missing = self.observer_models_missing()
        if missing:
            messagebox.showerror(
                "Observer",
                "Λείπουν μοντέλα: " + ", ".join(missing) + "\n\n"
                f"Τρέξτε το {OBSERVER_DIR / 'download_models.ps1'}"
            )
            return

        data = load_data()
        settings = data["observer_settings"]
        if not settings.get("consent_ack"):
            ok = messagebox.askokcancel(
                "Observer",
                "Ο Observer καταγράφει ήχο και εικόνα της συνάντησης "
                "(μόνο τοπικά, σε αυτόν τον υπολογιστή).\n\n"
                "Ενημερώστε τους συμμετέχοντες ότι κρατάτε σημειώσεις με "
                "καταγραφή, όπως ορίζει ο νόμος και η πολιτική του "
                "οργανισμού σας.\n\nΣυνέχεια;"
            )
            if not ok:
                return
            settings["consent_ack"] = True
            save_data(data)

        session = OBSERVER_SESSIONS / datetime.now().strftime("%Y-%m-%d_%H%M%S")
        session.mkdir(parents=True, exist_ok=True)
        cmd = [
            str(OBSERVER_EXE), "run",
            "--session", str(session),
            "--models", str(OBSERVER_MODELS),
            "--screen", settings["screen"],
            "--audio", settings["audio"],
            "--language", settings["language"],
        ]
        if settings.get("auto_stop"):
            cmd.append("--auto-stop")
        if settings.get("my_names", "").strip():
            cmd += ["--names", settings["my_names"]]
        if not settings.get("web_sources", True):
            cmd.append("--no-web")
        cmd += ["--quiz-lang", settings.get("quiz_language", "en")]
        if settings.get("auto_post") and settings.get("webhook"):
            cmd += ["--webhook", settings["webhook"], "--post"]

        try:
            self.observer_proc = subprocess.Popen(
                cmd,
                cwd=str(OBSERVER_DIR),
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0)
            )
        except Exception as exc:
            log(f"observer start error: {exc!r}")
            messagebox.showerror("Observer", f"Δεν ξεκίνησε: {exc}")
            return

        log(f"observer started: {session}")
        self.observer_session = session
        self.observer_shown = None
        self.observer_alerts_seen = set()
        self.set_status("Observer: ξεκίνησε")

    def stop_observer(self):
        # The agent writes the final summary and exits on its own.
        if self.observer_session:
            try:
                (self.observer_session / "stop").write_text(
                    "stop",
                    encoding="utf-8"
                )
            except Exception as exc:
                log(f"observer stop error: {exc!r}")
        self.observer_state_var.set(OBSERVER_STATES["finishing"])
        self.observer_button.configure(state="disabled")

    def read_observer_status(self):
        if not self.observer_session:
            return {}
        try:
            return json.loads(
                (self.observer_session / "status.json").read_text(
                    encoding="utf-8"
                )
            )
        except Exception:
            return {}

    def refresh_observer(self):
        # Reschedules itself: every 2 s while running, else every 10 s.
        try:
            self.update_observer_view()
        except Exception as exc:
            log(f"refresh_observer error: {exc!r}")
        delay = 2000 if self.observer_running() else 10000
        self.root.after(delay, self.refresh_observer)

    def update_observer_view(self):
        running = self.observer_running()
        status = self.read_observer_status()
        state = status.get("state", "")

        if running:
            self.observer_button.configure(
                text="Stop Observer",
                state="disabled" if state == "finishing" else "normal"
            )
        else:
            self.observer_button.configure(
                text="Send Observer",
                state="normal"
            )
            if self.observer_proc is not None:
                # Just finished.
                self.observer_proc = None
                quiz = self.observer_session / "quiz.html"
                if state == "done" and quiz.exists():
                    notify(
                        "Observer",
                        "Οι σημειώσεις και το quiz της συνάντησης είναι "
                        "έτοιμα. Δες τι θυμάσαι!",
                        quiz.as_uri()
                    )
                elif state == "done":
                    notify(
                        "Observer",
                        "Οι σημειώσεις της συνάντησης είναι έτοιμες.",
                        (self.observer_session / "notes.md").as_uri()
                    )

        if running:
            self.notify_observer_alerts(status)

        if running:
            label = OBSERVER_STATES.get(state, "Ξεκινά...")
        elif state in ("done", "error"):
            label = OBSERVER_STATES[state]
        else:
            label = "Ανενεργός"
        self.observer_state_var.set(label)

        info = []
        if status:
            elapsed = int(status.get("elapsed", 0))
            info.append(f"{elapsed // 60:02d}:{elapsed % 60:02d}")
            info.append(f"ομιλίες {status.get('segments', 0)}")
            info.append(f"στιγμιότυπα {status.get('keyframes', 0)}")
            info.append(f"ενέργειες/αποφάσεις {status.get('items', 0)}")
            if status.get("alerts"):
                info.append(f"σημαντικά {len(status['alerts'])}")
            if status.get("sources"):
                info.append(f"πηγές {status['sources']}")
            if status.get("backlog", 0) > 5:
                info.append(f"σε αναμονή {int(status['backlog'])} s ήχου")
            if status.get("teams"):
                info.append(f"Teams: {status['teams'][:60]}")
            if status.get("error"):
                info.append(f"σφάλμα: {status['error']}")
            warnings = status.get("warnings") or []
            if warnings:
                info.append(f"προειδοποιήσεις: {len(warnings)} (observer.log)")
        elif self.observer_models_missing():
            info.append("Λείπουν μοντέλα: τρέξτε observer\\download_models.ps1")
        self.observer_info_var.set("   ·   ".join(info))

        self.show_observer_notes()

    def notify_observer_alerts(self, status):
        seen = getattr(self, "observer_alerts_seen", set())
        for alert in status.get("alerts") or []:
            if alert["id"] in seen:
                continue
            seen.add(alert["id"])
            at = int(alert.get("at", 0))
            notify(
                f"Observer: {alert['why']}",
                f"[{at // 60:02d}:{at % 60:02d}] {alert['text'][:200]}"
            )
        self.observer_alerts_seen = seen

    def open_observer_quiz(self):
        session = self.observer_session
        if not session or not (session / "notes.json").exists():
            messagebox.showinfo("Observer", "Δεν υπάρχει συνάντηση ακόμα.")
            return
        quiz = session / "quiz.html"
        if quiz.exists():
            os.startfile(str(quiz))
            return
        if self.observer_running():
            messagebox.showinfo(
                "Observer",
                "Το quiz φτιάχνεται όταν σταματήσει ο Observer."
            )
            return
        # A session from before the quiz existed: build it now.
        self.set_status("Observer: φτιάχνει το quiz...")
        cmd = [
            str(OBSERVER_EXE), "quiz",
            "--session", str(session),
            "--models", str(OBSERVER_MODELS),
        ]
        settings = load_data()["observer_settings"]
        names = settings.get("my_names", "").strip()
        if names:
            cmd += ["--names", names]
        if not settings.get("web_sources", True):
            cmd.append("--no-web")
        cmd += ["--quiz-lang", settings.get("quiz_language", "en")]

        def job():
            try:
                subprocess.run(
                    cmd,
                    capture_output=True,
                    timeout=300,
                    creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0)
                )
            except Exception as exc:
                log(f"observer quiz error: {exc!r}")

            def done():
                self.set_status("Έτοιμο")
                if quiz.exists():
                    os.startfile(str(quiz))
                else:
                    messagebox.showerror("Observer", "Δεν φτιάχτηκε quiz.")

            self.root.after(0, done)

        threading.Thread(target=job, daemon=True).start()

    def show_observer_notes(self):
        if not self.observer_session:
            return
        path = self.observer_session / "notes.md"
        try:
            stamp = path.stat().st_mtime
        except OSError:
            return
        if self.observer_shown == (path, stamp):
            return
        self.observer_shown = (path, stamp)
        try:
            text = path.read_text(encoding="utf-8")
        except Exception:
            return
        at_end = self.observer_text.yview()[1] >= 0.99
        self.observer_text.configure(state="normal")
        self.observer_text.delete("1.0", tk.END)
        self.observer_text.insert(tk.END, text)
        self.observer_text.configure(state="disabled")
        if at_end:
            self.observer_text.see(tk.END)

    def open_observer_notes(self):
        if not self.observer_session:
            messagebox.showinfo("Observer", "Δεν υπάρχει συνάντηση ακόμα.")
            return
        path = self.observer_session / "notes.md"
        if path.exists():
            os.startfile(str(path))
        else:
            os.startfile(str(self.observer_session))

    def open_observer_folder(self):
        OBSERVER_SESSIONS.mkdir(parents=True, exist_ok=True)
        os.startfile(str(OBSERVER_SESSIONS))

    def post_observer_to_teams(self):
        webhook = load_data()["observer_settings"].get("webhook", "")
        if not webhook:
            messagebox.showinfo(
                "Observer",
                "Ορίστε πρώτα το Teams webhook στις Ρυθμίσεις."
            )
            return
        if not self.observer_session or not (
            self.observer_session / "notes.json"
        ).exists():
            messagebox.showinfo("Observer", "Δεν υπάρχουν σημειώσεις.")
            return
        if not messagebox.askyesno(
            "Observer",
            "Αποστολή των σημειώσεων αυτής της συνάντησης στο Teams;"
        ):
            return
        session = self.observer_session

        def job():
            try:
                result = subprocess.run(
                    [
                        str(OBSERVER_EXE), "post",
                        "--session", str(session),
                        "--webhook", webhook,
                    ],
                    capture_output=True,
                    text=True,
                    encoding="utf-8",
                    timeout=60,
                    creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0)
                )
                ok = result.returncode == 0
                lines = result.stderr.strip().splitlines()
                msg = "Στάλθηκε στο Teams." if ok else (
                    lines[-1] if lines else "Αποτυχία"
                )
            except Exception as exc:
                ok, msg = False, str(exc)

            def show():
                if ok:
                    messagebox.showinfo("Observer", msg)
                else:
                    messagebox.showerror("Observer", msg)

            self.root.after(0, show)

        threading.Thread(target=job, daemon=True).start()

    def search_observer(self):
        query = self.observer_query.get().strip()
        if not query or not OBSERVER_EXE.exists():
            return
        self.set_status("Observer: αναζήτηση...")

        def job():
            try:
                result = subprocess.run(
                    [
                        str(OBSERVER_EXE), "search",
                        "--root", str(OBSERVER_SESSIONS),
                        "--models", str(OBSERVER_MODELS),
                        "--json", query,
                    ],
                    capture_output=True,
                    text=True,
                    encoding="utf-8",
                    timeout=120,
                    creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0)
                )
                hits = json.loads(result.stdout or "[]")
                lines = [f"# Αναζήτηση: {query}", ""]
                for h in hits:
                    at = int(h["at"])
                    lines.append(
                        f"{h['score']:.2f}  {h['session']} "
                        f"[{at // 60:02d}:{at % 60:02d}] "
                        f"{h['kind']}: {h['text']}"
                    )
                if not hits:
                    lines.append("Δεν βρέθηκε τίποτα.")
                text = "\n".join(lines)
            except Exception as exc:
                text = f"Σφάλμα αναζήτησης: {exc}"

            def show():
                # Pinned until the notes file changes again.
                self.observer_shown = (
                    (self.observer_session / "notes.md",
                     (self.observer_session / "notes.md").stat().st_mtime)
                    if self.observer_session
                    and (self.observer_session / "notes.md").exists()
                    else None
                )
                self.observer_text.configure(state="normal")
                self.observer_text.delete("1.0", tk.END)
                self.observer_text.insert(tk.END, text)
                self.observer_text.configure(state="disabled")
                self.set_status("Έτοιμο")

            self.root.after(0, show)

        threading.Thread(target=job, daemon=True).start()

    def open_observer_settings(self):
        settings = load_data()["observer_settings"]

        window = tk.Toplevel(self.root)
        window.title("Ρυθμίσεις Observer")
        window.transient(self.root)
        window.resizable(False, False)

        box = ttk.Frame(window)
        box.pack(fill="both", padx=12, pady=12)

        ttk.Label(box, text="Teams Workflows webhook:").grid(
            row=0, column=0, sticky="w"
        )
        webhook = ttk.Entry(box, width=64)
        webhook.insert(0, settings["webhook"])
        webhook.grid(row=0, column=1, sticky="w", pady=3)
        ttk.Label(
            box,
            text=(
                "Teams > κανάλι > ... > Workflows > 'Post to a channel when "
                "a webhook request is received' > αντιγραφή URL"
            ),
            foreground="#555"
        ).grid(row=1, column=1, sticky="w")

        auto_post = tk.BooleanVar(value=settings["auto_post"])
        ttk.Checkbutton(
            box,
            text="Αυτόματη αποστολή στο Teams στο τέλος",
            variable=auto_post
        ).grid(row=2, column=1, sticky="w", pady=3)

        ttk.Label(box, text="Γλώσσα ομιλίας:").grid(
            row=3, column=0, sticky="w"
        )
        language = ttk.Combobox(
            box,
            values=OBSERVER_LANGUAGES,
            state="readonly",
            width=8
        )
        language.set(settings["language"])
        language.grid(row=3, column=1, sticky="w", pady=3)
        ttk.Label(
            box,
            text="Το Whistle αναγνωρίζει en, de, fr, es, it, nl, pl (όχι ελληνικά).",
            foreground="#555"
        ).grid(row=4, column=1, sticky="w")

        ttk.Label(box, text="Οθόνη:").grid(row=5, column=0, sticky="w")
        screen = ttk.Combobox(
            box,
            values=["teams", "monitor", "off"],
            state="readonly",
            width=10
        )
        screen.set(settings["screen"])
        screen.grid(row=5, column=1, sticky="w", pady=3)

        ttk.Label(box, text="Ήχος:").grid(row=6, column=0, sticky="w")
        audio = ttk.Combobox(
            box,
            values=["both", "others", "me"],
            state="readonly",
            width=10
        )
        audio.set(settings["audio"])
        audio.grid(row=6, column=1, sticky="w", pady=3)

        auto_stop = tk.BooleanVar(value=settings["auto_stop"])
        ttk.Checkbutton(
            box,
            text="Διακοπή όταν κλείσει η συνάντηση στο Teams",
            variable=auto_stop
        ).grid(row=7, column=1, sticky="w", pady=3)

        ttk.Label(box, text="Τα ονόματά μου:").grid(
            row=8, column=0, sticky="w"
        )
        my_names = ttk.Entry(box, width=40)
        my_names.insert(0, settings.get("my_names", ""))
        my_names.grid(row=8, column=1, sticky="w", pady=3)
        ttk.Label(
            box,
            text="Χωρισμένα με κόμμα, όπως ακούγονται (π.χ. Alexandros, Alex). "
                 "Ειδοποίηση όταν τα πει κάποιος.",
            foreground="#555"
        ).grid(row=9, column=1, sticky="w")

        web_sources = tk.BooleanVar(value=settings.get("web_sources", True))
        ttk.Checkbutton(
            box,
            text="Αναζήτηση σχετικών πηγών (Wikipedia) για το quiz",
            variable=web_sources
        ).grid(row=10, column=1, sticky="w", pady=3)
        ttk.Label(
            box,
            text="Στέλνονται μόνο λέξεις-κλειδιά των θεμάτων, όχι η απομαγνητοφώνηση.",
            foreground="#555"
        ).grid(row=11, column=1, sticky="w")

        ttk.Label(box, text="Γλώσσα quiz:").grid(row=12, column=0, sticky="w")
        quiz_language = ttk.Combobox(
            box,
            values=["en", "el"],
            state="readonly",
            width=8
        )
        quiz_language.set(settings.get("quiz_language", "en"))
        quiz_language.grid(row=12, column=1, sticky="w", pady=3)

        def save():
            url = webhook.get().strip()
            if url and not url.startswith("https://"):
                messagebox.showerror(
                    "Observer",
                    "Το webhook πρέπει να ξεκινά με https://",
                    parent=window
                )
                return
            data = load_data()
            s = data["observer_settings"]
            s["webhook"] = url
            s["auto_post"] = bool(auto_post.get())
            s["language"] = language.get()
            s["screen"] = screen.get()
            s["audio"] = audio.get()
            s["auto_stop"] = bool(auto_stop.get())
            s["my_names"] = my_names.get().strip()
            s["web_sources"] = bool(web_sources.get())
            s["quiz_language"] = quiz_language.get()
            save_data(data)
            window.destroy()

        buttons = ttk.Frame(window)
        buttons.pack(fill="x", padx=12, pady=(0, 12))
        ttk.Button(buttons, text="Αποθήκευση", command=save).pack(
            side="right"
        )
        ttk.Button(buttons, text="Άκυρο", command=window.destroy).pack(
            side="right", padx=6
        )

    def toggle_task(self, task_id):
        done = self.task_vars[task_id].get()

        with DATA_LOCK:
            data = load_data()
            ensure_checklist_today(data)
            set_checklist_item(data, task_id, done, "user")
            save_data(data)

        self.refresh_checklist()

    def check_checklist(self):
        now = datetime.now()

        with DATA_LOCK:
            data = load_data()
            settings = data["checklist_settings"]
            checklist = data["checklist"]

            changed = ensure_checklist_today(data, now)
            newly = apply_checklist_rules(data, now)
            changed = changed or bool(newly)

            open_test = (
                not checklist["items"].get("driving", {}).get("done")
                and not checklist.get("driving_opened")
                and now.strftime("%H:%M") >= settings["driving_open_time"]
            )
            if open_test:
                checklist["driving_opened"] = True
                changed = True

            if changed:
                save_data(data)

        labels = dict(CHECKLIST_TASKS)
        for task_id in newly:
            notify(
                f"✓ {labels[task_id]}",
                "Σημειώθηκε αυτόματα στη λίστα της ημέρας."
            )

        if open_test:
            notify(
                "Τεστ Κ.Ο.Κ. της ημέρας",
                "Άνοιξε το καθημερινό τεστ οδήγησης."
            )
            self.root.after(0, self.open_driving_test)

        if changed:
            self.root.after(0, self.refresh_checklist)

    def refresh_checklist(self):
        now = datetime.now()

        with DATA_LOCK:
            data = load_data()
            if ensure_checklist_today(data, now):
                save_data(data)

        settings = data["checklist_settings"]
        checklist = data["checklist"]
        items = checklist["items"]
        weekday = now.strftime("%A")

        self.checklist_date_var.set(
            f"{DAY_LABELS[weekday]} {now:%d/%m/%Y}"
        )

        done_count = 0
        for task_id, _ in CHECKLIST_TASKS:
            state = items.get(task_id, {})
            done = bool(state.get("done"))
            done_count += done
            self.task_vars[task_id].set(done)

            if task_id == "work":
                if weekday in settings["work_days"]:
                    info = f"Αυτόματο τικ στις {settings['work_auto_time']}"
                else:
                    info = "Μη εργάσιμη — χειροκίνητα"
            elif task_id == "masters":
                end = settings["masters_schedule"].get(weekday, "")
                if end:
                    info = f"Μάθημα σήμερα — αυτόματο τικ στις {end}"
                else:
                    info = "Χωρίς μάθημα σήμερα — χειροκίνητα"
            elif task_id == "driving":
                if state.get("by") == "test" and done:
                    info = state.get("note", "Τεστ ολοκληρώθηκε")
                else:
                    info = (
                        f"Το τεστ ανοίγει αυτόματα στις "
                        f"{settings['driving_open_time']} — τικ μόλις "
                        f"ολοκληρωθεί"
                    )
            else:
                info = "Χειροκίνητα"

            if done and state.get("by") == "auto":
                info += f"  (✓ αυτόματα {state.get('at', '')})"
            elif done and state.get("by") == "user":
                info += f"  (✓ {state.get('at', '')})"

            self.task_info_vars[task_id].set(info)

        self.checklist_progress_var.set(
            f"{done_count}/{len(CHECKLIST_TASKS)} ολοκληρώθηκαν"
        )

        for row in self.history_tree.get_children():
            self.history_tree.delete(row)

        history = checklist.get("history", {})
        for date_key in sorted(history, reverse=True)[:14]:
            try:
                day = datetime.strptime(date_key, "%Y-%m-%d")
                date_text = f"{DAY_LABELS[day.strftime('%A')][:3]} {day:%d/%m}"
            except ValueError:
                date_text = date_key
            done_ids = history[date_key]
            self.history_tree.insert(
                "",
                tk.END,
                values=(date_text,) + tuple(
                    "✓" if task_id in done_ids else "—"
                    for task_id, _ in CHECKLIST_TASKS
                )
            )

    def start_quiz_server(self):
        settings = load_data()["checklist_settings"]
        site = Path(settings["alex_dir"])

        if not (site / "quizzes_kok.html").exists():
            log(f"quiz server: quizzes_kok.html not found in {site}")
            return

        try:
            server = ThreadingHTTPServer(
                ("127.0.0.1", int(settings["server_port"])),
                partial(QuizRequestHandler, directory=str(site))
            )
        except OSError as exc:
            log(f"quiz server could not start: {exc!r}")
            return

        server.daemon_threads = True
        server.on_driving_done = lambda: threading.Thread(
            target=self.check_checklist, daemon=True
        ).start()
        self.quiz_server = server
        threading.Thread(target=server.serve_forever, daemon=True).start()

    def open_driving_test(self):
        if not self.quiz_server:
            self.start_quiz_server()

        if not self.quiz_server:
            messagebox.showerror(
                "Τεστ Κ.Ο.Κ.",
                (
                    "Δεν βρέθηκε το quizzes_kok.html ή η θύρα είναι "
                    "κατειλημμένη.\nΈλεγξε τον φάκελο alex151807 στις "
                    "Ρυθμίσεις της λίστας."
                )
            )
            return

        port = self.quiz_server.server_port
        webbrowser.open(f"http://127.0.0.1:{port}/quizzes_kok.html")

    def open_checklist_settings(self):
        settings = load_data()["checklist_settings"]

        window = tk.Toplevel(self.root)
        window.title("Ρυθμίσεις λίστας")
        window.transient(self.root)
        window.resizable(False, False)

        work_box = ttk.LabelFrame(window, text="Δουλειά")
        work_box.pack(fill="x", padx=12, pady=(12, 6))

        ttk.Label(work_box, text="Αυτόματο τικ στις:").grid(
            row=0, column=0, padx=5, pady=5, sticky="w"
        )
        work_time = ttk.Entry(work_box, width=8)
        work_time.insert(0, settings["work_auto_time"])
        work_time.grid(row=0, column=1, padx=5, pady=5, sticky="w")

        work_days_frame = ttk.Frame(work_box)
        work_days_frame.grid(row=1, column=0, columnspan=2, padx=5, pady=5)
        work_day_vars = {}
        for i, day in enumerate(DAY_NAMES):
            var = tk.BooleanVar(value=day in settings["work_days"])
            work_day_vars[day] = var
            ttk.Checkbutton(
                work_days_frame, text=DAY_LABELS[day], variable=var
            ).grid(row=0, column=i, padx=3)

        masters_box = ttk.LabelFrame(
            window,
            text="Μεταπτυχιακό — ώρα λήξης μαθημάτων (κενό = χωρίς μάθημα)"
        )
        masters_box.pack(fill="x", padx=12, pady=6)

        masters_entries = {}
        for i, day in enumerate(DAY_NAMES):
            ttk.Label(masters_box, text=DAY_LABELS[day]).grid(
                row=i // 4, column=(i % 4) * 2, padx=5, pady=4, sticky="e"
            )
            entry = ttk.Entry(masters_box, width=7)
            entry.insert(0, settings["masters_schedule"].get(day, ""))
            entry.grid(row=i // 4, column=(i % 4) * 2 + 1, padx=5, pady=4)
            masters_entries[day] = entry

        def sync_from_links():
            schedule = masters_schedule_from_links(
                load_data()["scheduled_links"]
            )
            for day, entry in masters_entries.items():
                entry.delete(0, tk.END)
                entry.insert(0, schedule.get(day, ""))

        ttk.Button(
            masters_box,
            text="Από τους Συνδέσμους",
            command=sync_from_links
        ).grid(row=2, column=0, columnspan=8, padx=5, pady=6, sticky="w")

        driving_box = ttk.LabelFrame(window, text="Οδήγηση")
        driving_box.pack(fill="x", padx=12, pady=6)

        ttk.Label(driving_box, text="Άνοιγμα τεστ στις:").grid(
            row=0, column=0, padx=5, pady=5, sticky="w"
        )
        driving_time = ttk.Entry(driving_box, width=8)
        driving_time.insert(0, settings["driving_open_time"])
        driving_time.grid(row=0, column=1, padx=5, pady=5, sticky="w")

        ttk.Label(driving_box, text="Φάκελος alex151807:").grid(
            row=1, column=0, padx=5, pady=5, sticky="w"
        )
        alex_dir = ttk.Entry(driving_box, width=52)
        alex_dir.insert(0, settings["alex_dir"])
        alex_dir.grid(row=1, column=1, padx=5, pady=5)

        def browse():
            chosen = filedialog.askdirectory(
                parent=window, initialdir=alex_dir.get()
            )
            if chosen:
                alex_dir.delete(0, tk.END)
                alex_dir.insert(0, str(Path(chosen)))

        ttk.Button(driving_box, text="...", width=3, command=browse).grid(
            row=1, column=2, padx=5
        )

        def normalize_time(value):
            return datetime.strptime(value.strip(), "%H:%M").strftime("%H:%M")

        def save():
            try:
                new_work_time = normalize_time(work_time.get())
                new_driving_time = normalize_time(driving_time.get())
                schedule = {
                    day: normalize_time(entry.get())
                    for day, entry in masters_entries.items()
                    if entry.get().strip()
                }
            except ValueError:
                messagebox.showerror(
                    "Σφάλμα",
                    "Οι ώρες πρέπει να είναι σε μορφή HH:MM.",
                    parent=window
                )
                return

            new_dir = alex_dir.get().strip()
            dir_changed = False

            with DATA_LOCK:
                data = load_data()
                s = data["checklist_settings"]
                dir_changed = s["alex_dir"] != new_dir
                s["work_auto_time"] = new_work_time
                s["work_days"] = [
                    d for d, v in work_day_vars.items() if v.get()
                ]
                s["masters_schedule"] = schedule
                s["driving_open_time"] = new_driving_time
                s["alex_dir"] = new_dir
                save_data(data)

            if dir_changed and self.quiz_server:
                self.quiz_server.shutdown()
                self.quiz_server.server_close()
                self.quiz_server = None
                self.start_quiz_server()

            window.destroy()
            self.refresh_checklist()
            threading.Thread(
                target=self.check_checklist, daemon=True
            ).start()

        buttons = ttk.Frame(window)
        buttons.pack(fill="x", padx=12, pady=(6, 12))
        ttk.Button(buttons, text="Αποθήκευση", command=save).pack(
            side="left", padx=(0, 6)
        )
        ttk.Button(buttons, text="Άκυρο", command=window.destroy).pack(
            side="left"
        )

    # --------------------------------------------------------
    # Scheduled links
    # --------------------------------------------------------

    def valid_time(self, value):
        try:
            datetime.strptime(value, "%H:%M")
            return True
        except ValueError:
            return False

    def add_link(self):
        name = self.link_name.get().strip()
        url = self.link_url.get().strip()
        start = self.start_time.get().strip()
        end = self.end_time.get().strip()

        days = [
            day for day, var in self.day_vars.items()
            if var.get()
        ]

        if not name or not url or not days:
            messagebox.showerror(
                "Σφάλμα",
                "Συμπλήρωσε όνομα, URL και τουλάχιστον μία ημέρα."
            )
            return

        if not self.valid_time(start) or not self.valid_time(end):
            messagebox.showerror(
                "Σφάλμα",
                "Οι ώρες πρέπει να είναι σε μορφή HH:MM."
            )
            return

        if not url.startswith(("http://", "https://")):
            messagebox.showerror(
                "Σφάλμα",
                "Το URL πρέπει να ξεκινά με http:// ή https://"
            )
            return

        data = load_data()

        data["scheduled_links"].append({
            "name": name,
            "url": url,
            "days": days,
            "start_time": start,
            "end_time": end,
            "last_window_notification": ""
        })

        save_data(data)
        self.refresh_links()

        self.link_name.delete(0, tk.END)
        self.link_url.delete(0, tk.END)

    def open_selected_link(self):
        selected = self.links_tree.selection()
        if not selected:
            return

        item = load_data()["scheduled_links"][int(selected[0])]

        if not is_link_available(item):
            messagebox.showwarning(
                "Μη διαθέσιμο",
                (
                    f"Ο σύνδεσμος είναι διαθέσιμος μόνο "
                    f"{item['start_time']}–{item['end_time']} "
                    f"στις επιλεγμένες ημέρες."
                )
            )
            return

        webbrowser.open(item["url"])

    def delete_selected_link(self):
        selected = self.links_tree.selection()
        if not selected:
            return

        data = load_data()
        del data["scheduled_links"][int(selected[0])]
        save_data(data)
        self.refresh_links()

    def update_open_button(self):
        selected = self.links_tree.selection()

        if not selected:
            self.open_link_button.config(state="disabled")
            return

        item = load_data()["scheduled_links"][int(selected[0])]

        self.open_link_button.config(
            state="normal" if is_link_available(item) else "disabled"
        )

    # --------------------------------------------------------
    # YouTube
    # --------------------------------------------------------

    def add_youtube(self):
        name = self.yt_name.get().strip()
        url = self.yt_url.get().strip()

        if not name or not url:
            messagebox.showerror(
                "Σφάλμα",
                "Συμπλήρωσε όνομα και URL."
            )
            return

        self.set_status("Προσθήκη YouTube καναλιού...")

        def job():
            try:
                channel_id = extract_youtube_channel_id(url)
                video_id, _, _ = get_latest_youtube_video(channel_id)

                data = load_data()
                data["youtube_channels"].append({
                    "name": name,
                    "url": url,
                    "channel_id": channel_id,
                    "last_video_id": video_id
                })
                save_data(data)

                self.root.after(0, self.refresh_youtube)
                self.root.after(
                    0,
                    lambda: self.set_status(
                        f"Προστέθηκε το κανάλι: {name}"
                    )
                )

            except Exception as exc:
                self.root.after(
                    0,
                    lambda e=str(exc): messagebox.showerror(
                        "YouTube",
                        e
                    )
                )

        threading.Thread(target=job, daemon=True).start()

    def open_bulk_youtube_import(self):
        window = tk.Toplevel(self.root)
        window.title("Μαζική εισαγωγή YouTube καναλιών")
        window.geometry("900x560")
        window.transient(self.root)

        ttk.Label(
            window,
            text=(
                "Επικόλλησε μία εγγραφή ανά γραμμή στη μορφή:\n"
                "Όνομα καναλιού https://www.youtube.com/channel/UC...\n\n"
                "Γίνονται δεκτά και κείμενα όπου οι αλλαγές γραμμής "
                "έχουν αντιγραφεί ως κυριολεκτικό \\\\n."
            ),
            justify="left",
            wraplength=850
        ).pack(anchor="w", padx=12, pady=(12, 6))

        example = (
            "Two Minute Papers https://www.youtube.com/channel/UCbfYPyITQ-7l4upoX8nvctg\n"
            "AI Search https://www.youtube.com/channel/UCIgnGlGkVRhd4qNFcEwLL4A\n"
            "Bycloud https://www.youtube.com/channel/UCgfe2ooZD3VJPB6aJAnuQng"
        )

        text_box = tk.Text(window, wrap="none", height=18)
        text_box.pack(fill="both", expand=True, padx=12, pady=6)
        text_box.insert("1.0", example)

        status_var = tk.StringVar(value="")
        ttk.Label(
            window,
            textvariable=status_var,
            wraplength=850
        ).pack(anchor="w", padx=12, pady=(4, 6))

        buttons = ttk.Frame(window)
        buttons.pack(fill="x", padx=12, pady=(0, 12))

        import_button = ttk.Button(
            buttons,
            text="Εισαγωγή λίστας"
        )
        import_button.pack(side="left", padx=(0, 6))

        ttk.Button(
            buttons,
            text="Καθαρισμός",
            command=lambda: text_box.delete("1.0", "end")
        ).pack(side="left", padx=(0, 6))

        ttk.Button(
            buttons,
            text="Κλείσιμο",
            command=window.destroy
        ).pack(side="left")

        def start_import():
            raw = text_box.get("1.0", "end").strip()

            if not raw:
                messagebox.showwarning(
                    "Μαζική εισαγωγή",
                    "Δεν υπάρχει κείμενο για εισαγωγή.",
                    parent=window
                )
                return

            # Some copied text contains the two literal characters "\n"
            # instead of a real newline. Normalize both forms.
            raw = raw.replace("\\r\\n", "\n")
            raw = raw.replace("\\n", "\n")
            raw = raw.replace("\r\n", "\n").replace("\r", "\n")

            entries = []
            invalid_lines = []

            for line_number, raw_line in enumerate(raw.split("\n"), start=1):
                line = raw_line.strip()
                if not line:
                    continue

                # URL must be the final whitespace-separated item.
                match = re.match(
                    r"^(.*?)\s+(https?://(?:www\.)?youtube\.com/\S+)\s*$",
                    line,
                    flags=re.IGNORECASE
                )

                if not match:
                    invalid_lines.append(line_number)
                    continue

                name = match.group(1).strip()
                url = match.group(2).strip()

                if not name:
                    invalid_lines.append(line_number)
                    continue

                entries.append((name, url, line_number))

            if not entries:
                messagebox.showerror(
                    "Μαζική εισαγωγή",
                    "Δεν βρέθηκαν έγκυρες εγγραφές.",
                    parent=window
                )
                return

            import_button.config(state="disabled")
            status_var.set(
                f"Επεξεργασία {len(entries)} καναλιών..."
            )

            def job():
                data = load_data()

                existing_urls = {
                    item.get("url", "").rstrip("/").lower()
                    for item in data["youtube_channels"]
                }

                added_ready = 0
                added_pending = 0
                skipped = 0
                warnings = []

                for name, url, line_number in entries:
                    normalized_url = url.rstrip("/").lower()

                    if normalized_url in existing_urls:
                        skipped += 1
                        continue

                    channel_id = ""
                    video_id = ""
                    last_error = ""

                    try:
                        channel_id = extract_youtube_channel_id(url)

                        try:
                            video_id, _, _ = get_latest_youtube_video(
                                channel_id
                            )
                            added_ready += 1
                        except Exception as exc:
                            # The URL can still be stored. The normal
                            # 3-hour checker will retry later.
                            last_error = str(exc)
                            added_pending += 1
                            warnings.append(
                                f"Γραμμή {line_number}: {name} — "
                                f"προστέθηκε, αλλά ο πρώτος έλεγχος video "
                                f"απέτυχε: {exc}"
                            )

                    except Exception as exc:
                        # Keep direct /channel/ URLs even when YouTube
                        # temporarily returns an error.
                        direct = re.search(
                            r"/channel/(UC[A-Za-z0-9_-]+)",
                            url
                        )

                        if direct:
                            channel_id = direct.group(1)
                            last_error = str(exc)
                            added_pending += 1
                            warnings.append(
                                f"Γραμμή {line_number}: {name} — "
                                f"προστέθηκε για επανέλεγχο: {exc}"
                            )
                        else:
                            warnings.append(
                                f"Γραμμή {line_number}: {name} — "
                                f"δεν μπόρεσε να αναγνωριστεί: {exc}"
                            )
                            continue

                    data["youtube_channels"].append({
                        "name": name,
                        "url": url,
                        "channel_id": channel_id,
                        "last_video_id": video_id,
                        "last_error": last_error
                    })

                    existing_urls.add(normalized_url)

                save_data(data)

                summary_parts = [
                    f"Προστέθηκαν και ελέγχθηκαν: {added_ready}",
                    f"Προστέθηκαν για επανέλεγχο: {added_pending}",
                    f"Παραλείφθηκαν ως διπλότυπα: {skipped}"
                ]

                if invalid_lines:
                    summary_parts.append(
                        "Μη έγκυρη μορφή στις γραμμές: "
                        + ", ".join(map(str, invalid_lines))
                    )

                summary = "\n".join(summary_parts)

                def finish():
                    self.refresh_youtube()
                    import_button.config(state="normal")
                    status_var.set(summary)

                    if warnings:
                        details = "\n".join(warnings[:12])
                        if len(warnings) > 12:
                            details += (
                                f"\n...και άλλες "
                                f"{len(warnings) - 12} προειδοποιήσεις."
                            )

                        messagebox.showwarning(
                            "Η εισαγωγή ολοκληρώθηκε",
                            summary + "\n\n" + details,
                            parent=window
                        )
                    else:
                        messagebox.showinfo(
                            "Μαζική εισαγωγή",
                            summary,
                            parent=window
                        )

                self.root.after(0, finish)

            threading.Thread(target=job, daemon=True).start()

        import_button.config(command=start_import)

    def open_selected_youtube(self):
        selected = self.youtube_tree.selection()
        if not selected:
            return

        item = load_data()["youtube_channels"][int(selected[0])]
        webbrowser.open(item["url"])

    def delete_selected_youtube(self):
        selected = self.youtube_tree.selection()
        if not selected:
            return

        data = load_data()
        del data["youtube_channels"][int(selected[0])]
        save_data(data)
        self.refresh_youtube()

    def check_youtube_now(self):
        threading.Thread(
            target=self.check_youtube_channels,
            daemon=True
        ).start()

    # --------------------------------------------------------
    # Pages
    # --------------------------------------------------------

    def open_selected_page(self):
        selected = self.pages_tree.selection()
        if not selected:
            return

        item = load_data()["watched_pages"][int(selected[0])]
        webbrowser.open(item["url"])

    def check_pages_now(self):
        threading.Thread(
            target=self.check_watched_pages,
            daemon=True
        ).start()

    # --------------------------------------------------------
    # Background monitoring
    # --------------------------------------------------------

    def background_loop(self):
        # Perform the 3-hour online check once on startup,
        # then every 3 hours.
        next_online_check = 0

        while not self.stop_event.is_set():
            try:
                self.check_checklist()
                self.check_scheduled_links()

                now_ts = time.time()
                if now_ts >= next_online_check:
                    self.check_youtube_channels()
                    self.check_watched_pages()
                    next_online_check = now_ts + ONLINE_CHECK_INTERVAL

            except Exception as exc:
                log(f"background_loop error: {exc!r}")

            self.stop_event.wait(CHECK_INTERVAL_SECONDS)

    def check_scheduled_links(self):
        data = load_data()
        now = datetime.now()

        today = now.strftime("%A")
        current = now.strftime("%H:%M")
        date_key = now.strftime("%Y-%m-%d")
        changed = False

        for item in data["scheduled_links"]:
            if today not in item.get("days", []):
                continue

            if current != item.get("start_time"):
                continue

            key = f"{date_key} {current}"

            if item.get("last_window_notification") == key:
                continue

            notify(
                f"Διαθέσιμος σύνδεσμος: {item['name']}",
                (
                    f"Μπορείς να τον ανοίξεις έως "
                    f"{item['end_time']}."
                ),
                item["url"]
            )

            item["last_window_notification"] = key
            changed = True

        if changed:
            save_data(data)
            self.root.after(0, self.refresh_links)

    def check_youtube_channels(self):
        data = load_data()
        changed = False

        self.root.after(
            0,
            lambda: self.set_status("Έλεγχος YouTube...")
        )

        for item in data["youtube_channels"]:
            try:
                channel_id = (
                    item.get("channel_id")
                    or extract_youtube_channel_id(item["url"])
                )

                item["channel_id"] = channel_id

                video_id, title, video_url = (
                    get_latest_youtube_video(channel_id)
                )

                old_id = item.get("last_video_id", "")

                item["last_error"] = ""

                if not old_id:
                    item["last_video_id"] = video_id
                    changed = True

                elif video_id != old_id:
                    notify(
                        f"Νέο video: {item['name']}",
                        title,
                        video_url
                    )

                    item["last_video_id"] = video_id
                    changed = True

            except Exception as exc:
                item["last_error"] = str(exc)
                changed = True
                log(
                    f"YouTube check failed "
                    f"for {item.get('name')}: {exc!r}"
                )

        if changed:
            save_data(data)

        self.root.after(
            0,
            lambda: self.set_status(
                "Ο έλεγχος YouTube ολοκληρώθηκε."
            )
        )

    def check_watched_pages(self):
        data = load_data()
        changed = False

        self.root.after(
            0,
            lambda: self.set_status("Έλεγχος ιστοσελίδων...")
        )

        for item in data["watched_pages"]:
            try:
                new_hash = page_fingerprint(item["url"])
                old_hash = item.get("last_hash", "")

                if not old_hash:
                    # First check establishes a baseline.
                    item["last_hash"] = new_hash
                    changed = True

                elif new_hash != old_hash:
                    notify(
                        f"Αλλαγή: {item['name']}",
                        "Εντοπίστηκε νέο ή τροποποιημένο περιεχόμενο.",
                        item["url"]
                    )

                    item["last_hash"] = new_hash
                    changed = True

            except Exception as exc:
                log(
                    f"Page check failed "
                    f"for {item.get('name')}: {exc!r}"
                )

        data["last_online_check"] = (
            datetime.now().isoformat(timespec="seconds")
        )
        save_data(data)

        self.root.after(0, self.refresh_pages)
        self.root.after(
            0,
            lambda: self.set_status(
                "Ο έλεγχος ιστοσελίδων ολοκληρώθηκε."
            )
        )

    # --------------------------------------------------------
    # Refresh
    # --------------------------------------------------------

    def refresh_all(self):
        self.refresh_checklist()
        self.refresh_links()
        self.refresh_youtube()
        self.refresh_pages()

    def refresh_links(self):
        data = load_data()

        for row in self.links_tree.get_children():
            self.links_tree.delete(row)

        for i, item in enumerate(data["scheduled_links"]):
            days = ", ".join(
                DAY_LABELS.get(day, day)
                for day in item.get("days", [])
            )

            status = (
                "ΔΙΑΘΕΣΙΜΟ"
                if is_link_available(item)
                else "Κλειστό"
            )

            self.links_tree.insert(
                "",
                tk.END,
                iid=str(i),
                values=(
                    item["name"],
                    days,
                    f"{item['start_time']} - {item['end_time']}",
                    status,
                    item["url"]
                )
            )

        self.update_open_button()

    def refresh_youtube(self):
        data = load_data()

        for row in self.youtube_tree.get_children():
            self.youtube_tree.delete(row)

        for i, item in enumerate(data["youtube_channels"]):
            self.youtube_tree.insert(
                "",
                tk.END,
                iid=str(i),
                values=(item["name"], item["url"])
            )

    def refresh_pages(self):
        data = load_data()

        for row in self.pages_tree.get_children():
            self.pages_tree.delete(row)

        for i, item in enumerate(data["watched_pages"]):
            status = (
                "Baseline υπάρχει"
                if item.get("last_hash")
                else "Αναμονή πρώτου ελέγχου"
            )

            self.pages_tree.insert(
                "",
                tk.END,
                iid=str(i),
                values=(
                    item["name"],
                    item["url"],
                    status
                )
            )

        last_check = data.get("last_online_check", "")
        if last_check:
            try:
                dt = datetime.fromisoformat(last_check)
                text = dt.strftime("%d/%m/%Y %H:%M")
            except Exception:
                text = last_check
            self.last_check_var.set(
                f"Τελευταίος έλεγχος: {text}"
            )
        else:
            self.last_check_var.set(
                "Τελευταίος έλεγχος: -"
            )

    def set_status(self, text):
        self.status_var.set(text)

    def on_close(self):
        self.stop_event.set()
        if self.observer_running():
            # The agent finishes its notes after the window is gone.
            self.stop_observer()
        if self.quiz_server:
            self.quiz_server.shutdown()
        self.root.destroy()


if __name__ == "__main__":
    root = tk.Tk()
    app = LinkNotifierApp(root)

    # Update availability every 30 seconds in the GUI.
    def refresh_timer():
        try:
            app.refresh_links()
            app.refresh_checklist()
            root.after(30000, refresh_timer)
        except tk.TclError:
            pass

    root.after(30000, refresh_timer)
    root.mainloop()
