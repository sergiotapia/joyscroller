"""Probe how browsers store cookies on this OS (issues #1, #2, #3).

For each browser installed here, on its default profile (where real users'
cookies live):

1. Launch it visibly and open a local page that sets a persistent cookie.
2. While it runs, check whether its cookie database can be read: through
   SQLite's immutable mode, a read-only open, and a plain file copy (what
   browser.rs does today).
3. Quit it, then decrypt the cookie the way joyscroller would have to on this
   OS: the fixed or Secret Service key on Linux, the Keychain password on
   macOS, the DPAPI-protected key in `Local State` on Windows.

Firefox, the fallback where a Chromium browser can't be read, is checked the
same way; its cookies aren't encrypted.

Takes the browsers to probe as arguments (chrome, edge, brave, firefox), or
probes all of them. Writes a Markdown report to stdout (and the job summary) and report-<os>.json.
Never prints a key or password, only whether it was found and its length.
"""

import base64
import ctypes
import hashlib
import http.server
import json
import os
import platform
import secrets
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import traceback
from pathlib import Path

from cryptography.hazmat.primitives import padding
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
from cryptography.hazmat.primitives.ciphers.aead import AESGCM

COOKIE_NAME = "joyscroller_probe"
WAIT_FOR_COOKIE = 90  # Chromium commits cookies to disk about every 30 s.
# On a real Mac the Keychain read waits for the user to click Allow; in CI
# nobody can, so give up quickly there.
KEYCHAIN_TIMEOUT = 20 if os.environ.get("CI") else 180

HOME = Path.home()
if sys.platform == "win32":
    LOCAL = Path(os.environ["LOCALAPPDATA"])
    PF = Path(os.environ["ProgramFiles"])
    PF86 = Path(os.environ["ProgramFiles(x86)"])
    BROWSERS = {
        "chrome": {
            "exe": [PF / "Google/Chrome/Application/chrome.exe", PF86 / "Google/Chrome/Application/chrome.exe"],
            "data": LOCAL / "Google/Chrome/User Data",
            "kill": ["taskkill", "/F", "/T", "/IM", "chrome.exe"],
        },
        "edge": {
            "exe": [PF86 / "Microsoft/Edge/Application/msedge.exe", PF / "Microsoft/Edge/Application/msedge.exe"],
            "data": LOCAL / "Microsoft/Edge/User Data",
            "kill": ["taskkill", "/F", "/T", "/IM", "msedge.exe"],
        },
        "brave": {
            "exe": [
                PF / "BraveSoftware/Brave-Browser/Application/brave.exe",
                LOCAL / "BraveSoftware/Brave-Browser/Application/brave.exe",
            ],
            "data": LOCAL / "BraveSoftware/Brave-Browser/User Data",
            "kill": ["taskkill", "/F", "/T", "/IM", "brave.exe"],
        },
    }
elif sys.platform == "darwin":
    APPS = Path("/Applications")
    SUPPORT = HOME / "Library/Application Support"
    BROWSERS = {
        "chrome": {
            "exe": [APPS / "Google Chrome.app/Contents/MacOS/Google Chrome"],
            "data": SUPPORT / "Google/Chrome",
            "keychain": ("Chrome Safe Storage", "Chrome"),
            "kill": ["pkill", "-f", "Google Chrome.app"],
        },
        "edge": {
            "exe": [APPS / "Microsoft Edge.app/Contents/MacOS/Microsoft Edge"],
            "data": SUPPORT / "Microsoft Edge",
            "keychain": ("Microsoft Edge Safe Storage", "Microsoft Edge"),
            "kill": ["pkill", "-f", "Microsoft Edge.app"],
        },
        "brave": {
            "exe": [APPS / "Brave Browser.app/Contents/MacOS/Brave Browser"],
            "data": SUPPORT / "BraveSoftware/Brave-Browser",
            "keychain": ("Brave Safe Storage", "Brave"),
            "kill": ["pkill", "-f", "Brave Browser.app"],
        },
    }
