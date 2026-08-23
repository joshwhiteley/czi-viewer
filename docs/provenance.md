# CZI implementation provenance

This project implements CZI behavior from public documentation, independently observed fixture structure, and interoperability tests.

## Rules

- Do not translate ZEISS/libCZI line by line.
- Do not copy LGPL or GPL implementation code or tests.
- Do not commit non-redistributable CZI specifications or fixtures.
- Record public references for format decisions that are not demonstrated by project-owned tests.
- Preserve unknown fields and segments rather than inventing semantics.
- Use libCZI, CZICheck, ZEN, and Bio-Formats only as external development-time compatibility oracles.

## Initial public references

- ZEISS CZI overview: <https://www.zeiss.com/microscopy/en/products/software/zeiss-zen/czi-image-file-format.html>
- ZEISS libCZI repository: <https://github.com/ZEISS/libczi>
- `czi-rs` public API and permissively licensed source: <https://github.com/keejkrej/czi-rs>
- OpenSSH protocol documentation: <https://github.com/openssh/openssh-portable/tree/master>
- BaSiC publication: Peng et al., “A BaSiC tool for background and shading correction of optical microscopy images,” *Nature Communications* 8, 14836 (2017), <https://doi.org/10.1038/ncomms14836>

A source review does not imply that source was copied. Implementation commits must cite project tests or the relevant public reference when format behavior is not obvious.
