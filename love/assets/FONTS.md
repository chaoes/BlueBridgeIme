# Bundled fonts

- `DejaVuSans.ttf`, `DejaVuSans-Bold.ttf` — DejaVu fonts (Bitstream Vera / public domain derived).
  Used for Latin, Greek and Cyrillic text and as a fallback when no CJK font is installed.
- CJK text uses a system font at runtime (Noto Sans CJK, WenQuanYi Micro Hei, PingFang, or
  Microsoft YaHei). LÖVE has no font fallback, so one family is picked for the whole string.
