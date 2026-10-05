# Tray digit fonts

League Gothic 2.001 by The League of Moveable Type, under the SIL Open Font License 1.1
([OFL.txt](OFL.txt)). Upstream: <https://github.com/theleagueof/league-gothic>; source file:
google/fonts `ofl/leaguegothic/LeagueGothic[wdth].ttf`.

Static instances subset to `0-9`: `LeagueGothic-Digits.ttf` (`wdth=100`) and
`LeagueGothicCondensed-Digits.ttf` (`wdth=75`). Regenerate with:

```sh
fonttools varLib.instancer 'LeagueGothic[wdth].ttf' wdth=100 -o i.ttf
pyftsubset i.ttf --text=0123456789 --layout-features='' --no-hinting --desubroutinize --name-IDs='*'
```

Same with `wdth=75` for the condensed file.
