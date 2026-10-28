# Generates EmojiSbixTest.ttf: `uv run --with fonttools python make_emoji_sbix_test.py`
import struct, zlib
from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.ttLib import newTable
from fontTools.ttLib.tables.sbixGlyph import Glyph as SbixGlyph
from fontTools.ttLib.tables.sbixStrike import Strike

def png(w, h, rgba):
    raw = b"".join(b"\x00" + bytes(rgba) * w for _ in range(h))
    def chunk(t, d):
        c = struct.pack(">I", len(d)) + t + d
        return c + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")

chars = {0x2665: "heart", 0x26A0: "warning", 0x263A: "smile", 0x2714: "check", 0x25B6: "play", 0x1F44D: "thumbsup", 0x2605: "star"}
# Note: only some of these get bitmaps (color); the rest are plain outlines in the same font
color = {0x2665, 0x26A0, 0x263A, 0x2714, 0x25B6, 0x1F44D}
order = [".notdef"] + list(chars.values())
fb = FontBuilder(1000, isTTF=True)
fb.setupGlyphOrder(order)
fb.setupCharacterMap(chars)
pen = TTGlyphPen(None)
pen.moveTo((100, 0)); pen.lineTo((100, 700)); pen.lineTo((600, 700)); pen.lineTo((600, 0)); pen.closePath()
box = pen.glyph()
fb.setupGlyf({n: box for n in order})
fb.setupHorizontalMetrics({n: (800, 100) for n in order})
fb.setupHorizontalHeader(ascent=800, descent=-200)
fb.setupNameTable({"familyName": "EmojiSbixTest", "styleName": "Regular"})
fb.setupOS2(sTypoAscender=800, sTypoDescender=-200, usWinAscent=800, usWinDescent=200)
fb.setupPost()
sbix = newTable("sbix")
sbix.version = 1; sbix.flags = 1
sbix.strikes = {}
strike = Strike(ppem=32, resolution=72)
for cp, name in chars.items():
    if cp in color:
        strike.glyphs[name] = SbixGlyph(glyphName=name, graphicType="png", imageData=png(8, 8, (255, 0, 0, 255)), originOffsetX=0, originOffsetY=0)
sbix.strikes[32] = strike
fb.font["sbix"] = sbix
fb.save("EmojiSbixTest.ttf")