else:
    CONFIG = HOME / ".config"

    def which(*names):
        return [Path(p) for p in map(shutil.which, names) if p]

    BROWSERS = {
        "chrome": {
            "exe": which("google-chrome-stable", "google-chrome"),
            "data": CONFIG / "google-chrome",
            "secret_app": ["chrome"],
            "kill": ["pkill", "-f", "/opt/google/chrome/"],
        },
        "edge": {
            "exe": which("microsoft-edge-stable", "microsoft-edge"),
            "data": CONFIG / "microsoft-edge",
            "secret_app": ["microsoft-edge", "chromium"],
            "kill": ["pkill", "-f", "/opt/microsoft/msedge/"],
        },
        "brave": {
            "exe": which("brave-browser-stable", "brave-browser"),
            "data": CONFIG / "BraveSoftware/Brave-Browser",
            "secret_app": ["brave"],
            "kill": ["pkill", "-f", "/opt/brave.com/"],
        },
    }


def serve_cookie(value):
    """A local page that sets a persistent cookie; returns its URL."""

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200)
            self.send_header("Set-Cookie", f"{COOKIE_NAME}={value}; Max-Age=31536000; Path=/")
            self.send_header("Content-Type", "text/html")
            self.end_headers()
            self.wfile.write(b"<h1>joyscroller cookie probe</h1>")

        def log_message(self, *_):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, f"http://127.0.0.1:{server.server_port}/"


def run(cmd, timeout=30, stdin=None):
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, input=stdin)
        return p.returncode, p.stdout, p.stderr.strip()
    except subprocess.TimeoutExpired:
        return None, "", f"timed out after {timeout}s"


def browser_version(exe):
    if sys.platform == "win32":
        _, out, _ = run(["powershell", "-NoProfile", "-Command", f"(Get-Item '{exe}').VersionInfo.ProductVersion"])
    else:
        _, out, _ = run([str(exe), "--version"])
    return out.strip()


def cookie_db(data):
    for p in (data / "Default/Network/Cookies", data / "Default/Cookies"):
        if p.exists():
            return p
    return None


def read_cookie(db, uri_params):
    """(row, db_version, error) reading `db` through a SQLite URI."""
    try:
        conn = sqlite3.connect(db.as_uri() + "?" + uri_params, uri=True, timeout=2)
        try:
            version = conn.execute("SELECT value FROM meta WHERE key = 'version'").fetchone()
            row = conn.execute(
                "SELECT host_key, value, encrypted_value FROM cookies WHERE name = ?", (COOKIE_NAME,)
            ).fetchone()
            return row, int(version[0]) if version else None, None
        finally:
            conn.close()
    except Exception as e:
        return None, None, f"{type(e).__name__}: {e}"


def try_copy(db):
    with tempfile.TemporaryDirectory() as d:
        try:
            shutil.copy(db, Path(d) / "Cookies")
            return "ok"
        except Exception as e:
            return f"{type(e).__name__}: {e}"


def aes_cbc(key, data):
    dec = Cipher(algorithms.AES(key), modes.CBC(b" " * 16)).decryptor()
    padded = dec.update(data) + dec.finalize()
    unpadder = padding.PKCS7(128).unpadder()
    return unpadder.update(padded) + unpadder.finalize()


class DataBlob(ctypes.Structure):
    _fields_ = [("cbData", ctypes.c_uint32), ("pbData", ctypes.POINTER(ctypes.c_char))]


def dpapi_unprotect(data):
    buf = ctypes.create_string_buffer(data, len(data))
    blob_in = DataBlob(len(data), ctypes.cast(buf, ctypes.POINTER(ctypes.c_char)))
    blob_out = DataBlob()
    if not ctypes.windll.crypt32.CryptUnprotectData(
        ctypes.byref(blob_in), None, None, None, None, 0, ctypes.byref(blob_out)
    ):
        raise ctypes.WinError()
    try:
        return ctypes.string_at(blob_out.pbData, blob_out.cbData)
    finally:
        ctypes.windll.kernel32.LocalFree(blob_out.pbData)


