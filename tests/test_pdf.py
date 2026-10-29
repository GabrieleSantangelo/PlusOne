"""Saving marks onto a PDF: content kept, marks where the pen put them."""
import os
import threading

import cairo
import pytest

from plusone.pdf import Stroke, Text, display_size, output_path, save_signed

pypdf = pytest.importorskip("pypdf")


def make_pdf(path, rotate_second=0):
    """Two A4 pages with some text; the second one optionally rotated."""
    surface = cairo.PDFSurface(str(path), 595, 842)
    cr = cairo.Context(surface)
    for n in (1, 2):
        cr.set_font_size(14)
        cr.move_to(72, 72)
        cr.show_text(f"Original text page {n}")
        cr.show_page()
    surface.finish()
    if rotate_second:
        writer = pypdf.PdfWriter(clone_from=pypdf.PdfReader(str(path)))
        writer.pages[1].rotate(rotate_second)
        with open(path, "wb") as f:
            writer.write(f)


def blob(x, y):
    """A thick dot of ink at display position (x, y)."""
    return Stroke([(x, y, 1.0), (x + 0.1, y, 1.0)], width=12)


def ink_at(path, index, x, y):
    """Darkness of the rendered page around display point (x, y), 0..1."""
    gi = pytest.importorskip("gi")
    try:
        gi.require_version("Poppler", "0.18")
    except ValueError:
        pytest.skip("poppler introspection data not installed")
    from gi.repository import Poppler
    page = Poppler.Document.new_from_file(path.as_uri(), None).get_page(index)
    w, h = page.get_size()
    surface = cairo.ImageSurface(cairo.FORMAT_RGB24, int(w), int(h))
    cr = cairo.Context(surface)
    cr.set_source_rgb(1, 1, 1)
    cr.paint()
    page.render(cr)
    surface.flush()
    data, stride = surface.get_data(), surface.get_stride()
    px = data[int(y) * stride + int(x) * 4: int(y) * stride + int(x) * 4 + 3]
    return 1 - sum(px) / (3 * 255)


def test_output_path_never_overwrites(tmp_path):
    src = tmp_path / "contract.pdf"
    assert output_path(str(src)) == str(tmp_path / "contract-signed.pdf")
    (tmp_path / "contract-signed.pdf").write_bytes(b"x")
    assert output_path(str(src)) == str(tmp_path / "contract-signed-2.pdf")


def test_a_link_to_nothing_named_as_the_copy_is_left_as_it_is(tmp_path, monkeypatch):
    # exists() says no for it, link() says it is there: tried again and
    # again, for ever.
    src = tmp_path / "contract.pdf"
    make_pdf(src)
    os.symlink(str(tmp_path / "gone.pdf"), str(tmp_path / "contract-signed.pdf"))
    tried, link = [], os.link

    def counted(a, b):
        tried.append(b)
        if len(tried) > 100:
            raise RuntimeError("the same name tried again and again")
        return link(a, b)
    monkeypatch.setattr(os, "link", counted)
    done = save_signed(str(src), {0: [blob(100, 100)]})
    assert done == str(tmp_path / "contract-signed-2.pdf") and os.path.isfile(done)
    assert os.path.islink(tmp_path / "contract-signed.pdf")


def test_a_name_taken_meanwhile_is_passed_by(tmp_path, monkeypatch):
    src = tmp_path / "contract.pdf"
    make_pdf(src)
    tried, link = [], os.link

    def taken_first(a, b):
        tried.append(b)
        if len(tried) > 100:
            raise RuntimeError("the same name tried again and again")
        if b.endswith("contract-signed.pdf"):
            raise FileExistsError(17, "File exists")  # appeared since output_path() looked (it stays unseen)
        return link(a, b)
    monkeypatch.setattr(os, "link", taken_first)
    assert save_signed(str(src), {0: [blob(100, 100)]}) == str(tmp_path / "contract-signed-2.pdf")


