"""Sign a PDF on the tablet in one step.

The PDF goes to Plus One's app on the tablet in use (sign_jobs.py) over the
network, and the ink is drawn on the tablet itself, under the nib. The
signed copy comes back next to the original, with a notification. When
the app cannot be reached, the notification says so and offers signing on
this PC (sign.py's window) instead.
"""
import collections
import os
import shutil
import tempfile
import threading
import uuid

from gi.repository import Gio, GLib

from . import link, log, once, sign_jobs, signatures
from .link import channels
from .i18n import _

NOTIFICATION = "sign"
# Shared with the signing window (sign.py).
PEN = os.path.join(GLib.get_user_config_dir(), "plus-one", "pen.json")
# The PDFs as they were sent to the tablet's app: signed, that copy is what
# the marks go on (the original may change or move meanwhile).
SENT = os.path.join(GLib.get_user_cache_dir(), "plus-one", "signing")


def copy_of(path):
    """A copy of path as it is now, in SENT (this user's alone): what is
    signed, whatever becomes of path meanwhile (moved, deleted, replaced).
    OSError if it cannot be made, and nothing left behind."""
    os.makedirs(SENT, mode=0o700, exist_ok=True)
    fd, copy = tempfile.mkstemp(suffix=".pdf", dir=SENT)
    try:
        with os.fdopen(fd, "wb") as f, open(path, "rb") as original:
            shutil.copyfileobj(original, f)
    except BaseException:
        try:
            os.unlink(copy)  # half a copy: not left behind
        except OSError:
            pass
        raise
    return copy


