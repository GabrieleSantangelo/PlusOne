"""Every user-visible English string has an Italian translation."""
import ast
import os

import pytest

from plusone import i18n
from plusone.engine import NOT_ANSWERING

ROOT = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "plusone")


def strings_in(filename):
    tree = ast.parse(open(os.path.join(ROOT, filename)).read())
    found = set()
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        name = getattr(node.func, "id", None) or getattr(node.func, "attr", None)
        args = node.args
        if name == "_" and args and isinstance(args[0], ast.Constant):
            found.add(args[0].value)
        if name == "_set_state" and len(args) > 1 and isinstance(args[1], ast.Constant):
            found.add(args[1].value)
    return found


def app_constants():
    from_module = set()
    tree = ast.parse(open(os.path.join(ROOT, "app.py")).read())
    for node in ast.walk(tree):
        if isinstance(node, ast.Assign) and getattr(node.targets[0], "id", "") in ("TITLES", "IDLE_HINT"):
            for const in ast.walk(node.value):
                if isinstance(const, ast.Constant) and isinstance(const.value, str) \
                        and not const.value.endswith("-symbolic"):
                    from_module.add(const.value)
    return from_module


@pytest.mark.parametrize("text", sorted(
    (strings_in("gui.py") | strings_in("app.py") | strings_in("engine.py") | strings_in("sign.py")
     | strings_in("signing.py") | app_constants() | {NOT_ANSWERING})
    - i18n.NEUTRAL))
def test_has_italian(text):
    assert text in i18n._IT, f"missing Italian translation for {text!r}"


def test_translations_keep_placeholders():
    for en, it in i18n._IT.items():
        assert sorted(part.split("}")[0] for part in en.split("{")[1:]) == \
            sorted(part.split("}")[0] for part in it.split("{")[1:]), en


def test_language_selection(monkeypatch):
    for var in ("LANGUAGE", "LC_ALL", "LC_MESSAGES"):
        monkeypatch.delenv(var, raising=False)
    monkeypatch.setenv("LANG", "it_IT.UTF-8")
    i18n.set_language("auto")
    assert i18n._("Connect") == "Collega"
    i18n.set_language("en")
    assert i18n._("Connect") == "Connect"
    monkeypatch.setenv("LANG", "de_DE.UTF-8")
    i18n.set_language("auto")
    assert i18n._("Connect") == "Connect"