def test_text_kept_and_marks_added(tmp_path):
    src = tmp_path / "doc.pdf"
    make_pdf(src)
    out = save_signed(str(src), {0: [Text(80, 400, "SIGNED-HERE")], 1: []})
    assert out.endswith("doc-signed.pdf")
    reader = pypdf.PdfReader(out)
    assert len(reader.pages) == 2
    first = reader.pages[0].extract_text()
    assert "Original text page 1" in first and "SIGNED-HERE" in first
    assert "SIGNED-HERE" not in reader.pages[1].extract_text()
    # The original is untouched.
    assert "SIGNED-HERE" not in pypdf.PdfReader(str(src)).pages[0].extract_text()


@pytest.mark.parametrize("rotation", [0, 90, 180, 270])
def test_ink_lands_where_drawn(tmp_path, rotation):
    src = tmp_path / "doc.pdf"
    make_pdf(src, rotate_second=rotation)
    page = pypdf.PdfReader(str(src)).pages[1]
    width, height = display_size(page)
    # Near the top-left corner of the page as displayed: a wrong rotation or
    # flip would move the dot to another corner.
    x, y = 60, 90
    out = tmp_path / "out.pdf"
    save_signed(str(src), {1: [blob(x, y)]}, dst=str(out))
    assert ink_at(out, 1, x, y) > 0.5
    for other in ((width - x, y), (x, height - y), (width - x, height - y)):
        assert ink_at(out, 1, *other) < 0.1, other


@pytest.mark.skipif(os.geteuid() == 0, reason="root ignores folder permissions")
def test_read_only_folder_raises_permission_error(tmp_path):
    # The signing window answers PermissionError with a "save as" dialog
    # (read-only folders, and Flatpak documents shared one file at a time).
    folder = tmp_path / "ro"
    folder.mkdir()
    src = folder / "doc.pdf"
    make_pdf(src)
    folder.chmod(0o555)
    try:
        with pytest.raises(PermissionError):
            save_signed(str(src), {0: [Text(80, 400, "x")]})
    finally:
        folder.chmod(0o755)


def test_a_crop_box_beyond_the_media_box(tmp_path):
    # Poppler shows the crop box within the media box; the ink goes where it
    # was drawn on what poppler showed.
    src, dst = tmp_path / "a.pdf", tmp_path / "b.pdf"
    make_pdf(src)
    writer = pypdf.PdfWriter(clone_from=pypdf.PdfReader(str(src)))
    writer.pages[0].cropbox = pypdf.generic.RectangleObject([-100, -100, 695, 942])
    with open(src, "wb") as f:
        writer.write(f)
    save_signed(str(src), {0: [blob(300, 400)]}, str(dst))
    assert ink_at(dst, 0, 300, 400) > 0.5 and ink_at(dst, 0, 200, 300) < 0.1


def test_the_original_content_is_left_as_it_is(tmp_path):
    # No re-encoding (a big scanned page doubled in size), and no state of
    # it leaking into the ink: its streams are the same objects, only wrapped.
    src, dst = tmp_path / "a.pdf", tmp_path / "b.pdf"
    make_pdf(src)
    original = pypdf.PdfReader(str(src)).pages[0].get_contents().get_data()
    save_signed(str(src), {0: [blob(100, 100)], 7: [blob(1, 1)]}, str(dst))  # page 7: none, left out
    page = pypdf.PdfReader(str(dst)).pages[0]
    streams = [s.get_object().get_data() for s in page["/Contents"]]
    assert streams[0] == b"q\n" and original in streams and streams[-1].rstrip().endswith(b"Do Q")


def test_no_info_dictionary(tmp_path):
    # pypdf 4.0 pointed /Info at the page tree when the original had none.
    src, dst = tmp_path / "a.pdf", tmp_path / "b.pdf"
    make_pdf(src)
    # cairo's PDFs carry /Info; one without: the trailer rewritten by hand.
    data = src.read_bytes()
    reader = pypdf.PdfReader(str(src))
    assert "/Info" in reader.trailer
    info_ref = reader.trailer.raw_get("/Info")
    data = data.replace(f"/Info {info_ref.idnum} {info_ref.generation} R".encode(), b"")
    src.write_bytes(data)
    assert "/Info" not in pypdf.PdfReader(str(src)).trailer
    save_signed(str(src), {0: [blob(100, 100)]}, str(dst))
    info = pypdf.PdfReader(str(dst)).trailer.get("/Info")
    assert info is None or info.get_object().get("/Type") != "/Pages"


