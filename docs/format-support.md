# Format support

CZI Viewer reads a deliberate, bounded subset of CZI for interactive viewing.

## Supported today

- Tiled mosaics and pyramid levels described by the CZI summary directory.
- Sparse C, S, Z, and T plane selection.
- Exact viewport queries over tile geometry; these queries do not read pixel payloads.
- Uncompressed Gray8 and Gray16 tile payloads.
- Per-tile X/Y stored dimensions and logical-to-stored pyramid ratios.
- Global metadata XML when it fits configured bounds, including best-effort channel names, calibration, acquisition date, and objective fields.
- Local random-access files and read-only SFTP random-access sources.

Pyramid selection uses the finest available level that is not undersampled. The app retains the previous coarser level while the requested level loads.

## Not supported or intentionally limited

- Compressed pixel codecs, including JPEG and other compressed payloads.
- Dense full-frame or full-mosaic decoding.
- CZI/TIFF conversion or corrected-file export.
- A guarantee that malformed, oversized, or vendor-specific metadata can be completely retained.
- Quantitative validation of the optional BaSiC display preview.

Unsupported tile pixel types, compression, malformed segments, unsafe counts, offsets, and payload sizes return structured errors. Metadata is different: a failed, malformed, or over-limit global metadata block records a diagnostic and does not by itself stop an image from opening.

## Metadata behavior

The metadata reader uses plain XML parsing with explicit limits on input, nodes, depth, text, attributes, allocation, raw XML retention, and summary extraction. Namespace prefixes are not retained in element names. Unknown elements remain in document order when retained. Raw XML is available only when it fits the 2 MiB raw-XML limit.

The viewer treats CZI metadata as vendor data. It reports partial or malformed metadata rather than claiming a complete semantic interpretation.

## Test coverage and provenance

The parser tests exercise valid synthetic segments, malformed headers and counts, unsupported schemas/codecs/pixels, lazy payload validation, metadata diagnostics, attachments, overflow, and random-access bounds. Query tests exercise sparse planes, noncanonical dimensions, negative coordinates, scales, exact-plane filtering, and paint order.

The synthetic demo CZI has three named channels and a tiled native/coarse pyramid. Generate it with the [user-guide command](user-guide.md#make-safe-demo-data); it is ignored and contains no external microscopy data.

CZI behavior is independently implemented. See [implementation provenance](provenance.md) for public references and rules. The project is not affiliated with, endorsed by, or sponsored by ZEISS.
