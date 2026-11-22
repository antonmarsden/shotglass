# Test fonts

Subsets of Noto fonts (SIL Open Font License 1.1, see `OFL.txt`; no Reserved Font
Names), made with fonttools' `pyftsubset --no-hinting` from the variable fonts in
[google/fonts](https://github.com/google/fonts):

| file | from | kept |
|---|---|---|
| `NotoSans-subset.ttf` | `ofl/notosans/NotoSans[wdth,wght].ttf` | Basic Latin, Latin-1, dashes, quotes, ellipsis; `wght`/`wdth` axes |
| `NotoSansHebrew-subset.ttf` | `ofl/notosanshebrew/NotoSansHebrew[wdth,wght].ttf` | Hebrew letters and points |
| `NotoNaskhArabic-subset.ttf` | `ofl/notonaskharabic/NotoNaskhArabic[wght].ttf` | Arabic letters, harakat, Arabic-Indic digits |

Layout features kept: kern, liga, calt, mark, mkmk, init, medi, fina, isol, rlig, ccmp,
locl, ss01, tnum. Used by `tests/films/text` (the golden film), the text unit tests and the selftest
(embedded in the binary).