def decrypt(info, r, enc):
    """Decrypt the cookie value as joyscroller would on this OS; fills `r`."""
    prefix = enc[:3]
    r["prefix"] = prefix.decode("ascii", "replace") if prefix[:1] == b"v" else f"raw({prefix.hex()})"
    if sys.platform == "win32":
        state = json.loads((info["data"] / "Local State").read_text(encoding="utf-8"))
        os_crypt = state.get("os_crypt", {})
        r["local_state_os_crypt_keys"] = sorted(os_crypt)
        if prefix == b"v20":
            r["key_source"] = "app-bound (v20): needs the browser's elevation service, not attempted"
            return None
        wrapped = base64.b64decode(os_crypt["encrypted_key"])
        if wrapped[:5] != b"DPAPI":
            raise ValueError(f"encrypted_key starts with {wrapped[:5]!r}, not DPAPI")
        key = dpapi_unprotect(wrapped[5:])
        r["key_source"] = f"Local State encrypted_key via DPAPI ({len(key)}-byte key)"
        return AESGCM(key).decrypt(enc[3:15], enc[15:], None)
    if sys.platform == "darwin":
        service, account = info["keychain"]
        code, out, err = run(["security", "find-generic-password", "-w", "-s", service, "-a", account], timeout=KEYCHAIN_TIMEOUT)
        if code != 0:
            r["key_source"] = f"Keychain '{service}' read failed (exit {code}): {err}"
            return None
        password = out.rstrip("\n").encode()
        r["key_source"] = f"Keychain '{service}' ({len(password)}-char password), PBKDF2 x1003"
        key = hashlib.pbkdf2_hmac("sha1", password, b"saltysalt", 1003, 16)
        return aes_cbc(key, enc[3:])
    if prefix == b"v10":
        r["key_source"] = "fixed 'peanuts' key (v10)"
        return aes_cbc(hashlib.pbkdf2_hmac("sha1", b"peanuts", b"saltysalt", 1, 16), enc[3:])
    tried = []
    for app in info["secret_app"]:
        code, out, err = run(["secret-tool", "lookup", "application", app])
        if code != 0 or not out:
            tried.append(f"{app}: exit {code}")
            continue
        password = out.rstrip("\n").encode()
        key = hashlib.pbkdf2_hmac("sha1", password, b"saltysalt", 1, 16)
        try:
            plain = aes_cbc(key, enc[3:])
        except ValueError:
            tried.append(f"{app}: wrong key")
            continue
        r["key_source"] = f"Secret Service application={app} ({len(password)}-char password)"
        return plain
    r["key_source"] = "Secret Service lookup failed: " + "; ".join(tried)
    return None


