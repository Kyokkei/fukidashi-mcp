# Bundled comic fonts

The MCP embeds these unmodified TTF files with `include_bytes!` in
`src/fonts.rs`. A typeset request with no explicit `font_path` uses Comic Neue
Regular and copies it atomically into the managed job's `fonts/` directory.
Caller fallbacks retain precedence; the built-in fallback order then uses
Patrick Hand Regular for glyph coverage (including Vietnamese) and Comic Neue
Bold for an available heavier face. Configured and platform font discovery
remains the final fallback for scripts these three faces do not contain.

The files were downloaded from the authoritative Google Fonts repository at
commit `5e35378e6bda803962ee6fd257e444a7d459660`:

- [Comic Neue directory](https://github.com/google/fonts/tree/5e35378e6bda803962ee6fd257e444a7d459660/ofl/comicneue)
- [Patrick Hand directory](https://github.com/google/fonts/tree/5e35378e6bda803962ee6fd257e444a7d459660/ofl/patrickhand)

`ComicNeue-OFL.txt` and `PatrickHand-OFL.txt` are the upstream SIL Open Font
License 1.1 texts. `ComicNeue-METADATA.pb` and `PatrickHand-METADATA.pb` are
the corresponding upstream metadata files and are retained for attribution
and provenance.

Attribution from the upstream metadata:

- Comic Neue: Comic Neue Project Authors, copyright 2014; designer Craig
  Rozynski and Hrant Papazian.
- Patrick Hand: Patrick Wagesreiter, copyright 2010–2012; designer Patrick
  Wagesreiter.

SHA-256 hashes of the vendored TTFs:

| File | SHA-256 |
| --- | --- |
| `ComicNeue-Regular.ttf` | `a0ee5a37c8b27c4db0700137d928598b1e23b0089e1546a8961909176b779360` |
| `ComicNeue-Bold.ttf` | `3e7e5fccfd7e0788f317b43312151c1bd5cf058c9697a8d83eac3939050bd61e` |
| `PatrickHand-Regular.ttf` | `0f173b3e6cb6d1af25babf7f0057c5ac4ee11f9992b0469bb817e967ef4ad0fc` |

Patrick Hand Regular's official metadata declares the `vietnamese` subset.
The repository test also checks the Vietnamese base letters ă, â, đ, ê, ô, ơ,
ư, their uppercase forms, precomposed tone forms, and combining marks. Comic
Neue's official metadata declares the smaller `latin` subset, so Vietnamese
grapheme clusters deliberately reach Patrick Hand through per-grapheme
fallback while supported ASCII clusters stay in Comic Neue.

When distributing a binary, retain the two OFL texts and this attribution with
the release notices. The binary itself contains the font bytes; it does not
read this directory at runtime and does not download fonts.