def test_any_script_as_text(tmp_path):
    src, dst = tmp_path / "a.pdf", tmp_path / "b.pdf"
    make_pdf(src)
    save_signed(str(src), {0: [Text(72, 300, "名前 مرحبا Ελληνικά", size=14)]}, str(dst))
    assert os.path.getsize(dst) > 0  # drawn without boxes is for the eye; here: it saves


def test_the_copy_keeps_the_originals_permissions(tmp_path):
    src, dst = tmp_path / "a.pdf", tmp_path / "b.pdf"
    make_pdf(src)
    os.chmod(src, 0o600)
    save_signed(str(src), {0: [blob(100, 100)]}, str(dst))
    assert os.stat(dst).st_mode & 0o777 == 0o600
    assert not [p for p in os.listdir(tmp_path) if p.endswith(".part")]


def test_a_digital_signature_is_noticed(tmp_path):
    from plusone.pdf import digitally_signed
    src = tmp_path / "a.pdf"
    make_pdf(src)
    assert not digitally_signed(str(src))
    writer = pypdf.PdfWriter(clone_from=pypdf.PdfReader(str(src)))
    writer._root_object[pypdf.generic.NameObject("/AcroForm")] = pypdf.generic.DictionaryObject(
        {pypdf.generic.NameObject("/SigFlags"): pypdf.generic.NumberObject(3),
         pypdf.generic.NameObject("/Fields"): pypdf.generic.ArrayObject()})
    with open(src, "wb") as f:
        writer.write(f)
    assert digitally_signed(str(src))


def test_a_pdf_that_needs_a_password_is_said_before_it_goes(tmp_path):
    from pypdf import PdfWriter
    from plusone.pdf import problem
    writer = PdfWriter()
    writer.add_blank_page(200, 200)
    writer.encrypt("secret")
    path = tmp_path / "locked.pdf"
    with open(path, "wb") as f:
        writer.write(f)
    assert problem(str(path))[0] == "{name} needs a password to open: unlock it first"


def test_page_boxes_with_indirect_corners(tmp_path):
    # A height kept as an indirect object: the box as poppler reads it, not US Letter.
    from pypdf import PdfReader, PdfWriter
    from pypdf.generic import ArrayObject, FloatObject, NameObject
    from plusone.pdf import visible_box
    writer = PdfWriter()
    page = writer.add_blank_page(300, 400)
    height = writer._add_object(FloatObject(400))
    page[NameObject("/MediaBox")] = ArrayObject([FloatObject(0), FloatObject(0), FloatObject(300), height])
    path = tmp_path / "indirect.pdf"
    with open(path, "wb") as f:
        writer.write(f)
    assert visible_box(PdfReader(str(path)).pages[0]) == (0.0, 0.0, 300.0, 400.0)


def test_a_page_tree_that_is_its_own_parent(tmp_path):
    # Nothing to inherit, and no end going up it: the save went round at
    # 100 % for ever (the window could not close, Plus One stayed).
    from pypdf.generic import NameObject
    writer = pypdf.PdfWriter()
    page = writer.add_blank_page(200, 200)
    if "/Resources" in page:
        del page[NameObject("/Resources")]
    writer._root_object["/Pages"][NameObject("/Parent")] = writer._root_object.raw_get("/Pages")
    src = tmp_path / "loop.pdf"
    with open(src, "wb") as f:
        writer.write(f)
    done = {}
    saving = threading.Thread(target=lambda: done.update(path=save_signed(str(src), {0: [blob(100, 100)]})),
                              daemon=True)
    saving.start()
    saving.join(5)
    assert not saving.is_alive() and os.path.isfile(done["path"])