def probe(name, info):
    r = {"browser": name}
    exe = next((p for p in info["exe"] if p.exists()), None)
    if exe is None:
        r["error"] = "not installed"
        return r
    r["version"] = browser_version(exe)
    value = secrets.token_hex(8)
    server, url = serve_cookie(value)
    args = [str(exe), "--no-first-run", "--no-default-browser-check"]
    if sys.platform.startswith("linux"):
        # Ubuntu 24.04's AppArmor rules can block the sandbox in CI; it doesn't
        # affect how cookies are stored.
        args.append("--no-sandbox")
    proc = subprocess.Popen(args + [url], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.time() + WAIT_FOR_COOKIE
        while time.time() < deadline:
            time.sleep(5)
            db = cookie_db(info["data"])
            if db is None:
                continue
            row, _, err = read_cookie(db, "mode=ro&immutable=1")
            r["while_running_immutable"] = "ok" if row else (err or "cookie not written yet")
            if row:
                break
        r["seconds_until_on_disk"] = round(WAIT_FOR_COOKIE - (deadline - time.time()))
        db = cookie_db(info["data"])
        if db is None:
            r["error"] = f"no cookie database under {info['data']}"
            return r
        r["db_path"] = str(db.relative_to(info["data"]))
        r["while_running_copy"] = try_copy(db)
        row, _, err = read_cookie(db, "mode=ro")
        r["while_running_readonly"] = "ok" if row else (err or "opened, cookie not visible")
    finally:
        proc.terminate()
        run(info["kill"])
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
        time.sleep(3)
        server.shutdown()

    row, db_version, err = read_cookie(db, "mode=ro")
    r["db_version"] = db_version
    if row is None:
        r["error"] = f"cookie not found after quitting: {err or 'no row'}"
        return r
    _, plain, enc = row
    if plain:
        r["prefix"] = "unencrypted"
        r["decrypted_ok"] = plain == value
        return r
    try:
        out = decrypt(info, r, enc)
    except Exception as e:
        r["decrypt_error"] = f"{type(e).__name__}: {e}"
        return r
    if out is None:
        r["decrypted_ok"] = False
        return r
    # Chromium 130+ (database version 24) prepends SHA-256 of the host.
    if db_version and db_version >= 24:
        out = out[32:]
    r["decrypted_ok"] = out.decode("utf-8", "replace") == value
    return r


# Firefox keeps cookies unencrypted in cookies.sqlite, in a profile named by
# profiles.ini. Newer Linux builds use XDG paths and Ubuntu's is often a snap.
if sys.platform == "win32":
    FIREFOX = {
        "exe": [PF / "Mozilla Firefox/firefox.exe", PF86 / "Mozilla Firefox/firefox.exe"],
        "roots": [Path(os.environ["APPDATA"]) / "Mozilla/Firefox"],
        "kill": ["taskkill", "/F", "/T", "/IM", "firefox.exe"],
    }
elif sys.platform == "darwin":
    FIREFOX = {
        "exe": [APPS / "Firefox.app/Contents/MacOS/firefox"],
        "roots": [SUPPORT / "Firefox"],
        "kill": ["pkill", "-f", "Firefox.app"],
    }
else:
    FIREFOX = {
        "exe": which("firefox"),
        "roots": [CONFIG / "mozilla/firefox", HOME / ".mozilla/firefox", HOME / "snap/firefox/common/.mozilla/firefox"],
        "kill": ["pkill", "-f", "firefox"],
    }


def firefox_profile():
    """The default profile's folder, from the first profiles.ini found."""
    import configparser

    for root in FIREFOX["roots"]:
        ini = root / "profiles.ini"
        if not ini.exists():
            continue
        cfg = configparser.ConfigParser()
        cfg.read(ini)
        # [Install...] Default= is the profile this install uses; fall back to
        # the [Profile...] marked Default=1.
        paths = [cfg[s]["Default"] for s in cfg.sections() if s.startswith("Install") and "Default" in cfg[s]]
        paths += [cfg[s]["Path"] for s in cfg.sections() if s.startswith("Profile") and cfg[s].get("Default") == "1"]
        for rel in paths:
            folder = root / rel if not Path(rel).is_absolute() else Path(rel)
            if folder.exists():
                return folder
    return None


def read_firefox_cookie(db, uri_params):
    try:
        conn = sqlite3.connect(db.as_uri() + "?" + uri_params, uri=True, timeout=2)
        try:
            row = conn.execute("SELECT value FROM moz_cookies WHERE name = ?", (COOKIE_NAME,)).fetchone()
            return row, None
        finally:
            conn.close()
    except Exception as e:
        return None, f"{type(e).__name__}: {e}"


def copy_with_wal(db):
    """Copy cookies.sqlite with its -wal file, where Firefox's newest writes sit
    until a checkpoint, and look for the cookie in the copy."""
    with tempfile.TemporaryDirectory() as d:
        try:
            dst = Path(d) / "cookies.sqlite"
            shutil.copy(db, dst)
            wal = db.with_name(db.name + "-wal")
            if wal.exists():
                shutil.copy(wal, dst.with_name(dst.name + "-wal"))
            row, err = read_firefox_cookie(dst, "mode=rw")
            return "ok" if row else (err or "copied, cookie not visible")
        except Exception as e:
            return f"{type(e).__name__}: {e}"


def probe_firefox():
    r = {"browser": "firefox", "prefix": "unencrypted", "key_source": "none needed"}
    exe = next((p for p in FIREFOX["exe"] if p.exists()), None)
    if exe is None:
        r["error"] = "not installed"
        return r
    r["version"] = browser_version(exe)
    value = secrets.token_hex(8)
    server, url = serve_cookie(value)
    proc = subprocess.Popen([str(exe), url], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    db = None
    try:
        deadline = time.time() + WAIT_FOR_COOKIE
        while time.time() < deadline:
            time.sleep(5)
            profile = firefox_profile()
            db = profile / "cookies.sqlite" if profile else None
            if db is None or not db.exists():
                continue
            row, err = read_firefox_cookie(db, "mode=ro&immutable=1")
            r["while_running_immutable"] = "ok" if row else (err or "cookie not written yet")
            if row:
                break
        r["seconds_until_on_disk"] = round(WAIT_FOR_COOKIE - (deadline - time.time()))
        if db is None or not db.exists():
            r["error"] = f"no cookies.sqlite under {FIREFOX['roots']}"
            return r
        r["db_path"] = str(db)
        r["while_running_copy"] = try_copy(db)
        r["while_running_copy_with_wal"] = copy_with_wal(db)
        row, err = read_firefox_cookie(db, "mode=ro")
        r["while_running_readonly"] = "ok" if row else (err or "opened, cookie not visible")
    finally:
        proc.terminate()
        run(FIREFOX["kill"])
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
        time.sleep(3)
        server.shutdown()
    row, err = read_firefox_cookie(db, "mode=ro")
    if row is None:
        r["error"] = f"cookie not found after quitting: {err or 'no row'}"
        return r
    r["decrypted_ok"] = row[0] == value
    return r


def keyring_inventory():
    """Names of the browser secrets in the OS keyring, without their values."""
    if sys.platform == "darwin":
        _, out, err = run(["security", "dump-keychain"])
        lines = [line.strip() for line in out.splitlines() if "Safe Storage" in line]
        return sorted(set(lines)) or [f"none ({err})" if err else "none"]
    if sys.platform.startswith("linux"):
        code, out, err = run(["secret-tool", "search", "--all", "xdg:schema", "chrome_libsecret_os_crypt_password_v2"])
        lines = [line for line in (out + "\n" + err).splitlines() if line.startswith("attribute.application")]
        return lines or [f"none (exit {code})"]
    return []


def main():
    os_name = {"win32": "windows", "darwin": "macos"}.get(sys.platform, "linux")
    results = []
    # Browsers named on the command line, or all of them.
    wanted = set(sys.argv[1:]) or {*BROWSERS, "firefox"}
    for name, info in BROWSERS.items():
        if name not in wanted:
            continue
        print(f"probe: {name}: opening it, waiting for the cookie, then quitting it", file=sys.stderr, flush=True)
        try:
            results.append(probe(name, info))
        except Exception:
            results.append({"browser": name, "error": traceback.format_exc()})
        print(json.dumps(results[-1], indent=2), file=sys.stderr, flush=True)
    if "firefox" in wanted:
        print("probe: firefox: opening it, waiting for the cookie, then quitting it", file=sys.stderr, flush=True)
        try:
            results.append(probe_firefox())
        except Exception:
            results.append({"browser": "firefox", "error": traceback.format_exc()})
    report = {
        "os": os_name,
        "platform": platform.platform(),
        "machine": platform.machine(),
        "keyring": keyring_inventory(),
        "results": results,
    }
    Path(f"report-{os_name}.json").write_text(json.dumps(report, indent=2))

    md = [f"## {os_name}: {report['platform']} ({report['machine']})", ""]
    md += [
        "| Browser | Version | Prefix | DB ver | Read while running (immutable / read-only / copy / copy with WAL) | Key | Decrypts? |",
        "|---|---|---|---|---|---|---|",
    ]
    for r in results:
        running = " / ".join(
            r.get(k, "-") for k in ("while_running_immutable", "while_running_readonly", "while_running_copy", "while_running_copy_with_wal")
        )
        decrypted = {True: "yes", False: "no"}.get(r.get("decrypted_ok"), r.get("decrypt_error", r.get("error", "-")))
        cells = [r["browser"], r.get("version", "-"), r.get("prefix", "-"), str(r.get("db_version", "-")),
                 running, r.get("key_source", "-"), decrypted]
        md.append("| " + " | ".join(str(c).replace("|", "\\|").replace("\n", " ") for c in cells) + " |")
    if report["keyring"]:
        md += ["", "Keyring entries:", ""] + [f"- `{line}`" for line in report["keyring"]]
    if sys.platform == "win32":
        md += ["", "`Local State` os_crypt keys:", ""]
        md += [f"- {r['browser']}: {r.get('local_state_os_crypt_keys', '-')}" for r in results]
    text = "\n".join(md) + "\n"
    print(text)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as f:
            f.write(text)


if __name__ == "__main__":
    main()
