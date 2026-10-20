"""Assembles image-test.epub: two pages, one orientation PNG each."""

import zipfile

BOOK = ".scratch/png-images/image-test.epub"

container = """<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"""

opf = """<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">urn:uuid:00000000-0000-0000-0000-000000000001</dc:identifier>
    <dc:title>Image Orientation Test</dc:title>
    <dc:creator>marigold-os</dc:creator>
    <dc:language>en</dc:language>
    <meta property="dcterms:modified">2026-10-20T00:00:00Z</meta>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="p1" href="page1.xhtml" media-type="application/xhtml+xml"/>
    <item id="p2" href="page2.xhtml" media-type="application/xhtml+xml"/>
    <item id="tall" href="images/tall.png" media-type="image/png"/>
    <item id="full" href="images/fullpage.png" media-type="image/png"/>
  </manifest>
  <spine><itemref idref="p1"/><itemref idref="p2"/></spine>
</package>"""

nav = """<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Contents</title></head>
<body><nav epub:type="toc"><ol>
<li><a href="page1.xhtml">Tall marker</a></li>
<li><a href="page2.xhtml">Full-page marker</a></li>
</ol></nav></body></html>"""

def page(src, label):
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>{label}</title></head>
<body><p><img src="{src}" alt="{label}"/></p></body></html>"""

with zipfile.ZipFile(BOOK, "w") as z:
    z.writestr(zipfile.ZipInfo("mimetype"), "application/epub+zip", compress_type=zipfile.ZIP_STORED)
    z.writestr("META-INF/container.xml", container, compress_type=zipfile.ZIP_DEFLATED)
    z.writestr("OEBPS/content.opf", opf, compress_type=zipfile.ZIP_DEFLATED)
    z.writestr("OEBPS/nav.xhtml", nav, compress_type=zipfile.ZIP_DEFLATED)
    z.writestr("OEBPS/page1.xhtml", page("images/tall.png", "Tall marker"), compress_type=zipfile.ZIP_DEFLATED)
    z.writestr("OEBPS/page2.xhtml", page("images/fullpage.png", "Full-page marker"), compress_type=zipfile.ZIP_DEFLATED)
    z.write(".scratch/png-images/orientation-test.png", "OEBPS/images/tall.png", compress_type=zipfile.ZIP_STORED)
    z.write(".scratch/png-images/orientation-fullpage.png", "OEBPS/images/fullpage.png", compress_type=zipfile.ZIP_STORED)
print("wrote", BOOK)
