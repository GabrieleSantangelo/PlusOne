"""Background application helpers: settings file and autostart entry."""
import json

from plusone.app import APP_ID, load_settings, set_autostart
from plusone.engine import Settings


def test_settings_from_older_versions(tmp_path):
    # 0.2 wrote "stay_ready"; unknown keys must not make the file unreadable.
    path = tmp_path / "settings.json"
    path.write_text(json.dumps({"bitrate": 15000, "stay_ready": False}))
    settings = load_settings(str(path))
    assert settings.bitrate == 15000
    assert settings.background is True


def test_unreadable_settings_fall_back_to_defaults(tmp_path):
    path = tmp_path / "settings.json"
    path.write_text("{not json")
    assert load_settings(str(path)) == Settings()


def test_autostart_entry(tmp_path):
    set_autostart(True, config_dir=tmp_path)
    entry = (tmp_path / "autostart" / f"{APP_ID}.desktop").read_text()
    assert "--gapplication-service" in entry
    assert "NoDisplay=true" in entry
    set_autostart(False, config_dir=tmp_path)
    assert not (tmp_path / "autostart" / f"{APP_ID}.desktop").exists()


def test_sign_entry_hides_the_system_one(tmp_path):
    from plusone.app import SIGN_ENTRY, set_sign_entry
    path = tmp_path / "applications" / SIGN_ENTRY
    set_sign_entry(True, data_dir=tmp_path)
    assert not path.exists()  # the system entry shows as it is
    set_sign_entry(False, data_dir=tmp_path)
    text = path.read_text()
    assert "Type=Application" in text and "Hidden=true" in text
    set_sign_entry(False, data_dir=tmp_path)
    assert path.read_text() == text
    set_sign_entry(True, data_dir=tmp_path)
    assert not path.exists()


def test_sign_entry_of_a_per_user_install_comes_back(tmp_path):
    from gi.repository import GLib

    from plusone.app import SIGN_ENTRY, set_sign_entry
    path = tmp_path / "applications" / SIGN_ENTRY
    path.parent.mkdir()
    path.write_text("[Desktop Entry]\nType=Application\nName=Sign on Tablet\nName[it]=Firma sul tablet\n"
                    "Exec=/home/me/plus-one/bin/plus-one --sign %F\nMimeType=application/pdf;\n")
    set_sign_entry(False, data_dir=tmp_path)
    entry = GLib.KeyFile()
    entry.load_from_file(str(path), GLib.KeyFileFlags.KEEP_TRANSLATIONS)
    assert entry.get_boolean("Desktop Entry", "Hidden")
    assert entry.get_string("Desktop Entry", "Exec") == "/home/me/plus-one/bin/plus-one --sign %F"
    set_sign_entry(True, data_dir=tmp_path)
    entry.load_from_file(str(path), GLib.KeyFileFlags.KEEP_TRANSLATIONS)
    assert "Hidden" not in entry.get_keys("Desktop Entry")[0]
    assert entry.get_locale_string("Desktop Entry", "Name", "it") == "Firma sul tablet"