class Signing:
    def __init__(self, app):
        self.app = app
        self.windows = []          # signing on this PC
        self.in_app = {}           # paths the tablet's app is signing -> that request's token
        self.tablet = None         # its name
        self.signed = []           # signed in a row (several PDFs sent at once): one notification
        self.reading = 0           # PDFs being read (_read): Plus One stays, busy, meanwhile
        self._reads = collections.deque()  # (work, then) still to do, in the order asked
        self._reader = False       # the thread that does them, running
        self._reads_lock = threading.Lock()
        for name, callback in (("sign-here", self._sign_here), ("open-file", self._open_file),
                               ("show-file", self._show_file)):
            action = Gio.SimpleAction.new(name, GLib.VariantType("s"))
            action.connect("activate", callback)
            app.add_action(action)

    def request(self, path):
        """Sign the PDF at path on the tablet in use, in its app (once read,
        off the main loop)."""
        path = os.path.abspath(path)
        log("signing", path)
        from .pdf import problem
        self._read(lambda: problem(path), lambda wrong: self._checked(path, wrong))

    def _checked(self, path, wrong):
        """request()'s, once the PDF is read: wrong, pdf.problem()'s."""
        if wrong:
            # Not a PDF, too big, locked: said now, not after signing it.
            template, args = wrong
            note = Gio.Notification.new(_("Cannot sign {name}").format(name=os.path.basename(path)))
            note.set_body(_(template).format(name=os.path.basename(path), **args))
            self.app.send_notification(NOTIFICATION, note)
            return
        if self.app.tablets.active() is None:
            note = Gio.Notification.new(_("No tablet paired"))
            note.set_body(_("Pair your tablet in Plus One to sign {name} on it")
                          .format(name=os.path.basename(path)))
            note.set_default_action("app.show")
            note.add_button_with_target(_("Sign on this PC"), "app.sign-here", GLib.Variant("s", path))
            self.app.send_notification(NOTIFICATION, note)
            return
        if not (self.in_app or self.windows):
            self.signed = []  # nothing on the way: a new batch
        # Asked again while the tablet takes its time: one window per document.
        for window in self.windows:
            if window.path == path:
                window.present()
                return
        if path in self.in_app:
            return
        # (The "Sign on Tablet" switch is Files' entry only: the app signs
        # whenever it is there. Showing the desktop, it signs too: the
        # desktop rests meanwhile and comes back after.)
        target = self._find_app()
        if target:
            self._sign_in_app(path, target)
            return
        tablet = self.app.tablets.active()
        self._notify_waiting(path, _("Plus One on {tablet} is not on the network: open it there, or sign here")
                          .format(tablet=tablet.name))

    # -- in the tablet's app ---------------------------------------------------
    def _sign_in_app(self, path, target):
        _targets, tablet, _app_id = target
        log(f"signing in the app on {tablet}: {path}")
        token = uuid.uuid4().hex
        self.in_app[path] = token
        # Plus One stays until the answer comes, window closed or not.
        self.app.hold()
        self._notify_in_app(path, _("Sending {name} to Plus One on {tablet}"), tablet)
        # The copy that goes (up to pdf.MAX_SIZE) made off the main loop too,
        # in the same order: the PDFs' turns are taken in it.
        self._read(lambda: self._copy(path), lambda sent: self._copied(path, token, target, sent))

    @staticmethod
    def _copy(path):
        """copy_of(path): its name, or the OSError."""
        try:
            return copy_of(path)
        except OSError as e:
            return e

    def _copied(self, path, token, target, sent):
        """_sign_in_app's, once the copy that goes is made (sent: its name,
        or why not)."""
        targets, _tablet, app_id = target
        if not isinstance(sent, str):
            if self.in_app.get(path) == token:
                del self.in_app[path]
                note = Gio.Notification.new(_("Cannot open {name}").format(name=os.path.basename(path)))
                note.set_body(getattr(sent, "strerror", None) or str(sent))
                self.app.send_notification(NOTIFICATION, note)
            self.app.release()
            return
        if self.in_app.get(path) != token:
            self._forget_sent(sent)  # the user chose to sign on the PC meanwhile (or asked again since)
            return

        from .ink import load_pen
        color, width, _smoothing = load_pen(PEN)
        pen = {"color": list(color), "width": width}
        # The saved signatures go to the tablet, and come from it, only when
        # the user shares them (here, and for this PC there).
        shared = self.app.settings.share_signatures
        ours = signatures.message() if shared else None

        def theirs(found):
            GLib.idle_add(once(self._merge_signatures, found))

        def opened(tablet):
            GLib.idle_add(once(self._notify_in_app, path, _("Sign {name} on {tablet}"), tablet))

        # Taken now, in the order the PDFs were asked for: they reach the tablet in it.
        turn = sign_jobs.next_turn()

        def work():
            result = error = None
            try:
                result = sign_jobs.sign(targets, sent, pen=pen, signatures=ours, expect=app_id,
                                        on_signatures=theirs if shared else None, on_opened=opened,
                                        locate=lambda: self._locate_app(app_id),
                                        turn=turn, title=os.path.basename(path))
            except link.PairingRefused:
                error = "refused"
            except sign_jobs.Lost:
                error = "lost"
            except sign_jobs.Mismatch as e:
                log("signing:", e)
                error = "mismatch"
            except link.Incompatible as e:
                log("signing:", e)
                error = "incompatible"
            except Exception as e:  # noqa: BLE001 - whatever it was, Second Screen instead
                error = e
            finally:
                GLib.idle_add(once(self._app_done, path, token, sent, result, error))
        threading.Thread(target=work, daemon=True).start()

    def _find_app(self):
        """(addresses, name, app id) of the tablet in use's app on the
        network, or None. Every address it is announced at: someone may
        announce it too, and only the real one proves it (TLS)."""
        tablet = self.app.tablets.active()
        where = self.app.tablets.targets(tablet) if tablet else []
        # No address, but over the link (the cable): it goes there.
        reached = bool(where) or (tablet is not None and channels.connected(tablet.app_id))
        return (where, tablet.name, tablet.app_id) if reached else None

    def _read(self, work, then):
        """work() on a thread of its own, the PDFs read one after the other
        in the order asked; then(its result, None if it failed) on the main
        loop after. Reading one (up to pdf.MAX_SIZE, parsed) took seconds,
        the window and D-Bus frozen meanwhile."""
        self.reading += 1
        self.app.hold()
        with self._reads_lock:
            self._reads.append((work, then))
            if self._reader:
                return
            self._reader = True
        threading.Thread(target=self._read_all, name="plusone-pdf", daemon=True).start()

    def _read_all(self):
        while True:
            with self._reads_lock:
                if not self._reads:
                    self._reader = False
                    return
                work, then = self._reads.popleft()
            try:
                result = work()
            except Exception as e:  # noqa: BLE001 - not known, as for a PDF pypdf cannot read
                log("signing: not read:", repr(e))
                result = None
            GLib.idle_add(once(self._read_done, then, result))

    def _read_done(self, then, result):
        self.reading -= 1
        try:
            then(result)
        finally:
            self.app.release()

    def _locate_app(self, app_id):
        """Where the app the PDF went to is now (its port changes when it
        restarts): that tablet's addresses, not another's."""
        return self.app.discovery.addresses(app_id) or None

    def _app_done(self, path, token, sent, result, error):
        saving = False
        try:
            saving = self._answer(path, token, sent, result, error)
        finally:
            if not saving:
                self._forget_sent(sent)

    def _forget_sent(self, sent):
        try:
            os.unlink(sent)
        except OSError:
            pass
        self.app.release()

    def _answer(self, path, token, sent, result, error):
        """What the tablet's app answered for one PDF: True while its signed
        copy is being saved (which ends the request), else done."""
        if self.in_app.get(path) != token:
            return False  # the user chose to sign on the PC meanwhile (or asked again since)
        del self.in_app[path]
        if error == "refused":
            log("signing: the tablet does not know this PC")
            note = Gio.Notification.new(_("Not paired"))
            note.set_body(_("{name} no longer knows this PC: pair it again").format(name=self.tablet))
            note.set_default_action("app.show")
            self.app.send_notification(NOTIFICATION, note)
            return False
        if error == "incompatible":
            # (The tablet's app is brought to this PC's version by itself: updates.py.)
            self._notify_waiting(path, _("Plus One on the tablet is another version than on this PC: "
                                      "it is updated once nothing is in use; or sign here"))
            return False
        if error == "lost":
            note = Gio.Notification.new(_("Lost the tablet"))
            note.set_body(_("What was signed on the tablet did not reach the PC: {name} is unchanged")
                          .format(name=os.path.basename(path)))
            self.app.send_notification(NOTIFICATION, note)
            return False
        if error == "mismatch":
            note = Gio.Notification.new(_("Not saved"))
            note.set_body(_("The tablet answered for another document: {name} is unchanged")
                          .format(name=os.path.basename(path)))
            self.app.send_notification(NOTIFICATION, note)
            return False
        if error is not None:
            log("signing in the app failed:", repr(error))
            self._notify_waiting(path, _("Plus One on {tablet} did not answer: open it there, or sign here")
                              .format(tablet=self.tablet))
            return False
        if isinstance(result, dict):
            self._keep_from_app(result)
        marks = sign_jobs.to_marks(result) if result else {}
        if not marks:
            # Not signed: what still waits on the tablet, if anything, or
            # what was signed before it.
            if self.in_app:
                left = next(iter(self.in_app))
                self._notify_in_app(left, _("Sign {name} on {tablet}"), self.tablet)
            elif not self.signed:
                self.app.withdraw_notification(NOTIFICATION)
            return False

        def save():
            # On the copy that was signed, next to the original; where that
            # folder takes nothing (read-only, gone), in Documents: what was
            # signed on the tablet is not lost for it.
            from .pdf import digitally_signed, save_signed
            try:
                done = save_signed(sent, marks, like=path)
            except OSError as e:
                log("signing: not next to the original:", repr(e))
                folder = GLib.get_user_special_dir(GLib.UserDirectory.DIRECTORY_DOCUMENTS) or GLib.get_home_dir()
                try:
                    done = save_signed(sent, marks, like=os.path.join(folder, os.path.basename(path)))
                except Exception as e2:  # noqa: BLE001 - shown to the user
                    log("signing: could not save:", repr(e2))
                    done = e2
            except Exception as e:  # noqa: BLE001 - shown to the user
                log("signing: could not save:", repr(e))
                done = e
            certified = digitally_signed(sent)
            GLib.idle_add(once(self._app_saved, done, certified, sent))
        threading.Thread(target=save, daemon=True).start()
        return True

    @staticmethod
    def _merge_signatures(found):
        """Signatures added or removed on the tablet."""
        try:
            signatures.merge(found)
        except OSError as e:
            log("signing: signatures from the tablet not kept:", repr(e))

    def _keep_from_app(self, result):
        """The pen chosen on the tablet, for next time (on the PC too)."""
        from .ink import load_pen, save_pen
        pen = result.get("pen")
        if isinstance(pen, dict):
            try:
                _color, _width, smoothing = load_pen(PEN)
                save_pen(PEN, tuple(float(c) for c in pen["color"]), float(pen["width"]), smoothing)
            except (KeyError, TypeError, ValueError, OSError) as e:
                log("signing: pen from the tablet not kept:", repr(e))

    def _app_saved(self, result, certified=False, sent=None):
        if sent:
            self._forget_sent(sent)
        if isinstance(result, Exception):
            note = Gio.Notification.new(_("Could not save: {error}").format(error=result))
            self.app.send_notification(NOTIFICATION, note)
            return
        self._signed(result, certified)

    def _notify_in_app(self, path, template, tablet):
        """What the tablet's app has to sign: this PDF (template takes its
        name and the tablet's), or how many when there are several."""
        self.tablet = tablet
        if len(self.in_app) > 1:
            note = Gio.Notification.new(_("Sign on the tablet"))
            note.set_body(_("{count} PDFs to sign on {tablet}").format(count=len(self.in_app), tablet=tablet))
            self.app.send_notification(NOTIFICATION, note)
        elif path in self.in_app:
            self._notify_waiting(path, template.format(name=os.path.basename(path), tablet=tablet))

    def _notify_waiting(self, path, body):
        """About this PDF, with the way to sign it here instead."""
        note = Gio.Notification.new(_("Sign on the tablet"))
        note.set_body(body)
        note.set_default_action("app.show")
        note.add_button_with_target(_("Sign on this PC"), "app.sign-here", GLib.Variant("s", path))
        self.app.send_notification(NOTIFICATION, note)

    # -- signing on this PC ------------------------------------------------------
    def _open(self, path):
        from .sign import SignWindow
        try:
            window = SignWindow(self.app, path, lambda result: self._done(window, path, result))
        except Exception as e:  # noqa: BLE001 - GLib.Error mostly (damaged, a password), but whatever it is
            note = Gio.Notification.new(_("Cannot open {name}").format(name=os.path.basename(path)))
            note.set_body(getattr(e, "message", None) or str(e))
            self.app.send_notification(NOTIFICATION, note)
            return
        self.windows.append(window)
        window.maximize()
        window.present()

    def _done(self, window, path, result):
        if window in self.windows:
            self.windows.remove(window)
        if result:
            from .pdf import digitally_signed
            self._read(lambda: digitally_signed(path), lambda certified: self._signed(result, bool(certified)))

    def _signed(self, result, certified=False):
        """One notification for the PDFs signed in a row. certified: the
        original carried a digital signature, which the copy cannot keep."""
        log("signed:", result)
        self.signed.append(result)
        if len(self.signed) > 1:
            note = Gio.Notification.new(_("Signed {count} PDFs").format(count=len(self.signed)))
            note.set_body(", ".join(os.path.basename(p) for p in self.signed))
            note.set_default_action_and_target("app.show-file", GLib.Variant("s", result))
        else:
            note = Gio.Notification.new(_("Signed"))
            note.set_body(os.path.basename(result) + ("\n" + _("It had a digital signature: in the signed copy, "
                                                                 "it no longer holds") if certified else ""))
            note.set_default_action_and_target("app.open-file", GLib.Variant("s", result))
            note.add_button_with_target(_("Open"), "app.open-file", GLib.Variant("s", result))
        note.add_button_with_target(_("Show in folder"), "app.show-file", GLib.Variant("s", result))
        self.app.send_notification(NOTIFICATION, note)

    # -- notification actions --------------------------------------------------
    def _sign_here(self, _action, param):
        path = param.get_string()
        self.in_app.pop(path, None)  # whatever the app sends back now is ignored
        self.app.withdraw_notification(NOTIFICATION)
        self._open(path)

    def _open_file(self, _action, param):
        Gio.AppInfo.launch_default_for_uri(Gio.File.new_for_path(param.get_string()).get_uri(), None)

    def _show_file(self, _action, param):
        uri = Gio.File.new_for_path(param.get_string()).get_uri()
        Gio.bus_get_sync(Gio.BusType.SESSION, None).call(
            "org.freedesktop.FileManager1", "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1", "ShowItems", GLib.Variant("(ass)", ([uri], "")),
            None, Gio.DBusCallFlags.NONE, -1, None, None, None)
